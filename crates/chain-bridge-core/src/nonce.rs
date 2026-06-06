//! Durable-nonce allocation domain types.
//!
//! At high throughput the recent-blockhash validity window (~60s) becomes a
//! liveness constraint. A pool of durable nonce accounts lets the submit saga
//! pin a transaction to an allocated nonce instead of a fresh blockhash. The
//! [`NoncePort`](crate::ports::NoncePort) adapter owns the pool; these are the
//! values it hands back.

use serde::{Deserialize, Serialize};
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;

/// Database row id of a nonce allocation lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonceId(pub i64);

/// A leased durable-nonce account and its current stored nonce value (which is
/// used in place of `recent_blockhash` when building the transaction message).
#[derive(Debug, Clone)]
pub struct NonceAllocation {
    pub id: NonceId,
    pub nonce_account: Pubkey,
    pub nonce_authority: Pubkey,
    pub nonce_value: Hash,
}
