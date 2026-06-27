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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_variants_may_retry() {
        // Node behind / rate-limited / HSM blip — a retry could plausibly succeed.
        assert!(ChainBridgeError::ChainClient("rpc timeout".into()).is_transient());
        assert!(ChainBridgeError::Simulation("blockhash not found".into()).is_transient());
        assert!(ChainBridgeError::Signer("vault 503".into()).is_transient());
        assert!(ChainBridgeError::Nonce("pool exhausted".into()).is_transient());
    }

    #[test]
    fn terminal_variants_must_not_retry() {
        // Static failures — re-entering the retry loop would just burn attempts.
        assert!(!ChainBridgeError::PolicyRejected("program not allowed".into()).is_transient());
        assert!(!ChainBridgeError::Unauthorized("role denied".into()).is_transient());
        assert!(!ChainBridgeError::InvalidTransaction("bad base64".into()).is_transient());
        assert!(!ChainBridgeError::Audit("hash-chain break".into()).is_transient());
    }

    #[test]
    fn display_carries_variant_prefix_and_message() {
        assert_eq!(
            ChainBridgeError::PolicyRejected("program X".into()).to_string(),
            "policy rejected: program X"
        );
        assert_eq!(
            ChainBridgeError::Unauthorized("role Y".into()).to_string(),
            "unauthorized: role Y"
        );
    }
}
