// pool_oracle.rs

use crate::config::Config;
use crate::dual_provider::DualProvider;
use crate::pool_discovery::PoolDiscovery;
use anyhow::Result;
use ethers::{
    contract::abigen,
    types::{Address, U256},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, Duration};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ========== CONSTANTES ==========
const PRICE_CACHE_TTL_SECS: u64 = 60;
const MIN_POOL_LIQUIDITY_USD: f64 = 2_000.0;
const MIN_PRICE_SOURCES: usize = 1;
const MAX_PRICE_DEVIATION_BPS: u64 = 1000;
const MAX_PRICE_FETCH_RETRIES: usize = 3; 
const QUOTE_TIMEOUT_MS: u64 = 3000;

// ========== ABIs ==========

abigen!(
    AerodromeSlipstreamQuoter,
     r#"[
        {
            "inputs": [
                {
                    "components": [
                        {"internalType": "address", "name": "tokenIn", "type": "address"},
                        {"internalType": "address", "name": "tokenOut", "type": "address"},
                        {"internalType": "uint256", "name": "amountIn", "type": "uint256"},
                        {"internalType": "int24", "name": "tickSpacing", "type": "int24"},
                        {"internalType": "uint160", "name": "sqrtPriceLimitX96", "type": "uint160"}
                    ],
                    "internalType": "struct IQuoterV2.QuoteExactInputSingleParams",
                    "name": "params",
                    "type": "tuple"
                }
            ],
            "name": "quoteExactInputSingle",
            "outputs": [
                {"internalType": "uint256", "name": "amountOut", "type": "uint256"},
                {"internalType": "uint160", "name": "sqrtPriceX96After", "type": "uint160"},
                {"internalType": "uint32", "name": "initializedTicksCrossed", "type": "uint32"},
                {"internalType": "uint256", "name": "gasEstimate", "type": "uint256"}
            ],
            "stateMutability": "nonpayable",
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
        }
    ]"#
);

// ========== STRUCTURES ==========

#[derive(Debug, Clone)]
struct PriceSource {
    dex_name: String,
    price: f64,
    liquidity_usd: f64,
}

#[derive(Debug, Clone)]
struct CachedPrice {
    price: f64,
    sources: Vec<PriceSource>,
    timestamp: u64,
    confidence: f64,
    deviation_bps: u64,
}

impl CachedPrice {
    fn is_valid(&self) -> bool {
        let age = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() - self.timestamp;
        age < PRICE_CACHE_TTL_SECS
    }
}

pub struct PoolOracle {
    config: Arc<Config>,
    dual_provider: Arc<DualProvider>,
    price_cache: Arc<RwLock<HashMap<Address, CachedPrice>>>,
    pool_discovery: Arc<PoolDiscovery>,
    tokens: Vec<crate::detector::TokenInfo>,
    weth_usd_price: Arc<RwLock<f64>>,
    aerodrome_slipstream_quoter: Address,
    discovery_locks: Arc<RwLock<HashSet<(Address, Address)>>>,
    fetch_locks: Arc<RwLock<HashMap<Address, Arc<tokio::sync::Mutex<()>>>>>,
    quote_cache: Arc<RwLock<HashMap<QuoteKey, (U256, SystemTime)>>>,
    stats: Arc<RwLock<OracleStats>>,
}

#[derive(Hash, Eq, PartialEq, Clone)]
struct QuoteKey {
    token_in: Address,
    token_out: Address,
    amount: U256,
    pool: Address,
}

#[derive(Debug, Default)]
struct OracleStats {
    price_fetches: u64,
    cache_hits: u64,
    cache_misses: u64,
    dynamic_discoveries: u64,
    api_fallbacks: u64,
    failed_quotes: u64,
}

impl PoolOracle {
    pub fn new(config: Arc<Config>, dual_provider: Arc<DualProvider>) -> Result<Self> {
        info!("🔮 Initializing Pool Oracle with DYNAMIC discovery");
        let pool_discovery = Arc::new(PoolDiscovery::new()?);
        let tokens = Self::load_tokens_from_file()?;

        Ok(Self {
            config,
            dual_provider,
            pool_discovery,
            tokens,
            price_cache: Arc::new(RwLock::new(HashMap::new())),
            weth_usd_price: Arc::new(RwLock::new(3300.0)),
            aerodrome_slipstream_quoter: "0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0".parse()?,
            discovery_locks: Arc::new(RwLock::new(HashSet::new())),
            fetch_locks: Arc::new(RwLock::new(HashMap::new())),
            quote_cache: Arc::new(RwLock::new(HashMap::new())),
            stats: Arc::new(RwLock::new(OracleStats::default())),
        })
    }

