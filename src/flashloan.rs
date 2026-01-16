// flashloan.rs

use anyhow::{Context, Result};
use ethers::signers::Signer;
use ethers::middleware::Middleware;
use ethers::{
    contract::abigen,
    types::{Address, TransactionReceipt, U256, I256},
};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn, debug};

use crate::config::Config;
use crate::dual_provider::DualProvider;
use crate::types::{ArbitrageOpportunity, DexType};

// ========== ABI DU SMART CONTRACT ==========

abigen!(
    UltimateArbitrageBot,
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
                {"internalType": "uint32", "name": "maxBlock", "type": "uint32"},
                {"internalType": "uint256", "name": "nonce", "type": "uint256"}
            ],
            "name": "execute",
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
                {"internalType": "uint256", "name": "nonce", "type": "uint256"}
            ],
            "name": "executeAtomic",
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
            "inputs": [],
            "name": "getMetrics",
            "outputs": [
                {"internalType": "uint256", "name": "trades", "type": "uint256"},
                {"internalType": "uint256", "name": "totalProfit", "type": "uint256"},
                {"internalType": "uint256", "name": "tradesRemaining", "type": "uint256"}
            ],
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
            "inputs": [
                {"internalType": "uint256", "name": "grossProfit", "type": "uint256"},
                {"internalType": "uint256", "name": "flashLoanAmount", "type": "uint256"},
                {"internalType": "uint256", "name": "flashLoanFee", "type": "uint256"},
                {"internalType": "uint256", "name": "estimatedGasUnits", "type": "uint256"},
                {"internalType": "uint256", "name": "gasPriceGwei", "type": "uint256"}
            ],
            "name": "estimateNetProfit",
            "outputs": [{"internalType": "int256", "name": "netProfit", "type": "int256"}],
            "stateMutability": "pure",
            "type": "function"
        },
        {
            "anonymous": false,
            "inputs": [
                {"indexed": true, "internalType": "address", "name": "asset", "type": "address"},
                {"indexed": false, "internalType": "uint256", "name": "profit", "type": "uint256"},
                {"indexed": true, "internalType": "uint256", "name": "tradeNumber", "type": "uint256"},
                {"indexed": false, "internalType": "uint256", "name": "gasUsed", "type": "uint256"},
                {"indexed": false, "internalType": "uint256", "name": "effectiveGasPrice", "type": "uint256"}
            ],
            "name": "Executed",
            "type": "event"
        }
    ]"#
);

// ========== CONSTANTES ==========

const AAVE_FLASH_LOAN_FEE_BPS: u64 = 9; // 0.09% sur Base
const BASE_GAS_ESTIMATE: u64 = 400_000;
const FLASH_LOAN_OVERHEAD: u64 = 100_000;
const SIMULATION_TIMEOUT_MS: u64 = 2000;
const MAX_EXECUTION_RETRIES: usize = 2;

// ========== STRUCTURES ==========

#[derive(Clone)]
pub struct FlashLoanExecutor {
    contract_address: Address,
    config: Arc<Config>,
    dual_provider: Arc<DualProvider>,
    nonce_tracker: Arc<tokio::sync::RwLock<u64>>, 
    wallet: ethers::signers::LocalWallet, 
}

impl std::fmt::Debug for FlashLoanExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlashLoanExecutor")
            .field("contract_address", &self.contract_address)
            .field("wallet_address", &self.wallet.address())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct FlashLoanResult {
    pub success: bool,
    pub tx_hash: String,
    pub profit: U256,
    pub gas_used: U256,
    pub effective_gas_price: U256,
    pub execution_time_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ExecutionMode {
    pub use_flash_loan: bool,
    pub use_private_mempool: bool,
    pub max_priority_fee_gwei: u64,
    pub gas_multiplier: f64,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self {
            use_flash_loan: true,
            use_private_mempool: true,
            max_priority_fee_gwei: 2,
            gas_multiplier: 1.2,
        }
    }
}

// ========== IMPLÉMENTATION ==========

