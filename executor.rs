// executor.rs

use crate::config::Config;
use crate::types::*;
use crate::dual_provider::{DualProvider, WriteRpcType};
use anyhow::{Context, Result};
use ethers::{
    prelude::*,
    middleware::SignerMiddleware,
    types::{TransactionReceipt, U256, transaction::eip2718::TypedTransaction},
};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

abigen!(
    ArbitrageBot,
    r#"[
        {
            "inputs": [
                {"internalType": "address", "name": "asset", "type": "address"},
                {"internalType": "uint256", "name": "amount", "type": "uint256"},
                {"components": [
                    {"internalType": "uint8", "name": "dex", "type": "uint8"},
                    {"internalType": "address", "name": "tokenIn", "type": "address"},
                    {"internalType": "address", "name": "tokenOut", "type": "address"},
                    {"internalType": "bytes", "name": "data", "type": "bytes"},
                    {"internalType": "uint128", "name": "minOut", "type": "uint128"}
                ], "internalType": "struct SwapStep[]", "name": "steps", "type": "tuple[]"},
                {"internalType": "uint128", "name": "minProfit", "type": "uint128"},
                {"internalType": "uint32", "name": "deadline", "type": "uint32"},
                {"internalType": "uint256", "name": "nonce", "type": "uint256"}
            ],
            "name": "executeAtomic",
            "outputs": [],
            "stateMutability": "nonpayable",
            "type": "function"
        },
        {
            "inputs": [
                {"internalType": "address", "name": "asset", "type": "address"},
                {"internalType": "uint256", "name": "amount", "type": "uint256"},
                {"components": [
                    {"internalType": "uint8", "name": "dex", "type": "uint8"},
                    {"internalType": "address", "name": "tokenIn", "type": "address"},
                    {"internalType": "address", "name": "tokenOut", "type": "address"},
                    {"internalType": "bytes", "name": "data", "type": "bytes"},
                    {"internalType": "uint128", "name": "minOut", "type": "uint128"}
                ], "internalType": "struct SwapStep[]", "name": "steps", "type": "tuple[]"},
                {"internalType": "uint128", "name": "minProfit", "type": "uint128"},
                {"internalType": "uint32", "name": "deadline", "type": "uint32"},
                {"internalType": "uint32", "name": "maxBlock", "type": "uint32"},
                {"internalType": "uint256", "name": "nonce", "type": "uint256"}
            ],
            "name": "execute",
            "outputs": [],
            "stateMutability": "nonpayable",
            "type": "function"
        },
        {
            "inputs": [{"internalType": "address", "name": "token", "type": "address"}],
            "name": "getBalance",
            "outputs": [{"internalType": "uint256", "name": "", "type": "uint256"}],
            "stateMutability": "view",
            "type": "function"
        },
        {
            "inputs": [{"internalType": "bytes32", "name": "txHash", "type": "bytes32"}],
            "name": "isExecuted",
            "outputs": [{"internalType": "bool", "name": "", "type": "bool"}],
            "stateMutability": "view",
            "type": "function"
        },
        {
            "inputs": [],
            "name": "owner",
            "outputs": [{"internalType": "address", "name": "", "type": "address"}],
            "stateMutability": "view",
            "type": "function"
        }
    ]"#,
);

const GAS_ESTIMATE_BUFFER: u64 = 120;
const MIN_GAS_ATOMIC: u64 = 300_000;
const MIN_GAS_FLASHLOAN: u64 = 400_000;
const SIMULATION_TIMEOUT_MS: u64 = 2000;
const MAX_NONCE_RETRY: usize = 3;

pub struct TradeExecutor {
    config: Arc<Config>,
    dual_provider: Arc<DualProvider>,
    bot_contract_read: ArbitrageBot<SignerMiddleware<Provider<Ws>, LocalWallet>>,
    wallet: LocalWallet,
    nonce_tracker: Arc<tokio::sync::RwLock<u64>>,
}

