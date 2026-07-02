use super::*;

impl NatsConsumer {
    pub fn new(
        jetstream: async_nats::jetstream::Context,
        signing_service: Arc<ChainBridgeGrpcService>,
        metrics: Arc<dyn BlockchainMetrics>,
        auth_policy: NatsAuthPolicy,
    ) -> Self {
        Self {
            jetstream,
            signing_service,
            metrics,
            idempotency_cache: DashMap::new(),
            dedup_store: Arc::new(DashMap::new()),
            auth_policy,
        }
    }

    /// Verify one envelope's [`EnvelopeAuth`] and decide whether to proceed.
    ///
    /// `Err(reason)` ⇒ reject. Rejection happens when the global signing policy
    /// (`require_signed`) is on **or** the caller passes `force_signed = true`.
    /// `force_signed` exists for value-moving subjects (e.g. `chain.tx.mint`)
    /// that must never run on a self-asserted identity, regardless of the
    /// log-only global default — an unsigned mint would let any publisher forge
    /// tokens to any wallet. Note: a forced-signed subject with no CA configured
    /// fails closed (no CA ⇒ nothing can be verified ⇒ reject), which is the
    /// correct posture for a mint.
    /// All three outcomes are logged/metered so log-only mode still surfaces
    /// unsigned publishers (`nats_auth_unsigned`) and broken signatures
    /// (`nats_auth_failed`) before enforcement is flipped on.
    fn evaluate_envelope_auth(
        &self,
        auth: Option<&gridtokenx_blockchain_core::rpc::nats_schema::EnvelopeAuth>,
        claimed_identity: &str,
        canonical: &[u8],
        correlation_id: &str,
        force_signed: bool,
    ) -> Result<(), String> {
        let require_signed = self.auth_policy.require_signed || force_signed;
        let now_secs = gridtokenx_telemetry::time::now().timestamp();
        let check = super::auth::check_envelope_auth(
            self.auth_policy.ca_der.as_deref(),
            auth,
            claimed_identity,
            canonical,
            now_secs,
        );
        match &check {
            AuthCheck::Verified => {
                self.metrics.track_operation("nats_auth_verified", 0.0, true);
            }
            AuthCheck::Unsigned => {
                warn!(
                    "⚠️ Unsigned NATS envelope from '{}' ({}) — {}",
                    claimed_identity,
                    correlation_id,
                    if require_signed {
                        "rejecting (signing required)"
                    } else {
                        "accepted without signing enforcement"
                    },
                );
                self.metrics.track_operation("nats_auth_unsigned", 0.0, true);
            }
            AuthCheck::Failed(reason) => {
                error!(
                    "🚨 NATS envelope auth FAILED for '{}' ({}): {}",
                    claimed_identity, correlation_id, reason
                );
                self.metrics.track_operation("nats_auth_failed", 0.0, false);
            }
        }
        super::auth::auth_decision(&check, require_signed)
    }

