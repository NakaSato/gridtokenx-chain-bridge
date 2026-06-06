//! `chain-bridge-persistence` — the driven adapters.
//!
//! Each module implements a port from `chain-bridge-core` against a concrete
//! technology, so the logic layer stays free of Vault/Solana/SQL/SVM types:
//! - [`vault_signer`]   — `SignerPort` (Vault Transit / dev keypair)
//! - [`solana_client`]  — `ChainClientPort` + the rich read `SolanaProvider`
//! - [`litesvm_sim`]    — `PreSignSimulatorPort` (RPC or in-memory LiteSVM)
//! - [`postgres_audit`] — `AuditPort` (Postgres hash-chain / in-memory)
//! - [`nonce_store`]    — `NoncePort` (Postgres durable-nonce leases)

pub mod litesvm_sim;
pub mod nonce_store;
pub mod postgres_audit;
pub mod solana_client;
pub mod vault_signer;

pub use nonce_store::PostgresNonceStore;
pub use postgres_audit::{InMemoryAuditStore, PostgresAuditStore};
pub use solana_client::{
    BlockhashCache, RealSolanaProvider, SolanaProvider, SurfpoolSolanaProvider,
};
pub use vault_signer::{InsecureKeypairProvider, VaultProvider, VaultTransitClient};
