//! Audit log domain types with tamper-evident hash-chaining (Gap #2).
//!
//! Every mediated effect (allow/deny/submit) appends an [`AuditEntry`]. Each
//! entry's [`AuditHash`] is `SHA-256(prev_hash || canonical_fields)`, so any
//! retroactive edit breaks every subsequent link. The persistence adapter
//! stores entries; `audit_anchor` in the logic layer periodically commits the
//! tip hash (a Merkle root) to Solana for external verifiability.
//!
//! Hashing uses `solana_sdk::hash` (SHA-256) to avoid pulling a second crypto
//! dependency into `core`.

use serde::{Deserialize, Serialize};
use solana_sdk::hash::hashv;

/// Database row id of a persisted audit entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditId(pub i64);

/// 32-byte SHA-256 link in the audit chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditHash(pub [u8; 32]);

impl AuditHash {
    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }
}

/// What happened to a mediated transaction at the audit point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditOutcome {
    /// Passed policy + simulation and was submitted.
    Submitted { signature: String, slot: u64 },
    /// Rejected before signing. `stage` is `policy` | `simulation` | `auth` |
    /// `rbac` | `stale`. For `auth`/`rbac` rejections on the NATS path the
    /// entry's `identity` is the envelope's *claimed* (unverified)
    /// `service_identity`.
    Rejected { stage: String, reason: String },
}

/// One link in the hash-chained audit trail. `prev_hash` is filled by the
/// adapter at append time from the current chain tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub prev_hash: Option<AuditHash>,
    pub correlation_id: String,
    pub identity: String,
    pub action: String,
    pub outcome: AuditOutcome,
    pub created_at_ms: u64,
}

impl AuditEntry {
    /// Build an entry without `prev_hash`; the adapter links it on append.
    pub fn new(
        correlation_id: impl Into<String>,
        identity: impl Into<String>,
        action: impl Into<String>,
        outcome: AuditOutcome,
        created_at_ms: u64,
    ) -> Self {
        Self {
            prev_hash: None,
            correlation_id: correlation_id.into(),
            identity: identity.into(),
            action: action.into(),
            outcome,
            created_at_ms,
        }
    }

    /// Deterministic hash of this entry given the chain tip it links to.
    /// `SHA-256(prev_hash || correlation_id || identity || action ||
    /// outcome_json || created_at_ms)`. Pure and side-effect free.
    pub fn entry_hash(&self, prev: Option<AuditHash>) -> AuditHash {
        let prev_bytes = prev.map(|h| h.0).unwrap_or([0u8; 32]);
        let outcome_json = serde_json::to_string(&self.outcome).unwrap_or_default();
        let digest = hashv(&[
            &prev_bytes,
            self.correlation_id.as_bytes(),
            self.identity.as_bytes(),
            self.action.as_bytes(),
            outcome_json.as_bytes(),
            &self.created_at_ms.to_le_bytes(),
        ]);
        AuditHash(digest.to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> AuditEntry {
        AuditEntry::new(
            "corr-1",
            "spiffe://gridtokenx.th/prod/trading-service/matcher",
            "submit",
            AuditOutcome::Submitted {
                signature: "sig".into(),
                slot: 7,
            },
            1_700_000_000_000,
        )
    }

    #[test]
    fn hash_is_deterministic() {
        let e = entry();
        assert_eq!(e.entry_hash(None), e.entry_hash(None));
    }

    #[test]
    fn prev_hash_changes_digest() {
        let e = entry();
        let h0 = e.entry_hash(None);
        let h1 = e.entry_hash(Some(AuditHash([1u8; 32])));
        assert_ne!(h0, h1, "linking to a different tip must change the hash");
    }

    #[test]
    fn field_change_changes_digest() {
        let a = entry();
        let mut b = entry();
        b.correlation_id = "corr-2".into();
        assert_ne!(a.entry_hash(None), b.entry_hash(None));
    }

    #[test]
    fn hex_is_64_chars() {
        assert_eq!(AuditHash([0xab; 32]).to_hex().len(), 64);
    }
}