    pub async fn start(self) -> anyhow::Result<()> {
        info!("📥 NATS Consumer starting...");
        
        // Ensure stream exists (idempotent; each run_consumer re-fetches its own handle).
        self.jetstream.get_or_create_stream(async_nats::jetstream::stream::Config {
            name: "CHAIN_TX".to_string(),
            subjects: vec!["chain.tx.*".to_string()],
            ..Default::default()
        }).await?;

        // Background idempotency cleanup task (Wall 4 mitigation)
        let cleanup_cache = self.idempotency_cache.clone();
        let cleanup_dedup = self.dedup_store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let now_ms = gridtokenx_telemetry::time::now().timestamp_millis() as u64;
                cleanup_cache.retain(|_, &mut expiry| expiry > now_ms);
                cleanup_dedup.retain(|_, rec| rec.expiry_ms > now_ms);
            }
        });

        let self_arc = Arc::new(self);

        // F1 (HOL blocking): TWO independent pull consumers on the CHAIN_TX stream, with
        // DISJOINT subject filters, so the slow mint confirm-poll (handle_mint polls on-chain
        // finality, seconds) can never occupy the concurrency slots that serve the fast
        // submit/simulate/cancel/status path. Each consumer has its own durable name (→ its
        // own server-side cursor), ack_wait, and concurrency budget. Same durable names across
        // replicas still form pull queue-groups, so horizontal scaling is preserved.
        // F2: the mint consumer's ack_wait (90s) exceeds the worst-case confirmation window so
        // a single delivery completes within one lease — no staleness×redelivery thrash.
        let fast = Self::run_consumer(
            self_arc.clone(),
            "chain-bridge-worker",
            vec![
                "chain.tx.submit".to_string(),
                "chain.tx.simulate".to_string(),
                "chain.tx.cancel".to_string(),
                "chain.tx.status".to_string(),
            ],
            std::time::Duration::from_secs(30),
            32,
        );
        let mint = Self::run_consumer(
            self_arc.clone(),
            "chain-bridge-mint-worker",
            vec!["chain.tx.mint".to_string()],
            std::time::Duration::from_secs(90),
            8,
        );

        // Both loop forever; an unrecoverable error in EITHER propagates so the main()
        // supervisor reconnects NATS from scratch.
        tokio::try_join!(fast, mint)?;
        Ok(())
    }

    /// One pull consumer's resilience loop: (re)open the messages stream and dispatch each
    /// message by subject. `filter_subjects` scopes which subjects this consumer pulls — the
    /// fast and mint consumers use DISJOINT filters so no message is processed twice. Returns
    /// `Err` only when the messages stream can't be reopened after repeated attempts (bubbled
    /// to the main supervisor, which reconnects NATS).
    async fn run_consumer(
        self_arc: Arc<Self>,
        durable: &'static str,
        filter_subjects: Vec<String>,
        ack_wait: std::time::Duration,
        concurrency: usize,
    ) -> anyhow::Result<()> {
        // Re-fetch the (already-created) stream handle for this consumer's task.
        let stream = self_arc
            .jetstream
            .get_or_create_stream(async_nats::jetstream::stream::Config {
                name: "CHAIN_TX".to_string(),
                subjects: vec!["chain.tx.*".to_string()],
                ..Default::default()
            })
            .await?;

        // Pull consumer with optimized batching (Wall 3 mitigation). Explicit idle_heartbeat +
        // ack_wait so a jittery CB<->NATS link recovers instead of choking with repeated
        // "missed idle heartbeat".
        let consumer = stream
            .get_or_create_consumer(durable, async_nats::jetstream::consumer::pull::Config {
                durable_name: Some(durable.to_string()),
                ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                ack_wait,
                max_batch: 128,
                max_waiting: 512,
                filter_subjects,
                ..Default::default()
            })
            .await?;
        info!("📥 NATS consumer '{durable}' ready (ack_wait={ack_wait:?}, concurrency={concurrency})");

        let mut backoff_ms = 100u64;
        let mut create_failures = 0u32;

        // Resilience loop. A missed-heartbeat or transient error can *end* the `messages()`
        // stream; without re-creating it the consumer task would exit for good and messages
        // would pile up unprocessed (the dead-consumer bug). On stream end we re-open the pull
        // stream and keep going.
        loop {
            // Per-pull batch MUST stay <= the consumer's `max_batch` (128) or the server
            // rejects every pull (409 ExceededMaxRequestBatch) — the default
            // `consumer.messages()` asks for 200 and silently starves.
            let messages = match consumer
                .stream()
                .max_messages_per_batch(100)
                .expires(std::time::Duration::from_secs(30))
                .heartbeat(std::time::Duration::from_secs(10))
                .messages()
                .await
            {
                Ok(m) => {
                    create_failures = 0;
                    backoff_ms = 100;
                    m
                }
                Err(e) => {
                    create_failures += 1;
                    if create_failures >= 5 {
                        return Err(anyhow::anyhow!(
                            "NATS messages stream for '{durable}' unrecoverable after {create_failures} attempts: {e}"
                        ));
                    }
                    error!("Failed to open NATS messages stream for '{durable}' (attempt {create_failures}): {e}; retrying in {backoff_ms}ms");
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(5_000);
                    continue;
                }
            };

            // Concurrent message processing (Wall 3 mitigation). The match handles every
            // subject; each consumer only RECEIVES its filtered subset, so the extra arms are
            // inert (defense-in-depth if a filter ever widens).
            messages.for_each_concurrent(concurrency, |result| {
                let self_clone = self_arc.clone();
                async move {
                    match result {
                        Ok(msg) => {
                            match msg.subject.as_str() {
                                "chain.tx.submit" => self_clone.handle_submit(msg).await,
                                "chain.tx.cancel" => self_clone.handle_cancel(msg).await,
                                "chain.tx.mint" => self_clone.handle_mint(msg).await,
                                "chain.tx.status" => self_clone.handle_status(msg).await,
                                _ => {
                                    warn!("Unknown NATS subject: {}", msg.subject);
                                    let _ = msg.ack().await;
                                }
                            }
                        }
                        Err(e) => {
                            error!("NATS message error on '{durable}': {}", e);
                        }
                    }
                }
            }).await;

            warn!("NATS messages stream '{durable}' ended; recreating consumer pull stream");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// Append a `Rejected` entry to the tamper-evident audit chain for a
    /// pre-pipeline NATS rejection (envelope auth, RBAC, staleness, policy,
    /// key authorization). `claimed_identity` is the envelope's self-asserted
    /// `service_identity` — for stage `auth`/`rbac` it is by definition
    /// unverified. Best-effort like every audit append; a failed append is
    /// logged inside `record_audit` and never blocks the rejection itself.
    async fn audit_rejection(
        &self,
        correlation_id: &str,
        claimed_identity: &str,
        action: &str,
        stage: &str,
        reason: String,
    ) {
        self.signing_service
            .record_audit(
                correlation_id,
                &SpiffeIdentity(claimed_identity.to_string()),
                action,
                AuditOutcome::Rejected { stage: stage.to_string(), reason },
            )
            .await;
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

        // 0. Envelope authentication — must precede RBAC (which consumes the
        // self-asserted service_identity) and any dedup claim (an unverified
        // message must never claim a dedup slot).
        let canonical =
            gridtokenx_blockchain_core::rpc::envelope_auth::canonical_submit_bytes(&envelope);
        if let Err(reason) = self.evaluate_envelope_auth(
            envelope.auth.as_ref(),
            &envelope.service_identity,
            &canonical,
            &envelope.correlation_id,
            false,
        ) {
            self.metrics.track_operation("nats_auth_rejected", 0.0, false);
            let reason = format!("envelope authentication failed: {}", reason);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "submit", "auth", reason.clone()).await;
            self.publish_result(&envelope.reply_subject, TxResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                signature: None,
                error: Some(reason),
                slot: 0,
                deduplicated: false,
            }).await;
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // 1. Unified RBAC check
        let role = ServiceRole::from(&SpiffeIdentity(envelope.service_identity.clone()));
        if role == ServiceRole::Unknown {
            warn!("🚨 Unauthorised NATS service identity: {}", envelope.service_identity);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "submit", "rbac", "unknown service role".to_string()).await;
            // Reply with an explicit error — a silent Term ack leaves the
            // publisher blocked until its submit_transaction timeout.
            self.publish_result(&envelope.reply_subject, TxResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                signature: None,
                error: Some(format!("Unauthorised service identity: {}", envelope.service_identity)),
                slot: 0,
                deduplicated: false,
            }).await;
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
        let now_ms = gridtokenx_telemetry::time::now().timestamp_millis() as u64;
        
        let age_ms = now_ms.saturating_sub(envelope.created_at_ms);
        if age_ms > 55_000 {
            warn!("Rejecting stale tx {}: age {}ms", envelope.correlation_id, age_ms);
            self.metrics.track_operation("nats_submit_stale_rejected", 0.0, false);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "submit", "stale", format!("stale envelope: age {}ms", age_ms)).await;
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
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "submit", "policy", e.to_string()).await;
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
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "submit", "auth", format!("Key ID not authorized: {}", envelope.key_id)).await;
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
        // Namespace by effect type: the dedup_store is shared with handle_mint,
        // so an un-prefixed key would let a submit's Done record replay as a mint
        // success (and vice versa). See F4.
        let dedup_key = (!envelope.idempotency_key.is_empty())
            .then(|| format!("submit:{}", envelope.idempotency_key));
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

        // 0. Envelope authentication (see handle_submit).
        let canonical =
            gridtokenx_blockchain_core::rpc::envelope_auth::canonical_cancel_bytes(&envelope);
        if let Err(reason) = self.evaluate_envelope_auth(
            envelope.auth.as_ref(),
            &envelope.service_identity,
            &canonical,
            &envelope.correlation_id,
            false,
        ) {
            self.metrics.track_operation("nats_auth_rejected", 0.0, false);
            let reason = format!("envelope authentication failed: {}", reason);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "cancel", "auth", reason.clone()).await;
            self.publish_cancel_result(&envelope.reply_subject, TxCancelResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                error: Some(reason),
            }).await;
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // 1. RBAC Check
        let role = ServiceRole::from(&SpiffeIdentity(envelope.service_identity.clone()));
        if role == ServiceRole::Unknown {
            warn!("🚨 Unauthorised NATS cancel identity: {}", envelope.service_identity);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "cancel", "rbac", "unknown service role".to_string()).await;
            // Reply with an explicit error — see handle_submit's RBAC branch.
            self.publish_cancel_result(&envelope.reply_subject, TxCancelResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                error: Some(format!("Unauthorised service identity: {}", envelope.service_identity)),
            }).await;
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
        let now_ms = gridtokenx_telemetry::time::now().timestamp_millis() as u64;
        
        if now_ms.saturating_sub(envelope.created_at_ms) > 55_000 {
            warn!("Rejecting stale cancellation: {}", envelope.correlation_id);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "cancel", "stale", "stale cancellation request".to_string()).await;
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

    /// Handle `chain.tx.mint` — a semantic energy-token generation-mint intent.
    /// Unlike `handle_submit` (caller built the tx), the bridge BUILDS the mint
    /// transaction here, then signs (Vault `platform_admin`) and submits. Mirrors
    /// the submit pipeline: envelope auth → RBAC → idempotency → staleness →
    /// effect dedup → build/sign/submit.
    async fn handle_mint(&self, msg: async_nats::jetstream::Message) {
        let start_time = std::time::Instant::now();
        let envelope: MintEnergyMessage = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                warn!("Invalid payload on chain.tx.mint: {}", e);
                let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                return;
            }
        };

        // 0. Envelope authentication — precedes RBAC and any dedup claim.
        let canonical =
            gridtokenx_blockchain_core::rpc::envelope_auth::canonical_mint_bytes(&envelope);
        // force_signed = true: minting is value-moving and must never run on a
        // self-asserted identity, even when the global NATS signing policy is in
        // log-only mode. Unsigned/forged mint = tokens to an attacker wallet.
        if let Err(reason) = self.evaluate_envelope_auth(
            envelope.auth.as_ref(),
            &envelope.service_identity,
            &canonical,
            &envelope.correlation_id,
            true,
        ) {
            self.metrics.track_operation("nats_auth_rejected", 0.0, false);
            let reason = format!("envelope authentication failed: {}", reason);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "mint", "auth", reason.clone()).await;
            self.publish_mint_result(&envelope.reply_subject, MintEnergyResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                signature: None,
                error: Some(reason),
                slot: 0,
                deduplicated: false,
            }).await;
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // 1. RBAC — the generation-mint path is owned by the aggregator-bridge
        // (surplus window flush publishes on chain.tx.mint); meter-service retains
        // the legacy bridge-built mint. Both are gated again by the PolicyEngine
        // inside sign_and_submit (program ids — see policy.rs AggregatorBridge arm).
        let identity = SpiffeIdentity(envelope.service_identity.clone());
        let role = ServiceRole::from(&identity);
        if role
            .require_any(&[ServiceRole::AggregatorBridge, ServiceRole::MeterService])
            .is_err()
        {
            warn!("🚨 Unauthorised mint request from identity: {}", envelope.service_identity);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "mint", "rbac", "role not permitted to mint".to_string()).await;
            self.publish_mint_result(&envelope.reply_subject, MintEnergyResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                signature: None,
                error: Some(format!("Unauthorised service identity for mint: {}", envelope.service_identity)),
                slot: 0,
                deduplicated: false,
            }).await;
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // 2. Idempotency check (per-attempt correlation_id).
        if self.idempotency_cache.contains_key(&envelope.correlation_id) {
            warn!("Duplicate mint detected: {}", envelope.correlation_id);
            let _ = msg.ack().await;
            return;
        }

        // 3. Staleness check (55s).
        let now_ms = gridtokenx_telemetry::time::now().timestamp_millis() as u64;
        let age_ms = now_ms.saturating_sub(envelope.created_at_ms);
        if age_ms > 55_000 {
            warn!("Rejecting stale mint {}: age {}ms", envelope.correlation_id, age_ms);
            self.metrics.track_operation("nats_mint_stale_rejected", 0.0, false);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "mint", "stale", format!("stale envelope: age {}ms", age_ms)).await;
            self.publish_mint_result(&envelope.reply_subject, MintEnergyResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                signature: None,
                error: Some("Stale mint request".to_string()),
                slot: 0,
                deduplicated: false,
            }).await;
            msg.ack().await.ok();
            return;
        }

        // 4. Effect-level dedup on the stable idempotency_key (empty = no dedup).
        // Namespace by effect type so a mint key can never collide with a submit
        // key in the shared dedup_store (would otherwise replay a submit Done as a
        // forged mint success). See F4.
        let dedup_key = (!envelope.idempotency_key.is_empty())
            .then(|| format!("mint:{}", envelope.idempotency_key));
        if let Some(prior) =
            Self::claim_or_replay(&self.dedup_store, dedup_key.as_deref(), &envelope.correlation_id, now_ms)
        {
            self.metrics.track_operation("nats_mint_deduplicated", 0.0, prior.success);
            self.publish_mint_result(&envelope.reply_subject, MintEnergyResultMessage {
                correlation_id: prior.correlation_id,
                success: prior.success,
                signature: prior.signature,
                error: prior.error,
                slot: prior.slot,
                deduplicated: true,
            }).await;
            let _ = msg.ack().await;
            return;
        }

        // 5. Build, sign (Vault platform_admin), and submit the generation mint.
        let result = self
            .signing_service
            .build_and_submit_generation_mint(
                &envelope.recipient_wallet,
                envelope.energy_kwh,
                &envelope.meter_id,
                envelope.window_start_ms,
                &identity,
                &envelope.correlation_id,
            )
            .await;

        let result_msg = match result {
            Ok((sig, slot)) => {
                self.idempotency_cache.insert(envelope.correlation_id.clone(), now_ms + 60_000);
                // Record the effect Done so a re-send with this key replays it
                // instead of minting a second time (on-chain PDA is the backstop).
                if let Some(key) = &dedup_key {
                    self.dedup_store.insert(key.clone(), DedupRecord {
                        expiry_ms: now_ms + 120_000,
                        state: DedupState::Done { signature: Some(sig.to_string()), slot },
                    });
                }
                MintEnergyResultMessage {
                    correlation_id: envelope.correlation_id,
                    success: true,
                    signature: Some(sig.to_string()),
                    error: None,
                    slot,
                    deduplicated: false,
                }
            }
            Err(e) => {
                // Failure is not a completed effect — release the InFlight claim.
                if let Some(key) = &dedup_key {
                    self.dedup_store.remove(key);
                }
                MintEnergyResultMessage {
                    correlation_id: envelope.correlation_id,
                    success: false,
                    signature: None,
                    error: Some(format!("{:#}", e)),
                    slot: 0,
                    deduplicated: false,
                }
            }
        };

        self.metrics.track_operation("nats_consumer_mint", start_time.elapsed().as_millis() as f64, result_msg.success);
        self.publish_mint_result(&envelope.reply_subject, result_msg).await;
        let _ = msg.ack().await;
    }

    async fn publish_mint_result(&self, subject: &str, result: MintEnergyResultMessage) {
        let payload = match serde_json::to_vec(&result) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to serialize mint result for {}: {}", subject, e);
                return;
            }
        };
        if let Err(e) = self.jetstream.publish(subject.to_string(), payload.into()).await {
            error!("Failed to publish mint result to {}: {}", subject, e);
        }
    }

    /// Read-only on-chain confirmation-status query (`chain.tx.status`). Same
    /// envelope-auth + RBAC gates as the write paths, but it never signs or
    /// submits — it just forwards the signature to a `getSignatureStatuses` read
    /// and replies with the commitment level. Used by meter-service's confirmer
    /// to advance `submitted` readings to finalized.
    async fn handle_status(&self, msg: async_nats::jetstream::Message) {
        let envelope: TxStatusMessage = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                warn!("Invalid payload on chain.tx.status: {}", e);
                let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                return;
            }
        };

        // Envelope authentication (parity with the write paths; dev accepts unsigned).
        let canonical =
            gridtokenx_blockchain_core::rpc::envelope_auth::canonical_status_bytes(&envelope);
        // Read-only status query — stays on the global signing policy.
        if let Err(reason) = self.evaluate_envelope_auth(
            envelope.auth.as_ref(),
            &envelope.service_identity,
            &canonical,
            &envelope.correlation_id,
            false,
        ) {
            self.metrics.track_operation("nats_auth_rejected", 0.0, false);
            let reason = format!("envelope authentication failed: {}", reason);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "status", "auth", reason.clone()).await;
            self.publish_status_result(&envelope.reply_subject, TxStatusResultMessage {
                correlation_id: envelope.correlation_id,
                found: false,
                confirmed: false,
                status: "Error".to_string(),
                slot: 0,
                error: Some(reason),
            }).await;
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // RBAC — only meter-service (the confirmer) or Admin may query status.
        let identity = SpiffeIdentity(envelope.service_identity.clone());
        let role = ServiceRole::from(&identity);
        if role.require_any(&[ServiceRole::MeterService, ServiceRole::Admin]).is_err() {
            warn!("🚨 Unauthorised status query from identity: {}", envelope.service_identity);
            self.audit_rejection(&envelope.correlation_id, &envelope.service_identity, "status", "rbac", "role not permitted to query status".to_string()).await;
            self.publish_status_result(&envelope.reply_subject, TxStatusResultMessage {
                correlation_id: envelope.correlation_id,
                found: false,
                confirmed: false,
                status: "Error".to_string(),
                slot: 0,
                error: Some(format!("Unauthorised service identity for status: {}", envelope.service_identity)),
            }).await;
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        let result = match self.signing_service.signature_status(&envelope.signature).await {
            Ok((found, confirmed, status, slot)) => TxStatusResultMessage {
                correlation_id: envelope.correlation_id,
                found,
                confirmed,
                status,
                slot,
                error: None,
            },
            Err(e) => TxStatusResultMessage {
                correlation_id: envelope.correlation_id,
                found: false,
                confirmed: false,
                status: "Error".to_string(),
                slot: 0,
                error: Some(format!("{:#}", e)),
            },
        };

        self.publish_status_result(&envelope.reply_subject, result).await;
        let _ = msg.ack().await;
    }

    async fn publish_status_result(&self, subject: &str, result: TxStatusResultMessage) {
        let payload = match serde_json::to_vec(&result) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to serialize status result for {}: {}", subject, e);
                return;
            }
        };
        if let Err(e) = self.jetstream.publish(subject.to_string(), payload.into()).await {
            error!("Failed to publish status result to {}: {}", subject, e);
        }
    }

    async fn publish_result(&self, subject: &str, result: TxResultMessage) {
        let payload = match serde_json::to_vec(&result) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to serialize result for {}: {}", subject, e);
                return;
            }
        };
        if let Err(e) = self.jetstream.publish(subject.to_string(), payload.into()).await {
            error!("Failed to publish result to {}: {}", subject, e);
        }
    }

    async fn publish_cancel_result(&self, subject: &str, result: TxCancelResultMessage) {
        let payload = match serde_json::to_vec(&result) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to serialize cancel result for {}: {}", subject, e);
                return;
            }
        };
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
