// multicall_helper.rs

use anyhow::Result;
use ethers::{
    contract::{abigen, Multicall},
    providers::{Provider, Ws},
    types::{Address, U256},
};
use std::sync::Arc;

abigen!(
    IUniswapV2Pair,
    r#"[
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast)
    ]"#
);

pub struct MulticallHelper {
    multicall: Multicall<Provider<Ws>>,
}

impl MulticallHelper {
    pub async fn new(provider: Arc<Provider<Ws>>) -> Result<Self> {
        let multicall = Multicall::new((*provider).clone(), None).await?;
        Ok(Self { multicall })
    }

    pub async fn batch_get_reserves(
        &mut self,
        pool_addresses: Vec<Address>,
        provider: Arc<Provider<Ws>>,
    ) -> Result<Vec<Option<(U256, U256)>>> {
        self.multicall.clear_calls();

        for pool_address in &pool_addresses {
            let pair = IUniswapV2Pair::new(*pool_address, provider.clone());
            self.multicall.add_call(pair.get_reserves(), false);
        }

        let results: Vec<(u128, u128, u32)> = match self.multicall.call().await {
            Ok(r) => r,
            Err(e) => {
                // Sur erreur, retourner None pour tous
                return Ok(vec![None; pool_addresses.len()]);
            }
        };

        Ok(results
            .into_iter()
            .map(|(r0, r1, _)| Some((U256::from(r0), U256::from(r1))))
            .collect())
    }
}