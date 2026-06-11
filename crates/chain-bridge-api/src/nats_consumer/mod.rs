//! NATS JetStream consumer for chain-bridge tx submit/simulate/cancel.
//!
//! Split from the former monolithic `nats_consumer.rs`:
//! - [`dedup`]     — effect-level dedup types (`DedupRecord`, `DedupState`)
//! - [`auth`]      — envelope authentication (cert + signature verification)
//! - [`consumer`]  — the `NatsConsumer` impl (subscribe loops + handlers)
//! - `tests`       — unit tests (cfg(test))

// Shared imports re-exported for submodules, which pull them in via `use super::*;`.
pub(crate) use std::sync::Arc;
pub(crate) use tracing::{info, warn, error};
pub(crate) use futures::StreamExt;
pub(crate) use dashmap::DashMap;
pub(crate) use tokio_retry::{Retry, strategy::FixedInterval};
pub(crate) use gridtokenx_blockchain_core::rpc::nats_schema::{
    TxSubmitMessage, TxResultMessage, TxSimulateMessage, TxSimulateResultMessage,
    TxCancelMessage, TxCancelResultMessage,
};
pub(crate) use gridtokenx_blockchain_core::rpc::metrics::BlockchainMetrics;
pub(crate) use gridtokenx_blockchain_core::auth::{ServiceRole, SpiffeIdentity};
pub(crate) use crate::api::ChainBridgeGrpcService;
pub(crate) use solana_sdk::transaction::Transaction;
pub(crate) use solana_sdk::signature::Signature;

pub mod auth;
mod dedup;
mod consumer;

pub(crate) use dedup::{DedupRecord, DedupState};
pub use auth::NatsAuthPolicy;
pub(crate) use auth::AuthCheck;

pub struct NatsConsumer {
    jetstream: async_nats::jetstream::Context,
    signing_service: Arc<ChainBridgeGrpcService>,
    metrics: Arc<dyn BlockchainMetrics>,
    /// correlation_id -> expiry_timestamp (ms)
    idempotency_cache: DashMap<String, u64>,
    /// idempotency_key -> dedup record (effect-level, replayable).
    /// `Arc` so the background cleanup task shares the same map (a bare
    /// `DashMap::clone()` deep-copies — see `idempotency_cache`).
    dedup_store: Arc<DashMap<String, DedupRecord>>,
    /// Envelope-auth enforcement + CA trust root (see [`auth`]).
    auth_policy: NatsAuthPolicy,
}

#[cfg(test)]
mod tests;
