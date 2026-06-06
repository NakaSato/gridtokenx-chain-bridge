//! Ports — the traits the edges implement and the logic layer depends on.
//!
//! These are `async` because each fronts a real network/HSM/DB boundary. The
//! concrete adapters live in `chain-bridge-persistence`; the orchestrator in
//! `chain-bridge-logic` is generic over these traits, so the data flow
//! (policy → simulate → sign → submit → audit) is explicit and each step is
//! independently testable with a fake.

use async_trait::async_trait;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;

use crate::audit::{AuditEntry, AuditHash, AuditId};
use crate::error::ChainBridgeError;
use crate::nonce::{NonceAllocation, NonceId};

/// Outcome of a (pre-sign) simulation. Framework-neutral projection of the
/// Solana / LiteSVM simulation result.
#[derive(Debug, Clone, Default)]
pub struct SimulationOutcome {
    pub success: bool,
    pub compute_units: u64,
    pub logs: Vec<String>,
    pub error: Option<String>,
}

/// Outcome of submitting a signed transaction to the chain.
#[derive(Debug, Clone)]
pub struct SubmitOutcome {
    pub signature: Signature,
    pub slot: u64,
}

/// Signing oracle. The private key never enters the process address space; the
/// adapter delegates to Vault Transit (prod) or a local keypair (dev).
#[async_trait]
pub trait SignerPort: Send + Sync {
    async fn public_key(&self, key_name: &str) -> Result<Pubkey, ChainBridgeError>;
    async fn sign_message(
        &self,
        key_name: &str,
        message: &[u8],
    ) -> Result<Signature, ChainBridgeError>;
}

/// The write/blockhash subset of Solana RPC the submit saga needs. Read-only
/// query methods (balance, account, slot, …) remain on the richer persistence
/// `SolanaProvider`; this port is deliberately minimal.
#[async_trait]
pub trait ChainClientPort: Send + Sync {
    async fn latest_blockhash(&self) -> Result<(Hash, u64), ChainBridgeError>;
    async fn send_transaction(&self, tx: &Transaction) -> Result<SubmitOutcome, ChainBridgeError>;
}

/// Pre-sign simulation against an in-memory SVM (LiteSVM) or RPC. Lets the saga
/// reject a doomed transaction before spending a Vault signing operation.
#[async_trait]
pub trait PreSignSimulatorPort: Send + Sync {
    async fn simulate(&self, tx: &Transaction) -> Result<SimulationOutcome, ChainBridgeError>;
}

/// Append-only, hash-chained audit trail. The adapter persists entries and is
/// responsible for verifying `prev_hash` linkage on append.
#[async_trait]
pub trait AuditPort: Send + Sync {
    /// Append an entry; the adapter stamps `prev_hash` from the current tip and
    /// returns the assigned id.
    async fn append(&self, entry: AuditEntry) -> Result<AuditId, ChainBridgeError>;
    /// Hash of the current chain tip, or `None` for an empty log.
    async fn latest_hash(&self) -> Result<Option<AuditHash>, ChainBridgeError>;
}

/// Durable-nonce allocation, so high-throughput submitters aren't bottlenecked
/// on the recent-blockhash validity window.
#[async_trait]
pub trait NoncePort: Send + Sync {
    async fn allocate(&self, key_name: &str) -> Result<NonceAllocation, ChainBridgeError>;
    async fn release(&self, id: NonceId) -> Result<(), ChainBridgeError>;
}
