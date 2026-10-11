//! Transaction plans: the fee parameters of a transfer, computed the same
//! way for a quote and for the transaction the engine then signs, with the
//! most it can cost (spec §7.6–7.7).

use anyhow::{Context, Result};
use chain::btc_tx::{select_coins, CoinSelection, EsploraRpc};
use chain::destination::parse_bitcoin_address;
use chain::evm::{erc20_transfer_data, EvmFees, EvmRpc, ERC20_GAS_ESTIMATE_PERCENT, ETH_TRANSFER_GAS};
use chain::solana_tx::{SolanaFees, SolanaRpc, CU_LIMIT_SOL_TRANSFER, CU_LIMIT_USDT_TRANSFER, CU_LIMIT_USDT_TRANSFER_WITH_ATA, TOKEN_ACCOUNT_RENT_LAMPORTS};
use chain::tron_tx::{fee_limit_sun, TronChainFees, TronRpc, TRC20_TRANSFER_BANDWIDTH, TRX_TRANSFER_BANDWIDTH};
use config::{
    BTC_FEE_TARGET_BLOCKS, BTC_MAX_FEE_RATE_SAT_VB, BTC_MAX_INPUTS, BTC_MIN_FEE_RATE_SAT_VB,
    TRON_FEE_LIMIT_MAX_SUN,
};
use domain::Token;

use crate::withdrawals::resolve::SolanaRoute;
use crate::AppState;

/// Gas assumed for a USDT `transfer` when `eth_estimateGas` cannot run
/// (e.g. the treasury does not hold the amount yet): above what Tether's
/// contract ever needs.
pub const USDT_ERC20_GAS_FALLBACK: u64 = 100_000;
/// Energy assumed for a TRC-20 `transfer` when the node cannot simulate it
/// (twice the usual ~65 000 with the dynamic-energy penalty).
pub const TRC20_ENERGY_FALLBACK: u64 = 130_000;

/// An Ethereum transfer ready to be numbered and signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthPlan {
    /// The recipient (ETH) or the token contract (USDT).
    pub to: String,
    pub value: u128,
    pub data: Vec<u8>,
    pub gas_limit: u64,
    pub fees: EvmFees,
}

impl EthPlan {
    pub fn max_fee(&self) -> u128 {
        self.fees.max_cost(self.gas_limit)
    }
}

/// Gas limit from an estimate: a plain transfer to an account costs
/// exactly 21 000; anything more (a contract) is padded ×1.3.
pub fn padded_gas(estimate: Option<u64>, fallback: u64) -> u64 {
    match estimate {
        Some(gas) if gas <= ETH_TRANSFER_GAS => ETH_TRANSFER_GAS,
        Some(gas) => gas.saturating_mul(ERC20_GAS_ESTIMATE_PERCENT).div_ceil(100),
        None => fallback,
    }
}

pub async fn plan_eth(
    rpc: &dyn EvmRpc,
    from: &str,
    token: Token,
    usdt_contract: &str,
    destination: &str,
    amount: u128,
) -> Result<EthPlan> {
    let fees = rpc.fee_quote().await?;
    match token {
        Token::Eth => {
            let estimate = rpc.estimate_gas(from, destination, amount, &[]).await.ok();
            Ok(EthPlan {
                to: destination.to_string(),
                value: amount,
                data: Vec::new(),
                gas_limit: padded_gas(estimate, ETH_TRANSFER_GAS),
                fees,
            })
        }
        Token::Usdt => {
            let data = erc20_transfer_data(destination, amount)?;
            let estimate = rpc.estimate_gas(from, usdt_contract, 0, &data).await.ok();
            Ok(EthPlan {
                to: usdt_contract.to_string(),
                value: 0,
                data,
                gas_limit: padded_gas(estimate, USDT_ERC20_GAS_FALLBACK).max(ETH_TRANSFER_GAS),
                fees,
            })
        }
        other => anyhow::bail!("{} is not sent on ethereum", other.as_api()),
    }
}

/// A Tron transfer's resources and their price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TronPlan {
    pub energy: u64,
    /// `fee_limit` of the contract call (0 for TRX).
    pub fee_limit: u64,
    pub prices: TronChainFees,
    /// Burns the destination's activation (TRX to a new account).
    pub activates_destination: bool,
}

impl TronPlan {
    /// Energy fee limit + bandwidth + activation, in sun.
    pub fn max_fee(&self) -> u128 {
        let bandwidth = if self.fee_limit > 0 { TRC20_TRANSFER_BANDWIDTH } else { TRX_TRANSFER_BANDWIDTH };
        let activation = if self.activates_destination { self.prices.create_account_sun } else { 0 };
        u128::from(self.fee_limit)
            + u128::from(bandwidth.saturating_mul(self.prices.bandwidth_sun))
            + u128::from(activation)
    }
}