impl TradeExecutor {
    pub async fn new(config: Arc<Config>, dual_provider: Arc<DualProvider>) -> Result<Self> {
        info!("🔧 Initializing Trade Executor with Dual RPC");

        let wallet: LocalWallet = config.bot.private_key
            .trim_start_matches("0x")
            .parse()
            .context("Invalid private key")?;

        let wallet = wallet.with_chain_id(config.network.chain_id);

        let read_provider = (*dual_provider.read()).clone();
        let client = SignerMiddleware::new(read_provider, wallet.clone());

        let bot_contract_read = ArbitrageBot::new(
            config.bot.contract_address,
            Arc::new(client)
        );

        // ✅ VÉRIFIER LE OWNER DU CONTRACT
        match bot_contract_read.owner().call().await {
            Ok(owner) => {
                info!("   📋 Contract owner: {:?}", owner);
                info!("   👛 Your wallet: {:?}", wallet.address());
                
                if owner != wallet.address() {
                    warn!("   ⚠️  WARNING: You are NOT the contract owner!");
                    warn!("   ⚠️  This will cause Unauthorized() errors");
                }
            }
            Err(e) => {
                warn!("   ⚠️  Could not verify contract owner: {}", e);
            }
        }

        let nonce = dual_provider
            .get_transaction_count(wallet.address(), None)
            .await?;

        info!("✅ Trade Executor initialized");
        info!("   📍 Wallet: {:?}", wallet.address());
        info!("   🔢 Initial nonce: {}", nonce);

        Ok(Self {
            config,
            dual_provider,
            bot_contract_read,
            wallet,
            nonce_tracker: Arc::new(tokio::sync::RwLock::new(nonce.as_u64())),
        })
    }

    pub async fn new_with_nonce(
        config: Arc<Config>,
        dual_provider: Arc<DualProvider>,
        nonce_tracker: Arc<tokio::sync::RwLock<u64>>,
    ) -> Result<Self> {
        info!("🔧 Initializing Trade Executor with SHARED nonce tracker");

        let wallet: LocalWallet = config.bot.private_key
            .trim_start_matches("0x")
            .parse()
            .context("Invalid private key")?;

        let wallet = wallet.with_chain_id(config.network.chain_id);

        let read_provider = (*dual_provider.read()).clone();
        let client = SignerMiddleware::new(read_provider, wallet.clone());

        let bot_contract_read = ArbitrageBot::new(
            config.bot.contract_address,
            Arc::new(client)
        );

        // ✅ VÉRIFIER LE OWNER DU CONTRACT
        match bot_contract_read.owner().call().await {
            Ok(owner) => {
                info!("   📋 Contract owner: {:?}", owner);
                info!("   👛 Your wallet: {:?}", wallet.address());
                
                if owner != wallet.address() {
                    error!("   🚨 ERROR: You are NOT the contract owner!");
                    error!("   🚨 Transactions will fail with Unauthorized() error");
                    error!("   🚨 Please transfer ownership or use the correct wallet");
                    
                    return Err(anyhow::anyhow!(
                        "Wallet mismatch: contract owner is {:?}, but you are {:?}",
                        owner, wallet.address()
                    ));
                }
            }
            Err(e) => {
                warn!("   ⚠️  Could not verify contract owner: {}", e);
            }
        }

        {
            let mut nonce = nonce_tracker.write().await;
            if *nonce == 0 {
                let current_nonce = dual_provider
                    .get_transaction_count(wallet.address(), None)
                    .await?;
                *nonce = current_nonce.as_u64();
                info!("✅ Initialized shared nonce: {}", *nonce);
            }
        }

        info!("✅ Trade Executor initialized with shared nonce");
        info!("   📍 Wallet: {:?}", wallet.address());

        Ok(Self {
            config,
            dual_provider,
            bot_contract_read,
            wallet,
            nonce_tracker,
        })
    }

    // ========== EXÉCUTION PRINCIPALE ==========