    fn load_tokens_from_file() -> Result<Vec<crate::detector::TokenInfo>> {
        use serde::Deserialize;
        
        #[derive(Deserialize)]
        struct TokenConfig {
            tokens: Vec<crate::detector::TokenInfo>,
        }
        
        let tokens_json = include_str!("../tokens.json");
        let config: TokenConfig = serde_json::from_str(tokens_json)?;
        Ok(config.tokens)
    }

    // ========== DISCOVERY AVEC LOCKS (ajouter après new()) ==========

    async fn discover_pools_with_lock(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Vec<crate::pool_discovery::PoolInfo>> {
        // Normaliser les adresses (token_a < token_b)
        let (token_min, token_max) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };
        let pair_key = (token_min, token_max);
        
        // Vérifier si discovery déjà en cours
        {
            let locks = self.discovery_locks.read().await;
            if locks.contains(&pair_key) {
                debug!("🔒 Discovery in progress for {:?}/{:?}, waiting...", token_min, token_max);
                drop(locks);
                
                // Attendre un peu
                tokio::time::sleep(Duration::from_millis(150)).await;
                
                // Si toujours en lock, continuer quand même après timeout
                let locks = self.discovery_locks.read().await;
                if locks.contains(&pair_key) {
                    debug!("⏱️ Discovery lock timeout, proceeding anyway");
                }
            }
        }
        
        // Acquérir lock
        {
            let mut locks = self.discovery_locks.write().await;
            locks.insert(pair_key);
        }
        
        debug!("🔍 Starting discovery for {:?}/{:?}", token_min, token_max);
        
        // Discovery réelle
        let result = self.pool_discovery.discover_pools(
            token_min,
            token_max,
            self.dual_provider.read().clone()
        ).await;
        
        // Libérer lock
        {
            let mut locks = self.discovery_locks.write().await;
            locks.remove(&pair_key);
        }
        
        result
    }

    /// Nettoyage des locks bloqués
    pub async fn cleanup_stale_locks(&self) {
        let mut locks = self.discovery_locks.write().await;
        if !locks.is_empty() {
            warn!("🧹 Cleaning {} stale discovery locks", locks.len());
            locks.clear();
        }
    }


    // ========== MAIN PRICE FETCHING ==========
    
    pub async fn get_price_usd(&self, token: Address, symbol: &str) -> Result<f64> {
        {
            let mut stats = self.stats.write().await;
            stats.price_fetches += 1;
        }

        // Check cache
        {
            let cache = self.price_cache.read().await;
            if let Some(cached) = cache.get(&token) {
                if cached.is_valid() {
                    // ✅ Accept ANY cached price (even low confidence fallback)
                    // This prevents repeated fetches for tokens that consistently fail
                    let mut stats = self.stats.write().await;
                    stats.cache_hits += 1;
                    return Ok(cached.price);
                }
            }
            
            let mut stats = self.stats.write().await;
            stats.cache_misses += 1;
        }

        // ✅ Get or create lock for this token
        let lock = {
            let mut locks = self.fetch_locks.write().await;
            locks.entry(token)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };

        // ✅ Acquire lock (serializes fetches for same token)
        let _guard = lock.lock().await;

        // ✅ Double-check cache (another task may have fetched while we waited)
        {
            let cache = self.price_cache.read().await;
            if let Some(cached) = cache.get(&token) {
                if cached.is_valid() {
                    debug!("   ♻️ Cache hit after lock (fetched by another task)");
                    return Ok(cached.price);
                }
            }
        }

         // ✅ Try on-chain fetch
        match self.fetch_price_with_validation(token, symbol).await {
            Ok(price) if price > 0.0 => return Ok(price),
            Err(e) => {
                debug!("  ⚠️  On-chain price failed for {}: {}", symbol, e);
            }
            _ => {}
        }
        
        // ✅ Try API fallback
        if let Ok(price) = self.fetch_coingecko_price(symbol).await {
            info!("  🌐 Using CoinGecko API for {}: ${:.6}", symbol, price);
            
            let source = PriceSource {
                dex_name: "CoinGecko API".to_string(),
                price,
                liquidity_usd: 0.0,
            };
            self.cache_price(token, price, vec![source], 0.7, 0).await;
            
            let mut stats = self.stats.write().await;
            stats.api_fallbacks += 1;
            
            return Ok(price);
        }
        
        // ✅ Last resort: static fallback
        warn!("  ⚠️  Using static fallback for {}", symbol);
        let fallback_price = Self::get_fallback_price(symbol);
        
        let source = PriceSource {
            dex_name: "Static Fallback".to_string(),
            price: fallback_price,
            liquidity_usd: 0.0,
        };
        self.cache_price(token, fallback_price, vec![source], 0.5, 0).await;
        
        Ok(fallback_price)
    }

