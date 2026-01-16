// main.rs

mod config;
mod detector;
mod executor;
mod metrics;
mod types;
mod dual_provider;
mod flashloan;
mod pool_oracle;
mod pool_discovery;
mod circuit_breaker;
mod multicall_helper;

use anyhow::{Context, Result};
use config::Config;
use detector::OpportunityDetector;
use executor::TradeExecutor;
use metrics::MetricsServer;
use types::BotMetrics;
use dual_provider::{DualProvider, DualRpcConfig};
use pool_oracle::PoolOracle;
use flashloan::{FlashLoanExecutor, ExecutionMode, is_profitable_with_flashloan};

use std::sync::Arc;
use tokio::sync::RwLock;
use ethers::signers::Signer;
use ethers::types::U256;
use tracing::{error, info, warn, debug};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// ========== CONSTANTES ==========
const SCAN_INTERVAL_FAST_MS: u64 = 5000;  // 5s si opportunités trouvées
const SCAN_INTERVAL_NORMAL_MS: u64 = 10000; // 10s sinon
const STATS_INTERVAL_SECS: u64 = 600;
const CACHE_CLEANUP_INTERVAL_SECS: u64 = 300;
const PRICE_UPDATE_INTERVAL_SECS: u64 = 30;
const METRICS_UPDATE_INTERVAL_SECS: u64 = 300;