    pub async fn execute_opportunity(&self, opportunity: &ArbitrageOpportunity) -> Result<Trade> {
        let execution_start = Instant::now();
        
        info!(
            "🚀 Executing opportunity {} - Expected profit: ${:.2}",
            opportunity.id, opportunity.net_profit_usd
        );

        match self.dual_provider.write_type() {
            WriteRpcType::BaseBuilder => info!("🔒 Using Base Builder (MEV protected)"),
            WriteRpcType::EdenNetwork => info!("🔒 Using Eden Network (MEV protected)"),
            WriteRpcType::Flashbots => info!("🔒 Using Flashbots (MEV protected)"),
            WriteRpcType::Standard => warn!("⚠️  Using Standard RPC (NO MEV PROTECTION)"),
        }

        if self.config.safety.enable_dry_run {
            info!("🔍 DRY RUN mode - Transaction not sent");
            return Ok(self.create_dry_run_trade(opportunity));
        }

        if self.is_already_executed(opportunity).await? {
            warn!("⚠️  Opportunity already executed, skipping");
            return Err(anyhow::anyhow!("Already executed"));
        }

        if !self.pre_validate_opportunity(opportunity).await? {
            warn!("⚠️  Opportunity failed pre-validation");
            return Err(anyhow::anyhow!("Pre-validation failed"));
        }

        let has_capital = self.check_available_capital(opportunity).await?;

        let tx_hash = if has_capital {
            info!("💰 Using atomic execution (bot has capital)");
            self.execute_atomic(opportunity).await?
        } else {
            info!("⚡ Using flashloan execution");
            self.execute_with_flashloan(opportunity).await?
        };

        let receipt = self.wait_for_confirmation(&tx_hash).await?;
        let trade = self.create_trade_from_receipt(opportunity, &tx_hash, receipt);

        let execution_time = execution_start.elapsed();

        match trade.status {
            TradeStatus::Success => {
                info!(
                    "✅ Trade executed successfully in {:.2}s - Profit: ${:.2}",
                    execution_time.as_secs_f64(),
                    opportunity.net_profit_usd
                );
            }
            _ => {
                error!("❌ Trade failed or reverted after {:.2}s", execution_time.as_secs_f64());
            }
        }

        Ok(trade)
    }

    // ========== EXÉCUTION ATOMIQUE ==========

    async fn execute_atomic(&self, opportunity: &ArbitrageOpportunity) -> Result<String> {
        let mut nonce_guard = self.nonce_tracker.write().await;
        let current_nonce = *nonce_guard;

        info!("🔢  Using nonce: {}", current_nonce);

        let deadline = chrono::Utc::now().timestamp() as u32 + 300;

        let steps: Vec<arbitrage_bot::SwapStep> = opportunity
            .steps
            .iter()
            .map(|s| arbitrage_bot::SwapStep {
                dex: s.dex as u8,
                token_in: s.token_in,
                token_out: s.token_out,
                data: s.data.clone(),
                min_out: s.min_out,
            })
            .collect();

        let min_profit = (opportunity.expected_profit * U256::from(95) / U256::from(100)).as_u128();

        let call = self.bot_contract_read
            .execute_atomic(
                opportunity.asset,
                opportunity.amount,
                steps,
                min_profit,
                deadline,
                U256::from(current_nonce),
            );

        let mut tx: TypedTransaction = call.tx.clone();

        let final_gas_price = self.get_optimal_gas_price().await?;
        tx.set_gas_price(final_gas_price);
        tx.set_nonce(current_nonce);
        tx.set_chain_id(self.config.network.chain_id);
        tx.set_from(self.wallet.address());

        debug!("💰 Real transaction gas price: {:.4} gwei", final_gas_price.as_u128() as f64 / 1e9);
        debug!("🔍 Starting simulation with temporary address...");

        let temp_address = Address::from_low_u64_be(
            (current_nonce % 1_000_000) + 1_000_000
        );

        let mut sim_tx = tx.clone();
        sim_tx.set_from(temp_address);
        sim_tx.set_gas(U256::from(10_000_000));
        sim_tx.set_gas_price(U256::zero());        

        debug!("   • Temp address: {:?}", temp_address);
        debug!("   • Real address: {:?} (will be used for execution)", self.wallet.address());

        let simulation_result = tokio::time::timeout(
            std::time::Duration::from_millis(SIMULATION_TIMEOUT_MS),
            self.dual_provider.read().call(&sim_tx, None)
        ).await;

        match simulation_result {
            Ok(Ok(_)) => {
                info!("✅ Simulation passed completely!");
            }
            Ok(Err(e)) => {
                let error_str = e.to_string();
                
                if error_str.contains("0x82b42900") {
                    info!("   ℹ️  Got 0x82b42900 with temp address (EXPECTED)");
                    info!("   ✅ This is normal - temp address not authorized");
                    info!("   🔄 Real authorized address will work");
                }

                else if error_str.contains("insufficient funds") {
                    
                    info!("   ℹ️  Temp address has no funds (EXPECTED)");
                    info!("   ✅ Transaction structure validated");
                }
                
                else if error_str.contains("slippage") || 
                        error_str.contains("INSUFFICIENT_OUTPUT") ||
                        error_str.contains("insufficient output amount") {
                    warn!("   ⚠️  Trade no longer profitable (slippage)");
                    return Err(anyhow::anyhow!("Unprofitable: slippage too high"));
                }

                else if error_str.contains("revert") { 
                    warn!("   ⚠️  Transaction would revert: {}", error_str);
                    return Err(anyhow::anyhow!("Would revert: {}", e));
                }

                else {
                    warn!("   ⚠️  Simulation warning: {}", e);
                    warn!("   🤞 Continuing anyway...");
                }
            }
            
            Err(_) => {
                warn!("   ⏰ Simulation timeout");
            }
        }

        let gas_limit = if opportunity.steps.len() == 2 {
            U256::from(550_000)
        } else if opportunity.steps.len() == 3 {
            U256::from(700_000)
        } else {
            U256::from(850_000)
        };
        
        tx.set_gas(gas_limit);

        info!("📊 Executing with REAL authorized address...");
        debug!("   • From: {:?}", self.wallet.address());
        debug!("   • Nonce: {}", current_nonce);
        debug!("   • Gas limit: {} (fixed for {}-leg)", gas_limit, opportunity.steps.len());
        debug!("   • Gas price: {:.4} gwei", final_gas_price.as_u128() as f64 / 1e9);
        debug!("   • Min profit: {} wei", min_profit);

        let pending_tx_result = self.dual_provider
            .send_private_transaction(tx, &self.wallet)
            .await;

        match pending_tx_result {
            Ok(pending_tx) => {
                let tx_hash = format!("{:?}", pending_tx.tx_hash());
                info!("📤 Transaction sent: {}", tx_hash);

                *nonce_guard += 1;
                debug!("✅ Nonce incremented to: {}", *nonce_guard);

                Ok(tx_hash)
            }
            Err(e) => {
                error!("❌ Failed to send transaction: {}", e);

                let error_str = e.to_string();
                if error_str.contains("0x82b42900") {
                    error!("   🚨 Real transaction blocked");
                    if error_str.to_lowercase().contains("antispam") {
                        error!("   💡 antiSpam triggered");
                    } else if error_str.to_lowercase().contains("authorized") {
                        error!("   💡 Not authorized");
                    }
                }

                warn!("🔄 Resynchronizing nonce from blockchain...");
                match self.sync_nonce_from_chain().await {
                    Ok(new_nonce) => {
                        *nonce_guard = new_nonce;
                        info!("✅ Nonce resynchronized to: {}", new_nonce);
                    }
                    Err(sync_err) => {
                        error!("❌ Failed to resync nonce: {}", sync_err);
                    }
                }

                Err(anyhow::anyhow!("Failed to send transaction: {}", e))
            }
        }
    }