    async fn fetch_price_with_validation(&self, token: Address, symbol: &str) -> Result<f64> {
        let usdc = self.config.tokens.usdc;
        let weth = self.config.tokens.weth;

        // Cas spéciaux
        if token == usdc {
            return Ok(1.0);
        }
        if token == weth {
            let price = *self.weth_usd_price.read().await;
            return Ok(price);
        }
        if Self::is_stablecoin(symbol) {
            return Ok(1.0);
        }

        // ✅ DYNAMIC DISCOVERY : Utiliser pool_discovery pour trouver les pools
        info!("🔍 Starting pool discovery for {} ({:?})", symbol, token);
        debug!("🔍 Discovering pools dynamically for {}", symbol);
        
        // Try USDC first
        let discovered_pools = match tokio::time::timeout(
            Duration::from_secs(5), 
            self.discover_pools_with_lock(token, usdc)
        ).await {
            Ok(Ok(pools)) if !pools.is_empty() => {
                info!("  ✅ Discovery returned {} pools against USDC", pools.len());
                pools
            }
            Ok(Ok(_)) => {
                // No pools found against USDC, try WETH
                info!("  ⚠️  No pools found against USDC, trying WETH...");
                match tokio::time::timeout(
                    Duration::from_secs(5),
                    self.discover_pools_with_lock(token, weth)
                ).await {
                    Ok(Ok(pools)) => {
                        info!("  ✅ Discovery returned {} pools against WETH", pools.len());
                        pools
                    }
                    Ok(Err(e)) => {
                        warn!("  ⚠️  Pool discovery error (WETH): {}", e);
                        vec![]
                    }
                    Err(_) => {
                        warn!("  ⏱️  Pool discovery timeout (WETH, 5s)");
                        vec![]
                    }
                }
            }
            Ok(Err(e)) => {
                warn!("  ⚠️  Pool discovery error: {}", e);
                vec![]
            }
            Err(_) => {
                warn!("  ⏱️  Pool discovery timeout (5s)");
                vec![]
            }
        };

        if discovered_pools.is_empty() {
            return Err(anyhow::anyhow!("No pools discovered for {}", symbol));
        }

        {
            let mut stats = self.stats.write().await;
            stats.dynamic_discoveries += 1;
        }

        info!("  ✅ Found {} pool(s) for {}", discovered_pools.len(), symbol);

        // ✅ Fetch prices from discovered pools
        let mut price_sources = Vec::new();
        
        // Determine if these are WETH-based or USDC-based pools
        let is_weth_pair = !discovered_pools.is_empty() && 
            (discovered_pools[0].token0 == weth || discovered_pools[0].token1 == weth);
        
        if is_weth_pair {
            info!("  💱 Pools are WETH-based, will convert to USD");
        }
        
        for pool_info in &discovered_pools {
            let source_opt = if pool_info.tick_spacing.is_some() {
                // Slipstream pool
                self.fetch_slipstream_price(
                    &pool_info,
                    token,
                    if is_weth_pair { weth } else { usdc },
                    symbol
                ).await
            } else {
                // Standard AMM pool
                self.fetch_standard_pool_price(
                    &pool_info,
                    token,
                    if is_weth_pair { weth } else { usdc },
                ).await
            };

            if let Some(mut source) = source_opt {
                // ✅ Validation stricte du prix
                if source.price > 0.0 && source.price < 1_000_000.0 {
                    // If WETH-based, convert to USD
                    if is_weth_pair {
                        let weth_price = *self.weth_usd_price.read().await;
                        source.price = source.price * weth_price;
                        debug!("    💱 Converted WETH price to USD: ${:.6}", source.price);
                    }
                    price_sources.push(source);
                } else {
                    warn!("    🚨 Rejected unrealistic price from {}: ${:.6}", 
                        pool_info.dex_name, source.price);
                }
            }
        }

        if price_sources.is_empty() {
            return Err(anyhow::anyhow!("No valid prices for {}", symbol));
        }

        // ✅ Calculer prix médian pondéré par liquidité
        let final_price = self.calculate_weighted_median(&price_sources);
        
        // ✅ Calculer déviation et confidence
        let max_deviation_bps = self.calculate_max_deviation(&price_sources, final_price);
        let confidence = self.calculate_confidence(&price_sources, max_deviation_bps);

        if max_deviation_bps > MAX_PRICE_DEVIATION_BPS {
            warn!("  ⚠️  High price deviation: {} bps", max_deviation_bps);
        }

        info!("  💰 Final price for {}: ${:.6} (confidence: {:.1}%, sources: {})", 
            symbol, final_price, confidence * 100.0, price_sources.len());

        // ✅ AJOUTE CE LOG TEMPORAIRE
        if price_sources.len() > 0 {
            info!("   📊 Price sources:");
            for source in &price_sources {
                info!("      • {}: ${:.6} (liq: ${:.0})", 
                    source.dex_name, source.price, source.liquidity_usd);
            }
        }
        

        // Cache
        self.cache_price(token, final_price, price_sources, confidence, max_deviation_bps).await;

        Ok(final_price)
    }