#[tokio::main]
async fn main() -> Result<()> {
    // ========== LOGGING ==========
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("🤖 Starting Advanced Arbitrage Bot v3.0...");
    info!("═══════════════════════════════════════════════════════════");

    // ========== CONFIGURATION ==========
    let config = Arc::new(Config::from_env()?);
    config.validate()?;

    info!("✅ Configuration loaded and validated");
    info!("📍 Network: Base (Chain ID: {})", config.network.chain_id);
    info!("📍 Bot contract: {:?}", config.bot.contract_address);
    info!("💰 Min profit: ${:.2}", config.trading.min_profit_usd);
    info!("🔒 Dry run: {}", config.safety.enable_dry_run);
    info!("═══════════════════════════════════════════════════════════");

    // ========== DUAL RPC SETUP ==========
    info!("🔗 Setting up Dual RPC providers...");

    let dual_rpc_config = DualRpcConfig {
        _read_rpc_url: config.network.read_rpc_url.clone(),
        read_wss_urls: vec![
            Some(config.network.read_wss_url.clone()),
            std::env::var("ANKR_WSS").ok(),
            std::env::var("QUICKNODE_WSS").ok(),
        ]
        .into_iter()
        .flatten()
        .collect(),
        write_rpc_url: config.network.write_rpc_url.clone(),
        write_type: match config.network.write_rpc_type.to_lowercase().as_str() {
            "basebuilder" | "base" | "builder" => dual_provider::WriteRpcType::BaseBuilder,
            "eden" | "edennetwork" => dual_provider::WriteRpcType::EdenNetwork,
            "flashbots" => dual_provider::WriteRpcType::Flashbots,
            _ => dual_provider::WriteRpcType::Standard,
        },
        _flashbots_auth_key: config.mev.flashbots_auth_key.clone(),
        _eden_api_key: config.mev.eden_api_key.clone(),
    };

    let dual_provider = Arc::new(DualProvider::new(dual_rpc_config).await?);

    info!("✅ Dual RPC providers connected");
    info!("   📖 READ:  {}", config.network.read_wss_url);
    info!("   ✍️  WRITE: {} ({:?})",
        config.network.write_rpc_url,
        dual_provider.write_type()
    );
    info!("═══════════════════════════════════════════════════════════");

    // Vérifier la connexion
    let block_number = dual_provider.get_block_number().await?;
    info!("📦 Current block: {}", block_number);

    // ========== POOL ORACLE ==========
    info!("🔮 Initializing Pool Oracle (real-time DEX prices)...");
    let pool_oracle = Arc::new(PoolOracle::new(config.clone(), dual_provider.clone())?);
    pool_oracle.update_weth_price().await?;
    info!("✅ Pool Oracle initialized");

    // ========== NONCE TRACKER PARTAGÉ ==========
    info!("🔢 Initializing shared nonce tracker...");

    let wallet: ethers::signers::LocalWallet = config.bot.private_key
        .trim_start_matches("0x")
        .parse()
        .context("Invalid private key")?;
    let wallet = wallet.with_chain_id(config.network.chain_id);

    info!("   👛 Wallet address: {:?}", wallet.address());

    let initial_nonce = dual_provider
        .get_transaction_count(wallet.address(), None)
        .await?;

    let nonce_tracker = Arc::new(RwLock::new(initial_nonce.as_u64()));

    info!("✅ Nonce tracker initialized");
    info!("🔢 Initial nonce: {}", initial_nonce);
    info!("═══════════════════════════════════════════════════════════");

    // ========== INITIALIZE COMPONENTS ==========

    // Detector
    let detector = Arc::new(OpportunityDetector::new(
        config.clone(),
        dual_provider.clone(),
        pool_oracle.clone(),
    )?);

    info!("✅ Opportunity Detector initialized (with parallel scanning)");

    info!("═══════════════════════════════════════════════════════════");
    info!("🔍 Initializing pool discovery system...");
    info!("   This will take 30-60 seconds but speeds up all future scans");

    match detector.initialize_pool_discovery().await {
        Ok(_) => {
            info!("✅ Pool discovery system ready!");
            info!("   All pools are now cached for fast scanning");
        }
        Err(e) => {
            error!("❌ Failed to initialize pool discovery: {:?}", e);
            error!("   The bot will continue but may be slower");
        }
    }
    info!("═══════════════════════════════════════════════════════════");

    info!("🔍 Checking blacklisted pools...");
    let blacklist_count = detector.get_blacklisted_count().await;
    info!("   • {} pools currently blacklisted", blacklist_count);
    if blacklist_count > 0 {
        let blacklisted = detector.get_blacklisted_pools_list(5).await;
        for pool in &blacklisted {
            warn!("   🚫 Blacklisted: {:?}", pool);
        }
    }


    // Executor
    let executor = Arc::new(TradeExecutor::new_with_nonce(
        config.clone(),
        dual_provider.clone(),
        nonce_tracker.clone(),
    ).await?);

    info!("✅ Trade Executor initialized with shared nonce");

    // Flash Loan Executor
    let flashloan_executor = if config.trading.enable_flash_loans {
        info!("⚡ Initializing Flash Loan Executor...");
        let contract_address = config.bot.contract_address;

        let fl_executor = Arc::new(FlashLoanExecutor::new_with_nonce(
            contract_address,
            config.clone(),
            dual_provider.clone(),
            nonce_tracker.clone(),
        )?);

        // Afficher les métriques du contrat
        match fl_executor.get_metrics().await {
            Ok((trades, profit, remaining)) => {
                info!("   📊 Contract metrics:");
                info!("      • Total trades: {}", trades);
                info!("      • Total profit: {} wei", profit);
                info!("      • Trades remaining this hour: {}", remaining);
            }
            Err(e) => warn!("   ⚠️  Failed to get metrics: {}", e),
        }

        Some(fl_executor)
    } else {
        info!("⚠️  Flash loans disabled in config");
        None
    };

    let metrics = Arc::new(RwLock::new(BotMetrics::new()));

    info!("✅ All components initialized with shared nonce tracker");
    info!("═══════════════════════════════════════════════════════════");

    // Afficher les prix actuels
    //let tokens: Vec<_> = detector.get_tokens()
    //    .iter()
    //    .map(|t| (t.address, t.symbol.clone()))
    //    .collect();
    //pool_oracle.print_all_prices(&tokens).await;

    // ========== BACKGROUND TASKS ==========

    // Task 1: Metrics Server
    if config.monitoring.enable_metrics {
        let metrics_clone = metrics.clone();
        let config_clone = config.clone();
        tokio::spawn(async move {
            if let Err(e) = start_metrics_server(config_clone, metrics_clone).await {
                error!("Metrics server error: {}", e);
            }
        });
    }

    // Task 2: Cache Cleanup
    {
        let detector_cleanup = detector.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                tokio::time::Duration::from_secs(CACHE_CLEANUP_INTERVAL_SECS)
            );
            loop {
                interval.tick().await;
                detector_cleanup.cleanup_cache().await;
            }
        });
    }

    // Task 3: Contract Metrics
    if let Some(ref fl_exec) = flashloan_executor {
        let fl_exec_clone = fl_exec.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                tokio::time::Duration::from_secs(METRICS_UPDATE_INTERVAL_SECS)
            );
            loop {
                interval.tick().await;
                if let Ok((trades, profit, remaining)) = fl_exec_clone.get_metrics().await {
                    info!("📊 Contract Stats:");
                    info!("   • Trades: {} | Profit: ${:.2} | Remaining: {}",
                        trades,
                        profit.as_u128() as f64 / 1e6,
                        remaining
                    );
                }
            }
        });
    }

    // Task 4: Cleanup stale locks
{
    let pool_oracle_cleanup = pool_oracle.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            tokio::time::Duration::from_secs(60)
        );
        loop {
            interval.tick().await;
            pool_oracle_cleanup.cleanup_stale_locks().await;
        }
    });
}


    // Task 5: Stats Printing
    {
        let detector_stats = detector.clone();
        let metrics_stats = metrics.clone();
        let dual_provider_stats = dual_provider.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                tokio::time::Duration::from_secs(STATS_INTERVAL_SECS)
            );
            loop {
                interval.tick().await;
                detector_stats.print_stats().await;
                print_metrics(&metrics_stats).await;
                dual_provider_stats.print_stats().await;
            }
        });
    }

    // Task 6: Price Updates
    {
        let pool_oracle_update = pool_oracle.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                tokio::time::Duration::from_secs(PRICE_UPDATE_INTERVAL_SECS)
            );
            loop {
                interval.tick().await;
                if let Err(e) = pool_oracle_update.update_weth_price().await {
                    warn!("Failed to update WETH price: {}", e);
                }
            }
        });
    }

    detector.reset_blacklist().await;
    info!("🔄 Blacklist reset - all pools re-enabled");


    // ========== START MAIN BOT LOOP ==========
    info!("═══════════════════════════════════════════════════════════");
    info!("🚀 Bot is now running! Scanning for opportunities...");
    info!("🔒 MEV Protection: {:?}", dual_provider.write_type());
    info!("🔮 Pool Oracle: ENABLED (Multi-source validation)");
    info!("⚡ Flash Loans: {}", if flashloan_executor.is_some() { "ENABLED" } else { "DISABLED" });
    info!("🔄 Parallel Scanning: ENABLED");
    if config.safety.enable_dry_run {
        warn!("🔍 DRY RUN MODE - Transactions will NOT be sent!");
    }
    info!("═══════════════════════════════════════════════════════════");

    run_bot_loop(config, detector, executor, flashloan_executor, metrics, pool_oracle).await?;

    Ok(())
}

