//! Effect-level deduplication types for the NATS submit/cancel consumer.

/// Effect-level dedup record keyed by `idempotency_key`. Distinct from
/// `idempotency_cache` (which guards per-`correlation_id` JetStream redelivery
/// and cancel bookkeeping). A successful submit is recorded `Done` and replayed
/// to later re-sends; a failure releases the claim so a genuine retry can run.
#[derive(Clone)]
pub(crate) struct DedupRecord {
    pub(crate) expiry_ms: u64,
    pub(crate) state: DedupState,
}

#[derive(Clone)]
pub(crate) enum DedupState {
    /// Claimed by an attempt that is currently signing+submitting.
    InFlight,
    /// Completed successfully — the on-chain effect happened exactly once. The
    /// stored result is replayed to any later re-send with the same key.
    Done {
        signature: Option<String>,
        slot: u64,
    },
}
