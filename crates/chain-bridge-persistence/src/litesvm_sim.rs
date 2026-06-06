//! Pre-sign simulation adapter (Gap #3) — implements
//! [`chain_bridge_core::PreSignSimulatorPort`].
//!
//! A transaction can be simulated against either a real RPC
//! ([`RealSolanaProvider`]) or the in-memory LiteSVM
//! ([`SurfpoolSolanaProvider`]) before a Vault signing operation is spent. The
//! submit saga uses this in *advisory* mode: a failed simulation is logged and
//! audited but does not block the existing submit path.
//!
//! Implemented per concrete provider (the orphan rule forbids a blanket impl of
//! core's foreign trait over an uncovered generic).
//!
//! [`RealSolanaProvider`]: crate::solana_client::RealSolanaProvider
//! [`SurfpoolSolanaProvider`]: crate::solana_client::SurfpoolSolanaProvider

use async_trait::async_trait;
use solana_sdk::transaction::Transaction;

use chain_bridge_core::{ChainBridgeError, PreSignSimulatorPort, SimulationOutcome};

use crate::solana_client::{RealSolanaProvider, SolanaProvider, SurfpoolSolanaProvider};

async fn port_simulate<P: SolanaProvider + ?Sized>(
    p: &P,
    tx: &Transaction,
) -> Result<SimulationOutcome, ChainBridgeError> {
    match SolanaProvider::simulate_transaction(p, tx).await {
        Ok(resp) => Ok(SimulationOutcome {
            success: resp.value.err.is_none(),
            compute_units: resp.value.units_consumed.unwrap_or(0),
            logs: resp.value.logs.unwrap_or_default(),
            error: resp.value.err.map(|e| format!("{:?}", e)),
        }),
        Err(e) => Err(ChainBridgeError::Simulation(e.to_string())),
    }
}

#[async_trait]
impl PreSignSimulatorPort for RealSolanaProvider {
    async fn simulate(&self, tx: &Transaction) -> Result<SimulationOutcome, ChainBridgeError> {
        port_simulate(self, tx).await
    }
}

#[async_trait]
impl PreSignSimulatorPort for SurfpoolSolanaProvider {
    async fn simulate(&self, tx: &Transaction) -> Result<SimulationOutcome, ChainBridgeError> {
        port_simulate(self, tx).await
    }
}
