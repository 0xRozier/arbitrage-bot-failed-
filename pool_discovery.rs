// pool_discovery.rs

use ethers::prelude::*;
use ethers::types::{Address, U256};
use ethers::contract::abigen;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use tracing::{debug, info, warn};

// ========== CONSTANTES OPTIMISÉES ==========
const INTER_DEX_DELAY_MS: u64 = 150;
const POOL_CALL_TIMEOUT_MS: u64 = 4000;
const CACHE_TTL_SECS: u64 = 600;
const MIN_LIQUIDITY_THRESHOLD: u128 = 5000;
const MIN_POOL_LIQUIDITY_USD: f64 = 2000.0;

// ========== ABIs ==========

abigen!(
    IUniswapV2Factory,
    r#"[
        function getPair(address tokenA, address tokenB) external view returns (address pair)
    ]"#
);

abigen!(
    IUniswapV2Pair,
    r#"[
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast)
        function token0() external view returns (address)
        function token1() external view returns (address)
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
        function stable() external view returns (bool)
    ]"#
);

abigen!(
    AerodromeSlipstreamFactory,
    r#"[
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address pool)
    ]"#
);

abigen!(
    AerodromeSlipstreamPool,
    r#"[
        function liquidity() external view returns (uint128)
        function token0() external view returns (address)
        function token1() external view returns (address)
        function tickSpacing() external view returns (int24)
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, bool unlocked)
    ]"#
);

abigen!(
    IERC20,
    r#"[
        function decimals() external view returns (uint8)
        function symbol() external view returns (string)
        function balanceOf(address account) external view returns (uint256)
    ]"#
);

// ========== STRUCTURES ==========

#[derive(Debug, Clone)]
pub struct PoolInfo {
    pub pool_address: Address,
    pub token0: Address,
    pub token1: Address,
    pub dex_name: String,
    pub reserve0: U256,
    pub reserve1: U256,
    pub liquidity_usd: f64,
    pub tick_spacing: Option<i32>,
    pub is_stable: bool,
    pub discovered_at: u64,
    pub sqrt_price_x96: Option<U256>,
    pub actual_liquidity: Option<u128>,
}

impl PoolInfo {
    fn is_cache_valid(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        now - self.discovered_at < CACHE_TTL_SECS
    }
}

// ========== POOL DISCOVERY ==========

pub struct PoolDiscovery {
    aerodrome_factory: Address,
    aerodrome_slipstream_factory: Address,
    aerodrome_slipstream_quoter: Address,
    discovered_pools: Arc<tokio::sync::RwLock<HashMap<(Address, Address), Vec<PoolInfo>>>>,
    usdc: Address,
    weth: Address,
}

impl PoolDiscovery {
    pub fn new() -> Result<Self> {
        Ok(Self {
            aerodrome_factory: "0x420DD381b31aEf6683db6B902084cB0FFECe40Da".parse()?,
            aerodrome_slipstream_factory: "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A".parse()?,
            aerodrome_slipstream_quoter: "0xA2DEcF05c16537C702779083Fe067e308463CE45".parse()?,
            discovered_pools: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".parse()?,
            weth: "0x4200000000000000000000000000000000000006".parse()?,
        })
    }
    
    /// ✅ DÉCOUVERTE OPTIMISÉE AVEC LOGS DÉTAILLÉS
    pub async fn discover_pools(
        &self,
        token_a: Address,
        token_b: Address,
        provider: Arc<Provider<Ws>>,
    ) -> Result<Vec<PoolInfo>> {
        info!("🚀 discover_pools() called for {:?} / {:?}", token_a, token_b);
        
        let cache_key = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };

        // Check cache
        {
            let cache = self.discovered_pools.read().await;
            if let Some(cached) = cache.get(&cache_key) {
                if !cached.is_empty() && cached[0].is_cache_valid() {
                    debug!("  ♻️  Cache hit: {} pools", cached.len());
                    return Ok(cached.clone());
                }
            }
        }

