use super::*;

impl NatsConsumer {
    pub fn new(
        jetstream: async_nats::jetstream::Context,
        signing_service: Arc<ChainBridgeGrpcService>,
        metrics: Arc<dyn BlockchainMetrics>,
    ) -> Self {
        Self {
            jetstream,
            signing_service,
            metrics,
            idempotency_cache: DashMap::new(),
            dedup_store: Arc::new(DashMap::new()),
        }
    }

    pub async fn start(self) -> anyhow::Result<()> {
        info!("📥 NATS Consumer starting...");
        
        // Ensure stream exists
        let stream = self.jetstream.get_or_create_stream(async_nats::jetstream::stream::Config {
            name: "CHAIN_TX".to_string(),
            subjects: vec!["chain.tx.*".to_string()],
            ..Default::default()
        }).await?;

        // Pull consumer with optimized batching (Wall 3 mitigation)
        let consumer = stream.get_or_create_consumer("chain-bridge-worker", async_nats::jetstream::consumer::pull::Config {
            durable_name: Some("chain-bridge-worker".to_string()),
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            max_batch: 128,
            max_waiting: 512,
            ..Default::default()
        }).await?;

        // Background idempotency cleanup task (Wall 4 mitigation)
        let cleanup_cache = self.idempotency_cache.clone();
        let cleanup_dedup = self.dedup_store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                cleanup_cache.retain(|_, &mut expiry| expiry > now_ms);
                cleanup_dedup.retain(|_, rec| rec.expiry_ms > now_ms);
            }
        });

        let messages = consumer.messages().await?;
        let self_arc = Arc::new(self);

        // Concurrent message processing (Wall 3 mitigation)
        messages.for_each_concurrent(32, |result| {
            let self_clone = self_arc.clone();
            async move {
                match result {
                    Ok(msg) => {
                        match msg.subject.as_str() {
                            "chain.tx.submit" => self_clone.handle_submit(msg).await,
                            "chain.tx.simulate" => self_clone.handle_simulate(msg).await,
                            "chain.tx.cancel" => self_clone.handle_cancel(msg).await,
                            _ => {
                                warn!("Unknown NATS subject: {}", msg.subject);
                                let _ = msg.ack().await;
                            }
                        }
                    }
                    Err(e) => {
                        error!("NATS message error: {}", e);
                    }
                }
            }
        }).await;

        Ok(())
    }

    /// Effect-level dedup decision for the stable `idempotency_key`.
    ///
    /// Returns `Some(result)` when the caller must **short-circuit** — replay a
    /// prior `Done` (`success: true`, `deduplicated: true`) or reject an
    /// `InFlight` collision without submitting a second on-chain tx; the caller
    /// publishes the message, acks, and stops. Returns `None` to **proceed**
    /// with signing.
    ///
    /// Side effect on the proceed path: an absent or expired key is claimed
    /// `InFlight` (TTL 120s). A `None`/empty key is unprotected — returns `None`
    /// and leaves the store untouched. Pure w.r.t. NATS IO so it is unit-tested
    /// directly against a bare store (`handle_submit` can't be — its
    /// `async_nats::Message` has no test constructor).
    pub(crate) fn claim_or_replay(
        dedup_store: &DashMap<String, DedupRecord>,
        dedup_key: Option<&str>,
        correlation_id: &str,
        now_ms: u64,
    ) -> Option<TxResultMessage> {
        let key = dedup_key?;
        use dashmap::mapref::entry::Entry;
        match dedup_store.entry(key.to_string()) {
            // Existing, unexpired record: replay instead of submitting again.
            Entry::Occupied(e) if e.get().expiry_ms > now_ms => {
                let rec = e.get().clone();
                let result_msg = match rec.state {
                    DedupState::Done { signature, slot } => {
                        info!("♻️ Dedup hit (done) for idempotency_key {}: replaying prior result", key);
                        TxResultMessage {
                            correlation_id: correlation_id.to_string(),
                            success: true,
                            signature,
                            error: None,
                            slot,
                            deduplicated: true,
                        }
                    }
                    DedupState::InFlight => {
                        // A concurrent attempt is mid-submit. Do NOT submit a
                        // second on-chain tx; tell the caller it is in flight.
                        warn!("♻️ Dedup hit (in-flight) for idempotency_key {}: concurrent submit in progress", key);
                        TxResultMessage {
                            correlation_id: correlation_id.to_string(),
                            success: false,
                            signature: None,
                            error: Some("Submit already in flight for this idempotency_key".to_string()),
                            slot: 0,
                            deduplicated: true,
                        }
                    }
                };
                Some(result_msg)
            }
            // No record (or an expired one): claim it InFlight and proceed.
            entry => {
                entry.insert(DedupRecord {
                    expiry_ms: now_ms + 120_000,
                    state: DedupState::InFlight,
                });
                None
            }
        }
    }

    async fn handle_submit(&self, msg: async_nats::jetstream::Message) {
        let start_time = std::time::Instant::now();
        let envelope: TxSubmitMessage = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                warn!("Invalid payload on chain.tx.submit: {}", e);
                let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                return;
            }
        };

        // 1. Unified RBAC check
        let role = ServiceRole::from(&SpiffeIdentity(envelope.service_identity.clone()));
        if role == ServiceRole::Unknown {
            warn!("🚨 Unauthorised NATS service identity: {}", envelope.service_identity);
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // 2. Idempotency Check
        if self.idempotency_cache.contains_key(&envelope.correlation_id) {
            warn!("Duplicate transaction detected: {}", envelope.correlation_id);
            let _ = msg.ack().await;
            return;
        }

        // 3. Staleness check (55s)
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        
        let age_ms = now_ms.saturating_sub(envelope.created_at_ms);
        if age_ms > 55_000 {
            warn!("Rejecting stale tx {}: age {}ms", envelope.correlation_id, age_ms);
            self.metrics.track_operation("nats_submit_stale_rejected", 0.0, false);
            self.publish_result(&envelope.reply_subject, TxResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                signature: None,
                error: Some("Stale transaction — blockhash may be expired".to_string()),
                slot: 0,
                deduplicated: false,
            }).await;
            msg.ack().await.ok();
            return;
        }

        // 4. Non-transient upfront checks (Malforming, Authorization & Policy validation)
        // Rejecting early prevents entering the retry loop for static errors.
        let transaction: Transaction = match bincode::deserialize(&envelope.serialized_tx) {
            Ok(tx) => tx,
            Err(e) => {
                warn!("Invalid transaction format in NATS envelope for {}: {}", envelope.correlation_id, e);
                self.publish_result(&envelope.reply_subject, TxResultMessage {
                    correlation_id: envelope.correlation_id,
                    success: false,
                    signature: None,
                    error: Some(format!("Invalid transaction format: {}", e)),
                    slot: 0,
                    deduplicated: false,
                }).await;
                let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                return;
            }
        };

        if let Err(e) = gridtokenx_blockchain_core::policy::PolicyEngine::validate_transaction(&SpiffeIdentity(envelope.service_identity.clone()), &transaction) {
            warn!("Policy Engine rejection in NATS submit for {}: {}", envelope.correlation_id, e);
            self.publish_result(&envelope.reply_subject, TxResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                signature: None,
                error: Some(format!("Policy Engine rejection: {}", e)),
                slot: 0,
                deduplicated: false,
            }).await;
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        if envelope.key_id != "platform_admin" && !envelope.key_id.is_empty() {
            warn!("Key ID not authorized in NATS submit for {}: {}", envelope.correlation_id, envelope.key_id);
            self.publish_result(&envelope.reply_subject, TxResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                signature: None,
                error: Some(format!("Key ID not authorized: {}", envelope.key_id)),
                slot: 0,
                deduplicated: false,
            }).await;
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // 4.5 Effect-level dedup on the stable idempotency_key. Empty key =
        // legacy/unprotected caller -> skip (submit normally, no dedup).
        let dedup_key = (!envelope.idempotency_key.is_empty()).then(|| envelope.idempotency_key.clone());
        if let Some(result_msg) =
            Self::claim_or_replay(&self.dedup_store, dedup_key.as_deref(), &envelope.correlation_id, now_ms)
        {
            self.metrics.track_operation("nats_submit_deduplicated", 0.0, result_msg.success);
            self.publish_result(&envelope.reply_subject, result_msg).await;
            let _ = msg.ack().await;
            return;
        }

        // 5. Sign and submit with retry policy for transient errors only
        // We retry only for transient Solana RPC errors (e.g. rate limit, node behind)
        let retry_strategy = FixedInterval::from_millis(500).take(3);
        
        // Use a wrapper to avoid borrowing 'self' into the Retry block improperly if needed, 
        // but since we are in handle_submit which takes &self, we are fine as long as sign_and_submit is async.
        let result: anyhow::Result<(Signature, u64)> = Retry::spawn(retry_strategy, || async {
            info!("🔄 Retrying sign_and_submit for {}", envelope.correlation_id);
            self.signing_service.sign_and_submit(&envelope.serialized_tx, &envelope.key_id, &SpiffeIdentity(envelope.service_identity.clone()), &envelope.correlation_id).await
        }).await;

        let result_msg = match result {
            Ok((sig, slot)) => {
                // Cache successful correlation_id for 60 seconds
                self.idempotency_cache.insert(envelope.correlation_id.clone(), now_ms + 60_000);

                // Record the effect as Done so re-sends with this key replay it
                // instead of submitting a second on-chain transaction.
                if let Some(key) = &dedup_key {
                    self.dedup_store.insert(key.clone(), DedupRecord {
                        expiry_ms: now_ms + 120_000,
                        state: DedupState::Done {
                            signature: Some(sig.to_string()),
                            slot,
                        },
                    });
                }

                TxResultMessage {
                    correlation_id: envelope.correlation_id,
                    success: true,
                    signature: Some(sig.to_string()),
                    error: None,
                    slot,
                    deduplicated: false,
                }
            },
            Err(e) => {
                // Failure is not a completed effect — release the InFlight claim
                // so a genuine retry with the same key can attempt again.
                if let Some(key) = &dedup_key {
                    self.dedup_store.remove(key);
                }
                TxResultMessage {
                    correlation_id: envelope.correlation_id,
                    success: false,
                    signature: None,
                    error: Some(format!("{:#}", e)),
                    slot: 0,
                    deduplicated: false,
                }
            },
        };

        self.metrics.track_operation("nats_consumer_submit", start_time.elapsed().as_millis() as f64, result_msg.success);
        self.publish_result(&envelope.reply_subject, result_msg).await;
        let _ = msg.ack().await;
    }

    async fn handle_simulate(&self, msg: async_nats::jetstream::Message) {
        let envelope: TxSimulateMessage = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                warn!("Invalid payload on chain.tx.simulate: {}", e);
                let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                return;
            }
        };

        // Identity Check
        let role = ServiceRole::from(&SpiffeIdentity(envelope.service_identity.clone()));
        if role == ServiceRole::Unknown {
            warn!("🚨 Unauthorised NATS simulate identity: {}", envelope.service_identity);
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // Real simulation
        let tx: Result<Transaction, _> = bincode::deserialize(&envelope.serialized_tx);
        let result_msg = match tx {
            Ok(t) => {
                match self.signing_service.provider().simulate_transaction(&t).await {
                    Ok(resp) => TxSimulateResultMessage {
                        correlation_id: envelope.correlation_id,
                        success: resp.value.err.is_none(),
                        compute_units_consumed: resp.value.units_consumed.unwrap_or(0),
                        error_message: resp.value.err.map(|e| format!("{:?}", e)).unwrap_or_default(),
                        logs: resp.value.logs.unwrap_or_default(),
                    },
                    Err(e) => TxSimulateResultMessage {
                        correlation_id: envelope.correlation_id,
                        success: false,
                        compute_units_consumed: 0,
                        error_message: e.to_string(),
                        logs: vec![],
                    },
                }
            }
            Err(e) => TxSimulateResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                compute_units_consumed: 0,
                error_message: format!("Deserialization error: {}", e),
                logs: vec![],
            },
        };

        let payload = serde_json::to_vec(&result_msg).unwrap();
        self.jetstream.publish(envelope.reply_subject, payload.into()).await.ok();
        let _ = msg.ack().await;
    }

    async fn handle_cancel(&self, msg: async_nats::jetstream::Message) {
        let start_time = std::time::Instant::now();
        let envelope: TxCancelMessage = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                warn!("Invalid payload on chain.tx.cancel: {}", e);
                let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                return;
            }
        };

        // 1. RBAC Check
        let role = ServiceRole::from(&SpiffeIdentity(envelope.service_identity.clone()));
        if role == ServiceRole::Unknown {
            warn!("🚨 Unauthorised NATS cancel identity: {}", envelope.service_identity);
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // 2. Idempotency Check (Has this cancellation already been processed?)
        if self.idempotency_cache.contains_key(&format!("cancel:{}", envelope.correlation_id)) {
            warn!("Duplicate cancellation detected: {}", envelope.correlation_id);
            let _ = msg.ack().await;
            return;
        }

        // 3. Staleness Check (55s)
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        
        if now_ms.saturating_sub(envelope.created_at_ms) > 55_000 {
            warn!("Rejecting stale cancellation: {}", envelope.correlation_id);
            self.publish_cancel_result(&envelope.reply_subject, TxCancelResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                error: Some("Cancellation request stale".to_string()),
            }).await;
            let _ = msg.ack().await;
            return;
        }

        // 4. Real Work: Cancellation Logic
        // In this bridge, cancellation likely means marking the correlation_id as cancelled 
        // to prevent subsequent signing, or just acknowledging the intent.
        info!("🚫 Processing cancellation for correlation_id: {}", envelope.correlation_id);
        
        // Mark as cancelled in idempotency cache to prevent future processing
        self.idempotency_cache.insert(format!("cancel:{}", envelope.correlation_id), now_ms + 60_000);
        self.idempotency_cache.insert(envelope.correlation_id.clone(), now_ms + 60_000); // Also block the original if it arrives late

        let result_msg = TxCancelResultMessage {
            correlation_id: envelope.correlation_id,
            success: true,
            error: None,
        };

        self.metrics.track_operation("nats_consumer_cancel", start_time.elapsed().as_millis() as f64, true);
        self.publish_cancel_result(&envelope.reply_subject, result_msg).await;
        let _ = msg.ack().await;
    }

    async fn publish_result(&self, subject: &str, result: TxResultMessage) {
        let payload = serde_json::to_vec(&result).unwrap();
        if let Err(e) = self.jetstream.publish(subject.to_string(), payload.into()).await {
            error!("Failed to publish result to {}: {}", subject, e);
        }
    }

    async fn publish_cancel_result(&self, subject: &str, result: TxCancelResultMessage) {
        let payload = serde_json::to_vec(&result).unwrap();
        if let Err(e) = self.jetstream.publish(subject.to_string(), payload.into()).await {
            error!("Failed to publish cancel result to {}: {}", subject, e);
        }
    }

    /// Expose internals for testing
    #[cfg(test)]
    pub fn idempotency_cache(&self) -> &DashMap<String, u64> {
        &self.idempotency_cache
    }
}
