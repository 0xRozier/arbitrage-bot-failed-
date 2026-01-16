// types.rs

use ethers::types::{Address, U256};
use serde::{Deserialize, Serialize};

// ========== DEX TYPES ==========

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum DexType {
    UniswapV3 = 0,
    UniswapV2 = 1,
    Aerodrome = 2,
    AerodromeSlipstream = 3,
    Curve = 4,
    Balancer = 5,
    Sushiswap = 6,
}

impl std::fmt::Display for DexType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DexType::UniswapV3 => write!(f, "UniswapV3"),
            DexType::UniswapV2 => write!(f, "UniswapV2"),
            DexType::Aerodrome => write!(f, "Aerodrome"),
            DexType::AerodromeSlipstream => write!(f, "Aerodrome Slipstream"),
            DexType::Curve => write!(f, "Curve"),
            DexType::Balancer => write!(f, "Balancer"),
            DexType::Sushiswap => write!(f, "Sushiswap"),
        }
    }
}

// ========== SWAP STEP ==========

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapStep {
    pub dex: DexType,
    pub token_in: Address,
    pub token_out: Address,
    pub data: ethers::types::Bytes,
    pub min_out: u128,
}

// ========== ARBITRAGE OPPORTUNITY ==========

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbitrageOpportunity {
    pub id: String,
    pub asset: Address,
    pub amount: U256,
    pub steps: Vec<SwapStep>,
    pub expected_profit: U256,
    pub expected_profit_usd: f64,
    pub net_profit_usd: f64,
    pub gas_estimate: U256,
    pub gas_cost_usd: f64,
    pub timestamp: u64,
    pub opportunity_type: OpportunityType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OpportunityType {
    TwoLeg,         // A → B → A
    Triangular,     // A → B → C → A
}

// ========== TRADE ==========

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub id: String,
    pub opportunity: ArbitrageOpportunity,
    pub tx_hash: String,
    pub status: TradeStatus,
    pub actual_profit: Option<U256>,
    pub gas_used: Option<U256>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TradeStatus {
    Pending,
    Success,
    Failed,
    Reverted,
}

// ========== BOT METRICS ==========

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotMetrics {
    pub total_trades: u64,
    pub successful_trades: u64,
    pub failed_trades: u64,
    pub total_profit_usd: f64,
    pub total_gas_cost_usd: f64,
    pub average_profit_per_trade: f64,
    pub opportunities_found: u64,
    pub opportunities_executed: u64,
    pub uptime_seconds: u64,

    pub avg_scan_time_ms: f64,
    pub opportunities_per_scan: f64,
    pub avg_execution_latency_ms: f64,
    pub front_runs_detected: u64,
    pub profit_vs_expected_ratio: f64,

    pub two_leg_executed: u64,
    pub triangular_executed: u64,
    pub flashloan_executed: u64,
    
    // Dernières 24h
    pub profit_last_24h: f64,
    pub trades_last_24h: u64,
}

impl BotMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_trade(&mut self, trade: &Trade) {
        self.total_trades += 1;

        match trade.status {
            TradeStatus::Success => {
                self.successful_trades += 1;
                self.total_profit_usd += trade.opportunity.net_profit_usd;
                self.total_gas_cost_usd += trade.opportunity.gas_cost_usd;
            }
            TradeStatus::Failed | TradeStatus::Reverted => {
                self.failed_trades += 1;
            }
            _ => {}
        }

        if self.successful_trades > 0 {
            self.average_profit_per_trade = self.total_profit_usd / self.successful_trades as f64;
        }
    }

    pub fn success_rate(&self) -> f64 {
        if self.total_trades > 0 {
            (self.successful_trades as f64 / self.total_trades as f64) * 100.0
        } else {
            0.0
        }
    }
}



impl ArbitrageOpportunity {
    pub fn calculate_net_profit(&mut self, gas_price_gwei: f64) {
        // Calculer le coût du gas en USD (assume ETH = $3300)
        let gas_cost_eth = self.gas_estimate.as_u128() as f64 * gas_price_gwei * 1e-9;
        self.gas_cost_usd = gas_cost_eth * 3300.0;
        
        // Profit net = profit brut - gas
        self.net_profit_usd = self.expected_profit_usd - self.gas_cost_usd;
    }
    
    pub fn is_profitable(&self, min_profit_usd: f64) -> bool {
        self.net_profit_usd >= min_profit_usd
    }

    pub fn calculate_score(&self) -> f64 {
        if self.gas_cost_usd == 0.0 {
            return 0.0;
        }
        
        // Score de base = profit net / gas cost
        let base_score = self.net_profit_usd / self.gas_cost_usd;
        
        // Pénalité complexité
        let complexity_penalty = match self.opportunity_type {
            OpportunityType::TwoLeg => 1.0,
            OpportunityType::Triangular => 0.85, // -15% pour triangulaire
        };
        
        // Pénalité âge
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let age = now.saturating_sub(self.timestamp);
        
        let age_penalty = if age < 2 {
            1.0
        } else if age < 5 {
            0.9  // -10% si 2-5s
        } else {
            0.7  // -30% si >5s
        };
        
        // Bonus pour gros profit
        let profit_bonus = if self.net_profit_usd > 50.0 {
            1.2
        } else if self.net_profit_usd > 20.0 {
            1.1
        } else {
            1.0
        };
        
        base_score * complexity_penalty * age_penalty * profit_bonus
    }

}