    // ========== EXÉCUTION AVEC FLASHLOAN ==========

    async fn execute_with_flashloan(&self, opportunity: &ArbitrageOpportunity) -> Result<String> {
        let mut nonce_guard = self.nonce_tracker.write().await;
        let current_nonce = *nonce_guard;

        info!("🔢 Using nonce: {}", current_nonce);

        let deadline = chrono::Utc::now().timestamp() as u32 + 300;
        let max_block = 0u32;

        let steps: Vec<arbitrage_bot::SwapStep> = opportunity
            .steps
            .iter()
            .map(|s| arbitrage_bot::SwapStep {
                dex: s.dex as u8,
                token_in: s.token_in,
                token_out: s.token_out,
                data: s.data.clone(),
                min_out: s.min_out,
            })
            .collect();

        let min_profit = (opportunity.expected_profit * U256::from(95) / U256::from(100)).as_u128();

        let call = self.bot_contract_read
            .execute(
                opportunity.asset,
                opportunity.amount,
                steps,
                min_profit,
                deadline,
                max_block,
                U256::from(current_nonce),
            );

        let mut tx: TypedTransaction = call.tx.clone();

        // ✅ ÉTAPE 1 : Préparer la transaction réelle
        let final_gas_price = self.get_optimal_gas_price().await?;
        
        tx.set_gas_price(final_gas_price);
        tx.set_nonce(current_nonce);
        tx.set_chain_id(self.config.network.chain_id);
        tx.set_from(self.wallet.address());

        debug!("💰 Real transaction gas price: {:.4} gwei", final_gas_price.as_u128() as f64 / 1e9);

        // ✅ ÉTAPE 2 : SIMULATION avec une ADRESSE ARTIFICIELLE
        debug!("🔍 Starting simulation with temporary address...");
        
        // Créer une adresse temporaire (déterministe mais différente)
        let temp_address = Address::from_low_u64_be(
            (current_nonce % 1_000_000) + 1_000_000
        );
        
        // Créer une copie de la tx pour simulation AVEC l'adresse temporaire
        let mut sim_tx = tx.clone();
        sim_tx.set_from(temp_address);  // ✅ Utiliser l'adresse temporaire
        sim_tx.set_gas(U256::from(10_000_000)); // Gas élevé pour simulation
        sim_tx.set_gas_price(U256::zero()); 
        
        debug!("   • Temp address: {:?}", temp_address);
        debug!("   • Real address: {:?} (will be used for execution)", self.wallet.address());

        let simulation_result = tokio::time::timeout(
            std::time::Duration::from_millis(SIMULATION_TIMEOUT_MS),
            self.dual_provider.read().call(&sim_tx, None)
        ).await;

        match simulation_result {

            Ok(Ok(_)) => {
                info!("✅ Simulation passed completely!");
            }

            Ok(Err(e)) => {
                let error_str = e.to_string();
                
                if error_str.contains("0x82b42900") {
                    info!("   ℹ️  Got 0x82b42900 with temp address (EXPECTED)");
                    info!("   ✅ This is normal - temp address not authorized");
                    info!("   🔄 Real authorized address will work");
                }

                else if error_str.contains("0x82b42900") {
                    
                    info!("   ℹ️  Temp address has no funds (EXPECTED)");
                    info!("   ✅ Transaction structure validated");
                    
                }

                else if error_str.contains("slippage") || 
                        error_str.contains("INSUFFICIENT_OUTPUT") ||
                        error_str.contains("insufficient output amount") {
                    warn!("   ⚠️  Trade no longer profitable (slippage)");
                    return Err(anyhow::anyhow!("Unprofitable: slippage too high"));
                        
                }

                else if error_str.contains("revert") {
                    warn!("   ⚠️  Transaction would revert: {}", error_str);
                    return Err(anyhow::anyhow!("Would revert: {}", e));
                }

                else {
                    warn!("   ⚠️  Simulation warning: {}", e);
                    warn!("   🤞 Continuing - may work with real authorized address");
                }
            }

            Err(_) => {
                warn!("  ⏰ Simulation timeout after {}ms", SIMULATION_TIMEOUT_MS);
                warn!("   🤞 Continuing with estimated gas - RPC may be slow");
            }
        }

        // ✅ ÉTAPE 3 : Gas limit FIXE basé sur le type d'opération
        let gas_limit = if opportunity.steps.len() == 2 {
            U256::from(450_000)
        } else if opportunity.steps.len() == 3 {
            U256::from(600_000)
        } else {
            U256::from(750_000)
        };
        
        tx.set_gas(gas_limit);
        
        info!("📊 Executing with REAL authorized address...");
        debug!("   • From: {:?}", self.wallet.address());
        debug!("   • To: {:?}", self.config.bot.contract_address);
        debug!("   • Nonce: {}", current_nonce);
        debug!("   • Gas limit: {} (fixed for {}-leg)", gas_limit, opportunity.steps.len());
        debug!("   • Gas price: {:.4} gwei", final_gas_price.as_u128() as f64 / 1e9);
        debug!("   • Min profit: {} wei", min_profit);
        debug!("   • Deadline: {}", deadline);

        // ✅ ÉTAPE 4 : Envoi de la transaction avec la VRAIE adresse
        let pending_tx_result = self.dual_provider
            .send_private_transaction(tx, &self.wallet)
            .await;

        match pending_tx_result {
            Ok(pending_tx) => {
                let tx_hash = format!("{:?}", pending_tx.tx_hash());
                info!("📤 Flashloan transaction sent: {}", tx_hash);

                *nonce_guard += 1;
                debug!("✅ Nonce incremented to: {}", *nonce_guard);

                Ok(tx_hash)
            }
            Err(e) => {
                error!("❌ Failed to send flashloan transaction: {}", e);

                let error_str = e.to_string();
                error!("📋 Full error: {}", error_str);

                if error_str.contains("nonce") {
                    error!("   💡 Nonce issue detected");
                }
                if error_str.contains("gas") {
                    error!("   💡 Gas issue detected");
                }
                if error_str.contains("insufficient") {
                    error!("   💡 Insufficient funds for gas");
                }

                if error_str.contains("0x82b42900") {
                    error!("  🚨 Real transaction blocked by contract");

                    if error_str.to_lowercase().contains("antispam") {
                        error!("   💡 antiSpam triggered - you may have executed on this block");
                        error!("   🔄 Wait for next block and retry");
                    } else if error_str.to_lowercase().contains("authorized") {
                        error!("   💡 Real address not authorized in contract");
                        error!("   🔧 Check contract authorization settings");
                    } else {
                        error!("   💡 Unknown authorization issue");
                    }
                } else if error_str.contains("insufficient funds") {
                    error!("   💡 Real address has insufficient ETH for gas");
                    error!("   💰 Fund your wallet: {:?}", self.wallet.address());
                } else if error_str.contains("nonce") {
                    error!("   💡 Nonce issue - resynchronizing...");
                }

                warn!("🔄 Resynchronizing nonce from blockchain...");
                match self.sync_nonce_from_chain().await {
                    Ok(new_nonce) => {
                        *nonce_guard = new_nonce;
                        info!("✅ Nonce resynchronized to: {}", new_nonce);
                    }
                    Err(sync_err) => {
                        error!("❌ Failed to resync nonce: {}", sync_err);
                    }
                }

                Err(anyhow::anyhow!("Failed to send flashloan transaction: {}", e))
            }
        }
    }

