// config.rs

use anyhow::{Context, Result};
use ethers::types::Address;
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub network: NetworkConfig,
    pub bot: BotConfig,
    pub tokens: TokenConfig,
    pub dex: DexConfig,
    pub trading: TradingConfig,
    pub mev: MevConfig,
    pub monitoring: MonitoringConfig,
    pub safety: SafetyConfig,
}

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub chain_id: u64,
    pub read_rpc_url: String,
    pub read_wss_url: String,
    pub write_rpc_url: String,
    pub write_rpc_type: String,
}

#[derive(Debug, Clone)]
pub struct BotConfig {
    pub contract_address: Address,
    pub private_key: String,
}

#[derive(Debug, Clone)]
pub struct TokenConfig {
    pub usdc: Address,
    pub weth: Address,
    pub dai: Address,
    pub usdt: Address,
    pub wsteth: Address,
}

#[derive(Debug, Clone)]
pub struct DexConfig {
    pub uniswap_v3_router: Address,
    pub uniswap_v3_factory: Address,
    pub uniswap_v2_router: Address,
    pub aerodrome_router: Address,
    pub aerodrome_factory: Address,
    pub aerodrome_slipstream_router: Address,
    pub aerodrome_slipstream_factory: Address,
    pub aerodrome_slipstream_quoter: Address,
    pub baseswap_router: Address,
    pub baseswap_factory: Address,
    pub sushiswap_router: Address,
    pub sushiswap_factory: Address,
}

#[derive(Debug, Clone)]
pub struct TradingConfig {
    pub min_profit_usd: f64,
    pub max_position_size_usd: f64,
    pub gas_price_multiplier: f64,
    pub max_slippage_bps: u64,
    pub enable_flash_loans: bool,
    pub flash_loan_min_profit_usd: f64,
    pub flash_loan_max_amount_eth: f64,
}

