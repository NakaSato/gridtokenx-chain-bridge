//! Audit log adapters (Gap #2) — implement [`chain_bridge_core::AuditPort`].
//!
//! [`PostgresAuditStore`] persists the hash-chained trail to the `audit_log`
//! table (see `migrations/`). On each append it reads the current chain tip,
//! computes `entry_hash = SHA-256(prev_hash || fields)` via the pure
//! [`AuditEntry::entry_hash`], and inserts the linked row. Any retroactive edit
//! breaks every subsequent link; `audit_anchor` (logic layer) periodically
//! commits the tip to Solana.
//!
//! [`InMemoryAuditStore`] is a dev/no-DB fallback with identical linkage
//! semantics, used when `DATABASE_URL` is unset and in unit tests.
//!
//! Queries use the runtime `sqlx::query` API (not the `query!` macro) so the
//! crate builds without a live database or offline metadata.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::Row;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::sync::Mutex;

use chain_bridge_core::audit::{AuditEntry, AuditHash, AuditId};
use chain_bridge_core::{AuditPort, ChainBridgeError};

fn hash_to_vec(h: AuditHash) -> Vec<u8> {
    h.0.to_vec()
}

fn vec_to_hash(v: &[u8]) -> Result<AuditHash, ChainBridgeError> {
    let arr: [u8; 32] = v
        .try_into()
        .map_err(|_| ChainBridgeError::Audit(format!("audit hash has {} bytes, expected 32", v.len())))?;
    Ok(AuditHash(arr))
}

// ---------------------------------------------------------------------------
// Postgres
// ---------------------------------------------------------------------------

pub struct PostgresAuditStore {
    pool: PgPool,
}

impl PostgresAuditStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Connect using `DATABASE_URL` (or the provided URL) and verify the pool.
    pub async fn connect(database_url: &str) -> Result<Self, ChainBridgeError> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .map_err(|e| ChainBridgeError::Audit(format!("audit DB connect failed: {e}")))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl AuditPort for PostgresAuditStore {
    async fn append(&self, entry: AuditEntry) -> Result<AuditId, ChainBridgeError> {
        let prev = self.latest_hash().await?;
        let entry_hash = entry.entry_hash(prev);

        let outcome_json = serde_json::to_string(&entry.outcome)
            .map_err(|e| ChainBridgeError::Audit(format!("serialize outcome: {e}")))?;

        let row = sqlx::query(
            r#"INSERT INTO audit_log
                 (prev_hash, entry_hash, correlation_id, identity, action, outcome_json, created_at_ms)
               VALUES ($1, $2, $3, $4, $5, $6, $7)
               RETURNING id"#,
        )
        .bind(prev.map(hash_to_vec))
        .bind(hash_to_vec(entry_hash))
        .bind(&entry.correlation_id)
        .bind(&entry.identity)
        .bind(&entry.action)
        .bind(outcome_json)
        .bind(entry.created_at_ms as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| ChainBridgeError::Audit(format!("audit insert failed: {e}")))?;

        let id: i64 = row
            .try_get("id")
            .map_err(|e| ChainBridgeError::Audit(format!("read inserted id: {e}")))?;
        Ok(AuditId(id))
    }

    async fn latest_hash(&self) -> Result<Option<AuditHash>, ChainBridgeError> {
        let row = sqlx::query("SELECT entry_hash FROM audit_log ORDER BY id DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ChainBridgeError::Audit(format!("audit tip query failed: {e}")))?;

        match row {
            Some(r) => {
                let bytes: Vec<u8> = r
                    .try_get("entry_hash")
                    .map_err(|e| ChainBridgeError::Audit(format!("read tip hash: {e}")))?;
                Ok(Some(vec_to_hash(&bytes)?))
            }
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// In-memory fallback (dev / no DATABASE_URL / tests)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct InMemoryAuditStore {
    inner: Arc<Mutex<Vec<(AuditId, AuditHash, AuditEntry)>>>,
}

impl InMemoryAuditStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}

#[async_trait]
impl AuditPort for InMemoryAuditStore {
    async fn append(&self, entry: AuditEntry) -> Result<AuditId, ChainBridgeError> {
        let mut log = self.inner.lock().await;
        let prev = log.last().map(|(_, h, _)| *h);
        let entry_hash = entry.entry_hash(prev);
        let id = AuditId(log.len() as i64 + 1);
        log.push((id, entry_hash, entry));
        Ok(id)
    }

    async fn latest_hash(&self) -> Result<Option<AuditHash>, ChainBridgeError> {
        Ok(self.inner.lock().await.last().map(|(_, h, _)| *h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chain_bridge_core::audit::AuditOutcome;

    fn entry(corr: &str) -> AuditEntry {
        AuditEntry::new(
            corr,
            "spiffe://gridtokenx.th/prod/trading-service/matcher",
            "submit",
            AuditOutcome::Submitted { signature: "s".into(), slot: 1 },
            1_700_000_000_000,
        )
    }

    #[tokio::test]
    async fn in_memory_chains_entries() {
        let store = InMemoryAuditStore::new();
        assert_eq!(store.latest_hash().await.unwrap(), None);

        let id1 = store.append(entry("c1")).await.unwrap();
        let tip1 = store.latest_hash().await.unwrap().unwrap();
        let id2 = store.append(entry("c2")).await.unwrap();
        let tip2 = store.latest_hash().await.unwrap().unwrap();

        assert_eq!(id1, AuditId(1));
        assert_eq!(id2, AuditId(2));
        assert_ne!(tip1, tip2, "appending must advance the chain tip");
        assert_eq!(store.len().await, 2);
    }

    #[tokio::test]
    async fn second_entry_links_to_first() {
        let store = InMemoryAuditStore::new();
        store.append(entry("c1")).await.unwrap();
        let tip1 = store.latest_hash().await.unwrap();
        // The second entry's stored hash must equal entry_hash(tip1).
        let e2 = entry("c2");
        let expected = e2.entry_hash(tip1);
        store.append(e2).await.unwrap();
        assert_eq!(store.latest_hash().await.unwrap(), Some(expected));
    }
}