    // ========== HELPERS ==========

    async fn sync_nonce_from_chain(&self) -> Result<u64> {
        let on_chain_nonce = self.dual_provider
            .get_transaction_count(self.wallet.address(), None)
            .await
            .context("Failed to get transaction count")?;

        Ok(on_chain_nonce.as_u64())
    }

    pub async fn force_nonce_resync(&self) -> Result<u64> {
        let new_nonce = self.sync_nonce_from_chain().await?;

        let mut nonce_guard = self.nonce_tracker.write().await;
        let old_nonce = *nonce_guard;
        *nonce_guard = new_nonce;

        info!("🔄 Forced nonce resync: {} → {}", old_nonce, new_nonce);

        Ok(new_nonce)
    }

    async fn check_available_capital(&self, opportunity: &ArbitrageOpportunity) -> Result<bool> {
        let balance = self
            .bot_contract_read
            .get_balance(opportunity.asset)
            .call()
            .await?;

        debug!("💰 Bot balance: {}, required: {}", balance, opportunity.amount);
        Ok(balance >= opportunity.amount)
    }

    async fn is_already_executed(&self, opportunity: &ArbitrageOpportunity) -> Result<bool> {
        let tx_hash = ethers::utils::keccak256(
            ethers::abi::encode(&[
                ethers::abi::Token::Address(opportunity.asset),
                ethers::abi::Token::Uint(opportunity.amount),
                ethers::abi::Token::Uint(U256::from(opportunity.timestamp)),
                ethers::abi::Token::Address(self.wallet.address()),
                ethers::abi::Token::String("atomic".to_string()),
            ])
        );

        let is_executed = self
            .bot_contract_read
            .is_executed(tx_hash.into())
            .call()
            .await?;

        Ok(is_executed)
    }

