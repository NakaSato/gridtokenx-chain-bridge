//! Durable-nonce lease adapter — implements [`chain_bridge_core::NoncePort`].
//!
//! Leases rows from a pre-seeded `nonce_allocations` pool (see `migrations/`).
//! This is the scaffold for the high-throughput path described in `SKILL.md`
//! (Wall 1): the pool of on-chain nonce accounts must be provisioned out of
//! band; this adapter only hands out and returns leases. `allocate` claims a
//! `free` row; `release` returns it. With an empty/unseeded pool, `allocate`
//! returns [`ChainBridgeError::Nonce`] — it never fabricates a nonce.

use std::str::FromStr;

use async_trait::async_trait;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use sqlx::Row;
use sqlx::postgres::PgPool;

use chain_bridge_core::nonce::{NonceAllocation, NonceId};
use chain_bridge_core::{ChainBridgeError, NoncePort};

pub struct PostgresNonceStore {
    pool: PgPool,
}

impl PostgresNonceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl NoncePort for PostgresNonceStore {
    async fn allocate(&self, _key_name: &str) -> Result<NonceAllocation, ChainBridgeError> {
        // Claim the oldest free lease atomically.
        let row = sqlx::query(
            r#"UPDATE nonce_allocations
                 SET status = 'leased', leased_at_ms = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
               WHERE id = (
                 SELECT id FROM nonce_allocations
                 WHERE status = 'free'
                 ORDER BY id ASC
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
               )
               RETURNING id, nonce_account, nonce_authority, nonce_value"#,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ChainBridgeError::Nonce(format!("nonce allocate query failed: {e}")))?
        .ok_or_else(|| ChainBridgeError::Nonce("durable-nonce pool exhausted or unseeded".into()))?;

        let id: i64 = row.try_get("id").map_err(|e| ChainBridgeError::Nonce(e.to_string()))?;
        let account: String = row.try_get("nonce_account").map_err(|e| ChainBridgeError::Nonce(e.to_string()))?;
        let authority: String = row.try_get("nonce_authority").map_err(|e| ChainBridgeError::Nonce(e.to_string()))?;
        let value: String = row.try_get("nonce_value").map_err(|e| ChainBridgeError::Nonce(e.to_string()))?;

        Ok(NonceAllocation {
            id: NonceId(id),
            nonce_account: Pubkey::from_str(&account)
                .map_err(|e| ChainBridgeError::Nonce(format!("bad nonce_account: {e}")))?,
            nonce_authority: Pubkey::from_str(&authority)
                .map_err(|e| ChainBridgeError::Nonce(format!("bad nonce_authority: {e}")))?,
            nonce_value: Hash::from_str(&value)
                .map_err(|e| ChainBridgeError::Nonce(format!("bad nonce_value: {e}")))?,
        })
    }

    async fn release(&self, id: NonceId) -> Result<(), ChainBridgeError> {
        sqlx::query("UPDATE nonce_allocations SET status = 'free', leased_at_ms = NULL WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| ChainBridgeError::Nonce(format!("nonce release failed: {e}")))?;
        Ok(())
    }
}