impl FlashLoanExecutor {
    pub fn new(
        contract_address: Address,
        config: Arc<Config>,
        dual_provider: Arc<DualProvider>,
    ) -> Result<Self> {
        info!("⚡ Flash Loan Executor initialized");
        info!("   📍 Contract: {:?}", contract_address);
        info!("   💰 Flash loan fee: 0.0{}%", AAVE_FLASH_LOAN_FEE_BPS);

        let wallet: ethers::signers::LocalWallet = config.bot.private_key
            .trim_start_matches("0x")
            .parse()
            .context("Invalid private key")?;

        let wallet = wallet.with_chain_id(config.network.chain_id);

        info!("   👛 Wallet: {:?}", wallet.address());

        Ok(Self {
            contract_address,
            config,
            dual_provider,
            nonce_tracker: Arc::new(tokio::sync::RwLock::new(0)),
            wallet,
        })
    }

    pub fn new_with_nonce(
        contract_address: Address,
        config: Arc<Config>,
        dual_provider: Arc<DualProvider>,
        nonce_tracker: Arc<tokio::sync::RwLock<u64>>,
    ) -> Result<Self> {
        info!("⚡ Flash Loan Executor initialized with shared nonce");
        info!("   📍 Contract: {:?}", contract_address);

        let wallet: ethers::signers::LocalWallet = config.bot.private_key
            .trim_start_matches("0x")
            .parse()
            .context("Invalid private key")?;

        let wallet = wallet.with_chain_id(config.network.chain_id);

        info!("   👛 Wallet: {:?}", wallet.address());

        Ok(Self {
            contract_address,
            config,
            dual_provider,
            nonce_tracker,
            wallet,
        })
    }

    // ========== EXÉCUTION PRINCIPALE ==========