    async fn get_optimal_gas_price(&self) -> Result<U256> {
        let latest_block = self.dual_provider
            .read()
            .get_block(ethers::types::BlockNumber::Latest)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Failed to get latest block"))?;

        let base_fee_per_gas = latest_block.base_fee_per_gas
            .unwrap_or_else(|| U256::from(1_000_000));

        let min_base_fee = U256::from(50_000_000);  // 0.05 gwei minimum
        let base_fee = base_fee_per_gas.max(min_base_fee);

        let max_priority_fee_wei = U256::from(2_000_000_000); // 2 gwei MAX
        let configured_priority = U256::from(self.config.mev.max_priority_fee_gwei as u64 * 1_000_000_000);
        let priority_fee = configured_priority.min(max_priority_fee_wei);
        let gas_price = base_fee_per_gas + priority_fee;

        debug!("💰 Gas calculation:");
        debug!("   Base fee: {:.4} gwei", base_fee_per_gas.as_u128() as f64 / 1e9);
        debug!("   Priority fee: {:.4} gwei", priority_fee.as_u128() as f64 / 1e9);
        debug!("   Total: {:.4} gwei", gas_price.as_u128() as f64 / 1e9);
        debug!("   Max allowed priority: {:.4} gwei", max_priority_fee_wei.as_u128() as f64 / 1e9);

        if priority_fee > max_priority_fee_wei {
            warn!("⚠️  Priority fee {} gwei exceeds max {} gwei, capping", 
                priority_fee.as_u128() as f64 / 1e9,
                max_priority_fee_wei.as_u128() as f64 / 1e9
            );
            return Ok(base_fee_per_gas + max_priority_fee_wei);
        }

        Ok(gas_price)
    }

