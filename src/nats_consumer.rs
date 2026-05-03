use std::sync::Arc;
use tracing::{info, warn, error};
use futures::StreamExt;
use dashmap::DashMap;
use tokio_retry::{Retry, strategy::FixedInterval};
use gridtokenx_blockchain_core::rpc::nats_schema::{TxSubmitMessage, TxResultMessage, TxSimulateMessage, TxSimulateResultMessage, TxCancelMessage, TxCancelResultMessage};
use gridtokenx_blockchain_core::rpc::metrics::BlockchainMetrics;
use gridtokenx_blockchain_core::auth::{ServiceRole, SpiffeIdentity};
use crate::api::ChainBridgeGrpcService;
use solana_sdk::transaction::Transaction;
use solana_sdk::signature::Signature;

pub struct NatsConsumer {
    jetstream: async_nats::jetstream::Context,
    signing_service: Arc<ChainBridgeGrpcService>,
    metrics: Arc<dyn BlockchainMetrics>,
    /// correlation_id -> expiry_timestamp (ms)
    idempotency_cache: DashMap<String, u64>,
}

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
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                cleanup_cache.retain(|_, &mut expiry| expiry > now_ms);
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
            }).await;
            msg.ack().await.ok();
            return;
        }

        // 4. Sign and submit with retry policy
        // We retry only for transient Solana RPC errors (e.g. rate limit, node behind)
        let retry_strategy = FixedInterval::from_millis(500).take(3);
        
        // Use a wrapper to avoid borrowing 'self' into the Retry block improperly if needed, 
        // but since we are in handle_submit which takes &self, we are fine as long as sign_and_submit is async.
        let result: anyhow::Result<(Signature, u64)> = Retry::spawn(retry_strategy, || async {
            info!("🔄 Retrying sign_and_submit for {}", envelope.correlation_id);
            self.signing_service.sign_and_submit(&envelope.serialized_tx, &envelope.key_id).await
        }).await;

        let result_msg = match result {
            Ok((sig, slot)) => {
                // Cache successful correlation_id for 60 seconds
                self.idempotency_cache.insert(envelope.correlation_id.clone(), now_ms + 60_000);
                
                TxResultMessage {
                    correlation_id: envelope.correlation_id,
                    success: true,
                    signature: Some(sig.to_string()),
                    error: None,
                    slot,
                }
            },
            Err(e) => TxResultMessage {
                correlation_id: envelope.correlation_id,
                success: false,
                signature: None,
                error: Some(format!("{:#}", e)),
                slot: 0,
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
}