pub async fn plan_tron(
    rpc: &dyn TronRpc,
    owner: &str,
    token: Token,
    usdt_contract: &str,
    destination: &str,
    amount: u128,
) -> Result<TronPlan> {
    let prices = rpc.chain_fees().await?;
    match token {
        Token::Usdt => {
            let energy = rpc
                .estimate_trc20_energy(owner, usdt_contract, destination, amount)
                .await
                .unwrap_or(TRC20_ENERGY_FALLBACK);
            Ok(TronPlan {
                energy,
                fee_limit: fee_limit_sun(energy, prices.energy_sun, TRON_FEE_LIMIT_MAX_SUN),
                prices,
                activates_destination: false,
            })
        }
        Token::Trx => Ok(TronPlan {
            energy: 0,
            fee_limit: 0,
            prices,
            activates_destination: rpc.account_balance(destination).await?.is_none(),
        }),
        other => anyhow::bail!("{} is not sent on tron", other.as_api()),
    }
}

/// A Solana transfer's compute budget and the rent it may pay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolanaPlan {
    pub fees: SolanaFees,
    /// The destination's USDT account rent, when the transfer creates it.
    pub rent: u64,
}

impl SolanaPlan {
    pub fn max_fee(&self) -> u128 {
        u128::from(self.fees.max_fee_lamports(1)) + u128::from(self.rent)
    }
}

pub async fn plan_solana(rpc: &dyn SolanaRpc, treasury_owner: &str, route: &SolanaRoute) -> Result<SolanaPlan> {
    let (cu_limit, rent, accounts) = match route {
        SolanaRoute::Wallet => (CU_LIMIT_SOL_TRANSFER, 0, vec![treasury_owner.to_string()]),
        SolanaRoute::TokenAccount { account, create_for_owner } => (
            if create_for_owner.is_some() { CU_LIMIT_USDT_TRANSFER_WITH_ATA } else { CU_LIMIT_USDT_TRANSFER },
            if create_for_owner.is_some() { TOKEN_ACCOUNT_RENT_LAMPORTS } else { 0 },
            vec![treasury_owner.to_string(), account.clone()],
        ),
    };
    // No estimate (provider without the method): no priority fee rather than
    // no withdrawal.
    let cu_price = rpc.priority_fee(&accounts).await.unwrap_or(0);
    Ok(SolanaPlan {
        fees: SolanaFees { cu_limit, cu_price },
        rent,
    })
}

/// Inputs, fee and change of a Bitcoin withdrawal at the current fee rate.
pub async fn plan_btc(
    state: &AppState,
    esplora: &dyn EsploraRpc,
    destination: &str,
    amount: u64,
) -> Result<(CoinSelection, u64)> {
    let destination = parse_bitcoin_address(destination, state.config.chain.is_testnet())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let fee_rate = esplora
        .fee_rate(BTC_FEE_TARGET_BLOCKS, BTC_MIN_FEE_RATE_SAT_VB, BTC_MAX_FEE_RATE_SAT_VB)
        .await?;
    let utxos = crate::treasury::spendable_btc_utxos(state, esplora).await?;
    let selection = select_coins(utxos, amount, fee_rate, destination.script_pubkey().len(), BTC_MAX_INPUTS)
        .context("no spendable inputs cover this amount")?;
    Ok((selection, fee_rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gas_limits() {
        assert_eq!(padded_gas(Some(21_000), 21_000), 21_000);
        // A contract recipient: padded ×1.3.
        assert_eq!(padded_gas(Some(30_000), 21_000), 39_000);
        assert_eq!(padded_gas(None, USDT_ERC20_GAS_FALLBACK), USDT_ERC20_GAS_FALLBACK);
    }

    #[test]
    fn tron_and_solana_maximum_fees() {
        let prices = TronChainFees { energy_sun: 100, bandwidth_sun: 1_000, create_account_sun: 100_000 };
        let usdt = TronPlan { energy: 64_285, fee_limit: 9_642_750, prices, activates_destination: false };
        assert_eq!(usdt.max_fee(), 9_642_750 + 345_000);
        let trx = TronPlan { energy: 0, fee_limit: 0, prices, activates_destination: true };
        assert_eq!(trx.max_fee(), 270_000 + 100_000);
        let solana = SolanaPlan { fees: SolanaFees { cu_limit: 80_000, cu_price: 12_500 }, rent: TOKEN_ACCOUNT_RENT_LAMPORTS };
        assert_eq!(solana.max_fee(), 5_000 + 1_000 + 2_039_280);
    }
}