    // ========== QUOTE AVEC CACHE ==========

    /// Quote avec cache pour éviter duplications
    async fn quote_with_cache<P>(
        &self,
        quoter: &AerodromeSlipstreamQuoter<P>,
        params: QuoteExactInputSingleParams,
        pool_address: Address,
    ) -> Result<(U256, U256, u32, U256)>
    where
        P: ethers::providers::Middleware + 'static,
    {
        let key = QuoteKey {
            token_in: params.token_in,
            token_out: params.token_out,
            amount: params.amount_in,
            pool: pool_address,
        };
        
        // Vérifier cache (TTL 1 seconde)
        {
            let cache = self.quote_cache.read().await;
            if let Some((cached_amount, timestamp)) = cache.get(&key) {
                if SystemTime::now()
                    .duration_since(*timestamp)
                    .unwrap_or_default()
                    .as_secs() < 1
                {
                    debug!("📦 Quote from cache: {}", cached_amount);
                    // Retourner format compatible avec le quoter
                    return Ok((*cached_amount, U256::zero(), 0, U256::zero()));
                }
            }
        }
        
        // Quote réel
        let result = quoter.quote_exact_input_single(params.clone()).await?;
        
        // Mettre en cache
        {
            let mut cache = self.quote_cache.write().await;
            cache.insert(key, (result.0, SystemTime::now()));
            
            // Cleanup des vieilles entrées (> 5 secondes)
            cache.retain(|_, (_, ts)| {
                SystemTime::now()
                    .duration_since(*ts)
                    .unwrap_or_default()
                    .as_secs() < 5
            });
        }
        
        Ok(result)
    }



    // ========== SLIPSTREAM PRICE FETCHING ==========
    