        info!("🔍 Discovering pools for {:?} / {:?}", token_a, token_b);

        let mut pools = Vec::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        // ✅ PRIORITY 1: Aerodrome Volatile
        info!("  📍 Checking Aerodrome Volatile...");
        match tokio::time::timeout(
            Duration::from_millis(POOL_CALL_TIMEOUT_MS),
            self.discover_aerodrome_pool(token_a, token_b, provider.clone(), false)
        ).await {
            Ok(Ok(Some(mut pool))) => {
                pool.discovered_at = now;
                debug!("     ✅ Found Aerodrome Volatile (liq: ${:.0})", pool.liquidity_usd);
                pools.push(pool);
        
            }
            Ok(Ok(None)) => debug!("     ⭕ No Aerodrome Volatile pool exists"),
            Ok(Err(e)) => debug!("     ❌ Aerodrome error: {}", e),
            Err(_) => warn!("     ⏱️  Aerodrome timeout (>{}ms)", POOL_CALL_TIMEOUT_MS),
        }
        
        tokio::time::sleep(Duration::from_millis(INTER_DEX_DELAY_MS)).await;
        
        // ✅ PRIORITY 2: Slipstream (TOUS les tick spacings)
        info!("  📍 Checking Slipstream (all tick spacings)...");
        match tokio::time::timeout(
            Duration::from_millis(POOL_CALL_TIMEOUT_MS * 4),
            self.discover_all_slipstream_pools(token_a, token_b, provider.clone())
        ).await {
            Ok(Ok(mut discovered)) => {
                for pool in &mut discovered {
                    pool.discovered_at = now;
                }
                
                if !discovered.is_empty() {
                    debug!("     ✅ Found {} Slipstream pool(s)", discovered.len());
                    for pool in &discovered {
                        debug!("        • {} (tick={}, liq=${:.0})", 
                            pool.dex_name, 
                            pool.tick_spacing.unwrap_or(0),
                            pool.liquidity_usd
                        );
                    }
                    pools.extend(discovered);
                } else {
                    debug!("     ⭕ No Slipstream pools discovered");
                }
            }
            Ok(Err(e)) => warn!("     ❌ Slipstream discovery error: {}", e),
            Err(_) => warn!("     ⏱️  Slipstream timeout (>{}ms)", POOL_CALL_TIMEOUT_MS * 4),
        }

        // Cache results
        if !pools.is_empty() {
            let mut cache = self.discovered_pools.write().await;
            cache.insert(cache_key, pools.clone());
            
            info!("✅ Discovered {} pool(s) for {:?}/{:?}", pools.len(), token_a, token_b);
            for pool in &pools {
                info!("   • {}: ${:.0} liquidity", pool.dex_name, pool.liquidity_usd);
            }
        } else {
            debug!("❌ No pools found with sufficient liquidity (min ${})", MIN_POOL_LIQUIDITY_USD);
        }

