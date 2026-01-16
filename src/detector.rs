// detector.rs

use crate::config::Config;
use crate::types::*;
use crate::dual_provider::DualProvider;
use crate::pool_oracle::PoolOracle;
use crate::circuit_breaker::CircuitBreaker;
use anyhow::Result;
use ethers::{
    contract::abigen,
    types::{Address, U256},
};
use crate::multicall_helper::MulticallHelper;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, Duration};
use tracing::{info, warn, debug, error};
use tokio::sync::RwLock;
use futures::stream::{self, StreamExt};

// ========== CONSTANTES OPTIMISÉES ==========
const MIN_SPREAD_BPS: u64 = 70;         // 2-leg minimum
const MAX_SPREAD_BPS: u64 = 500;        // 2-leg maximum
const POOL_CACHE_TTL_SECS: u64 = 10;
const MIN_RESERVE_MULTIPLIER: u64 = 3;
const MAX_TRADE_IMPACT_BPS: u64 = 300;
const SLIPPAGE_SAFETY_MARGIN_BPS: u64 = 10;
const MAX_OPPORTUNITY_AGE_SECS: u64 = 5;
const MIN_PROFIT_TO_GAS_RATIO: f64 = 1.5;
const MIN_POOL_LIQUIDITY_USD: f64 = 500.0;
const PARALLEL_SCAN_BUFFER: usize = 3;

// ========== ABIs ==========

abigen!(
    IUniswapV2Pair,
    r#"[
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast)
        function token0() external view returns (address)
        function token1() external view returns (address)
    ]"#
);

abigen!(
    IUniswapV2Factory,
    r#"[
        function getPair(address tokenA, address tokenB) external view returns (address pair)
    ]"#
);

abigen!(
    IAerodromeFactory,
    r#"[
        function getPool(address tokenA, address tokenB, bool stable) external view returns (address pool)
    ]"#
);

abigen!(
    IAerodromePool,
    r#"[
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast)
        function token0() external view returns (address)
        function token1() external view returns (address)
        function stable() external view returns (bool)
        function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256)
    ]"#
);

abigen!(
    AerodromeSlipstreamFactory,
    r#"[
        {
            "inputs": [
                {"internalType": "address", "name": "tokenA", "type": "address"},
                {"internalType": "address", "name": "tokenB", "type": "address"},
                {"internalType": "int24", "name": "tickSpacing", "type": "int24"}
            ],
            "name": "getPool",
            "outputs": [{"internalType": "address", "name": "pool", "type": "address"}],
            "stateMutability": "view",
            "type": "function"
        }
    ]"#
);

abigen!(
    AerodromeSlipstreamPool,
    r#"[
        {
            "inputs": [],
            "name": "liquidity",
            "outputs": [{"internalType": "uint128", "name": "", "type": "uint128"}],
            "stateMutability": "view",
            "type": "function"
        },
        {
            "inputs": [],
            "name": "slot0",
            "outputs": [
                {"internalType": "uint160", "name": "sqrtPriceX96", "type": "uint160"},
                {"internalType": "int24", "name": "tick", "type": "int24"},
                {"internalType": "uint16", "name": "observationIndex", "type": "uint16"},
                {"internalType": "uint16", "name": "observationCardinality", "type": "uint16"},
                {"internalType": "uint16", "name": "observationCardinalityNext", "type": "uint16"},
                {"internalType": "bool", "name": "unlocked", "type": "bool"}
            ],
            "stateMutability": "view",
            "type": "function"
        },
        {
            "inputs": [],
            "name": "token0",
            "outputs": [{"internalType": "address", "name": "", "type": "address"}],
            "stateMutability": "view",
            "type": "function"
        },
        {
            "inputs": [],
            "name": "token1",
            "outputs": [{"internalType": "address", "name": "", "type": "address"}],
            "stateMutability": "view",
            "type": "function"
        },
        {
            "inputs": [],
            "name": "tickSpacing",
            "outputs": [{"internalType": "int24", "name": "", "type": "int24"}],
            "stateMutability": "view",
            "type": "function"
        }
    ]"#
);

abigen!(
    AerodromeSlipstreamQuoter,
    r#"[
        {
            "inputs": [
                {"internalType": "address", "name": "tokenIn", "type": "address"},
                {"internalType": "address", "name": "tokenOut", "type": "address"},
                {"internalType": "uint256", "name": "amountIn", "type": "uint256"},
                {"internalType": "int24", "name": "tickSpacing", "type": "int24"}
            ],
            "name": "quoteExactInputSingle",
            "outputs": [
                {"internalType": "uint256", "name": "amountOut", "type": "uint256"},
                {"internalType": "uint160", "name": "sqrtPriceX96After", "type": "uint160"},
                {"internalType": "int24", "name": "tickAfter", "type": "int24"}
            ],
            "stateMutability": "nonpayable",
            "type": "function"
        }
    ]"#
);