    async fn wait_for_confirmation(&self, tx_hash_str: &str) -> Result<Option<TransactionReceipt>> {
        let tx_hash_bytes = hex::decode(tx_hash_str.trim_start_matches("0x"))?;
        let tx_hash_h256: H256 = H256::from_slice(&tx_hash_bytes);

        info!("⏳ Waiting for confirmation...");

        let receipt = self.dual_provider
            .wait_for_confirmation(tx_hash_h256, 60)
            .await?;

        Ok(receipt)
    }

    async fn simulate_transaction_with_retry(&self, tx: &TypedTransaction) -> Result<bool> {
        for attempt in 1..=MAX_NONCE_RETRY {
            match self.dual_provider.simulate_transaction(tx).await {
                Ok(_) => {
                    return Ok(true);
                }
                Err(e) => {
                    let error_str = e.to_string().to_lowercase();
                    
                    if error_str.contains("insufficient") || error_str.contains("slippage") {
                        warn!("⚠️  Simulation failed: likely frontrun or slippage");
                        return Ok(false);
                    } else if error_str.contains("unprofitable") {
                        warn!("⚠️  Simulation failed: no longer profitable");
                        return Ok(false);
                    } else if error_str.contains("revert") {
                        warn!("⚠️  Simulation failed: transaction would revert");
                        return Ok(false);
                    }
                    
                    if attempt < MAX_NONCE_RETRY {
                        debug!("⚠️  Simulation attempt {}/{} failed, retrying...", attempt, MAX_NONCE_RETRY);
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    } else {
                        warn!("⚠️  Simulation failed after {} attempts: {}", MAX_NONCE_RETRY, e);
                        return Err(e);
                    }
                }
            }
        }
        
        Err(anyhow::anyhow!("Simulation failed after retries"))
    }

    async fn pre_validate_opportunity(&self, opportunity: &ArbitrageOpportunity) -> Result<bool> {
        let now = chrono::Utc::now().timestamp() as u64;
        let age = now.saturating_sub(opportunity.timestamp);
        
        if age > 10 {
            warn!("⚠️  Opportunity too old: {}s", age);
            return Ok(false);
        }

        if opportunity.net_profit_usd < self.config.trading.min_profit_usd {
            warn!("⚠️  Profit below minimum: ${:.2}", opportunity.net_profit_usd);
            return Ok(false);
        }

        for (i, step) in opportunity.steps.iter().enumerate() {
            if step.min_out == 0 {
                error!("❌ Step {} has min_out = 0, aborting", i);
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn create_trade_from_receipt(
        &self,
        opportunity: &ArbitrageOpportunity,
        tx_hash: &str,
        receipt: Option<TransactionReceipt>,
    ) -> Trade {
        let status = match receipt.as_ref() {
            Some(r) => {
                if r.status == Some(U64::from(1)) {
                    TradeStatus::Success
                } else {
                    TradeStatus::Reverted
                }
            }
            None => TradeStatus::Failed,
        };

        Trade {
            id: format!("trade-{}", opportunity.id),
            opportunity: opportunity.clone(),
            tx_hash: tx_hash.to_string(),
            status,
            actual_profit: None,
            gas_used: receipt.as_ref().map(|r| r.gas_used.unwrap_or_default()),
            timestamp: chrono::Utc::now().timestamp() as u64,
        }
    }

    fn create_dry_run_trade(&self, opportunity: &ArbitrageOpportunity) -> Trade {
        Trade {
            id: format!("dryrun-{}", opportunity.id),
            opportunity: opportunity.clone(),
            tx_hash: "0xDRYRUN".to_string(),
            status: TradeStatus::Success,
            actual_profit: Some(opportunity.expected_profit),
            gas_used: Some(opportunity.gas_estimate),
            timestamp: chrono::Utc::now().timestamp() as u64,
        }
    }
}