    async fn fetch_slipstream_price(
        &self,
        pool_info: &crate::pool_discovery::PoolInfo,
        token_in: Address,
        token_out: Address,
        _symbol: &str,
    ) -> Option<PriceSource> {
        let tick_spacing = pool_info.tick_spacing?;

        warn!("🔍 [DEBUG] Attempting quote for {} (tick_spacing: {})", 
            pool_info.dex_name, tick_spacing);
        
        let quoter = AerodromeSlipstreamQuoter::new(
            self.aerodrome_slipstream_quoter,
            self.dual_provider.read().clone()
        );

        // ✅ Déterminer les infos des tokens
        let token_in_info = self.tokens.iter()
            .find(|t| t.address == token_in)?;
        
        let token_out_info = self.tokens.iter()
            .find(|t| t.address == token_out)?;

        // ✅ Test amount plus réaliste basé sur les decimals
        let test_amount = if token_in_info.decimals == 6 {
            U256::from(1u128 * 10u128.pow(6)) // 1 USDC
        } else if token_in_info.decimals == 8 {
            U256::from(1u128 * 10u128.pow(5)) // 0.001 BTC
        } else {
            U256::from(10u128.pow(15)) // 0.001 token
        };

        warn!("🔍 [DEBUG] Quote params: {} {} → {} {} (amount: {})", 
            token_in_info.symbol, token_in,
            token_out_info.symbol, token_out,
            test_amount);
        
        debug!("    📊 {} quote: {} {} → ? {}", 
            pool_info.dex_name,
            test_amount.as_u128() as f64 / 10f64.powi(token_in_info.decimals as i32),
            token_in_info.symbol,
            token_out_info.symbol
        );

        // ✅ Retry avec timeout augmenté
        for retry in 0..MAX_PRICE_FETCH_RETRIES {
            if retry > 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let params = QuoteExactInputSingleParams {
                token_in,
                token_out,
                amount_in: test_amount,
                tick_spacing: tick_spacing as i32,
                sqrt_price_limit_x96: U256::zero(),
            };
            match tokio::time::timeout(
                Duration::from_millis(QUOTE_TIMEOUT_MS),
                self.quote_with_cache(&quoter, params.clone(), pool_info.pool_address)
            ).await {
                Ok(Ok((amount_out, _, _, _))) if amount_out > U256::zero() => {
                    // ✅ Calcul prix avec decimals
                    let amount_in_f64 = test_amount.as_u128() as f64 / 10f64.powi(token_in_info.decimals as i32);
                    let amount_out_f64 = amount_out.as_u128() as f64 / 10f64.powi(token_out_info.decimals as i32);
                    
                    let price = amount_out_f64 / amount_in_f64;

                    warn!("✅ [DEBUG] Quote SUCCESS: {} → {} = price ${:.6}", 
                        amount_in_f64, amount_out_f64, price);

                    
                    // Validation
                    if price <= 0.0 || price > 100_000.0 {
                        warn!("    🚨 {} unrealistic price: {:.6}", pool_info.dex_name, price);
                        return None;
                    }
                    
                    // ✅ Liquidity from pool_info
                    let liquidity_usd = pool_info.liquidity_usd;
                    
                    // ✅ Validation de la liquidité minimale
                    if liquidity_usd < MIN_POOL_LIQUIDITY_USD {
                        debug!("    ⚠️  {} liquidity too low: ${:.0}", pool_info.dex_name, liquidity_usd);
                        return None;
                    }

                    info!("    ✅ {} price: ${:.6} (liq: ${:.0})", 
                        pool_info.dex_name, price, liquidity_usd);

                    return Some(PriceSource {
                        dex_name: pool_info.dex_name.clone(),
                        price,
                        liquidity_usd,
                    });
                }
                Ok(Ok(_)) => {
                    warn!("❌ [DEBUG] Quote returned ZERO");
                    
                    let mut stats = self.stats.write().await;
                    stats.failed_quotes += 1;
                    return None;
                }
                Ok(Err(e)) => {
                    if retry == MAX_PRICE_FETCH_RETRIES - 1 {
                        debug!("    ⚠️  {} error (final retry): {}", pool_info.dex_name, e);
                        let mut stats = self.stats.write().await;
                        stats.failed_quotes += 1;
                    } else {
                        debug!("    ⚠️  {} error (retry {}): {}", pool_info.dex_name, retry + 1, e);
                        warn!("❌ [DEBUG] Quote ERROR (retry {}): {}", retry + 1, e)
                    }
                }
                Err(_) => {
                    if retry == MAX_PRICE_FETCH_RETRIES - 1 {
                        debug!("    ⏱️  {} timeout (final retry)", pool_info.dex_name);
                        let mut stats = self.stats.write().await;
                        stats.failed_quotes += 1;
                    } else {
                        debug!("    ⏱️  {} timeout (retry {})", pool_info.dex_name, retry + 1);
                        warn!("⏱️ [DEBUG] Quote TIMEOUT (retry {})", retry + 1);
                    }
                }
            }
        }

        warn!("❌ [DEBUG] All quote attempts FAILED for {}", pool_info.dex_name);
        None
    }

