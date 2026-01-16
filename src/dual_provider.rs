// dual_provider.rs

use anyhow::{Context, Result};
use ethers::providers::{Provider, Ws, Http, Middleware};
use ethers::types::{Address, BlockId, TransactionReceipt, TxHash, U256, U64, H256};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::signers::LocalWallet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn, error};

// ========== CONSTANTES ==========
const CONFIRMATION_TIMEOUT_SECS: u64 = 60;
const MAX_RETRIES: usize = 3;
const RETRY_DELAY_MS: u64 = 500;
const PROVIDER_TIMEOUT_MS: u64 = 5000;
const RATE_LIMIT_COOLDOWN_MS: u64 = 1000;

// ========== STRUCTURES ==========

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub name: String,
    pub wss_url: String,
    pub rate_limit_per_sec: usize,
    pub priority: u8,
    pub max_latency_ms: u64,
}

#[derive(Default)]
struct ProviderStats {
    requests: AtomicU64,
    successes: AtomicU64,
    failures: AtomicU64,
    rate_limited: AtomicU64,
    total_latency_ms: AtomicU64,
    consecutive_failures: AtomicU64,
}

impl ProviderStats {
    fn to_snapshot(&self) -> ProviderStatsSnapshot {
        ProviderStatsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            successes: self.successes.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            rate_limited: self.rate_limited.load(Ordering::Relaxed),
            total_latency_ms: self.total_latency_ms.load(Ordering::Relaxed),
            consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
        }
    }

    fn record_success(&self, latency_ms: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.successes.fetch_add(1, Ordering::Relaxed);
        self.total_latency_ms.fetch_add(latency_ms, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    fn record_failure(&self, is_rate_limited: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.failures.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        
        if is_rate_limited {
            self.rate_limited.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderStatsSnapshot {
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub rate_limited: u64,
    pub total_latency_ms: u64,
    pub consecutive_failures: u64,
}

impl ProviderStatsSnapshot {
    pub fn avg_latency(&self) -> f64 {
        if self.requests > 0 {
            self.total_latency_ms as f64 / self.requests as f64
        } else {
            0.0
        }
    }
    
    pub fn success_rate(&self) -> f64 {
        if self.requests > 0 {
            (self.successes as f64 / self.requests as f64) * 100.0
        } else {
            0.0
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.consecutive_failures < 5 && self.success_rate() > 80.0
    }
}

#[derive(Clone)]
pub struct DualProvider {
    read_providers: Vec<Arc<Provider<Ws>>>,
    write_provider: Arc<Provider<Http>>,
    
    provider_configs: Vec<ProviderConfig>,
    _current_provider: Arc<AtomicUsize>,
    
    stats: Vec<Arc<ProviderStats>>,
    
    _write_rpc_url: String,
    write_rpc_type: WriteRpcType,
}

#[derive(Debug, Clone)]
pub enum WriteRpcType {
    BaseBuilder,
    EdenNetwork,
    Flashbots,
    Standard,
}

#[derive(Clone)]
pub struct DualRpcConfig {
    pub _read_rpc_url: String,
    pub read_wss_urls: Vec<String>,
    pub write_rpc_url: String,
    pub write_type: WriteRpcType,
    pub _flashbots_auth_key: Option<String>,
    pub _eden_api_key: Option<String>,
}

// ========== IMPLÉMENTATION ==========

impl DualProvider {

    fn detect_provider_name(url: &str) -> String {
        let url_lower = url.to_lowercase();

        if url_lower.contains("alchemy") {
            "Alchemy".to_string()
        } else if url_lower.contains("ankr") {
            "Ankr".to_string()
        } else if url_lower.contains("quicknode") || url_lower.contains("quiknode") {
            "QuickNode".to_string()
        } else if url_lower.contains("infura") {
            "Infura".to_string()
        } else {
            // Extraire le domaine comme nom
            if let Some(domain_start) = url.find("://") {
                if let Some(domain_end) = url[domain_start + 3..].find('/') {
                    let domain = &url[domain_start + 3..domain_start + 3 + domain_end];
                    // Prendre le premier segment du domaine
                    if let Some(first_part) = domain.split('.').next() {
                        return format!("RPC-{}", first_part);
                    }
                }
            }
            "Unknown-RPC".to_string()
        }
    }

    pub async fn new(config: DualRpcConfig) -> Result<Self> {
        info!("🔗 Initializing Multi-RPC Provider");
        
        // ✅ Configuration multi-providers avec priorités
        let mut provider_configs = Vec::new();
        
        for (i, wss_url) in config.read_wss_urls.iter().enumerate() {
            let name = Self::detect_provider_name(wss_url);
            
            provider_configs.push(ProviderConfig {
                name,
                wss_url: wss_url.clone(),
                rate_limit_per_sec: 300, // Conservative default
                priority: i as u8,
                max_latency_ms: 200,
            });
        }

        info!("   📖 Attempting to connect to {} read providers", provider_configs.len());
        
        let mut read_providers = Vec::new();
        let mut stats = Vec::new();
        let mut connected_count = 0;
        
        for (_i, pconfig) in provider_configs.iter().enumerate() {
            info!("   🔌 Connecting to {} (priority: {})...", pconfig.name, pconfig.priority);
            
            match Self::connect_ws(&pconfig.wss_url).await {
                Ok(provider) => {
                    info!("   ✅ {} connected (limit: {} req/s)", 
                        pconfig.name, pconfig.rate_limit_per_sec);
                    read_providers.push(Arc::new(provider));
                    stats.push(Arc::new(ProviderStats::default()));
                    connected_count += 1;
                }
                Err(e) => {
                    warn!("   ⚠️  Failed to connect to {}: {}", pconfig.name, e);
                }
            }
        }
        
        if read_providers.is_empty() {
            return Err(anyhow::anyhow!("No read providers available - all connections failed"));
        }
        
        info!("   ✅ {}/{} read providers connected successfully", 
            connected_count, provider_configs.len());
        
        info!("   ✍️  Write: {} ({:?})", config.write_rpc_url, config.write_type);
        
        let write_provider = Arc::new(
            Provider::<Http>::try_from(&config.write_rpc_url)
                .context("Failed to create write HTTP provider")?
        );
        
        // Test connectivity
        let block = read_providers[0].get_block_number().await?;
        info!("✅ Multi-RPC Provider initialized (current block: {})", block);
        
        Ok(Self {
            read_providers,
            write_provider,
            provider_configs: provider_configs.into_iter().take(connected_count).collect(),
            _current_provider: Arc::new(AtomicUsize::new(0)),
            stats,
            _write_rpc_url: config.write_rpc_url,
            write_rpc_type: config.write_type,
        })
    }
    
    async fn connect_ws(url: &str) -> Result<Provider<Ws>> {
        // ✅ Timeout de connexion de 10s
        tokio::time::timeout(
            Duration::from_secs(10),
            async {
                let ws = Ws::connect(url).await
                    .context("Failed to connect WebSocket")?;
                Ok(Provider::new(ws))
            }
        )
        .await
        .context("WebSocket connection timeout")?
    }
    
    fn get_next_provider(&self) -> (Arc<Provider<Ws>>, usize) {
        let mut best_idx = 0;
        let mut best_score = 0.0;
        
        for (idx, provider_stats) in self.stats.iter().enumerate() {
            let snapshot = provider_stats.to_snapshot();
            
            if !snapshot.is_healthy() {
                continue;
            }
            
            let success_score = snapshot.success_rate() / 100.0;
            let latency_score = if snapshot.avg_latency() > 0.0 {
                1.0 / snapshot.avg_latency()
            } else {
                1.0
            };
            
            let priority_bonus = 1.0 - (self.provider_configs[idx].priority as f64 / 10.0);
            
            let score = success_score * 0.5 + latency_score * 0.3 + priority_bonus * 0.2;
            
            if score > best_score {
                best_score = score;
                best_idx = idx;
            }
        }
        
        (self.read_providers[best_idx].clone(), best_idx)
    }
    
    async fn execute_with_fallback<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: Fn(Arc<Provider<Ws>>) -> Fut + Clone,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let max_attempts = self.read_providers.len() * MAX_RETRIES;
        let mut last_error = None;
        
        for attempt in 0..max_attempts {
            let (provider, provider_idx) = self.get_next_provider();
            let start = Instant::now();
            
            let result = tokio::time::timeout(
                Duration::from_millis(PROVIDER_TIMEOUT_MS),
                f(provider.clone())
            ).await;
            
            match result {
                Ok(Ok(value)) => {
                    let latency = start.elapsed().as_millis() as u64;
                    
                    let stats = &self.stats[provider_idx];
                    stats.record_success(latency);
                    
                    if latency > self.provider_configs[provider_idx].max_latency_ms {
                        warn!("⚠️  {} latency high: {}ms (max: {}ms)", 
                            self.provider_configs[provider_idx].name, 
                            latency,
                            self.provider_configs[provider_idx].max_latency_ms
                        );
                    }
                    
                    return Ok(value);
                }
                Ok(Err(e)) => {
                    let is_rate_limited = e.to_string().contains("429") 
                        || e.to_string().to_lowercase().contains("rate limit")
                        || e.to_string().to_lowercase().contains("too many requests");
                    
                    let stats = &self.stats[provider_idx];
                    stats.record_failure(is_rate_limited);
                    
                    if is_rate_limited {
                        warn!("⚠️  {} rate limited, switching provider...", 
                            self.provider_configs[provider_idx].name);
                        tokio::time::sleep(Duration::from_millis(RATE_LIMIT_COOLDOWN_MS)).await;
                    } else {
                        debug!("Provider {} error (attempt {}): {}", 
                            self.provider_configs[provider_idx].name, 
                            attempt + 1, 
                            e
                        );
                    }
                    
                    last_error = Some(e);
                    
                    if attempt < max_attempts - 1 {
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                    }
                }
                Err(_) => {
                    warn!("⚠️  {} timeout after {}ms", 
                        self.provider_configs[provider_idx].name,
                        PROVIDER_TIMEOUT_MS
                    );
                    
                    let stats = &self.stats[provider_idx];
                    stats.record_failure(false);
                    
                    last_error = Some(anyhow::anyhow!("Request timeout"));
                    
                    if attempt < max_attempts - 1 {
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                    }
                }
            }
        }
        
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All providers failed")))
    }
    
    // ========== PROXY METHODS READ ==========
    
    pub async fn get_block_number(&self) -> Result<U64> {
        self.execute_with_fallback(|provider| async move {
            provider.get_block_number().await
                .context("get_block_number failed")
        }).await
    }
    
    pub async fn get_gas_price(&self) -> Result<U256> {
        self.execute_with_fallback(|provider| async move {
            provider.get_gas_price().await
                .context("get_gas_price failed")
        }).await
    }
    
    pub async fn get_transaction_count(&self, address: Address, block: Option<BlockId>) -> Result<U256> {
        self.execute_with_fallback(|provider| async move {
            provider.get_transaction_count(address, block).await
                .context("get_transaction_count failed")
        }).await
    }
    
    pub async fn get_transaction_receipt(&self, tx_hash: TxHash) -> Result<Option<TransactionReceipt>> {
        self.execute_with_fallback(|provider| async move {
            provider.get_transaction_receipt(tx_hash).await
                .context("get_transaction_receipt failed")
        }).await
    }
    
    pub async fn simulate_transaction(&self, tx: &TypedTransaction) -> Result<()> {
        let tx = tx.clone();
        
        self.execute_with_fallback(move |provider| {
            let tx = tx.clone();
            async move {
                provider.call(&tx, None).await
                    .context("Simulation failed")?;
                Ok(())
            }
        }).await
    }
    
    pub async fn wait_for_confirmation(
        &self,
        tx_hash: H256,
        timeout_secs: u64,
    ) -> Result<Option<TransactionReceipt>> {
        let start = Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        
        loop {
            if start.elapsed() > timeout {
                warn!("⚠️  Transaction confirmation timeout after {}s", timeout_secs);
                return Ok(None);
            }
            
            match self.get_transaction_receipt(tx_hash).await {
                Ok(Some(receipt)) => {
                    info!("✅ Transaction confirmed in {:.1}s", start.elapsed().as_secs_f64());
                    return Ok(Some(receipt));
                }
                Ok(None) => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    debug!("Error checking receipt: {}", e);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    
    // ========== WRITE METHODS ==========
    
    pub fn write(&self) -> &Provider<Http> {
        &self.write_provider
    }
    
    pub fn write_type(&self) -> &WriteRpcType {
        &self.write_rpc_type
    }
    
    pub async fn send_private_transaction(
        &self,
        tx: TypedTransaction,
        wallet: &LocalWallet,
    ) -> Result<ethers::providers::PendingTransaction<'_, Http>> {
        use ethers::signers::Signer;
        
        debug!("📤 Sending via {:?} (private mempool)", self.write_rpc_type);
        
        let signature = wallet.sign_transaction(&tx).await
            .context("Failed to sign transaction")?;
        let signed_tx = tx.rlp_signed(&signature);
        
        let pending = match self.write_rpc_type {
            WriteRpcType::BaseBuilder => {
                self.write_provider
                    .send_raw_transaction(signed_tx)
                    .await
                    .context("Failed to send via Base Builder")?
            }
            WriteRpcType::EdenNetwork => {
                self.send_via_eden(signed_tx).await
                    .context("Failed to send via Eden Network")?
            }
            WriteRpcType::Flashbots => {
                self.send_via_flashbots(signed_tx).await
                    .context("Failed to send via Flashbots")?
            }
            WriteRpcType::Standard => {
                warn!("⚠️  Sending via standard RPC (NO MEV PROTECTION)");
                debug!("📤 Sending raw transaction:");
                debug!("   • Raw tx size: {} bytes", signed_tx.len());
                debug!("   • First 20 bytes: {:?}", &signed_tx[..20.min(signed_tx.len())]);

                match self.write_provider.send_raw_transaction(signed_tx).await {
                    Ok(pending) => {
                        info!("✅ Standard RPC accepted transaction");
                        pending
                    }
                    Err(e) => {
                        error!("🚨 STANDARD RPC ERROR DETAILS:");
                        error!("   Error type: {:?}", e);
                        error!("   Error display: {}", e);
                        error!("   Error debug: {:?}", e);
                        
                        // Essayer d'extraire le message d'erreur JSON
                        let err_str = format!("{:?}", e);
                        error!("   Full error string: {}", err_str);
                        
                        // Vérifier des patterns communs
                        if err_str.contains("insufficient") {
                            error!("   ⚠️  INSUFFICIENT FUNDS OR GAS!");
                        }
                        if err_str.contains("nonce") {
                            error!("   ⚠️  NONCE ISSUE!");
                        }
                        if err_str.contains("intrinsic gas") {
                            error!("   ⚠️  GAS LIMIT TOO LOW!");
                        }
                        
                        return Err(anyhow::anyhow!("Standard RPC error: {:?}", e));
                    }
                }
            }
        };
        
        info!("✅ Transaction sent: {:?}", pending.tx_hash());
        
        Ok(pending)
    }
    
    async fn send_via_eden(
        &self,
        signed_tx: ethers::types::Bytes,
    ) -> Result<ethers::providers::PendingTransaction<'_, Http>> {
        self.write_provider
            .send_raw_transaction(signed_tx)
            .await
            .context("Eden Network send failed")
    }
    
    async fn send_via_flashbots(
        &self,
        signed_tx: ethers::types::Bytes,
    ) -> Result<ethers::providers::PendingTransaction<'_, Http>> {
        self.write_provider
            .send_raw_transaction(signed_tx)
            .await
            .context("Flashbots send failed")
    }
    
    pub fn read(&self) -> Arc<Provider<Ws>> {
        let (provider, _) = self.get_next_provider();
        provider
    }
    
    pub async fn print_stats(&self) {
        info!("📊 Multi-RPC Provider Stats:");
        
        let mut total_requests = 0u64;
        let mut total_rate_limited = 0u64;
        let mut total_failures = 0u64;
        
        for (i, provider_stats) in self.stats.iter().enumerate() {
            let snapshot = provider_stats.to_snapshot();
            
            if snapshot.requests == 0 {
                continue;
            }
            
            total_requests += snapshot.requests;
            total_rate_limited += snapshot.rate_limited;
            total_failures += snapshot.failures;
            
            let health_icon = if snapshot.is_healthy() { "✅" } else { "⚠️" };

            let provider_name = if i < self.provider_configs.len() {
                &self.provider_configs[i].name
            } else {
                "Unknown"
            };
            
            info!("   {} {}: {:.1}% success ({}/{} req, {} rate limited, {:.0}ms avg, {} consecutive fails)",
                health_icon,
                provider_name,
                snapshot.success_rate(),
                snapshot.successes,
                snapshot.requests,
                snapshot.rate_limited,
                snapshot.avg_latency(),
                snapshot.consecutive_failures
            );
        }
        
        if total_requests > 0 {
            let rate_limit_pct = (total_rate_limited as f64 / total_requests as f64) * 100.0;
            let failure_pct = (total_failures as f64 / total_requests as f64) * 100.0;
            
            info!("   📈 Total: {} requests, {:.1}% rate limited, {:.1}% failed", 
                total_requests, rate_limit_pct, failure_pct);
        }
    }
}