// ========== MAIN BOT LOOP ==========

async fn run_bot_loop(
    config: Arc<Config>,
    detector: Arc<OpportunityDetector>,
    executor: Arc<TradeExecutor>,
    flashloan_executor: Option<Arc<FlashLoanExecutor>>,
    metrics: Arc<RwLock<BotMetrics>>,
    pool_oracle: Arc<PoolOracle>,
) -> Result<()> {
    let mut trades_this_hour = 0u32;
    let mut last_trade_time = std::time::Instant::now();
    let mut last_hour_reset = std::time::Instant::now();
    let start_time = std::time::Instant::now();
    let mut iteration = 0u64;
    let mut last_cache_cleanup = std::time::Instant::now();
    let mut last_cache_refresh = std::time::Instant::now();

    info!("🚀 Bot loop started - scanning every {}ms", 
        if SCAN_INTERVAL_NORMAL_MS > 1000 { 
            format!("{:.1}s", SCAN_INTERVAL_NORMAL_MS as f64 / 1000.0)
        } else {
            format!("{}ms", SCAN_INTERVAL_NORMAL_MS)
        }
    );

    loop {
        iteration += 1;

        // Mettre à jour l'uptime
        {
            let mut m = metrics.write().await;
            m.uptime_seconds = start_time.elapsed().as_secs();
        }

        // Reset horaire
        if last_hour_reset.elapsed().as_secs() >= 3600 {
            info!("🔄 Hourly reset - trades this hour: {}", trades_this_hour);
            trades_this_hour = 0;
            last_hour_reset = std::time::Instant::now();
        }

        // Vérifier limite de trades
        if trades_this_hour >= config.safety.max_trades_per_hour {
            warn!("⚠️  Max trades per hour reached ({}), waiting...", config.safety.max_trades_per_hour);
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            continue;
        }

        // Reset blacklist toutes les heures
        if last_hour_reset.elapsed().as_secs() >= 3600 {
            info!("🔄 Hourly reset - trades: {}", trades_this_hour);
            trades_this_hour = 0;
            last_hour_reset = std::time::Instant::now();
            
            detector.reset_blacklist().await;
            detector.reset_metrics().await;
        }


        // Stats périodiques
        if iteration % 50 == 0 {
            info!("📊 Iteration {} (uptime: {:.1}h, trades: {})",
                iteration,
                start_time.elapsed().as_secs() as f64 / 3600.0,
                trades_this_hour
            );

            detector.print_stats().await;
            detector.print_detection_stats().await;
        }

        if last_cache_cleanup.elapsed().as_secs() >= 300 {
            debug!("🧹 Cleaning up expired cache entries...");
            detector.cleanup_expired_cache().await;
            last_cache_cleanup = std::time::Instant::now();
        }

        if last_cache_refresh.elapsed().as_secs() >= 600 {
            info!("🔄 Periodic cache refresh (every 10 minutes)...");
            
            match detector.initialize_pool_discovery().await {
                Ok(_) => {
                    info!("✅ Cache refresh complete - pools updated");
                }
                Err(e) => {
                    warn!("⚠️  Cache refresh failed: {:?}", e);
                }
            }
            
            last_cache_refresh = std::time::Instant::now();
        }      


        // ========== SCAN FOR OPPORTUNITIES ==========

        // Scan 2-leg arbitrage (parallèle)
        let scan_start = std::time::Instant::now();
        let mut opportunities = match detector.scan_opportunities().await {
            Ok(opps) => opps,
            Err(e) => {
                error!("❌ Error scanning 2-leg opportunities: {}", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        if iteration % 100 == 0 {
            let scan_time_ms = scan_start.elapsed().as_millis();
            info!("⚡ Scan performance: {:.0}ms (iteration {})", scan_time_ms, iteration);
            if scan_time_ms > 2000 {
                warn!("   ⚠️  Scan time > 2s - consider checking RPC latency");
            }
        }

        // Scan triangular arbitrage (parallèle)
        if config.trading.enable_flash_loans && flashloan_executor.is_some() {
            match detector.scan_triangular_opportunities().await {
                Ok(mut tri_opps) => {
                    opportunities.append(&mut tri_opps);
                }
                Err(e) => {
                    error!("❌ Error scanning triangular opportunities: {}", e);
                }
            }
        }

        let scan_time = scan_start.elapsed();
        let opp_count = opportunities.len();

        if opp_count > 0 {
            info!("🔍 Found {} opportunities in {:.0}ms (iteration {})", 
                opp_count, scan_time.as_millis(), iteration);

            for (idx, opp) in opportunities.iter().enumerate() {
                debug!("   Opp #{}: ${:.2} profit, spread {:.2} bps, {} steps",
                    idx + 1,
                    opp.net_profit_usd,
                    {
                        let spread = if opp.amount > U256::zero() {
                            (opp.expected_profit.as_u128() as f64 / opp.amount.as_u128() as f64) * 10000.0
                        } else {
                            0.0
                        };
                        spread
                    },
                    opp.steps.len()
                );
            }

            {
                let mut m = metrics.write().await;
                m.opportunities_found += opp_count as u64;
            }

            // Trier par profit net décroissant
            opportunities.sort_by(|a, b| {
                b.net_profit_usd
                    .partial_cmp(&a.net_profit_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            if let Some(best_opp) = opportunities.first() {
                info!(
                    "💎 Best opportunity: ${:.2} profit (after gas)",
                    best_opp.net_profit_usd
                );

                // Vérifier taille de position
                let position_usd = best_opp.amount.as_u128() as f64
                    * 3300.0 / 1e18; // Approximation avec ETH

                if position_usd > config.trading.max_position_size_usd {
                    warn!(
                        "⚠️  Position size ${:.2} exceeds limit ${:.2}, skipping",
                        position_usd, config.trading.max_position_size_usd
                    );
                    continue;
                }

                // ========== DECIDE: FLASH LOAN OR NORMAL ==========
                let use_flashloan = should_use_flashloan(&config, best_opp, &flashloan_executor).await;

                if use_flashloan && flashloan_executor.is_some() {
                    // ⚡ EXECUTE WITH FLASH LOAN
                    match execute_with_flashloan(
                        &config,
                        best_opp,
                        flashloan_executor.as_ref().unwrap(),
                        &metrics,
                    ).await {
                        Ok(_) => {
                            trades_this_hour += 1;
                            last_trade_time = std::time::Instant::now();
                        }
                        Err(e) => {
                            error!("❌ Flash loan execution failed: {}", e);

                            let mut m = metrics.write().await;
                            m.failed_trades += 1;
                        }
                    }
                } else {
                    // 💰 EXECUTE NORMALLY (with capital)
                    match executor.execute_opportunity(best_opp).await {
                        Ok(trade) => {
                            {
                                let mut m = metrics.write().await;
                                m.record_trade(&trade);
                                m.opportunities_executed += 1;
                            }

                            trades_this_hour += 1;
                            last_trade_time = std::time::Instant::now();

                            // Send notification if configured
                            if let Err(e) = send_notification(&config, &trade).await {
                                warn!("Failed to send notification: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("❌ Failed to execute trade: {}", e);

                            if e.to_string().contains("insufficient liquidity") ||
                                e.to_string().contains("INSUFFICIENT_OUTPUT_AMOUNT") {
                                    for step in &best_opp.steps {
                                        // Marquer les pools impliqués
                                    }
                                }

                            let mut m = metrics.write().await;
                            m.failed_trades += 1;
                        }
                    }
                }
            }
        } else if iteration % 25 == 0 {
            debug!("👀 Scanning... (iteration {}, scan time: {:.0}ms, no opportunities)", 
                iteration, scan_time.as_millis());
        }

        let interval = if opportunities.len() > 0 {
            SCAN_INTERVAL_FAST_MS
        } else {
            SCAN_INTERVAL_NORMAL_MS
        };

        tokio::time::sleep(tokio::time::Duration::from_millis(interval)).await;
    }
}

// ========== HELPER: DECIDE IF USING FLASH LOAN ==========

async fn should_use_flashloan(
    config: &Config,
    opp: &types::ArbitrageOpportunity,
    flashloan_executor: &Option<Arc<FlashLoanExecutor>>,
) -> bool {
    // Flash loans disabled
    if flashloan_executor.is_none() || !config.trading.enable_flash_loans {
        return false;
    }

    // Check profit minimum for flash loans
    if opp.net_profit_usd < config.trading.flash_loan_min_profit_usd {
        debug!("⏭️  Profit ${:.2} < min ${:.2} for flashloan", 
            opp.net_profit_usd, config.trading.flash_loan_min_profit_usd);
        return false;
    }

    // Check amount maximum
    let amount_eth = opp.amount.as_u128() as f64 / 1e18;
    if amount_eth > config.trading.flash_loan_max_amount_eth {
        debug!("⏭️  Amount {:.2} ETH > max {:.2} ETH for flashloan", 
            amount_eth, config.trading.flash_loan_max_amount_eth);
        return false;
    }

    // Check profitability with flash loan costs
    let gas_price_gwei = 0.05; // Conservative estimate for Base
    let profitable = is_profitable_with_flashloan(opp, gas_price_gwei).await;

    if profitable {
        info!("✅ Using FLASH LOAN for this opportunity");
    } else {
        debug!("⏭️  Flash loan not profitable after fees");
    }

    profitable
}

// ========== HELPER: EXECUTE WITH FLASH LOAN ==========

async fn execute_with_flashloan(
    config: &Config,
    opp: &types::ArbitrageOpportunity,
    flashloan_executor: &Arc<FlashLoanExecutor>,
    metrics: &Arc<RwLock<BotMetrics>>,
) -> Result<()> {
    info!("⚡ Executing arbitrage with FLASH LOAN");
    info!("   Asset: {:?}", opp.asset);
    info!("   Amount: {:.6}", opp.amount.as_u128() as f64 / 1e18);
    info!("   Expected net profit: ${:.2}", opp.net_profit_usd);

    let mode = ExecutionMode {
        use_flash_loan: true,
        use_private_mempool: config.mev.use_private_mempool,
        max_priority_fee_gwei: config.mev.max_priority_fee_gwei as u64,
        gas_multiplier: config.trading.gas_price_multiplier,
    };

    match flashloan_executor.execute_with_flashloan(opp, mode).await {
        Ok(result) => {
            if result.success {
                info!("✅ Flash loan arbitrage SUCCESS!");
                info!("   💰 Profit: {:.6}", result.profit.as_u128() as f64 / 1e6);
                info!("   ⛽ Gas used: {}", result.gas_used);
                info!("   💸 Gas price: {:.2} gwei", result.effective_gas_price.as_u64() as f64 / 1e9);
                info!("   ⏱️  Execution time: {}ms", result.execution_time_ms);
                info!("   🔗 TX: {}", result.tx_hash);

                // Update metrics
                let mut m = metrics.write().await;
                m.total_trades += 1;
                m.successful_trades += 1;
                m.total_profit_usd += result.profit.as_u128() as f64 / 1e6;
                m.opportunities_executed += 1;

                Ok(())
            } else {
                warn!("❌ Flash loan arbitrage FAILED");
                Err(anyhow::anyhow!("Flash loan execution failed"))
            }
        }
        Err(e) => {
            error!("❌ Flash loan execution error: {}", e);
            Err(e)
        }
    }
}

// ========== HELPER: METRICS SERVER ==========

async fn start_metrics_server(config: Arc<Config>, metrics: Arc<RwLock<BotMetrics>>) -> Result<()> {
    let server = MetricsServer::new(config.monitoring.metrics_port, metrics);

    info!(
        "📊 Metrics server started on http://0.0.0.0:{}",
        config.monitoring.metrics_port
    );
    info!("   • Prometheus: http://localhost:{}/metrics", config.monitoring.metrics_port);
    info!("   • JSON Stats: http://localhost:{}/stats", config.monitoring.metrics_port);
    info!("   • Health: http://localhost:{}/health", config.monitoring.metrics_port);

    server.run().await
}

// ========== HELPER: PRINT METRICS ==========

async fn print_metrics(metrics: &Arc<RwLock<BotMetrics>>) {
    let m = metrics.read().await;

    info!("═══════════════════════════════════════════════════════════");
    info!("📊 Bot Metrics");
    info!("═══════════════════════════════════════════════════════════");
    info!("   Total trades: {}", m.total_trades);
    info!("   Successful: {} ({:.1}%)", m.successful_trades, m.success_rate());
    info!("   Failed: {}", m.failed_trades);
    info!("   Total profit: ${:.2}", m.total_profit_usd);
    info!("   Avg profit/trade: ${:.2}", m.average_profit_per_trade);
    info!("   Total gas cost: ${:.2}", m.total_gas_cost_usd);
    info!("   Net profit: ${:.2}", m.total_profit_usd - m.total_gas_cost_usd);
    info!("   Opportunities found: {}", m.opportunities_found);
    info!("   Opportunities executed: {}", m.opportunities_executed);
    info!("   Execution rate: {:.1}%",
        if m.opportunities_found > 0 {
            (m.opportunities_executed as f64 / m.opportunities_found as f64) * 100.0
        } else {
            0.0
        }
    );
    info!("   Uptime: {}s ({:.1} hours)", m.uptime_seconds, m.uptime_seconds as f64 / 3600.0);
    info!("═══════════════════════════════════════════════════════════");
}

// ========== HELPER: SEND NOTIFICATION ==========

async fn send_notification(config: &Config, trade: &types::Trade) -> Result<()> {
    let status_emoji = match trade.status {
        types::TradeStatus::Success => "✅",
        types::TradeStatus::Failed => "❌",
        types::TradeStatus::Reverted => "⚠️",
        _ => "⏳",
    };

    // Telegram notification
    if let (Some(token), Some(chat_id)) = (
        &config.monitoring.telegram_bot_token,
        &config.monitoring.telegram_chat_id,
    ) {
        let message = format!(
            "{} *Arbitrage Executed*\n\
             \n\
             Trade ID: `{}`\n\
             Status: {:?}\n\
             Profit: ${:.2}\n\
             Gas Used: {}\n\
             TX: `{}`\n\
             \n\
             🔒 Sent via private mempool",
            status_emoji,
            trade.id,
            trade.status,
            trade.opportunity.net_profit_usd,
            trade.gas_used.map(|g| g.to_string()).unwrap_or_else(|| "N/A".to_string()),
            trade.tx_hash
        );

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            token
        );

        let client = reqwest::Client::new();
        let _ = client
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "text": message,
                "parse_mode": "Markdown"
            }))
            .send()
            .await;
    }

    // Discord webhook (optionnel)
    if let Some(webhook_url) = &config.monitoring.discord_webhook_url {
        let embed = serde_json::json!({
            "embeds": [{
                "title": format!("{} Arbitrage Executed", status_emoji),
                "color": match trade.status {
                    types::TradeStatus::Success => 0x00FF00,
                    types::TradeStatus::Failed => 0xFF0000,
                    types::TradeStatus::Reverted => 0xFFA500,
                    _ => 0x808080,
                },
                "fields": [
                    {
                        "name": "Trade ID",
                        "value": &trade.id,
                        "inline": true
                    },
                    {
                        "name": "Status",
                        "value": format!("{:?}", trade.status),
                        "inline": true
                    },
                    {
                        "name": "Profit",
                        "value": format!("${:.2}", trade.opportunity.net_profit_usd),
                        "inline": true
                    },
                    {
                        "name": "Gas Used",
                        "value": trade.gas_used.map(|g| g.to_string()).unwrap_or_else(|| "N/A".to_string()),
                        "inline": true
                    },
                    {
                        "name": "Transaction",
                        "value": format!("[View on BaseScan](https://basescan.org/tx/{})", trade.tx_hash),
                        "inline": false
                    }
                ],
                "timestamp": chrono::Utc::now().to_rfc3339()
            }]
        });

        let client = reqwest::Client::new();
        let _ = client
            .post(webhook_url)
            .json(&embed)
            .send()
            .await;
    }

    Ok(())
}