#[derive(Debug, Clone)]
pub struct MevConfig {
    pub use_private_mempool: bool,
    pub max_priority_fee_gwei: f64,
    pub flashbots_auth_key: Option<String>,
    pub eden_api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MonitoringConfig {
    pub enable_metrics: bool,
    pub metrics_port: u16,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub discord_webhook_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SafetyConfig {
    pub enable_dry_run: bool,
    pub max_trades_per_hour: u32,
    pub cooldown_seconds: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenv::dotenv().ok();

        Ok(Self {
            network: NetworkConfig {
                chain_id: env::var("CHAIN_ID")
                    .unwrap_or_else(|_| "8453".to_string())
                    .parse()
                    .context("Invalid CHAIN_ID")?,
                read_rpc_url: env::var("READ_RPC_URL")
                    .context("READ_RPC_URL not set")?,
                read_wss_url: env::var("READ_WSS_URL")
                    .context("READ_WSS_URL not set")?,
                write_rpc_url: env::var("WRITE_RPC_URL")
                    .context("WRITE_RPC_URL not set")?,
                write_rpc_type: env::var("WRITE_RPC_TYPE")
                    .unwrap_or_else(|_| "basebuilder".to_string()),
            },
            bot: BotConfig {
                contract_address: env::var("BOT_CONTRACT_ADDRESS")
                    .context("BOT_CONTRACT_ADDRESS not set")?
                    .parse()
                    .context("Invalid BOT_CONTRACT_ADDRESS")?,
                private_key: env::var("PRIVATE_KEY")
                    .context("PRIVATE_KEY not set")?,
            },
            tokens: TokenConfig {
                usdc: env::var("USDC_ADDRESS")
                    .unwrap_or_else(|_| "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".to_string())
                    .parse()?,
                weth: env::var("WETH_ADDRESS")
                    .unwrap_or_else(|_| "0x4200000000000000000000000000000000000006".to_string())
                    .parse()?,
                dai: env::var("DAI_ADDRESS")
                    .unwrap_or_else(|_| "0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb".to_string())
                    .parse()?,
                usdt: env::var("USDT_ADDRESS")
                    .unwrap_or_else(|_| "0xfde4C96c8593536E31F229EA8f37b2ADa2699bb2".to_string())
                    .parse()?,
                wsteth: env::var("WSTETH_ADDRESS")
                    .unwrap_or_else(|_| "0xc1CBa3fCea344f92D9239c08C0568f6F2F0ee452".to_string())
                    .parse()?,
            },
            dex: DexConfig {
                uniswap_v3_router: env::var("UNISWAP_V3_ROUTER")
                    .unwrap_or_else(|_| "0x2626664c2603336E57B271c5C0b26F421741e481".to_string())
                    .parse()?,
                uniswap_v3_factory: env::var("UNISWAP_V3_FACTORY")
                    .unwrap_or_else(|_| "0x33128a8fC17869897dcE68Ed026d694621f6FDfD".to_string())
                    .parse()?,
                uniswap_v2_router: env::var("UNISWAP_V2_ROUTER")
                    .unwrap_or_else(|_| "0x4752ba5dbc23f44d87826276bf6fd6b1c372ad24".to_string())
                    .parse()?,
                aerodrome_router: env::var("AERODROME_ROUTER")
                    .unwrap_or_else(|_| "0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43".to_string())
                    .parse()?,
                aerodrome_factory: env::var("AERODROME_FACTORY")
                    .unwrap_or_else(|_| "0x420DD381b31aEf6683db6B902084cB0FFECe40Da".to_string())
                    .parse()?,
                aerodrome_slipstream_router: env::var("AERODROME_SLIPSTREAM_ROUTER")
                    .unwrap_or_else(|_| "0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5".to_string())
                    .parse()?,
                aerodrome_slipstream_factory: env::var("AERODROME_SLIPSTREAM_FACTORY")
                    .unwrap_or_else(|_| "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A".to_string())
                    .parse()?,
                aerodrome_slipstream_quoter: env::var("AERODROME_SLIPSTREAM_QUOTER")
                    .unwrap_or_else(|_| "0xA2DEcF05c16537C702779083Fe067e308463CE45".to_string())
                    .parse()?,
                baseswap_router: env::var("BASESWAP_ROUTER")
                    .unwrap_or_else(|_| "0x327Df1E6de05895d2ab08513aaDD9313Fe505d86".to_string())
                    .parse()?,
                baseswap_factory: env::var("BASESWAP_FACTORY")
                    .unwrap_or_else(|_| "0xFDa619b6d20975be80A10332cD39b9a4b0FAa8BB".to_string())
                    .parse()?,
                sushiswap_router: env::var("SUSHISWAP_ROUTER")
                    .unwrap_or_else(|_| "0x6BDED42c6DA8FBf0d2bA55B2fa120C5e0c8D7891".to_string())
                    .parse()?,
                sushiswap_factory: env::var("SUSHISWAP_FACTORY")
                    .unwrap_or_else(|_| "0x71524B4f93c58fcbF659783284E38825f0622859".to_string())
                    .parse()?,
            },
            trading: TradingConfig {
                min_profit_usd: env::var("MIN_PROFIT_USD")
                    .unwrap_or_else(|_| "5.0".to_string())
                    .parse()?,
                max_position_size_usd: env::var("MAX_POSITION_SIZE_USD")
                    .unwrap_or_else(|_| "100000.0".to_string())
                    .parse()?,
                gas_price_multiplier: env::var("GAS_PRICE_MULTIPLIER")
                    .unwrap_or_else(|_| "1.2".to_string())
                    .parse()?,
                max_slippage_bps: env::var("MAX_SLIPPAGE_BPS")
                    .unwrap_or_else(|_| "50".to_string())
                    .parse()?,
                enable_flash_loans: env::var("ENABLE_FLASH_LOANS")
                    .unwrap_or_else(|_| "true".to_string())
                    .parse()?,
                flash_loan_min_profit_usd: env::var("FLASH_LOAN_MIN_PROFIT_USD")
                    .unwrap_or_else(|_| "5.0".to_string())
                    .parse()?,
                flash_loan_max_amount_eth: env::var("FLASH_LOAN_MAX_AMOUNT_ETH")
                    .unwrap_or_else(|_| "100.0".to_string())
                    .parse()?,
            },
            mev: MevConfig {
                use_private_mempool: env::var("USE_PRIVATE_MEMPOOL")
                    .unwrap_or_else(|_| "true".to_string())
                    .parse()?,
                max_priority_fee_gwei: env::var("MAX_PRIORITY_FEE_GWEI")
                    .unwrap_or_else(|_| "2.0".to_string())
                    .parse()?,
                flashbots_auth_key: env::var("FLASHBOTS_AUTH_KEY").ok(),
                eden_api_key: env::var("EDEN_API_KEY").ok(),
            },
            monitoring: MonitoringConfig {
                enable_metrics: env::var("ENABLE_METRICS")
                    .unwrap_or_else(|_| "true".to_string())
                    .parse()?,
                metrics_port: env::var("METRICS_PORT")
                    .unwrap_or_else(|_| "9090".to_string())
                    .parse()?,
                telegram_bot_token: env::var("TELEGRAM_BOT_TOKEN").ok(),
                telegram_chat_id: env::var("TELEGRAM_CHAT_ID").ok(),
                discord_webhook_url: env::var("DISCORD_WEBHOOK_URL").ok(),
            },
            safety: SafetyConfig {
                enable_dry_run: env::var("ENABLE_DRY_RUN")
                    .unwrap_or_else(|_| "false".to_string())
                    .parse()?,
                max_trades_per_hour: env::var("MAX_TRADES_PER_HOUR")
                    .unwrap_or_else(|_| "50".to_string())
                    .parse()?,
                cooldown_seconds: env::var("COOLDOWN_SECONDS")
                    .unwrap_or_else(|_| "2".to_string())
                    .parse()?,
            },
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.bot.private_key.is_empty() {
            anyhow::bail!("Private key is empty");
        }

        if self.trading.min_profit_usd <= 0.0 {
            anyhow::bail!("MIN_PROFIT_USD must be > 0");
        }

        if self.trading.max_slippage_bps > 1000 {
            anyhow::bail!("MAX_SLIPPAGE_BPS too high (>10%)");
        }

        if self.mev.use_private_mempool && self.network.write_rpc_type == "standard" {
            tracing::warn!("⚠️  Private mempool enabled but using standard RPC");
        }

        Ok(())
    }
}