    // ========== STANDARD POOL PRICE FETCHING ==========
    
    async fn fetch_standard_pool_price(
        &self,
        pool_info: &crate::pool_discovery::PoolInfo,
        token_in: Address,
        token_out: Address,
    ) -> Option<PriceSource> {
        let (reserve_in, reserve_out) = if pool_info.token0 == token_in {
            (pool_info.reserve0, pool_info.reserve1)
        } else {
            (pool_info.reserve1, pool_info.reserve0)
        };

        if reserve_in.is_zero() || reserve_out.is_zero() {
            return None;
        }

        let token_in_info = self.tokens.iter()
            .find(|t| t.address == token_in)?;
        
        let token_out_info = self.tokens.iter()
            .find(|t| t.address == token_out)?;

        let reserve_in_f64 = reserve_in.as_u128() as f64 / 10f64.powi(token_in_info.decimals as i32);
        let reserve_out_f64 = reserve_out.as_u128() as f64 / 10f64.powi(token_out_info.decimals as i32);

        if reserve_in_f64 == 0.0 {
            return None;
        }

        let price = reserve_out_f64 / reserve_in_f64;

        if price <= 0.0 || price > 100_000.0 {
            return None;
        }

        let liquidity_usd = pool_info.liquidity_usd;
        
        // ✅ Validation de la liquidité minimale
        if liquidity_usd < MIN_POOL_LIQUIDITY_USD {
            debug!("    ⚠️  {} liquidity too low: ${:.0}", pool_info.dex_name, liquidity_usd);
            return None;
        }

        info!("    ✅ {} price: ${:.6} (liq: ${:.0})", 
            pool_info.dex_name, price, liquidity_usd);

        Some(PriceSource {
            dex_name: pool_info.dex_name.clone(),
            price,
            liquidity_usd,
        })
    }

    // ========== HELPERS ==========
    
    fn calculate_weighted_median(&self, sources: &[PriceSource]) -> f64 {
        if sources.is_empty() {
            return 0.0;
        }
        
        if sources.len() == 1 {
            return sources[0].price;
        }
        
        // Trier par prix
        let mut sorted: Vec<_> = sources.iter().collect();
        sorted.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap());
        
        // Calculer liquidité totale
        let total_liq: f64 = sources.iter().map(|s| s.liquidity_usd).sum();
        
        if total_liq == 0.0 {
            // Fallback: médian simple
            return self.calculate_median(&mut sources.iter().map(|s| s.price).collect::<Vec<_>>());
        }
        
        // Trouver la médiane pondérée
        let target = total_liq / 2.0;
        let mut cumulative = 0.0;
        
        for source in &sorted {
            cumulative += source.liquidity_usd;
            if cumulative >= target {
                return source.price;
            }
        }
        