        Ok(pools)
    }

    async fn discover_aerodrome_pool(
        &self,
        token_a: Address,
        token_b: Address,
        provider: Arc<Provider<Ws>>,
        stable: bool,
    ) -> Result<Option<PoolInfo>> {
        let factory = IAerodromeFactory::new(self.aerodrome_factory, provider.clone());
        
        let pool_address = factory
            .get_pool(token_a, token_b, stable)
            .call()
            .await?;

        if pool_address.is_zero() {
            return Ok(None);
        }

        debug!("        🔗 Pool address: {:?}", pool_address);

        let pool = IAerodromePool::new(pool_address, provider.clone());
        let (reserve0, reserve1, _) = pool.get_reserves().call().await?;
        let token0 = pool.token_0().call().await?;

        debug!("        💧 Reserves: {} / {}", reserve0, reserve1);

        let (token0_final, token1_final) = if token0 == token_a {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };

        let liquidity_usd = self.estimate_liquidity_usd(reserve0, reserve1, token0_final, token1_final);
        
        debug!("        💰 Estimated liquidity: ${:.0}", liquidity_usd);

        Ok(Some(PoolInfo {
            pool_address,
            token0: token0_final,
            token1: token1_final,
            dex_name: if stable { "Aerodrome-Stable".to_string() } else { "Aerodrome-Volatile".to_string() },
            reserve0,
            reserve1,
            liquidity_usd,
            tick_spacing: None,
            is_stable: stable,
            discovered_at: 0,
            sqrt_price_x96: None,
            actual_liquidity: None,
        }))
    }

    async fn discover_all_slipstream_pools(
        &self,
        token_a: Address,
        token_b: Address,
        provider: Arc<Provider<Ws>>,
    ) -> Result<Vec<PoolInfo>> {
        let factory = AerodromeSlipstreamFactory::new(
            self.aerodrome_slipstream_factory,
            provider.clone()
        );

        // Try all common tick spacings: 1, 50, 100, 200, 2000
        let tick_spacings = vec![100, 1, 50, 200, 2000];
        debug!("     🔍 Trying tick spacings: {:?}", tick_spacings);
        
        let mut pools = Vec::new();

        for tick_spacing in tick_spacings {
            tokio::time::sleep(Duration::from_millis(100)).await;
            
            debug!("        📌 Checking tick_spacing={}", tick_spacing);
            
            match factory.get_pool(token_a, token_b, tick_spacing).call().await {
                Ok(pool_address) if !pool_address.is_zero() => {
                    debug!("           ✅ Pool exists at {:?}", pool_address);
                    
                    let pool = AerodromeSlipstreamPool::new(pool_address, provider.clone());
                    
                    // Get pool data
                    let liquidity_result = pool.liquidity().call().await;
                    let slot0_result = pool.slot_0().call().await;
                    
                    match (liquidity_result, slot0_result) {
                        (Ok(liquidity), Ok((sqrt_price_x96, tick, _, _, _, _))) => {
                            debug!("           💧 Liquidity: {}", liquidity);
                            debug!("           📊 SqrtPrice: {}, Tick: {}", sqrt_price_x96, tick);
                            
                            // ✅ Check minimum liquidity threshold
                            if liquidity < MIN_LIQUIDITY_THRESHOLD {
                                debug!("           ⚠️  Liquidity {} < {} threshold", 
                                    liquidity, MIN_LIQUIDITY_THRESHOLD);
                                continue;
                            }
                            
                            // ✅ Determine token order
                            let token0 = pool.token_0().call().await?;
                            let (token0_final, token1_final) = if token0 == token_a {
                                (token_a, token_b)
                            } else {
                                (token_b, token_a)
                            };
                            
                            // ✅ Calculate liquidity using FIXED quoter with fallback
                            let liquidity_usd = self.calculate_slipstream_liquidity_from_balances(
                                pool_address,
                                token0_final,
                                token1_final,
                                &provider,
                            ).await;
                            
                            debug!("           💰 Calculated liquidity: ${:.0}", liquidity_usd);
                            
                            // ✅ Add pool (liquidity filtering happens in detector)
                            pools.push(PoolInfo {
                                pool_address,
                                token0: token0_final,
                                token1: token1_final,
                                dex_name: format!("Slipstream-{}", tick_spacing),
                                reserve0: U256::zero(),
                                reserve1: U256::zero(),
                                liquidity_usd,
                                tick_spacing: Some(tick_spacing as i32),
                                is_stable: false,
                                discovered_at: 0,
                                sqrt_price_x96: Some(U256::from(sqrt_price_x96)),
                                actual_liquidity: Some(liquidity),
                            });

                            debug!("           ✅ Added pool with ${:.0} liquidity", liquidity_usd);
                        }
                        (Err(e), _) => debug!("           ❌ Liquidity error: {}", e),
                        (_, Err(e)) => debug!("           ❌ Slot0 error: {}", e),
                    }
                }
                Ok(_) => debug!("           ⭕ No pool for tick={}", tick_spacing),
                Err(e) => debug!("           ❌ Factory error: {}", e),
            }
        }
        
        pools.sort_by(|a, b| b.liquidity_usd.partial_cmp(&a.liquidity_usd).unwrap());
        
        debug!("     📊 Slipstream summary: {} valid pool(s)", pools.len());
        
        Ok(pools)
    }
    
    // ========== HELPERS ==========
        

    
    fn estimate_liquidity_usd(
        &self,
        reserve0: U256,
        reserve1: U256,
        token0: Address,
        token1: Address,
    ) -> f64 {
        let r0 = reserve0.as_u128() as f64;
        let r1 = reserve1.as_u128() as f64;
        
        // If one token is USDC (6 decimals)
        if token0 == self.usdc {
            return (r0 / 1e6) * 2.0;
        }
        if token1 == self.usdc {
            return (r1 / 1e6) * 2.0;
        }
        
        // If one token is WETH (18 decimals)
        if token0 == self.weth {
            return (r0 / 1e18) * 3300.0 * 2.0;
        }
        if token1 == self.weth {
            return (r1 / 1e18) * 3300.0 * 2.0;
        }
        
        // Fallback: detect by magnitude
        let is_r0_small = r0 < 1e12;
        let is_r1_small = r1 < 1e12;
        
        if is_r0_small && !is_r1_small {
            return r0 / 1e6 * 2.0; // r0 is probably USDC
        }
        if is_r1_small && !is_r0_small {
            return r1 / 1e6 * 2.0; // r1 is probably USDC
        }
        
        // Both 18 decimals: estimate with WETH price
        if !is_r0_small && !is_r1_small {
            let eth_reserve = r0.min(r1) / 1e18;
            return eth_reserve * 3300.0 * 2.0;
        }
        
        0.0
    }

    /// ✅ Calculate Slipstream pool TVL from real token balances
    async fn calculate_slipstream_liquidity_from_balances(
        &self,
        pool_address: Address,
        token0: Address,
        token1: Address,
        provider: &Arc<Provider<Ws>>,
    ) -> f64 {
        debug!("           💰 Fetching real token balances from pool");
        
        let token0_contract = IERC20::new(token0, provider.clone());
        let token1_contract = IERC20::new(token1, provider.clone());
        
        // Get balances with timeout
        let balance0 = match tokio::time::timeout(
            Duration::from_millis(2000),
            token0_contract.balance_of(pool_address).call()
        ).await {
            Ok(Ok(bal)) => bal,
            _ => {
                debug!("           ⚠️  Failed to get token0 balance");
                return 0.0;
            }
        };
        
        let balance1 = match tokio::time::timeout(
            Duration::from_millis(2000),
            token1_contract.balance_of(pool_address).call()
        ).await {
            Ok(Ok(bal)) => bal,
            _ => {
                debug!("           ⚠️  Failed to get token1 balance");
                return 0.0;
            }
        };
        
        debug!("           💧 Balances: {} / {}", balance0, balance1);
        
        // Use same calculation as Aerodrome (consistent!)
        let liquidity_usd = self.estimate_liquidity_usd(balance0, balance1, token0, token1);
        
        debug!("           💰 TVL: ${:.0}", liquidity_usd);
        
        liquidity_usd
    }

    
    pub async fn clear_cache(&self) {
        let mut cache = self.discovered_pools.write().await;
        cache.clear();
        info!("🧹 Pool discovery cache cleared");
    }
    
    pub async fn cleanup_cache(&self) {
        let mut cache = self.discovered_pools.write().await;
        let initial_size = cache.len();
        
        cache.retain(|_, pools| {
            !pools.is_empty() && pools[0].is_cache_valid()
        });
        
        let removed = initial_size - cache.len();
        if removed > 0 {
            debug!("🧹 Cleaned {} expired pool entries", removed);
        }
    }
}

impl Default for PoolDiscovery {
    fn default() -> Self {
        Self::new().expect("Failed to create PoolDiscovery")
    }
}