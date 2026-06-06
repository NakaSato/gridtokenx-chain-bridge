//! The single error type crossing port boundaries.
//!
//! Adapters (persistence) map their concrete failures (reqwest, sqlx, Solana
//! `ClientError`, litesvm) into these variants so the logic layer never sees a
//! framework-specific error.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ChainBridgeError {
    /// Vault Transit / keypair signing failed.
    #[error("signer error: {0}")]
    Signer(String),

    /// Solana RPC / SVM client failure (submit, simulate, read).
    #[error("chain client error: {0}")]
    ChainClient(String),

    /// Transaction violated the declarative policy. Terminal — never retried.
    #[error("policy rejected: {0}")]
    PolicyRejected(String),

    /// Pre-sign simulation reported the transaction would fail on-chain.
    #[error("simulation failed: {0}")]
    Simulation(String),

    /// Audit log append / read failure (DB or hash-chain integrity).
    #[error("audit error: {0}")]
    Audit(String),

    /// Durable-nonce allocation/release failure.
    #[error("nonce error: {0}")]
    Nonce(String),

    /// Payload could not be decoded into a `Transaction`. Terminal.
    #[error("invalid transaction: {0}")]
    InvalidTransaction(String),

    /// Caller identity/role not permitted for this effect. Terminal.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
}

impl ChainBridgeError {
    /// Whether a retry could plausibly succeed. Policy/auth/decoding failures
    /// are static and must not re-enter the retry loop; client/simulation
    /// failures may be transient (node behind, rate limited).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ChainBridgeError::ChainClient(_)
                | ChainBridgeError::Simulation(_)
                | ChainBridgeError::Signer(_)
                | ChainBridgeError::Nonce(_)
        )
    }
}