    pub async fn execute_with_flashloan(
        &self,
        opportunity: &ArbitrageOpportunity,
        mode: ExecutionMode,
    ) -> Result<FlashLoanResult> {
        let execution_start = Instant::now();
        
        info!("⚡ Executing flash loan arbitrage...");
        info!("   Asset: {:?}", opportunity.asset);
        info!("   Amount: {}", opportunity.amount);
        info!("   Expected profit: ${:.2}", opportunity.expected_profit_usd);
        info!("   Mode: {} | Private: {}", 
            if mode.use_flash_loan { "FlashLoan" } else { "Atomic" },
            mode.use_private_mempool
        );

        // ✅ 1. Vérification anti-replay
        let contract_nonce = chrono::Utc::now().timestamp_millis() as u64;
        info!("🔢 Using contract anti-replay nonce: {}", contract_nonce);
        let tx_hash = self.calculate_tx_hash(&opportunity.asset, &opportunity.amount, contract_nonce)?;

        if self.is_tx_executed(tx_hash).await? {
            warn!("⚠️  Transaction already executed (replay protection)");
            return Err(anyhow::anyhow!("Transaction already executed"));
        }

        // ✅ 2. Estimation du profit net
        let net_profit = self.estimate_net_profit_internal(opportunity).await?;

        if net_profit <= 0 {
            warn!("⚠️  Unprofitable after fees: ${:.2}", net_profit as f64 / 1e6);
            return Err(anyhow::anyhow!("Unprofitable after flash loan fees"));
        }

        info!("   💰 Net profit after fees: ${:.2}", net_profit as f64 / 1e6);

        // ✅ 3. Convertir les steps
        let steps = self.convert_steps_to_contract_type(&opportunity.steps)?;

        // ✅ 4. Calculer deadline et maxBlock
        let deadline = (chrono::Utc::now().timestamp() + 120) as u32;
        let current_block = self.dual_provider.get_block_number().await?;
        let max_block = (current_block.as_u64() + 5) as u32;

        // ✅ 5. Exécution avec retry
        let mut last_error = None;
        
        for attempt in 1..=MAX_EXECUTION_RETRIES {
            debug!("Execution attempt {}/{}", attempt, MAX_EXECUTION_RETRIES);
            
            let result = if mode.use_flash_loan {
                self.execute_flashloan(
                    opportunity,
                    steps.clone(),
                    deadline,
                    max_block,
                    contract_nonce,
                    mode.use_private_mempool,
                    mode.gas_multiplier,
                ).await
            } else {
                self.execute_atomic(
                    opportunity,
                    steps.clone(),
                    deadline,
                    contract_nonce,
                    mode.use_private_mempool,
                    mode.gas_multiplier,
                ).await
            };

            match result {
                Ok(receipt) => {
                    let execution_time = execution_start.elapsed().as_millis() as u64;
                    return self.parse_execution_result(receipt, execution_time).await;
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < MAX_EXECUTION_RETRIES {
                        warn!("⚠️  Attempt {} failed, retrying...", attempt);
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All execution attempts failed")))
    }

    // ========== EXÉCUTION FLASHLOAN ==========

    async fn execute_flashloan(
        &self,
        opportunity: &ArbitrageOpportunity,
        steps: Vec<ultimate_arbitrage_bot::SwapStep>,
        deadline: u32,
        max_block: u32,
        contract_nonce: u64,
        use_private_mempool: bool,
        gas_multiplier: f64,
    ) -> Result<TransactionReceipt> {
        use ethers::prelude::*;

        let contract = UltimateArbitrageBot::new(
            self.contract_address,
            Arc::new(self.dual_provider.read().clone())
        );

        // ✅ Réduire min_profit de 5% pour tolérance
        let min_profit = U256::from((opportunity.net_profit_usd * 0.95 * 1e6) as u64);

        let call = contract.execute(
            opportunity.asset,
            opportunity.amount,
            steps,
            min_profit.as_u128(),
            deadline,
            max_block,
            U256::from(chrono::Utc::now().timestamp_millis()),
        );

        let gas_estimate = call.estimate_gas().await.unwrap_or_else(|e| {
            warn!("Gas estimation failed: {}, using default", e);
            U256::from(BASE_GAS_ESTIMATE + FLASH_LOAN_OVERHEAD)
        });


        let gas_limit = (gas_estimate.as_u64() as f64 * gas_multiplier) as u64;
        let gas_limit = gas_limit.max(BASE_GAS_ESTIMATE + FLASH_LOAN_OVERHEAD);

        info!("   ⛽ Gas estimate: {} (limit: {})", gas_estimate, gas_limit);

        let mut tx = call.tx;
        tx.set_gas(U256::from(gas_limit));

        let gas_price = self.dual_provider.get_gas_price().await?;
        let adjusted_gas_price = (gas_price.as_u64() as f64 * self.config.trading.gas_price_multiplier) as u64;
        tx.set_gas_price(U256::from(adjusted_gas_price));

        let tx_nonce = {
            let mut nonce_guard = self.nonce_tracker.write().await;
            let current_nonce = *nonce_guard;
            *nonce_guard += 1;
            current_nonce
        };

        tx.set_nonce(U256::from(tx_nonce));
        tx.set_chain_id(self.config.network.chain_id);

        info!("   💸 Gas price: {:.2} gwei", adjusted_gas_price as f64 / 1e9);
        info!("   🔢 Blockchain TX nonce: {}", tx_nonce);
        info!("   🔢 Contract anti-replay nonce: {}", contract_nonce); 

        // ✅ Simulation avec timeout
        let simulation_result = tokio::time::timeout(
            std::time::Duration::from_millis(SIMULATION_TIMEOUT_MS),
            self.simulate_with_analysis(&tx)
        ).await;

        match simulation_result {
            Ok(Ok(true)) => debug!("✅ Simulation passed"),
            Ok(Ok(false)) => {
                warn!("⚠️  Simulation indicates unprofitable trade");
                return Err(anyhow::anyhow!("Simulation unprofitable"));
            }
            Ok(Err(e)) => {
                warn!("⚠️  Simulation failed: {}", e);
            }
            Err(_) => {
                warn!("⚠️  Simulation timeout");
            }
        }

        if use_private_mempool {
            self.send_private_transaction(tx).await
        } else {
            self.send_public_transaction(tx).await
        }
    }

    // ========== EXÉCUTION ATOMIC ==========

    async fn execute_atomic(
        &self,
        opportunity: &ArbitrageOpportunity,
        steps: Vec<ultimate_arbitrage_bot::SwapStep>,
        deadline: u32,
        contract_nonce: u64,
        use_private_mempool: bool,
        gas_multiplier: f64,
    ) -> Result<TransactionReceipt> {
        use ethers::prelude::*;

        let contract = UltimateArbitrageBot::new(
            self.contract_address,
            Arc::new(self.dual_provider.read().clone())
        );

        let balance = contract
            .get_balance(opportunity.asset)
            .call()
            .await?;

        if balance < opportunity.amount {
            return Err(anyhow::anyhow!(
                "Insufficient contract balance: {} < {}",
                balance,
                opportunity.amount
            ));
        }

        let min_profit = U256::from((opportunity.net_profit_usd * 0.95 * 1e6) as u64);

        let call = contract.execute_atomic(
            opportunity.asset,
            opportunity.amount,
            steps,
            min_profit.as_u128(),
            deadline,
            U256::from(chrono::Utc::now().timestamp_millis()),
        );

        let gas_estimate = call.estimate_gas().await.unwrap_or_else(|e| {
            warn!("Gas estimation failed: {}, using default", e);
            U256::from(BASE_GAS_ESTIMATE + FLASH_LOAN_OVERHEAD)
        });

        let gas_limit = (gas_estimate.as_u64() as f64 * gas_multiplier) as u64;
        let gas_limit = gas_limit.max(300_000);

        info!("   ⛽ Gas estimate: {} (limit: {})", gas_estimate, gas_limit);

        let mut tx = call.tx;
        tx.set_gas(U256::from(gas_limit));

        let gas_price = self.dual_provider.get_gas_price().await?;
        let adjusted_gas_price = (gas_price.as_u64() as f64 * self.config.trading.gas_price_multiplier) as u64;
        tx.set_gas_price(U256::from(adjusted_gas_price));

        let tx_nonce = {
            let mut nonce_guard = self.nonce_tracker.write().await;
            let current_nonce = *nonce_guard;
            *nonce_guard += 1;
            current_nonce
        };

        tx.set_nonce(U256::from(tx_nonce));
        tx.set_chain_id(self.config.network.chain_id);

        info!("   💸 Gas price: {:.2} gwei", adjusted_gas_price as f64 / 1e9);
        info!("   🔢 Blockchain TX nonce: {}", tx_nonce);
        info!("   🔢 Contract anti-replay nonce: {}", contract_nonce);

        if use_private_mempool {
            self.send_private_transaction(tx).await
        } else {
            self.send_public_transaction(tx).await
        }
    }

    // ========== HELPERS D'ENVOI ==========

    async fn send_private_transaction(
        &self,
        tx: ethers::types::transaction::eip2718::TypedTransaction,
    ) -> Result<TransactionReceipt> {
        info!("🔒 Sending via private mempool...");

        let pending = self.dual_provider
            .send_private_transaction(tx, &self.wallet)
            .await
            .context("Failed to send private transaction")?;

        info!("   📤 Transaction sent: {:?}", pending.tx_hash());

        let receipt = pending
            .await?
            .context("Transaction failed")?;

        Ok(receipt)
    }

    async fn send_public_transaction(
        &self,
        tx: ethers::types::transaction::eip2718::TypedTransaction,
    ) -> Result<TransactionReceipt> {
        warn!("⚠️  Sending via PUBLIC mempool (MEV risk!)");

        let signature = self.wallet.sign_transaction(&tx).await?;
        let signed_tx = tx.rlp_signed(&signature);

        let pending = self.dual_provider
            .write()
            .send_raw_transaction(signed_tx)
            .await?;

        info!("   📤 Transaction sent: {:?}", pending.tx_hash());

        let receipt = pending
            .await?
            .context("Transaction failed")?;

        Ok(receipt)
    }

    // ========== SIMULATION AVANCÉE ==========

    async fn simulate_with_analysis(
        &self,
        tx: &ethers::types::transaction::eip2718::TypedTransaction,
    ) -> Result<bool> {
        match self.dual_provider.simulate_transaction(tx).await {
            Ok(_) => Ok(true),
            Err(e) => {
                let error_str = e.to_string().to_lowercase();
                
                if error_str.contains("insufficient") || error_str.contains("slippage") {
                    warn!("⚠️  Simulation: insufficient funds or slippage");
                    Ok(false)
                } else if error_str.contains("unprofitable") {
                    warn!("⚠️  Simulation: unprofitable");
                    Ok(false)
                } else if error_str.contains("deadline") {
                    warn!("⚠️  Simulation: deadline expired");
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    // ========== PARSING RÉSULTAT ==========

    async fn parse_execution_result(
        &self,
        receipt: TransactionReceipt,
        execution_time_ms: u64,
    ) -> Result<FlashLoanResult> {
        let success = receipt.status == Some(1.into());
        let tx_hash = format!("{:?}", receipt.transaction_hash);
        let gas_used = receipt.gas_used.unwrap_or(U256::zero());
        let effective_gas_price = receipt.effective_gas_price.unwrap_or(U256::zero());

        let mut profit = U256::zero();

        // ✅ Parser les logs pour trouver l'événement Executed
        for log in &receipt.logs {
            if log.topics.len() >= 3 && log.data.len() >= 32 {
                profit = U256::from_big_endian(&log.data[0..32]);
                
                if profit > U256::zero() {
                    info!("✅ Arbitrage executed in {}ms!", execution_time_ms);
                    info!("   💰 Profit: {} wei (${:.2})", profit, profit.as_u128() as f64 / 1e6);
                    info!("   ⛽ Gas used: {}", gas_used);
                    info!("   💸 Gas price: {:.2} gwei", effective_gas_price.as_u64() as f64 / 1e9);
                }
            }
        }

        Ok(FlashLoanResult {
            success,
            tx_hash,
            profit,
            gas_used,
            effective_gas_price,
            execution_time_ms,
        })
    }

    // ========== ESTIMATION PROFIT NET ==========

    async fn estimate_net_profit_internal(&self, opportunity: &ArbitrageOpportunity) -> Result<i128> {
        let contract = UltimateArbitrageBot::new(
            self.contract_address,
            Arc::new(self.dual_provider.read().clone())
        );

        let gross_profit = U256::from((opportunity.expected_profit_usd * 1e6) as u64);
        let flash_loan_amount = opportunity.amount;
        let flash_loan_fee = U256::from(AAVE_FLASH_LOAN_FEE_BPS);
        let gas_estimate = U256::from(BASE_GAS_ESTIMATE + FLASH_LOAN_OVERHEAD);

        let gas_price = self.dual_provider.get_gas_price().await?;
        let gas_price_gwei = U256::from(gas_price.as_u64() / 1_000_000_000);

        let net_profit: I256 = contract
            .estimate_net_profit(
                gross_profit,
                flash_loan_amount,
                flash_loan_fee,
                gas_estimate,
                gas_price_gwei,
            )
            .call()
            .await?;

        let net_profit_i128: i128 = {
            let is_negative = net_profit.is_negative();
            let abs_value = if is_negative {
                net_profit.overflowing_neg().0
            } else {
                net_profit
            };

            let mut bytes = [0u8; 32];
            abs_value.to_big_endian(&mut bytes);

            let value = i128::from_be_bytes([
                bytes[16], bytes[17], bytes[18], bytes[19],
                bytes[20], bytes[21], bytes[22], bytes[23],
                bytes[24], bytes[25], bytes[26], bytes[27],
                bytes[28], bytes[29], bytes[30], bytes[31],
            ]);

            if is_negative { -value } else { value }
        };

        debug!("💰 Net profit estimate: ${:.2}", net_profit_i128 as f64 / 1e6);

        Ok(net_profit_i128)
    }

    // ========== CONVERSION STEPS ==========

    fn convert_steps_to_contract_type(
        &self,
        steps: &[crate::types::SwapStep],
    ) -> Result<Vec<ultimate_arbitrage_bot::SwapStep>> {
        steps
            .iter()
            .map(|step| {
                let dex_id = match step.dex {
                    DexType::UniswapV3 => 0u8,
                    DexType::UniswapV2 => 1u8,
                    DexType::Aerodrome => 2u8,
                    DexType::AerodromeSlipstream => 3u8,
                    DexType::Curve => 4u8,
                    DexType::Balancer => 5u8,
                    DexType::Sushiswap => 6u8,
                };

                Ok(ultimate_arbitrage_bot::SwapStep {
                    dex: dex_id,
                    token_in: step.token_in,
                    token_out: step.token_out,
                    data: step.data.clone(),
                    min_out: step.min_out,
                })
            })
            .collect()
    }

    // ========== MÉTRIQUES ==========

    pub async fn get_metrics(&self) -> Result<(u64, U256, u64)> {
        let contract = UltimateArbitrageBot::new(
            self.contract_address,
            Arc::new(self.dual_provider.read().clone())
        );

        let (trades, profit, remaining) = contract
            .get_metrics()
            .call()
            .await?;

        Ok((trades.as_u64(), profit, remaining.as_u64()))
    }

    // ========== HELPERS ==========

    async fn is_tx_executed(&self, tx_hash: [u8; 32]) -> Result<bool> {
        let contract = UltimateArbitrageBot::new(
            self.contract_address,
            Arc::new(self.dual_provider.read().clone())
        );

        Ok(contract.is_executed(tx_hash).call().await?)
    }

    fn calculate_tx_hash(&self, asset: &Address, amount: &U256, nonce: u64) -> Result<[u8; 32]> {
        use ethers::utils::keccak256;

        let mut data = Vec::new();
        data.extend_from_slice(asset.as_bytes());

        let mut amount_bytes = [0u8; 32];
        amount.to_big_endian(&mut amount_bytes);
        data.extend_from_slice(&amount_bytes);

        data.extend_from_slice(&nonce.to_be_bytes());
        data.extend_from_slice(self.contract_address.as_bytes());

        Ok(keccak256(&data))
    }

    async fn get_next_nonce(&self) -> u64 {
        let mut nonce = self.nonce_tracker.write().await;
        let current = *nonce;
        *nonce += 1;
        debug!("🔢 Using nonce: {} (next: {})", current, *nonce);
        current
    }
}

// ========== HELPERS PUBLICS ==========

pub async fn is_profitable_with_flashloan(
    opportunity: &ArbitrageOpportunity,
    gas_price_gwei: f64,
) -> bool {
    let flash_loan_cost_usd = opportunity.amount.as_u128() as f64
        * (AAVE_FLASH_LOAN_FEE_BPS as f64 / 10000.0)
        / 1e18
        * 3300.0; // Assume ETH = $3300

    let gas_cost_usd = (BASE_GAS_ESTIMATE + FLASH_LOAN_OVERHEAD) as f64
        * gas_price_gwei
        * 1e-9
        * 3300.0;

    let total_cost_usd = flash_loan_cost_usd + gas_cost_usd;

    debug!("Flash loan profitability check:");
    debug!("  Flash loan cost: ${:.2}", flash_loan_cost_usd);
    debug!("  Gas cost: ${:.2}", gas_cost_usd);
    debug!("  Total cost: ${:.2}", total_cost_usd);
    debug!("  Expected profit: ${:.2}", opportunity.expected_profit_usd);

    opportunity.expected_profit_usd > total_cost_usd
}

pub fn calculate_min_profit_for_flashloan(
    flash_loan_amount: U256,
    gas_price_gwei: f64,
) -> U256 {
    let flash_loan_cost = flash_loan_amount.as_u128() * (AAVE_FLASH_LOAN_FEE_BPS as u128) / 10000;
    let gas_cost = ((BASE_GAS_ESTIMATE + FLASH_LOAN_OVERHEAD) as f64 * gas_price_gwei * 1e9) as u128;

    U256::from(flash_loan_cost + gas_cost)
}

// ========== TESTS ==========

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dex_conversion() {
        assert_eq!(DexType::UniswapV3 as u8, 0);
        assert_eq!(DexType::UniswapV2 as u8, 1);
        assert_eq!(DexType::Aerodrome as u8, 2);
        assert_eq!(DexType::AerodromeSlipstream as u8, 3);
        assert_eq!(DexType::Curve as u8, 4);
        assert_eq!(DexType::Balancer as u8, 5);
        assert_eq!(DexType::Sushiswap as u8, 6);
    }

    #[test]
    fn test_flash_loan_fee() {
        let amount = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
        let fee = amount.as_u128() * AAVE_FLASH_LOAN_FEE_BPS as u128 / 10000;

        // 0.09% de 1 ETH = 0.0009 ETH = 900000000000000 wei
        assert_eq!(fee, 900_000_000_000_000);
    }

    #[test]
    fn test_min_profit_calculation() {
        let flash_loan_amount = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
        let gas_price_gwei = 0.001;

        let min_profit = calculate_min_profit_for_flashloan(flash_loan_amount, gas_price_gwei);

        assert!(min_profit > U256::zero());
    }
}