// ========== STRUCTURES ==========

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenInfo {
    pub symbol: String,
    pub address: Address,
    pub decimals: u8,
    pub stable: bool,
    pub priority: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenConfig {
    pub version: String,
    pub network: String,
    pub tokens: Vec<TokenInfo>,
}

#[derive(Debug, Clone, PartialEq)]
struct PoolInfo {
    address: Address,
    token0: Address,
    token1: Address,
    dex: DexType,
    name: &'static str,
    is_stable: bool,
    tick_spacing: Option<i32>,
    reserve0: U256,
    reserve1: U256,
    liquidity_usd: f64,
    cached_at: SystemTime,
}

impl PoolInfo {
    fn is_cache_valid(&self) -> bool {
        SystemTime::now()
            .duration_since(self.cached_at)
            .map(|d| d.as_secs() < POOL_CACHE_TTL_SECS)
            .unwrap_or(false)
    }
    
    fn has_sufficient_liquidity(&self) -> bool {
        self.liquidity_usd >= MIN_POOL_LIQUIDITY_USD
    }
}

#[derive(Debug, Clone)]
struct ProfitCalculation {
    amount_bought: U256,
    final_amount: U256,
    net_profit_wei: U256,
    profit_usd: f64,
    total_fees_bps: u64,
}

impl Default for ProfitCalculation {
    fn default() -> Self {
        Self {
            amount_bought: U256::zero(),
            final_amount: U256::zero(),
            net_profit_wei: U256::zero(),
            profit_usd: 0.0,
            total_fees_bps: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct PoolWithReserves {
    pool: PoolInfo,
    reserve0: U256,
    reserve1: U256,
    liquidity_usd: f64,
}

#[derive(Clone)]
struct PriceOracle {
    pool_oracle: Arc<PoolOracle>,
}

impl PriceOracle {
    fn new(pool_oracle: Arc<PoolOracle>) -> Self {
        Self { pool_oracle }
    }

    async fn get_price_usd(&self, token_info: &TokenInfo) -> f64 {
        match self.pool_oracle.get_price_usd(token_info.address, &token_info.symbol).await {
            Ok(price) => price,
            Err(_) => Self::get_fallback_price(&token_info.symbol),
        }
    }

    fn get_fallback_price(symbol: &str) -> f64 {
        match symbol {
            "WETH" => 3300.0,
            "wstETH" => 4000.0,
            "cbETH" => 3650.0,
            "USDC" | "USDT" | "DAI" | "USDbC" => 1.0,
            "cbBTC" => 87000.0,
            "AERO" => 0.52,
            "BRETT" => 0.017,
            "DEGEN" => 0.0013,
            "TOSHI" => 0.0004,
            _ => 1.0,
        }
    }
}

#[derive(Debug, Default)]
struct DetectorStats {
    pools_checked: u64,
    prices_fetched: u64,
    cache_hits: u64,
    cache_misses: u64,
    parallel_scans: u64,
    scan_time_ms: u64,
}

pub struct OpportunityDetector {
    config: Arc<Config>,
    dual_provider: Arc<DualProvider>,
    tokens: Vec<TokenInfo>,
    pool_cache: Arc<RwLock<HashMap<(Address, Address, DexType), Option<PoolInfo>>>>,
    price_oracle: PriceOracle,

    baseswap_factory: Address,
    sushiswap_factory: Address,
    uniswap_v2_factory: Address,
    aerodrome_factory: Address,
    aerodrome_slipstream_factory: Address,
    aerodrome_slipstream_quoter: Address,

    stats: Arc<RwLock<DetectorStats>>,
    circuit_breaker: Arc<CircuitBreaker>,
    multicall_helper: Arc<RwLock<MulticallHelper>>,
    metrics: Arc<RwLock<DetectionMetrics>>,

    blacklisted_pools: Arc<RwLock<HashSet<Address>>>,
    pool_failure_count: Arc<RwLock<HashMap<Address, u32>>>,
}

#[derive(Default, Clone)]
pub struct DetectionMetrics {
    // Scans
    pub scans_total: u64,
    pub scan_duration_ms: Vec<u64>,
    
    // Paires
    pub pairs_checked: u64,
    pub pairs_insufficient_pools: u64,
    pub pairs_insufficient_liquidity: u64,
    
    // Spreads
    pub spreads_calculated: u64,
    pub spreads_too_low: u64,
    pub spreads_too_high: u64,
    pub spreads_valid: u64,
    
    // Profits
    pub profits_calculated: u64,
    pub profits_too_low: u64,
    pub profits_after_gas_negative: u64,
    
    // Opportunités
    pub opportunities_found: u64,
    pub opportunities_executed: u64,
}



// ========== IMPLÉMENTATION - CONSTRUCTEUR ==========

impl OpportunityDetector {
    pub fn new(
        config: Arc<Config>,
        dual_provider: Arc<DualProvider>,
        pool_oracle: Arc<PoolOracle>,
    ) -> Result<Self> {
        let tokens = Self::load_tokens_from_file()?;

        info!("📊 Loaded {} tokens from configuration", tokens.len());

        let high_priority: Vec<_> = tokens.iter()
            .filter(|t| t.priority >= 5)
            .collect();

        info!("   🎯 High priority tokens (≥7): {}", high_priority.len());
        for token in high_priority {
            info!("      • {} (priority: {})", token.symbol, token.priority);
        }

        let multicall_helper = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                MulticallHelper::new(dual_provider.read().clone()).await
            })
        })?;

        Ok(Self {
            config,
            dual_provider,
            tokens,
            pool_cache: Arc::new(RwLock::new(HashMap::new())),
            price_oracle: PriceOracle::new(pool_oracle),
            baseswap_factory: "0xFDa619b6d20975be80A10332cD39b9a4b0FAa8BB".parse()?,
            sushiswap_factory: "0x71524B4f93c58fcbF659783284E38825f0622859".parse()?,
            uniswap_v2_factory: "0x8909Dc15e40173Ff4699343b6eB8132c65e18eC6".parse()?,
            aerodrome_factory: "0x420DD381b31aEf6683db6B902084cB0FFECe40Da".parse()?,
            aerodrome_slipstream_factory: "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A".parse()?,
            aerodrome_slipstream_quoter: "0xA2DEcF05c16537C702779083Fe067e308463CE45".parse()?,
            stats: Arc::new(RwLock::new(DetectorStats::default())),
            metrics: Arc::new(RwLock::new(DetectionMetrics::default())),
            circuit_breaker: Arc::new(CircuitBreaker::new(5, 60)),
            multicall_helper: Arc::new(RwLock::new(multicall_helper)),
            blacklisted_pools: Arc::new(RwLock::new(HashSet::new())),
            pool_failure_count: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    fn load_tokens_from_file() -> Result<Vec<TokenInfo>> {
        let tokens_json = include_str!("../tokens.json");
        let config: TokenConfig = serde_json::from_str(tokens_json)?;
        Ok(config.tokens)
    }


    async fn get_cached_pools(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Vec<PoolInfo>> {
        let mut pools = Vec::new();
        
        // Pool Aerodrome Volatile
        if let Some(pool) = self.get_pool_from_cache(token_a, token_b, DexType::Aerodrome).await {
            if !self.is_pool_blacklisted(pool.address).await {
                pools.push(pool);
            }
        }
        
        // Pool Aerodrome Stable (si tu en as)
        if let Some(pool) = self.get_pool_from_cache_stable(token_a, token_b).await {
            if !self.is_pool_blacklisted(pool.address).await {
                pools.push(pool);
            }
        }
        
        // Pools Aerodrome Slipstream (tous les tick spacings)
        if let Some(pool) = self.get_pool_from_cache(token_a, token_b, DexType::AerodromeSlipstream).await {
            if !self.is_pool_blacklisted(pool.address).await {
                pools.push(pool);
            }
        }
        
        Ok(pools)
    }

    async fn get_pool_from_cache(
        &self,
        token_a: Address,
        token_b: Address,
        dex: DexType,
    ) -> Option<PoolInfo> {
        let cache_key = (token_a, token_b, dex);
        let cache = self.pool_cache.read().await;
        
        if let Some(Some(pool)) = cache.get(&cache_key) {
            // Vérifier que le cache est encore valide (10 minutes par défaut)
            if pool.is_cache_valid() {
                return Some(pool.clone());
            } else {
                debug!("Cache expired for pool {:?}, will refresh", pool.address);
            }
        }
        
        None
    }

    async fn get_pool_from_cache_stable(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Option<PoolInfo> {
        let cache_key = (token_a, token_b, DexType::Aerodrome);
        let cache = self.pool_cache.read().await;
   
        if let Some(Some(pool)) = cache.get(&cache_key) {
            if pool.is_cache_valid() && pool.is_stable {
                return Some(pool.clone());
            }
        }
        
        None
    }

    pub async fn cleanup_expired_cache(&self) {
        let mut cache = self.pool_cache.write().await;
        let initial_size = cache.len();
        
        // Garder seulement les entrées valides
        cache.retain(|_key, pool_opt| {
            if let Some(pool) = pool_opt {
                pool.is_cache_valid()
            } else {
                false
            }
        });
        
        let removed = initial_size - cache.len();
        if removed > 0 {
            info!("🧹 Cleaned up {} expired cache entries", removed);
        }
    }

    pub async fn initialize_pool_discovery(&self) -> Result<()> {
        info!("🔍 Pre-discovering all pools for high-priority tokens...");
        info!("   This will take 30-60 seconds but significantly speeds up detection");
        
        // Récupérer les tokens haute priorité (priority >= 7)
        let high_priority_tokens: Vec<&TokenInfo> = self.tokens
            .iter()
            .filter(|t| t.priority >= 7)
            .collect();
        
        info!("   • {} high-priority tokens found", high_priority_tokens.len());
        
        let mut discovery_count = 0;
        let mut pairs_processed = 0;
        let start = std::time::Instant::now();
        
        // Découvrir toutes les combinaisons de paires
        for i in 0..high_priority_tokens.len() {
            for j in (i + 1)..high_priority_tokens.len() {
                let token_a = high_priority_tokens[i].address;
                let token_b = high_priority_tokens[j].address;
                let symbol_a = &high_priority_tokens[i].symbol;
                let symbol_b = &high_priority_tokens[j].symbol;
                
                // Log toutes les 10 paires pour montrer la progression
                if pairs_processed % 10 == 0 && pairs_processed > 0 {
                    info!("   • Processed {}/{} pairs...", 
                        pairs_processed,
                        (high_priority_tokens.len() * (high_priority_tokens.len() - 1)) / 2
                    );
                }
                
                // Découvrir tous les types de pools pour cette paire (en parallèle)
                let (volatile, stable, slipstream) = tokio::join!(
                    self.find_aerodrome_pool(token_a, token_b, false),
                    self.find_aerodrome_pool(token_a, token_b, true),
                    self.find_aerodrome_slipstream_pool(token_a, token_b),
                );
                
                // Compter les pools trouvés
                if volatile.is_ok() && volatile.as_ref().unwrap().is_some() {
                    discovery_count += 1;
                    debug!("   ✓ Found Aerodrome Volatile pool for {}/{}", symbol_a, symbol_b);
                }
                if stable.is_ok() && stable.as_ref().unwrap().is_some() {
                    discovery_count += 1;
                    debug!("   ✓ Found Aerodrome Stable pool for {}/{}", symbol_a, symbol_b);
                }
                if slipstream.is_ok() && slipstream.as_ref().unwrap().is_some() {
                    discovery_count += 1;
                    debug!("   ✓ Found Slipstream pool for {}/{}", symbol_a, symbol_b);
                }
                
                pairs_processed += 1;
                
                // Petit délai toutes les 5 paires pour ne pas surcharger le RPC
                if pairs_processed % 5 == 0 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        
        // Afficher le résumé
        let cache = self.pool_cache.read().await;
        let pools_cached = cache.len();
        let pools_found = cache.values().filter(|p| p.is_some()).count();
        drop(cache);
        
        let elapsed = start.elapsed().as_secs_f64();
        
        info!("✅ Pool discovery complete!");
        info!("   • Time taken: {:.1}s", elapsed);
        info!("   • Pairs checked: {}", pairs_processed);
        info!("   • Pools found: {}", pools_found);
        info!("   • Cache entries: {}", pools_cached);
        info!("   • Detection speed: {:.1} pairs/sec", pairs_processed as f64 / elapsed);
        
        if pools_found == 0 {
            warn!("⚠️  No pools found! Check your RPC connection and contract addresses");
        }
        
        Ok(())
    }

    pub fn get_token_by_symbol(&self, symbol: &str) -> Result<&TokenInfo> {
        self.tokens
            .iter()
            .find(|t| t.symbol.eq_ignore_ascii_case(symbol))
            .ok_or_else(|| anyhow::anyhow!("Token not found: {}", symbol))
    }

    fn get_token_info(&self, address: Address) -> Result<&TokenInfo> {
        self.tokens
            .iter()
            .find(|t| t.address == address)
            .ok_or_else(|| anyhow::anyhow!("Token not found: {:?}", address))
    }

    // ========== SCAN PRINCIPAL OPTIMISÉ ==========

    pub async fn scan_opportunities(&self) -> Result<Vec<ArbitrageOpportunity>> {
        use futures::stream::{self, StreamExt};

        let scan_start = std::time::Instant::now(); 
        debug!("🔍 Starting arbitrage scan...");

        {
            let mut metrics = self.metrics.write().await;
            metrics.scans_total += 1;
        }

        // Récupérer tokens haute priorité
        let high_priority_tokens: Vec<TokenInfo> = self.tokens
            .iter()
            .filter(|t| t.priority >= 7)
            .cloned()
            .collect();

        let mut pairs = Vec::new();
        for i in 0..high_priority_tokens.len() {
            for j in (i + 1)..high_priority_tokens.len() {
                pairs.push((
                    high_priority_tokens[i].address,
                    high_priority_tokens[j].address,
                ));
            }
        }
        
        let opportunities: Vec<_> = stream::iter(pairs)
            .map(|(token_a, token_b)| async move {
                self.find_arbitrage_for_pair(token_a, token_b).await
            })
            .buffer_unordered(10)  // 10 scans en parallèle simultanément
            .filter_map(|result| async move {
                match result {
                    Ok(Some(opp)) => Some(opp),
                    Ok(None) => None,
                    Err(e) => {
                        debug!("Error checking pair: {}", e);
                        None
                    }
                }
            })
            .collect()
            .await;
        
        let elapsed = scan_start.elapsed();
        
        // Log simplifié
        if opportunities.is_empty() {
            debug!("⏱️  Scan complete in {:.2}s - No opportunities found", elapsed.as_secs_f64());
        } else {
            info!("⏱️  Scan complete in {:.2}s - {} opportunities found!", 
                elapsed.as_secs_f64(), opportunities.len());
        }
        
        Ok(opportunities)
    }

    // ========== ARBITRAGE 2-LEGS ==========

    async fn find_arbitrage_for_pair(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Option<ArbitrageOpportunity>> {
        let token_a_info = self.get_token_info(token_a)?;
        let token_b_info = self.get_token_info(token_b)?;

        {
            let mut metrics = self.metrics.write().await;
            metrics.pairs_checked += 1;
        }

        info!("👀 Checking {}/{} pair", token_a_info.symbol, token_b_info.symbol);

        let (baseswap, sushiswap, uniswap_v2, aerodrome, slipstream) = tokio::join!(
            self.find_pool(token_a, token_b, DexType::UniswapV2, "BaseSwap", self.baseswap_factory),
            self.find_pool(token_a, token_b, DexType::Sushiswap, "SushiSwap", self.sushiswap_factory),
            self.find_pool(token_a, token_b, DexType::UniswapV2, "UniswapV2", self.uniswap_v2_factory),
            self.find_aerodrome_pool(token_a, token_b, false),
            self.find_aerodrome_slipstream_pool(token_a, token_b)
        );

        let mut pools = Vec::new();
        if let Ok(Some(pool)) = baseswap { 
            if !self.is_pool_blacklisted(pool.address).await {
                pools.push(pool);
            }
        }
        if let Ok(Some(pool)) = sushiswap {
            if !self.is_pool_blacklisted(pool.address).await {
                pools.push(pool);
            }
        }
        if let Ok(Some(pool)) = uniswap_v2 {
            if !self.is_pool_blacklisted(pool.address).await {
                pools.push(pool);
            }
        }
        if let Ok(Some(pool)) = aerodrome {
            if !self.is_pool_blacklisted(pool.address).await {
                pools.push(pool);
            }
        }
        if let Ok(Some(pool)) = slipstream {
            if !self.is_pool_blacklisted(pool.address).await {
                pools.push(pool);
            }
        }
        
        // Il faut au moins 2 pools pour un arbitrage
        if pools.len() < 2 {
            info!("⏭️  Not enough pools for {}/{} (found: {})", 
                token_a_info.symbol, token_b_info.symbol, pools.len());

            {
                let mut metrics = self.metrics.write().await;
                metrics.pairs_insufficient_pools += 1;
            }
            return Ok(None);
        }

        info!("✅ Found {} pools for {}/{}", 
            pools.len(), token_a_info.symbol, token_b_info.symbol);

        let token_a_info = self.get_token_info(token_a)?;
        let token_b_info = self.get_token_info(token_b)?;


        info!("🔍 Analyzing {} pools for {}/{}...", pools.len(), token_a_info.symbol, token_b_info.symbol);
        // ✅ Analyser tous les spreads possibles entre pools
        for i in 0..pools.len() {
            for j in (i + 1)..pools.len() {
                let pool_a = &pools[i];
                let pool_b = &pools[j];

                info!("   🔄 Comparing {} vs {}", pool_a.name, pool_b.name);

                // ✅ Calculer les prix de chaque pool
                let price_a_in_pool_a = self.get_effective_price_from_pool(pool_a, token_a, token_b).await?;
                let price_a_in_pool_b = self.get_effective_price_from_pool(pool_b, token_a, token_b).await?;

                info!("      Price in {}: {}", pool_a.name, price_a_in_pool_a);
                info!("      Price in {}: {}", pool_b.name, price_a_in_pool_b);

                if price_a_in_pool_a.is_zero() || price_a_in_pool_b.is_zero() {
                    continue;
                }

                // ✅ Calculer le spread (en bps = basis points)
                let price_a_f64 = price_a_in_pool_a.as_u128() as f64 / 1e18;
                let price_b_f64 = price_a_in_pool_b.as_u128() as f64 / 1e18;
                
                // Déterminer quel pool acheter et quel pool vendre
                let (pool_buy, pool_sell, spread_bps) = if price_a_f64 > price_b_f64 {
                    // Pool A donne PLUS de token_b → cbBTC moins cher dans pool_a → ACHETER cbBTC dans pool_a
                    let spread = ((price_a_f64 - price_b_f64) / price_b_f64) * 10000.0;
                    (pool_b, pool_a, spread)
                } else {
                    // Pool B donne PLUS de token_b → cbBTC moins cher dans pool_b → ACHETER cbBTC dans pool_b
                    let spread = ((price_b_f64 - price_a_f64) / price_a_f64) * 10000.0;
                    (pool_a, pool_b, spread)
                };

                {
                    let mut metrics = self.metrics.write().await;
                    metrics.spreads_calculated += 1;
                }

                info!("📊 Spread {}/{}: {:.2} bps (buy: {}, sell: {})", 
                    token_a_info.symbol, token_b_info.symbol, spread_bps,
                    pool_buy.name, pool_sell.name);

                if spread_bps < MIN_SPREAD_BPS as f64 {
                    info!("❌ Spread too low: {:.2} bps < {} bps", spread_bps, MIN_SPREAD_BPS);
                    
                    {
                        let mut metrics = self.metrics.write().await;
                        metrics.spreads_too_low += 1;
                    }
                    
                    continue;
                }

                if spread_bps > MAX_SPREAD_BPS as f64 {
                    info!("❌ Spread too high: {:.2} bps > {} bps", spread_bps, MAX_SPREAD_BPS);
                    
                    {
                        let mut metrics = self.metrics.write().await;
                        metrics.spreads_too_high += 1;
                    }
                    
                    continue;
                }

                {
                    let mut metrics = self.metrics.write().await;
                    metrics.spreads_valid += 1;
                }

                info!("✅ Valid spread: {:.2} bps", spread_bps);

                // ✅ Calculer le montant optimal basé sur la liquidité du pool
                let optimal_amount = self.calculate_optimal_size_for_pool(
                    pool_buy,
                    token_a,
                    token_a_info,
                );

                info!("   💰 Optimal amount: {} ({} {})", 
                    optimal_amount,
                    optimal_amount.as_u128() as f64 / 10f64.powi(token_a_info.decimals as i32),
                    token_a_info.symbol
                );

                if optimal_amount.is_zero() {
                    info!("   ❌ Optimal amount is zero!");
                    continue;
                }

                // ✅ Vérifier que le pool peut gérer ce montant
                if !self.check_sufficient_liquidity(pool_buy, optimal_amount, token_a).await? {
                    info!("   ❌ Insufficient liquidity in buy pool");
                    continue;
                }

                
                // ✅ Calculer le profit réel avec les swaps exacts
                // Step 1: Swap token_a → token_b dans pool_buy
                let amount_bought = self.calculate_swap_output(
                    pool_buy,
                    token_a,
                    optimal_amount,
                    pool_buy.dex,
                )?;

                info!("   🔄 Step 1 (buy {}): {} {} → {} {}", 
                    pool_buy.name,
                    optimal_amount.as_u128() as f64 / 10f64.powi(token_a_info.decimals as i32),
                    token_a_info.symbol,
                    amount_bought.as_u128() as f64 / 10f64.powi(token_b_info.decimals as i32),
                    token_b_info.symbol
                );

                if amount_bought.is_zero() {
                    info!("❌ First swap would return 0");
                    continue;
                }

                // Step 2: Swap token_b → token_a dans pool_sell
                let final_amount = self.calculate_swap_output(
                    pool_sell,
                    token_b,
                    amount_bought,
                    pool_sell.dex,
                )?;

                info!("   🔄 Step 2 (sell {}): {} {} → {} {}", 
                    pool_sell.name,
                    amount_bought.as_u128() as f64 / 10f64.powi(token_b_info.decimals as i32),
                    token_b_info.symbol,
                    final_amount.as_u128() as f64 / 10f64.powi(token_a_info.decimals as i32),
                    token_a_info.symbol
                );

                if final_amount.is_zero() {
                    info!("❌ Second swap would return 0");
                    continue;
                }

                // Calculer le profit net ou la perte
                let (net_profit_wei, is_loss) = if final_amount > optimal_amount {
                    (final_amount.saturating_sub(optimal_amount), false)
                } else {
                    (optimal_amount.saturating_sub(final_amount), true)
                };

                if is_loss {
                    info!("   💸 Net LOSS: -{} wei (-{} {})", 
                        net_profit_wei,
                        net_profit_wei.as_u128() as f64 / 10f64.powi(token_a_info.decimals as i32),
                        token_a_info.symbol
                    );
                    info!("   📊 Final: {} vs Initial: {} (LOSS: {})", 
                        final_amount, optimal_amount, net_profit_wei
                    );
                    continue;
                } else {
                    info!("   💵 Net profit: {} wei ({} {})", 
                        net_profit_wei,
                        net_profit_wei.as_u128() as f64 / 10f64.powi(token_a_info.decimals as i32),
                        token_a_info.symbol
                    );
                }

                if net_profit_wei.is_zero() {
                    continue;
                }

                // Convertir le profit en USD
                let profit_usd = self.wei_to_usd(net_profit_wei, token_a_info).await;

                {
                    let mut metrics = self.metrics.write().await;
                    metrics.profits_calculated += 1;
                }

                info!("💵 Gross profit: ${:.4} (min: ${:.2})", 
                    profit_usd, self.config.trading.min_profit_usd);

                if profit_usd < self.config.trading.min_profit_usd {
                    info!("❌ Profit too low: ${:.4} < ${:.2}", 
                        profit_usd, self.config.trading.min_profit_usd);

                    {
                        let mut metrics = self.metrics.write().await;
                        metrics.profits_too_low += 1;
                    }
                    continue;
                }

                // ✅ Créer l'opportunité
                info!("💰 OPPORTUNITY FOUND!");
                info!("   • Pair: {}/{}", token_a_info.symbol, token_b_info.symbol);
                info!("   • Spread: {:.2} bps", spread_bps);
                info!("   • Expected profit: ${:.2}", profit_usd);
                info!("   • Buy from: {:?} ({})", pool_buy.address, pool_buy.name);
                info!("   • Sell to: {:?} ({})", pool_sell.address, pool_sell.name);

                // ✅ Créer les steps pour l'arbitrage
                let mut steps = Vec::new();
                
                // Step 1: Acheter token_b avec token_a
                let data_buy = if pool_buy.dex == DexType::AerodromeSlipstream {
                    let tick_spacing = pool_buy.tick_spacing.unwrap_or(100);
                    ethers::abi::encode(&[ethers::abi::Token::Int(U256::from(tick_spacing as u32))])
                } else {
                    vec![]
                };
                
                steps.push(SwapStep {
                    dex: pool_buy.dex,
                    token_in: token_a,
                    token_out: token_b,
                    data: data_buy.into(),
                    min_out: self.calculate_min_output(
                        amount_bought, 
                        pool_buy.dex, 
                        token_b_info, 
                        pool_buy.liquidity_usd
                    ),
                });

                // Step 2: Vendre token_b pour récupérer token_a
                let data_sell = if pool_sell.dex == DexType::AerodromeSlipstream {
                    let tick_spacing = pool_sell.tick_spacing.unwrap_or(100);
                    ethers::abi::encode(&[ethers::abi::Token::Int(U256::from(tick_spacing as u32))])
                } else {
                    vec![]
                };
                
                steps.push(SwapStep {
                    dex: pool_sell.dex,
                    token_in: token_b,
                    token_out: token_a,
                    data: data_sell.into(),
                    min_out: self.calculate_min_output(
                        final_amount, 
                        pool_sell.dex, 
                        token_a_info, 
                        pool_sell.liquidity_usd
                    ),
                });

                // ✅ Créer l'opportunité complète
                let mut opportunity = ArbitrageOpportunity {
                    id: format!("arb-{:?}-{}", token_a, chrono::Utc::now().timestamp()),
                    asset: token_a,
                    amount: optimal_amount,
                    steps,
                    expected_profit: net_profit_wei,
                    expected_profit_usd: profit_usd,
                    gas_estimate: U256::from(300000), // 2 swaps = ~300k gas
                    gas_cost_usd: 0.0,
                    net_profit_usd: 0.0,
                    timestamp: chrono::Utc::now().timestamp() as u64,
                    opportunity_type: OpportunityType::TwoLeg,
                };

                // ✅ Calculer le coût en gas
                let gas_price = self.dual_provider.get_gas_price().await?;
                let gas_price_gwei = gas_price.as_u128() as f64 / 1e9;
                opportunity.calculate_net_profit(gas_price_gwei * self.config.trading.gas_price_multiplier);

                // ✅ Vérifier que c'est toujours profitable après gas
                if opportunity.is_profitable(self.config.trading.min_profit_usd) {
                    info!("✅ OPPORTUNITY VALIDATED - Net profit: ${:.4}", opportunity.net_profit_usd);
    
                    {
                        let mut metrics = self.metrics.write().await;
                        metrics.opportunities_found += 1;
                    }

                    return Ok(Some(opportunity));
                } else {
                    debug!("❌ Not profitable after gas: ${:.4} < ${:.2}", 
                        opportunity.net_profit_usd, self.config.trading.min_profit_usd);
                    
                    {
                        let mut metrics = self.metrics.write().await;
                        metrics.profits_after_gas_negative += 1;
                    }
                }
            }
        }

        Ok(None)
    }

    // ========== ARBITRAGE TRIANGULAIRE ==========

    pub async fn scan_triangular_opportunities(&self) -> Result<Vec<ArbitrageOpportunity>> {
        let scan_start = std::time::Instant::now();
        
        let triangles = vec![
            ("USDC", "WETH", "AERO"),
            ("USDC", "WETH", "cbBTC"),
            ("USDC", "WETH", "DAI"),
            ("USDC", "WETH", "wstETH"),
            ("USDC", "WETH", "USDT"),
            ("USDC", "WETH", "cbETH"),
            
            ("USDC", "AERO", "WETH"),
            ("USDC", "cbBTC", "WETH"),
            ("USDC", "DAI", "WETH"),
            ("USDC", "wstETH", "WETH"),
            ("USDC", "USDT", "WETH"),
            
            ("USDC", "DAI", "USDT"),
            ("USDC", "USDT", "DAI"),
            ("USDC", "DAI", "AERO"),
            ("USDC", "USDT", "AERO"),
            
            ("USDC", "WETH", "BRETT"),
            ("USDC", "BRETT", "WETH"),
            ("USDC", "WETH", "DEGEN"),
            ("USDC", "DEGEN", "WETH"),
            
            ("WETH", "USDC", "AERO"),
            ("WETH", "USDC", "cbBTC"),
            ("WETH", "USDC", "DAI"),
            ("WETH", "AERO", "USDC"),
            ("WETH", "cbBTC", "USDC"),
            ("WETH", "wstETH", "USDC"),
            ("WETH", "cbETH", "USDC"),
            ("WETH", "BRETT", "USDC"),
            
            ("USDC", "AERO", "cbBTC"),
            ("USDC", "cbBTC", "AERO"),
        ];

        debug!("🔺 Scanning {} triangular paths...", triangles.len());

        let mut tasks = Vec::new();
        
        for (symbol_a, symbol_b, symbol_c) in triangles {
            let token_a = match self.get_token_by_symbol(symbol_a) {
                Ok(t) => t.address,
                Err(_) => {
                    debug!("Token {} not found, skipping", symbol_a);
                    continue;
                }
            };
            let token_b = match self.get_token_by_symbol(symbol_b) {
                Ok(t) => t.address,
                Err(_) => {
                    debug!("Token {} not found, skipping", symbol_b);
                    continue;
                }
            };
            let token_c = match self.get_token_by_symbol(symbol_c) {
                Ok(t) => t.address,
                Err(_) => {
                    debug!("Token {} not found, skipping", symbol_c);
                    continue;
                }
            };

            let detector = Self {
                config: self.config.clone(),
                dual_provider: self.dual_provider.clone(),
                tokens: self.tokens.clone(),
                pool_cache: self.pool_cache.clone(),
                price_oracle: self.price_oracle.clone(),
                baseswap_factory: self.baseswap_factory,
                sushiswap_factory: self.sushiswap_factory,
                uniswap_v2_factory: self.uniswap_v2_factory,
                aerodrome_factory: self.aerodrome_factory,
                aerodrome_slipstream_factory: self.aerodrome_slipstream_factory,
                aerodrome_slipstream_quoter: self.aerodrome_slipstream_quoter,
                stats: self.stats.clone(),
                circuit_breaker: self.circuit_breaker.clone(),
                multicall_helper: self.multicall_helper.clone(),
                metrics: self.metrics.clone(),
                blacklisted_pools: self.blacklisted_pools.clone(),
                pool_failure_count: self.pool_failure_count.clone(),
            };
            
            tasks.push(tokio::spawn(async move {
                detector.find_triangular_arbitrage(token_a, token_b, token_c).await
            }));
        }

        let mut opportunities = Vec::new();
        let mut stream = stream::iter(tasks).buffer_unordered(PARALLEL_SCAN_BUFFER);
        
        while let Some(result) = stream.next().await {
            match result {
                Ok(Ok(Some(opp))) => opportunities.push(opp),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => debug!("Error in triangular path: {}", e),
                Err(e) => error!("Task join error: {}", e),
            }
        }

        opportunities.sort_by(|a, b| {
            b.net_profit_usd
                .partial_cmp(&a.net_profit_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let scan_time = scan_start.elapsed().as_millis() as u64;

        if !opportunities.is_empty() {
            info!("✨ Found {} triangular opportunities in {}ms", opportunities.len(), scan_time);
        }

        Ok(opportunities)
    }

    async fn find_triangular_arbitrage(
        &self,
        token_a: Address,
        token_b: Address,
        token_c: Address,
    ) -> Result<Option<ArbitrageOpportunity>> {
        let token_a_info = self.get_token_info(token_a)?;
        let token_b_info = self.get_token_info(token_b)?;
        let token_c_info = self.get_token_info(token_c)?;

        debug!(
            "🔺 Checking triangular: {} → {} → {} → {}",
            token_a_info.symbol,
            token_b_info.symbol,
            token_c_info.symbol,
            token_a_info.symbol
        );

        let (pool_ab_opt, pool_bc_opt, pool_ca_opt) = tokio::join!(
            self.find_best_pool(token_a, token_b),
            self.find_best_pool(token_b, token_c),
            self.find_best_pool(token_c, token_a)
        );

        let pool_ab = match pool_ab_opt? {
            Some(p) => p,
            None => {
                debug!("  ⏭️  No pool found for {}/{}", token_a_info.symbol, token_b_info.symbol);
                return Ok(None);
            }
        };

        let pool_bc = match pool_bc_opt? {
            Some(p) => p,
            None => {
                debug!("  ⏭️  No pool found for {}/{}", token_b_info.symbol, token_c_info.symbol);
                return Ok(None);
            }
        };

        let pool_ca = match pool_ca_opt? {
            Some(p) => p,
            None => {
                debug!("  ⏭️  No pool found for {}/{}", token_c_info.symbol, token_a_info.symbol);
                return Ok(None);
            }
        };

        let amount = self.calculate_optimal_size_for_pool(&pool_ab, token_a, token_a_info);

        if !self.validate_pool_liquidity(&pool_ab, amount, token_a_info).await? {
            debug!("  ⏭️  First pool insufficient liquidity");
            return Ok(None);
        }

        let (price_ab_res, price_bc_res, price_ca_res) = tokio::join!(
            self.get_effective_price_from_pool(&pool_ab, token_a, token_b),
            self.get_effective_price_from_pool(&pool_bc, token_b, token_c),
            self.get_effective_price_from_pool(&pool_ca, token_c, token_a)
        );

        let price_ab = price_ab_res?;
        let price_bc = price_bc_res?;
        let price_ca = price_ca_res?;

        if price_ab.is_zero() || price_bc.is_zero() || price_ca.is_zero() {
            debug!("  ⏭️  One or more zero prices");
            return Ok(None);
        }

        if !self.is_price_reasonable(price_ab, token_a, token_b) ||
           !self.is_price_reasonable(price_bc, token_b, token_c) ||
           !self.is_price_reasonable(price_ca, token_c, token_a) {
            debug!("  ⏭️  One or more unreasonable prices");
            return Ok(None);
        }

        debug!(
            "  📊 Prices: {} @ {:.6} | {} @ {:.6} | {} @ {:.6}",
            pool_ab.name, price_ab.as_u128() as f64 / 1e18,
            pool_bc.name, price_bc.as_u128() as f64 / 1e18,
            pool_ca.name, price_ca.as_u128() as f64 / 1e18,
        );

        let profit = self.calculate_triangular_profit(
            amount,
            token_a,
            token_b,
            token_c,
            price_ab,
            price_bc,
            price_ca,
            pool_ab.dex,
            pool_bc.dex,
            pool_ca.dex,
            token_a_info,
        ).await?;

        let token_path = format!("{}->{}->{}",
            token_a_info.symbol, token_b_info.symbol, token_c_info.symbol
        );
        if !self.validate_profit(profit.profit_usd, &token_path) {
            return Ok(None);
        }

        let amount_b_expected = profit.amount_bought;
        let amount_c_expected = (amount_b_expected.saturating_mul(price_bc))
            .checked_div(U256::from(10u128.pow(18)))
            .unwrap_or(U256::zero());

        let mut steps = Vec::new();
        
        let data_ab = if pool_ab.dex == DexType::AerodromeSlipstream {
            let tick_spacing = pool_ab.tick_spacing.unwrap_or(100);
            ethers::abi::encode(&[ethers::abi::Token::Int(U256::from(tick_spacing as u32))])
        } else {
            vec![]
        };
        steps.push(SwapStep {
            dex: pool_ab.dex,
            token_in: token_a,
            token_out: token_b,
            data: data_ab.into(),
            min_out: self.calculate_min_output(amount_b_expected, pool_ab.dex, token_b_info, pool_ab.liquidity_usd),
        });

        let data_bc = if pool_bc.dex == DexType::AerodromeSlipstream {
            let tick_spacing = pool_bc.tick_spacing.unwrap_or(100);
            ethers::abi::encode(&[ethers::abi::Token::Int(U256::from(tick_spacing as u32))])
        } else {
            vec![]
        };
        steps.push(SwapStep {
            dex: pool_bc.dex,
            token_in: token_b,
            token_out: token_c,
            data: data_bc.into(),
            min_out: self.calculate_min_output(amount_c_expected, pool_bc.dex, token_c_info, pool_bc.liquidity_usd),
        });

        let data_ca = if pool_ca.dex == DexType::AerodromeSlipstream {
            let tick_spacing = pool_ca.tick_spacing.unwrap_or(100);
            ethers::abi::encode(&[ethers::abi::Token::Int(U256::from(tick_spacing as u32))])
        } else {
            vec![]
        };
        steps.push(SwapStep {
            dex: pool_ca.dex,
            token_in: token_c,
            token_out: token_a,
            data: data_ca.into(),
            min_out: self.calculate_min_output(profit.final_amount, pool_ca.dex, token_a_info, pool_ca.liquidity_usd),
        });

        let mut opportunity = ArbitrageOpportunity {
            id: format!("tri-{:?}-{}", token_a, chrono::Utc::now().timestamp()),
            asset: token_a,
            amount,
            steps,
            expected_profit: profit.net_profit_wei,
            expected_profit_usd: profit.profit_usd,
            gas_estimate: U256::from(450000),
            gas_cost_usd: 0.0,
            net_profit_usd: 0.0,
            timestamp: chrono::Utc::now().timestamp() as u64,
            opportunity_type: OpportunityType::Triangular,
        };

        let gas_price = self.dual_provider.get_gas_price().await?;
        let gas_price_gwei = gas_price.as_u128() as f64 / 1e9;
        opportunity.calculate_net_profit(gas_price_gwei * self.config.trading.gas_price_multiplier);

        if opportunity.is_profitable(self.config.trading.min_profit_usd) {
            info!(
                "💎 TRIANGULAR FOUND: {} → {} → {} → {} | Net: ${:.2}",
                token_a_info.symbol,
                token_b_info.symbol,
                token_c_info.symbol,
                token_a_info.symbol,
                opportunity.net_profit_usd
            );
            return Ok(Some(opportunity));
        }

        debug!(" ❌  Not profitable after gas: ${:.2} (expected: ${:.2}, gas: ${:.2})",
        opportunity.net_profit_usd,
        opportunity.expected_profit_usd,
        opportunity.gas_cost_usd
        );
        Ok(None)
    }

    async fn find_best_pool(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Option<PoolInfo>> {
        let (baseswap, sushiswap, uniswap_v2, aerodrome, slipstream) = tokio::join!(
            self.find_pool(token_a, token_b, DexType::UniswapV2, "BaseSwap", self.baseswap_factory),
            self.find_pool(token_a, token_b, DexType::Sushiswap, "SushiSwap", self.sushiswap_factory),
            self.find_pool(token_a, token_b, DexType::UniswapV2, "UniswapV2", self.uniswap_v2_factory),
            self.find_aerodrome_pool(token_a, token_b, false),
            self.find_aerodrome_slipstream_pool(token_a, token_b)
        );

        let mut pools = Vec::new();
        if let Ok(Some(pool)) = baseswap { pools.push(pool); }
        if let Ok(Some(pool)) = sushiswap { pools.push(pool); }
        if let Ok(Some(pool)) = uniswap_v2 { pools.push(pool); }
        if let Ok(Some(pool)) = aerodrome { pools.push(pool); }
        if let Ok(Some(pool)) = slipstream { pools.push(pool); }

        if pools.is_empty() {
            return Ok(None);
        }

        let mut best_pool: Option<PoolInfo> = None;
        let mut best_liquidity_usd = 0.0;

        for pool in pools {
            if pool.liquidity_usd > best_liquidity_usd {
                best_liquidity_usd = pool.liquidity_usd;
                best_pool = Some(pool);
            }
        }

        if let Some(ref pool) = best_pool {
            debug!("  ✅ Best pool: {} (${:.0} liquidity)", pool.name, best_liquidity_usd);
        }

        Ok(best_pool)
    }

    async fn find_best_pool_by_dex(
        &self,
        token_a: Address,
        token_b: Address,
        dex: DexType,
    ) -> Result<Option<PoolInfo>> {
        let pools = self.get_cached_pools(token_a, token_b).await?;
        
        // Trouver le pool du DEX spécifié avec le plus de liquidité
        let mut best_pool: Option<PoolInfo> = None;
        
        for pool in pools {
            if pool.dex == dex {
                if best_pool.is_none() || pool.liquidity_usd > best_pool.as_ref().unwrap().liquidity_usd {
                    best_pool = Some(pool);
                }
            }
        }
        
        Ok(best_pool)
    }



    // ========== VALIDATION METHODS ==========

    async fn validate_pool_liquidity(
        &self,
        pool: &PoolInfo,
        amount_in: U256,
        token_in: &TokenInfo,
    ) -> Result<bool> {
        let reserve_in = if pool.token0 == token_in.address {
            pool.reserve0
        } else {
            pool.reserve1
        };

        let min_reserve = amount_in.saturating_mul(U256::from(MIN_RESERVE_MULTIPLIER));

        if reserve_in < min_reserve {
            if reserve_in < amount_in.saturating_mul(U256::from(3)) {
                debug!(
                    "  ⚠️  {} insufficient liquidity: {:.6} {} < {:.6} {} required",
                    pool.name,
                    reserve_in.as_u128() as f64 / 10f64.powi(token_in.decimals as i32),
                    token_in.symbol,
                    min_reserve.as_u128() as f64 / 10f64.powi(token_in.decimals as i32),
                    token_in.symbol
                );
            }
            return Ok(false);
        }

        let trade_impact_bps = if reserve_in > U256::zero() {
            (amount_in.saturating_mul(U256::from(10000)))
                .checked_div(reserve_in)
                .unwrap_or(U256::from(10000))
                .as_u64()
        } else {
            10000
        };

        if trade_impact_bps > MAX_TRADE_IMPACT_BPS {
            debug!(
                "  ⚠️  {} trade impact too high: {:.2}% > {:.2}%",
                pool.name,
                trade_impact_bps as f64 / 100.0,
                MAX_TRADE_IMPACT_BPS as f64 / 100.0
            );
            return Ok(false);
        }

        Ok(true)
    }

    fn validate_spread(&self, spread_bps: U256, token_a: &str, token_b: &str) -> bool {
        let spread = spread_bps.as_u64();

        // Check minimum spread
        if spread < MIN_SPREAD_BPS {
            debug!("  ⏭️  Spread too low: {} bps < {} bps for {}/{}",
                spread, MIN_SPREAD_BPS, token_a, token_b
            );
            return false;
        }

        // Check maximum spread
        if spread > MAX_SPREAD_BPS {
            warn!("  ⚠️  Spread suspiciously high: {} bps > {} bps for {}/{}",
                spread, MAX_SPREAD_BPS, token_a, token_b
            );
            return false;
        }

        true
    }

    async fn validate_opportunity_enhanced(
        &self,
        opp: &mut ArbitrageOpportunity,
    ) -> Result<bool> {
        let profit_to_gas_ratio = if opp.gas_cost_usd > 0.0 {
            opp.net_profit_usd / opp.gas_cost_usd
        } else {
            0.0
        };

        if profit_to_gas_ratio < MIN_PROFIT_TO_GAS_RATIO {
            debug!("  ❌ Profit/gas too low: {:.2}x (min: {:.2}x)", 
                profit_to_gas_ratio, MIN_PROFIT_TO_GAS_RATIO);
            return Ok(false);
        }

        if opp.steps.len() >= 2 {
            let spread_bps = opp.expected_profit
                .saturating_mul(U256::from(10000))
                .checked_div(opp.amount)
                .unwrap_or(U256::zero())
                .as_u64();

            if spread_bps > 1000 {
                warn!("  🚨 Suspicious spread: {} bps", spread_bps);
                return Ok(false);
            }
        }

        let age = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() - opp.timestamp;

        if age > MAX_OPPORTUNITY_AGE_SECS {
            debug!("  ⏰ Opportunity {} seconds old, adjusting slippage", age);

            for step in &mut opp.steps {
                let current = U256::from(step.min_out);
                let reduced = current
                    .saturating_mul(U256::from(98))
                    .checked_div(U256::from(100))
                    .unwrap_or(current);
                step.min_out = reduced.as_u128();
            }
        }

        Ok(true)
    }

    async fn check_sufficient_liquidity(
        &self,
        pool: &PoolInfo,
        amount: U256,
        token: Address,
    ) -> Result<bool> {

        // ✅ 1. Identifier les bonnes réserves
        let (reserve_in, _) = if pool.token0 == token {
            (pool.reserve0, pool.reserve1)
        } else {
            (pool.reserve1, pool.reserve0)
        };

        // ✅ 2. Vérifier que la réserve n'est pas nulle
        if reserve_in.is_zero() {
            warn!("  ⚠️  Pool {:?} has zero reserves for token {:?}", pool.address, token);
            return Ok(false);
        }

        // ✅ 3. Calculer l'impact CORRECTEMENT
        let impact_bps = amount
            .saturating_mul(U256::from(10000))
            .checked_div(reserve_in)
            .unwrap_or(U256::from(10000))  // ⚠️  FIX: 100% d'impact si la division échoue, PAS U256::MAX !
            .as_u64();

        // ✅ 4. Vérifier l'impact maximum
        if impact_bps > MAX_TRADE_IMPACT_BPS {
            warn!("  ⚠️  Impact {} bps too high (pool: {}, amount: {}, reserve: {})", 
                impact_bps, pool.name, amount, reserve_in);
            return Ok(false);
        }

        // ✅ 5. Vérifier la liquidité minimale (3x le montant du trade)
        let min_reserve = amount.saturating_mul(U256::from(MIN_RESERVE_MULTIPLIER));
        if reserve_in < min_reserve {
            warn!("  ⚠️  Insufficient liquidity (pool: {}, reserve: {}, min required: {})", 
                pool.name, reserve_in, min_reserve);
            return Ok(false);
        }

        Ok(true)
    }

    fn is_price_reasonable(&self, price: U256, token_in: Address, token_out: Address) -> bool {
        let token_in_info = match self.get_token_info(token_in) {
            Ok(info) => info,
            Err(_) => return false,
        };

        let token_out_info = match self.get_token_info(token_out) {
            Ok(info) => info,
            Err(_) => return false,
        };

        let price_f64 = price.as_u128() as f64 / 1e18;

        if price_f64 <= 0.0 {
            return false;
        }

        if Self::is_stablecoin(&token_in_info.symbol) && Self::is_stablecoin(&token_out_info.symbol) {
            if price_f64 < 0.90 || price_f64 > 1.10 {
                return false;
            }
            if price_f64 > 100.0 || price_f64 < 0.01 {
                return false;
            }
            return true;
        }

        if price_f64 > 1_000_000.0 {
            return false;
        }

        true
    }

    fn is_stablecoin(symbol: &str) -> bool {
        matches!(symbol, "USDC" | "USDT" | "DAI" | "USDbC")
    }

    fn is_eth_derivative(symbol: &str) -> bool {
        matches!(symbol, "WETH" | "wstETH")
    }

    fn validate_profit(&self, profit_usd: f64, token_pair: &str) -> bool {
        if profit_usd <= 0.0 {
            debug!("  ❌ {} profit negative: ${:.4}", token_pair, profit_usd);
            return false;
        }

        if profit_usd < self.config.trading.min_profit_usd {
            info!("  💰 {} profit ${:.4} < ${:.2} minimum (REJECTED)",
                token_pair, profit_usd, self.config.trading.min_profit_usd);
            return false;
        }

        if profit_usd > 1000.0 {
            warn!("  🚨 Profit suspiciously high: ${:.2} for {}", profit_usd, token_pair);
            return false;
        }

        true
    }

    // ========== PROFIT CALCULATION ==========

    async fn calculate_real_profit(
        &self,
        amount_in: U256,
        price_buy: U256,  // On va l'ignorer
        price_sell: U256, // On va l'ignorer
        dex_buy: DexType,
        dex_sell: DexType,
        token_a_info: &TokenInfo,
        token_b_info: &TokenInfo,
    ) -> Result<ProfitCalculation> {
        // ✅ STEP 1 : Récupérer le pool d'achat
        let pool_buy = match self.find_best_pool_by_dex(
            token_a_info.address, 
            token_b_info.address, 
            dex_buy
        ).await? {
            Some(p) => p,
            None => return Ok(ProfitCalculation::default()),
        };

        // ✅ STEP 2 : Simuler l'achat de token_b avec amount_in de token_a
        let amount_bought = self.calculate_swap_output(
            &pool_buy,
            token_a_info.address,
            amount_in,
            dex_buy,
        )?;

        if amount_bought.is_zero() {
            return Ok(ProfitCalculation::default());
        }

        // ✅ STEP 3 : Récupérer le pool de vente
        let pool_sell = match self.find_best_pool_by_dex(
            token_b_info.address,
            token_a_info.address,
            dex_sell
        ).await? {
            Some(p) => p,
            None => return Ok(ProfitCalculation::default()),
        };

        // ✅ STEP 4 : Simuler la vente de amount_bought de token_b pour token_a
        let final_amount = self.calculate_swap_output(
            &pool_sell,
            token_b_info.address,
            amount_bought,
            dex_sell,
        )?;

        // ✅ STEP 5 : Calculer le profit net
        let net_profit_wei = if final_amount > amount_in {
            final_amount.saturating_sub(amount_in)
        } else {
            U256::zero()
        };

        let profit_usd = self.wei_to_usd(net_profit_wei, token_a_info).await;

        let fee_buy_bps = self.get_dex_fee(dex_buy);
        let fee_sell_bps = self.get_dex_fee(dex_sell);
        let total_fees_bps = fee_buy_bps + fee_sell_bps;

        Ok(ProfitCalculation {
            amount_bought,
            final_amount,
            net_profit_wei,
            profit_usd,
            total_fees_bps,
        })
    }

    async fn calculate_triangular_profit(
        &self,
        amount_in: U256,
        token_a: Address,
        token_b: Address,
        token_c: Address,
        price_ab: U256,
        price_bc: U256,
        price_ca: U256,
        dex_ab: DexType,
        dex_bc: DexType,
        dex_ca: DexType,
        token_a_info: &TokenInfo,
    ) -> Result<ProfitCalculation> {
        let fee_ab = self.get_dex_fee(dex_ab);
        let fee_bc = self.get_dex_fee(dex_bc);
        let fee_ca = self.get_dex_fee(dex_ca);
        let total_fees_bps = fee_ab + fee_bc + fee_ca;

        let token_b_address = self.get_intermediate_token(token_a_info.address, dex_ab)?;
        let amount_b = self.quote_exact_output(
            token_a,
            token_b,
            amount_in,
            dex_ab
        ).await?;

        if amount_b.is_zero() {
            debug!("  ❌ Triangular: First swap would return 0");
            return Ok(ProfitCalculation::default());
        }

        let token_c_address = self.get_intermediate_token(token_b_address, dex_bc)?;
        let amount_c = self.quote_exact_output(
            token_b,
            token_c,
            amount_b,
            dex_bc
        ).await?;

        if amount_c.is_zero() {
            debug!("  ❌ Triangular: Second swap would return 0");
            return Ok(ProfitCalculation::default());
        }

        let final_amount = self.quote_exact_output(
            token_c,
            token_a,
            amount_c,
            dex_ca
        ).await?;

        if final_amount.is_zero() {
            debug!("  ❌ Triangular: Final swap would return 0");
            return Ok(ProfitCalculation::default());
        }

        let net_profit_wei = if final_amount > amount_in {
            final_amount.saturating_sub(amount_in)
        } else {
            U256::zero()
        };

        let profit_usd = self.wei_to_usd(net_profit_wei, token_a_info).await;


        debug!("  📊 Triangular quotes:");
        debug!("     Step 1 ({:?}): {} → {}", dex_ab, amount_in, amount_b);
        debug!("     Step 2 ({:?}): {} → {}", dex_bc, amount_b, amount_c);
        debug!("     Step 3 ({:?}): {} → {}", dex_ca, amount_c, final_amount);
        debug!("     Net profit: {} wei", net_profit_wei);
        debug!("     Profit USD: ${:.4}", profit_usd);


        Ok(ProfitCalculation {
            amount_bought: amount_b,
            final_amount,
            net_profit_wei,
            profit_usd,
            total_fees_bps,
        })
    }


    async fn quote_exact_output(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        dex: DexType,
    ) -> Result<U256> {
        match dex {
            DexType::Aerodrome | DexType::UniswapV2 | DexType::Sushiswap => {
                // UniswapV2-style: calculer via réserves (pas de quoter dédié)
                self.quote_v2_style(token_in, token_out, amount_in, dex).await
            }
            DexType::AerodromeSlipstream | DexType::UniswapV3 => {
                // UniswapV3-style: utiliser le quoter
                self.quote_v3_style(token_in, token_out, amount_in, dex).await
            }
            _ => {
                warn!("  ⚠️  No quoter for {:?}, using fallback", dex);
                self.quote_v2_style(token_in, token_out, amount_in, dex).await
            }
        }
    }

    // ========== QUOTER V2-STYLE (Aerodrome, UniV2, Sushi) ==========

    async fn quote_v2_style(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        dex: DexType,
    ) -> Result<U256> {
    
        // ✅ Pour Aerodrome: utiliser getAmountOut() on-chain
        if dex == DexType::Aerodrome {
            return self.quote_aerodrome_onchain(token_in, token_out, amount_in).await;
        }
        
        // Pour les autres DEX (UniV2, Sushi): calculer via réserves en cache
        let pool_info = self.find_pool_for_pair(token_in, token_out, dex).await?;
        
        if pool_info.is_none() {
            return Ok(U256::zero());
        }
        
        let pool = pool_info.unwrap();

        // Formule UniswapV2: amountOut = (amountIn * 997 * reserveOut) / (reserveIn * 1000 + amountIn * 997)
        let (reserve_in, reserve_out) = if pool.token0 == token_in {
            (pool.reserve0, pool.reserve1)
        } else {
            (pool.reserve1, pool.reserve0)
        };
        
        if reserve_in.is_zero() || reserve_out.is_zero() {
            return Ok(U256::zero());
        }
        
        let fee_bps = self.get_dex_fee(dex);
        let fee_multiplier = 10000 - fee_bps;
        
        // amountInWithFee = amountIn * (10000 - fee)
        let amount_in_with_fee = amount_in
            .saturating_mul(U256::from(fee_multiplier));
        
        // numerator = amountInWithFee * reserveOut
        let numerator = amount_in_with_fee
            .saturating_mul(reserve_out);
        
        // denominator = reserveIn * 10000 + amountInWithFee
        let denominator = reserve_in
            .saturating_mul(U256::from(10000))
            .saturating_add(amount_in_with_fee);
        
        if denominator.is_zero() {
            return Ok(U256::zero());
        }
        
        let amount_out = numerator
            .checked_div(denominator)
            .unwrap_or(U256::zero());
        
        Ok(amount_out)
    }

    // ========== QUOTER AERODROME ON-CHAIN ==========
    
    async fn quote_aerodrome_onchain(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256> {
        let pool_address = match self.get_pool_address(token_in, token_out, DexType::Aerodrome).await? {
            Some(addr) => addr,
            None => {
                debug!("  ⚠️  No Aerodrome pool found for {:?} → {:?}", token_in, token_out);
                return Ok(U256::zero());
            }
        };
        
        // ✅ Call getAmountOut() directly on-chain
        let pool_contract = IAerodromePool::new(pool_address, self.dual_provider.read().clone());
        
        match tokio::time::timeout(
            Duration::from_millis(2000),
            pool_contract.get_amount_out(amount_in, token_in).call()
        ).await {
            Ok(Ok(amount_out)) => {
                debug!("  ✅ Aerodrome on-chain quote: {} in → {} out (pool: {:?})", 
                    amount_in, amount_out, pool_address);
                Ok(amount_out)
            }
            Ok(Err(e)) => {
                debug!("  ⚠️  Aerodrome on-chain quote failed: {} (pool: {:?})", e, pool_address);
                Ok(U256::zero())
            }
            Err(_) => {
                debug!("  ⚠️  Aerodrome on-chain quote timeout (pool: {:?})", pool_address);
                Ok(U256::zero())
            }
        }
    }



    // ========== QUOTER V3-STYLE (Slipstream, UniV3) ==========

    async fn quote_v3_style(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        dex: DexType,
    ) -> Result<U256> {
        if dex == DexType::AerodromeSlipstream {
            let quoter = AerodromeSlipstreamQuoter::new(
                self.aerodrome_slipstream_quoter,
                self.dual_provider.read().clone()
            );
            
            // Try different tick spacings with timeout
            for tick_spacing in [100, 1, 50, 200] {
                match tokio::time::timeout(
                    Duration::from_millis(1500),
                    quoter.quote_exact_input_single(token_in, token_out, amount_in, tick_spacing).call()
                ).await {
                    Ok(Ok((amount_out, _, _))) if amount_out > U256::zero() => {
                        debug!("  ✅ Slipstream quote (tick {}): {} in → {} out", 
                            tick_spacing, amount_in, amount_out);
                        return Ok(amount_out);
                    }
                    Ok(Ok(_)) => {
                        debug!("  ⏭️  Slipstream tick {} returned 0", tick_spacing);
                        continue;
                    }
                    Ok(Err(e)) => {
                        debug!("  ⚠️  Slipstream tick {} failed: {}", tick_spacing, e);
                        continue;
                    }
                    Err(_) => {
                        debug!("  ⏱️  Slipstream tick {} timeout", tick_spacing);
                        continue;
                    }
                }
            }
            
            debug!("  ❌ No valid Slipstream quote found");
        }
        
        // Fallback to V2 if V3 fails
        Ok(U256::zero())
    }

    // ========== HELPER: TROUVER POOL ==========

    async fn find_pool_for_pair(
        &self,
        token_a: Address,
        token_b: Address,
        dex: DexType,
    ) -> Result<Option<PoolInfo>> {
        let cache_key = (token_a, token_b, dex);
        
        // Check cache
        {
            let cache = self.pool_cache.read().await;
            if let Some(cached) = cache.get(&cache_key) {
                if let Some(pool) = cached {
                    if pool.is_cache_valid() {
                        return Ok(Some(pool.clone()));
                    }
                }
            }
        }
        
        // Discover pool
        match dex {
            DexType::Aerodrome => {
                self.find_aerodrome_pool(token_a, token_b, false).await
            }
            DexType::AerodromeSlipstream => {
                self.find_aerodrome_slipstream_pool(token_a, token_b).await
            }
            DexType::UniswapV2 => {
                self.find_pool(token_a, token_b, dex, "UniswapV2", self.uniswap_v2_factory).await
            }
            DexType::Sushiswap => {
                self.find_pool(token_a, token_b, dex, "Sushiswap", self.sushiswap_factory).await
            }
            _ => Ok(None),
        }
    }

    // ========== HELPER: GET POOL ADDRESS (NO CACHE) ==========

    async fn get_pool_address(
        &self,
        token_a: Address,
        token_b: Address,
        dex: DexType,
    ) -> Result<Option<Address>> {
        match dex {
            DexType::Aerodrome => {
                let factory = IAerodromeFactory::new(
                    self.aerodrome_factory,
                    self.dual_provider.read().clone(),
                );
                
                // Try both volatile and stable
                match factory.get_pool(token_a, token_b, false).call().await {
                    Ok(addr) if addr != Address::zero() => Ok(Some(addr)),
                    _ => {
                        // Try stable pool
                        match factory.get_pool(token_a, token_b, true).call().await {
                            Ok(addr) if addr != Address::zero() => Ok(Some(addr)),
                            _ => Ok(None),
                        }
                    }
                }
            }
            DexType::AerodromeSlipstream => {
                let factory = AerodromeSlipstreamFactory::new(
                    self.aerodrome_slipstream_factory,
                    self.dual_provider.read().clone(),
                );
                
                let (token0, token1) = if token_a < token_b {
                    (token_a, token_b)
                } else {
                    (token_b, token_a)
                };
                
                // Try different tick spacings
                for tick_spacing in [100, 1, 50, 200] {
                    match factory.get_pool(token0, token1, tick_spacing).call().await {
                        Ok(addr) if addr != Address::zero() => {
                            return Ok(Some(addr));
                        }
                        _ => continue,
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }



    // ========== HELPER: TOKEN INTERMÉDIAIRE ==========

    fn get_intermediate_token(&self, from_token: Address, dex: DexType) -> Result<Address> {
        
        let usdc = self.config.tokens.usdc;
        let weth = self.config.tokens.weth;
        let dai = self.config.tokens.dai;
        
        // Logique simplifiée: USDC → AERO → WETH → USDC
        if from_token == usdc {
            Ok("0x940181a94A35A4569E4529A3CDfB74e38FD98631".parse()?) // AERO
        } else if from_token.to_string().to_lowercase() == "0x940181a94a35a4569e4529a3cdfb74e38fd98631" {
            Ok(weth)
        } else {
            Ok(usdc)
        }
    }


    fn apply_price_with_decimals(
        &self,
        amount: U256,
        price: U256,
        decimals_in: u8,
        decimals_out: u8,
    ) -> Result<U256> {
        let result = if decimals_in == decimals_out {
            amount
                .saturating_mul(price)
                .checked_div(U256::from(10u128.pow(18)))
                .unwrap_or(U256::zero())
        } else if decimals_in > decimals_out {
            let diff = decimals_in - decimals_out;
            if diff > 38 {
                return Ok(U256::zero());
            }

            let divisor = U256::from(10u128.pow(diff as u32));
            amount
                .saturating_mul(price)
                .checked_div(U256::from(10u128.pow(18)))
                .and_then(|v| v.checked_div(divisor))
                .unwrap_or(U256::zero())
        } else {
            let diff = decimals_out - decimals_in;
            if diff > 38 {
                return Ok(U256::zero());
            }

            let multiplier = U256::from(10u128.pow(diff as u32));
            amount
                .saturating_mul(price)
                .saturating_mul(multiplier)
                .checked_div(U256::from(10u128.pow(18)))
                .unwrap_or(U256::zero())
        };

        Ok(result)
    }

    fn get_dex_fee(&self, dex: DexType) -> u64 {
        match dex {
            DexType::Aerodrome => 2,
            DexType::AerodromeSlipstream => 10,
            DexType::UniswapV2 => 30,
            DexType::Sushiswap => 30,
            DexType::UniswapV3 => 30,
            DexType::Curve => 4,
            DexType::Balancer => 25,
        }
    }

    async fn wei_to_usd(&self, amount: U256, token_info: &TokenInfo) -> f64 {
        let amount_f64 = amount.as_u128() as f64 / 10f64.powi(token_info.decimals as i32);
        let price_usd = self.price_oracle.get_price_usd(token_info).await;
        amount_f64 * price_usd
    }

    fn calculate_min_output(
        &self, 
        expected: U256, 
        dex: DexType, 
        token_info: &TokenInfo,
        pool_liquidity_usd: f64,
    ) -> u128 {
        let base_slippage_bps = self.config.trading.max_slippage_bps;

        let liquidity_adjustment_bps = if pool_liquidity_usd < 50_000.0 {
            50
        } else if pool_liquidity_usd > 500_000.0 {
            10
        } else {
            30
        };

        let dex_adjustment_bps = match dex {
            DexType::AerodromeSlipstream => 100,
            DexType::UniswapV3 => 70,
            _ => 40,
        };

        let total_slippage_bps = base_slippage_bps
            .saturating_add(liquidity_adjustment_bps)
            .saturating_add(dex_adjustment_bps)
            .min(500);

        let min_out = expected
            .saturating_mul(U256::from(10000 - total_slippage_bps))
            .checked_div(U256::from(10000))
            .unwrap_or(U256::zero());

        let absolute_min = expected
            .saturating_mul(U256::from(9700))
            .checked_div(U256::from(10000))
            .unwrap_or_else(|| {
                let min_decimals = token_info.decimals.saturating_sub(2);
                U256::from(10u128.pow(min_decimals as u32))
            });

        let final_min_out = min_out.max(absolute_min);

        info!("📊 MinOut Calculation:");
        info!("   Token: {}", token_info.symbol);
        info!("   Expected: {}", expected);
        info!("   Base slippage: {} bps", base_slippage_bps);
        info!("   Liquidity adj: {} bps", liquidity_adjustment_bps);
        info!("   DEX adj: {} bps", dex_adjustment_bps);
        info!("   Total slippage: {} bps ({}%)", total_slippage_bps, total_slippage_bps as f64 / 100.0);
        info!("   Min out (slippage): {}", min_out);
        info!("   Absolute min: {}", absolute_min);
        info!("   ✅ Final min out: {} ({}% of expected)", 
            final_min_out, 
            (final_min_out.as_u128() * 100 / expected.as_u128())
        );
        
        if final_min_out.is_zero() {
            return absolute_min.as_u128();
        }
        
        final_min_out.as_u128()
        
    }

    fn calculate_optimal_size_for_pool(
        &self,
        pool: &PoolInfo,
        token: Address,
        token_info: &TokenInfo,
    ) -> U256 {
        let reserve_in = if pool.token0 == token {
            pool.reserve0
        } else {
            pool.reserve1
        };

        if reserve_in.is_zero() {
            return U256::zero();
        }

        let max_impact_fraction = 200;
        
        let optimal_amount = reserve_in
            .saturating_mul(U256::from(max_impact_fraction))
            .checked_div(U256::from(10000))
            .unwrap_or(U256::zero());

        let max_amount = if token_info.stable {
            U256::from(2000).saturating_mul(U256::exp10(token_info.decimals as usize))
        } else if Self::is_eth_derivative(&token_info.symbol) {
            U256::from(10u128.pow(17))
        } else {
            U256::from(1000).saturating_mul(U256::exp10(token_info.decimals as usize))
        };

        let final_amount = std::cmp::min(optimal_amount, max_amount);

        if final_amount.is_zero() {
            return U256::zero();
        }

        debug!("  💰 Optimal size: {} for {} (reserve: {}, impact: ~{}%)", 
            final_amount, 
            token_info.symbol,
            reserve_in,
            (final_amount.as_u128() * 100) / reserve_in.as_u128()
        );

        final_amount 
    }

    async fn calculate_pool_liquidity_usd(
        &self,
        token0: Address,
        token1: Address,
        reserve0: U256,
        reserve1: U256,
    ) -> f64 {
        let token0_info = match self.get_token_info(token0) {
            Ok(info) => info,
            Err(_) => return 0.0,
        };

        let token1_info = match self.get_token_info(token1) {
            Ok(info) => info,
            Err(_) => return 0.0,
        };

        let reserve0_f64 = reserve0.as_u128() as f64 / 10f64.powi(token0_info.decimals as i32);
        let reserve1_f64 = reserve1.as_u128() as f64 / 10f64.powi(token1_info.decimals as i32);

        let price0 = self.price_oracle.get_price_usd(token0_info).await;
        let price1 = self.price_oracle.get_price_usd(token1_info).await;

        (reserve0_f64 * price0) + (reserve1_f64 * price1)
    }

    fn calculate_swap_output(
        &self,
        pool: &PoolInfo,
        token_in: Address,
        amount_in: U256,
        dex: DexType,
    ) -> Result<U256> {
        
        // Déterminer les réserves dans le bon ordre
        let (reserve_in, reserve_out) = if pool.token0 == token_in {
            (pool.reserve0, pool.reserve1)
        } else {
            (pool.reserve1, pool.reserve0)
        };

        if reserve_in.is_zero() || reserve_out.is_zero() {
            return Ok(U256::zero());
        }

        // Obtenir les frais du DEX (en basis points)
        let fee_bps = self.get_dex_fee(dex);
        let fee_multiplier = 10000 - fee_bps;

        // ✅ Formule UniswapV2 SANS ajustement de décimales
        // Les réserves sont déjà dans les bonnes unités !
        let numerator = amount_in
            .saturating_mul(U256::from(fee_multiplier))
            .saturating_mul(reserve_out);

        let denominator = reserve_in
            .saturating_mul(U256::from(10000))
            .saturating_add(amount_in.saturating_mul(U256::from(fee_multiplier)));

        if denominator.is_zero() {
            return Ok(U256::zero());
        }

        let amount_out = numerator
            .checked_div(denominator)
            .unwrap_or(U256::zero());

        Ok(amount_out)
    }   

}

impl OpportunityDetector {

    // ========== POOL DISCOVERY METHODS ==========

    async fn find_pool(
        &self,
        token_a: Address,
        token_b: Address,
        dex: DexType,
        name: &'static str,
        factory_address: Address,
    ) -> Result<Option<PoolInfo>> {
        let cache_key = (token_a, token_b, dex);

        {
            let cache = self.pool_cache.read().await;
            if let Some(cached) = cache.get(&cache_key) {
                if cached.is_none() {
                    let mut stats = self.stats.write().await;
                    stats.cache_hits += 1;
                    return Ok(None);
                }

                if let Some(pool) = cached {
                    if pool.is_cache_valid() {
                        let mut stats = self.stats.write().await;
                        stats.cache_hits += 1;
                        return Ok(Some(pool.clone()));
                    }
                }
            }
        }

        {
            let mut stats = self.stats.write().await;
            stats.cache_misses += 1;
            stats.pools_checked += 1;
        }

        let factory = IUniswapV2Factory::new(
            factory_address,
            self.dual_provider.read().clone()
        );

        let pair_address = match factory.get_pair(token_a, token_b).call().await {
            Ok(addr) => addr,
            Err(_) => {
                let mut cache = self.pool_cache.write().await;
                cache.insert(cache_key, None);
                return Ok(None);
            }
        };

        if pair_address == Address::zero() {
            let mut cache = self.pool_cache.write().await;
            cache.insert(cache_key, None);
            return Ok(None);
        }

        let pair = IUniswapV2Pair::new(
            pair_address,
            self.dual_provider.read().clone()
        );

        let (reserve0, reserve1, _) = match pair.get_reserves().call().await {
            Ok(r) => r,
            Err(_) => {
                let mut cache = self.pool_cache.write().await;
                cache.insert(cache_key, None);
                return Ok(None);
            }
        };

        let token0 = match pair.token_0().call().await {
            Ok(t) => t,
            Err(_) => {
                let mut cache = self.pool_cache.write().await;
                cache.insert(cache_key, None);
                return Ok(None);
            }
        };

        let token1 = if token0 == token_a { token_b } else { token_a };

        let liquidity_usd = self.calculate_pool_liquidity_usd(
            token0,
            token1,
            U256::from(reserve0),
            U256::from(reserve1)
        ).await;

        let pool = PoolInfo {
            address: pair_address,
            token0,
            token1,
            dex,
            name,
            is_stable: false,
            tick_spacing: None,
            reserve0: U256::from(reserve0),
            reserve1: U256::from(reserve1),
            liquidity_usd,
            cached_at: SystemTime::now(),
        };

        let mut cache = self.pool_cache.write().await;
        cache.insert(cache_key, Some(pool.clone()));

        Ok(Some(pool))
    }

    async fn find_aerodrome_pool(
        &self,
        token_a: Address,
        token_b: Address,
        stable: bool,
    ) -> Result<Option<PoolInfo>> {
        let cache_key = (token_a, token_b, DexType::Aerodrome);

        {
            let cache = self.pool_cache.read().await;
            if let Some(cached) = cache.get(&cache_key) {
                if cached.is_none() {
                    return Ok(None);
                }

                if let Some(pool) = cached {
                    if pool.is_stable == stable && pool.is_cache_valid() {
                        return Ok(Some(pool.clone()));
                    }
                }
            }
        }

        let factory = IAerodromeFactory::new(
            self.aerodrome_factory,
            self.dual_provider.read().clone(),
        );

        let pool_address = match factory.get_pool(token_a, token_b, stable).call().await {
            Ok(addr) => addr,
            Err(_) => {
                let mut cache = self.pool_cache.write().await;
                cache.insert(cache_key, None);
                return Ok(None);
            }
        };

        if pool_address == Address::zero() {
            let mut cache = self.pool_cache.write().await;
            cache.insert(cache_key, None);
            return Ok(None);
        }

        let pool = IAerodromePool::new(
            pool_address,
            self.dual_provider.read().clone()
        );

        let (reserve0, reserve1, _) = match pool.get_reserves().call().await {
            Ok(r) => r,
            Err(_) => {
                let mut cache = self.pool_cache.write().await;
                cache.insert(cache_key, None);
                return Ok(None);
            }
        };

        let token0 = match pool.token_0().call().await {
            Ok(t) => t,
            Err(_) => {
                let mut cache = self.pool_cache.write().await;
                cache.insert(cache_key, None);
                return Ok(None);
            }
        };

        let token1 = if token0 == token_a { token_b } else { token_a };
        let name = if stable { "Aerodrome-Stable" } else { "Aerodrome-Volatile" };

        let liquidity_usd = self.calculate_pool_liquidity_usd(
            token0,
            token1,
            reserve0,
            reserve1
        ).await;

        let pool_info = PoolInfo {
            address: pool_address,
            token0,
            token1,
            dex: DexType::Aerodrome,
            name,
            is_stable: stable,
            tick_spacing: None,
            reserve0,
            reserve1,
            liquidity_usd,
            cached_at: SystemTime::now(),
        };

        let mut cache = self.pool_cache.write().await;
        cache.insert(cache_key, Some(pool_info.clone()));

        Ok(Some(pool_info))
    }

    async fn find_aerodrome_slipstream_pool(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Option<PoolInfo>> {
        let cache_key = (token_a, token_b, DexType::AerodromeSlipstream);

        {
            let cache = self.pool_cache.read().await;
            if let Some(cached) = cache.get(&cache_key) {
                if cached.is_none() {
                    return Ok(None);
                }

                if let Some(pool) = cached {
                    if pool.is_cache_valid() {
                        return Ok(Some(pool.clone()));
                    }
                }
            }
        }

        let (token0, token1) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };

        let factory = AerodromeSlipstreamFactory::new(
            self.aerodrome_slipstream_factory,
            self.dual_provider.read().clone(),
        );

        let quoter = AerodromeSlipstreamQuoter::new(
            self.aerodrome_slipstream_quoter,
            self.dual_provider.read().clone(),
        );

        let tick_spacings = vec![1, 50, 100, 200];

        for tick_spacing in tick_spacings {
            match factory.get_pool(token0, token1, tick_spacing).call().await {
                Ok(pool_address) if pool_address != Address::zero() => {
                    let pool = AerodromeSlipstreamPool::new(
                        pool_address,
                        self.dual_provider.read().clone()
                    );

                    let liquidity = match pool.liquidity().call().await {
                        Ok(l) => l,
                        Err(_) => continue,
                    };

                    if liquidity == 0 {
                        continue;
                    }

                    let token_a_info = self.get_token_info(token_a)?;
                    let test_amount = U256::from(1u128)
                        .saturating_mul(U256::exp10(token_a_info.decimals as usize));

                    let (amount_out_forward, _, _) = match quoter
                        .quote_exact_input_single(token0, token1, test_amount, tick_spacing)
                        .call()
                        .await
                    {
                        Ok(result) => result,
                        Err(_) => continue,
                    };

                    let (amount_out_backward, _, _) = match quoter
                        .quote_exact_input_single(token1, token0, test_amount, tick_spacing)
                        .call()
                        .await
                    {
                        Ok(result) => result,
                        Err(_) => continue,
                    };

                    let price_forward = amount_out_forward.as_u128() as f64 / test_amount.as_u128() as f64;
                    let price_backward = test_amount.as_u128() as f64 / amount_out_backward.as_u128() as f64;
                    let avg_price = (price_forward + price_backward) / 2.0;

                    let reserve0 = U256::from(liquidity);
                    let reserve1 = U256::from((liquidity as f64 * avg_price) as u128);

                    let liquidity_usd = self.calculate_pool_liquidity_usd(
                        token0,
                        token1,
                        reserve0,
                        reserve1
                    ).await;

                    let pool_info = PoolInfo {
                        address: pool_address,
                        token0,
                        token1,
                        dex: DexType::AerodromeSlipstream,
                        name: "Aerodrome Slipstream",
                        is_stable: false,
                        tick_spacing: Some(tick_spacing as i32),
                        reserve0,
                        reserve1,
                        liquidity_usd,
                        cached_at: SystemTime::now(),
                    };

                    let mut cache = self.pool_cache.write().await;
                    cache.insert(cache_key, Some(pool_info.clone()));

                    return Ok(Some(pool_info));
                }
                _ => continue,
            }
        }

        let mut cache = self.pool_cache.write().await;
        cache.insert(cache_key, None);

        Ok(None)
    }

    // ========== PRICE RETRIEVAL ==========

    async fn get_effective_price_from_pool(
        &self,
        pool: &PoolInfo,
        token_in: Address,
        token_out: Address,
    ) -> Result<U256> {
        // Utiliser 1 token comme montant de test
        let token_in_info = self.get_token_info(token_in)?;
        let test_amount = U256::from(10u128.pow(token_in_info.decimals as u32));
        
        // Calculer le swap AVEC frais inclus
        let amount_out = self.calculate_swap_output(
            pool,
            token_in,
            test_amount,
            pool.dex,
        )?;
        
        if amount_out.is_zero() {
            return Ok(U256::zero());
        }
        
        // Obtenir les infos du token out
        let token_out_addr = if pool.token0 == token_in { 
            pool.token1 
        } else { 
            pool.token0 
        };
        let token_out_info = self.get_token_info(token_out_addr)?;
        
        // Calculer le prix normalisé à 18 décimales
        // Prix = (amount_out * 10^18 * 10^decimals_in) / (test_amount * 10^decimals_out)
        let price = amount_out
            .saturating_mul(U256::from(10u128.pow(18)))
            .saturating_mul(U256::from(10u128.pow(token_in_info.decimals as u32)))
            .checked_div(
                test_amount.saturating_mul(U256::from(10u128.pow(token_out_info.decimals as u32)))
            )
            .unwrap_or(U256::zero());
        
        Ok(price)
    }


    // ========== PUBLIC METHODS ==========

    pub async fn print_stats(&self) {
        let stats = self.stats.read().await;

        info!("📊 Detector Stats:");
        info!("   • Pools checked: {}", stats.pools_checked);
        info!("   • Prices fetched: {}", stats.prices_fetched);
        info!("   • Cache hits: {} ({:.1}%)", 
            stats.cache_hits,
            if stats.cache_hits + stats.cache_misses > 0 {
                (stats.cache_hits as f64 / (stats.cache_hits + stats.cache_misses) as f64) * 100.0
            } else {
                0.0
            }
        );
        info!("   • Parallel scans: {}", stats.parallel_scans);
        info!("   • Avg scan time: {}ms", stats.scan_time_ms);
    }

    pub async fn cleanup_cache(&self) {
        let mut cache = self.pool_cache.write().await;
        let initial_size = cache.len();

        cache.retain(|_, pool_opt| {
            if let Some(pool) = pool_opt {
                pool.is_cache_valid()
            } else {
                true
            }
        });

        let removed = initial_size - cache.len();
        if removed > 0 {
            debug!("🧹 Cleaned {} expired pool entries from cache", removed);
        }
    }
    
    pub fn get_tokens(&self) -> &[TokenInfo] {
        &self.tokens
    }
    
    pub async fn mark_pool_failure(&self, pool_address: Address) {
        let mut failure_count = self.pool_failure_count.write().await;
        let count = failure_count.entry(pool_address).or_insert(0);
        *count += 1;
        
        if *count >= 10 {
            let mut blacklist = self.blacklisted_pools.write().await;
            blacklist.insert(pool_address);
            warn!("🚫 Pool {:?} blacklisted after {} failures", pool_address, count);
        }
    }
    
    pub async fn is_pool_blacklisted(&self, pool_address: Address) -> bool {
        let blacklist = self.blacklisted_pools.read().await;
        blacklist.contains(&pool_address)
    }
    
    pub async fn reset_blacklist(&self) {
        let mut blacklist = self.blacklisted_pools.write().await;
        let mut failure_count = self.pool_failure_count.write().await;
        blacklist.clear();
        failure_count.clear();
        info!("🔄 Pool blacklist reset");
    }

    pub async fn get_token_price(&self, token_symbol: &str) -> Result<f64> {
        let token = self.get_token_by_symbol(token_symbol)?;
        let price = self.price_oracle.get_price_usd(token).await;
        Ok(price)
    }

     pub async fn get_blacklisted_count(&self) -> usize {
        let blacklist = self.blacklisted_pools.read().await;
        blacklist.len()
    }
    
    /// Get a list of blacklisted pool addresses (limited to first N)
    pub async fn get_blacklisted_pools_list(&self, limit: usize) -> Vec<Address> {
        let blacklist = self.blacklisted_pools.read().await;
        blacklist.iter().take(limit).copied().collect()
    }


    // ========== MÉTRIQUES ET STATS ==========

    pub async fn get_detection_metrics(&self) -> DetectionMetrics {
        self.metrics.read().await.clone()
    }

    pub async fn reset_metrics(&self) {
        let mut metrics = self.metrics.write().await;
        *metrics = DetectionMetrics::default();
    }

    pub async fn print_detection_stats(&self) {
        let m = self.metrics.read().await;
        
        if m.scans_total == 0 {
            return;
        }
        
        info!("📊 Detection Statistics:");
        info!("   Scans & Pairs:");
        info!("      • Total scans: {}", m.scans_total);
        info!("      • Pairs checked: {}", m.pairs_checked);
        
        info!("   Filtrage:");
        info!("      • Insufficient pools: {}", m.pairs_insufficient_pools);
        info!("      • Insufficient liquidity: {}", m.pairs_insufficient_liquidity);
        
        info!("   Spreads:");
        info!("      • Calculated: {}", m.spreads_calculated);
        info!("      • Too low (<{} bps): {}", MIN_SPREAD_BPS, m.spreads_too_low);
        info!("      • Too high (>{} bps): {}", MAX_SPREAD_BPS, m.spreads_too_high);
        info!("      • Valid: {}", m.spreads_valid);
        
        if m.spreads_calculated > 0 {
            let pass_rate = (m.spreads_valid as f64 / m.spreads_calculated as f64) * 100.0;
            info!("      • Pass rate: {:.1}%", pass_rate);
        }
        
        info!("   Profits:");
        info!("      • Calculated: {}", m.profits_calculated);
        info!("      • Too low: {}", m.profits_too_low);
        info!("      • Negative after gas: {}", m.profits_after_gas_negative);
        
        info!("   Opportunities:");
        info!("      • 💰 Found: {}", m.opportunities_found);
        
        if m.pairs_checked > 0 {
            let opp_rate = (m.opportunities_found as f64 / m.pairs_checked as f64) * 100.0;
            info!("      • Discovery rate: {:.3}%", opp_rate);
        }
    }

}

// ========== TESTS UNITAIRES ==========

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_price_calculation_same_decimals() {
        let reserve_in = U256::from(1_000_000_000_000u64);
        let reserve_out = U256::from(1_000_000_000_000u64);

        let price = reserve_out
            .saturating_mul(U256::from(10u128.pow(18)))
            .checked_div(reserve_in)
            .unwrap();

        let price_f64 = price.as_u128() as f64 / 1e18;
        assert!(price_f64 > 0.99 && price_f64 < 1.01);
    }

    #[test]
    fn test_liquidity_validation() {
        let reserve = U256::from(100_000_000_000u64);
        let amount = U256::from(1_000_000_000u64);

        let impact_bps = amount
            .saturating_mul(U256::from(10000))
            .checked_div(reserve)
            .unwrap()
            .as_u64();

        assert_eq!(impact_bps, 100);
        assert!(impact_bps < MAX_TRADE_IMPACT_BPS);
    }

    #[test]
    fn test_min_output_calculation() {
        let expected = U256::from(1_000_000u64);
        let slippage_bps = 50u64;
        
        let min_out = expected
            .saturating_mul(U256::from(10000 - slippage_bps))
            .checked_div(U256::from(10000))
            .unwrap();

        assert_eq!(min_out, U256::from(995_000u64));
    }
}