        // Fallback
        sorted[sorted.len() / 2].price
    }
    
    fn calculate_median(&self, prices: &mut [f64]) -> f64 {
        if prices.is_empty() {
            return 0.0;
        }
        prices.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mid = prices.len() / 2;
        if prices.len() % 2 == 0 {
            (prices[mid - 1] + prices[mid]) / 2.0
        } else {
            prices[mid]
        }
    }
    
    /// ✅ Calcul de déviation maximale
    fn calculate_max_deviation(&self, sources: &[PriceSource], median: f64) -> u64 {
        if sources.is_empty() || median == 0.0 {
            return 0;
        }
        
        let max_deviation = sources.iter()
            .map(|s| ((s.price - median).abs() / median * 10000.0) as u64)
            .max()
            .unwrap_or(0);
        
        max_deviation
    }

    fn calculate_confidence(&self, sources: &[PriceSource], max_deviation_bps: u64) -> f64 {
        let mut confidence: f64 = 1.0;
        
        // Nombre de sources
        if sources.len() < 2 {
            confidence *= 0.7;
        } else if sources.len() >= 3 {
            confidence *= 1.1;
        }
        
        // Déviation
        if max_deviation_bps > 500 {
            confidence *= 0.7;
        } else if max_deviation_bps > 200 {
            confidence *= 0.9;
        }
        
        // Liquidité totale
        let total_liquidity: f64 = sources.iter().map(|s| s.liquidity_usd).sum();
        if total_liquidity > 100_000.0 {
            confidence *= 1.05;
        } else if total_liquidity < MIN_POOL_LIQUIDITY_USD {
            confidence *= 0.8;
        }
        
        confidence.min(1.0)
    }

    async fn cache_price(
        &self,
        token: Address,
        price: f64,
        sources: Vec<PriceSource>,
        confidence: f64,
        deviation_bps: u64,
    ) {
        let cached = CachedPrice {
            price,
            sources,
            timestamp: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            confidence,
            deviation_bps,
        };
        let mut cache = self.price_cache.write().await;
        cache.insert(token, cached);
    }

    fn get_fallback_price(symbol: &str) -> f64 {
        match symbol {
             "WETH" => 3300.0,
            "wstETH" => 3900.0,
            "USDC" | "USDT" | "DAI" | "USDbC" => 1.0,
            "AERO" => 1.20,
            "cbBTC" => 92000.0,
            "cbETH" => 3600.0,
            "BRETT" => 0.15,
            "TOSHI" => 0.0003,
            "DEGEN" => 0.0016,
            _ => 1.0,
        }
    }

    fn is_stablecoin(symbol: &str) -> bool {
        matches!(symbol, "USDC" | "USDT" | "DAI" | "USDbC")
    }

    async fn fetch_coingecko_price(&self, symbol: &str) -> Result<f64> {
        let coin_id = match symbol {
            "AERO" => "aerodrome-finance",
            "wstETH" => "wrapped-steth",
            "cbETH" => "coinbase-wrapped-staked-eth",
            _ => return Err(anyhow::anyhow!("Unknown CoinGecko ID")),
        };
        
        let url = format!(
            "https://api.coingecko.com/api/v3/simple/price?ids={}&vs_currencies=usd",
            coin_id
        );
        
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
            
        let response: serde_json::Value = client
            .get(&url)
            .header("User-Agent", "ArbitrageBot/1.0")
            .send()
            .await?
            .json()
            .await?;
        
        let price = response[coin_id]["usd"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("Price not found"))?;
        
        if price <= 0.0 || price > 1_000_000.0 {
            return Err(anyhow::anyhow!("Unrealistic price"));
        }
        
        Ok(price)
    }

    pub async fn get_cached_price(&self, token: Address) -> Option<f64> {
        let cache = self.price_cache.read().await;
        cache.get(&token).and_then(|cached| {
            if cached.is_valid() {
                Some(cached.price)
            } else {
                None
            }
        })
    }

    pub async fn update_weth_price(&self) -> Result<()> {
        // Could fetch from Chainlink or pools
        Ok(())
    }

    pub async fn print_all_prices(&self, tokens: &[(Address, String)]) {
        info!("🔮 Pool Oracle Prices:");
        for (token, symbol) in tokens {
            match self.get_price_usd(*token, symbol).await {
                Ok(price) => {
                    info!("   • {:<8} ${:>12.6}", symbol, price);
                }
                Err(e) => {
                    warn!("   • {:<8} Error: {}", symbol, e);
                }
            }
        }
    }

    pub async fn print_stats(&self) {
        let stats = self.stats.read().await;
        let locks = self.discovery_locks.read().await;
        let cache = self.quote_cache.read().await;
        info!("📊 Pool Oracle Stats:");
        info!("   • Price fetches: {}", stats.price_fetches);
        info!("   • Cache hits: {} ({:.1}%)", 
            stats.cache_hits,
            if stats.price_fetches > 0 {
                (stats.cache_hits as f64 / stats.price_fetches as f64) * 100.0
            } else {
                0.0
            }
        );
        info!("   • Dynamic discoveries: {}", stats.dynamic_discoveries);
        info!("   • API fallbacks: {}", stats.api_fallbacks);
        info!("   • Failed quotes: {}", stats.failed_quotes);
        info!("   • Active discovery locks: {}", locks.len());
        info!("   • Cached quotes: {}", cache.len());
    }
    
    pub async fn clear_cache(&self) {
        let mut cache = self.price_cache.write().await;
        cache.clear();
        info!("🧹 Price cache cleared");
    }
}