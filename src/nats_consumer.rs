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
            }).await;
            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
            return;
        }

        // 5. Sign and submit with retry policy for transient errors only
        // We retry only for transient Solana RPC errors (e.g. rate limit, node behind)
        let retry_strategy = FixedInterval::from_millis(500).take(3);
        
        // Use a wrapper to avoid borrowing 'self' into the Retry block improperly if needed, 
        // but since we are in handle_submit which takes &self, we are fine as long as sign_and_submit is async.
        let result: anyhow::Result<(Signature, u64)> = Retry::spawn(retry_strategy, || async {
            info!("🔄 Retrying sign_and_submit for {}", envelope.correlation_id);
            self.signing_service.sign_and_submit(&envelope.serialized_tx, &envelope.key_id, &SpiffeIdentity(envelope.service_identity.clone())).await
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

    /// Expose internals for testing
    #[cfg(test)]
    pub fn idempotency_cache(&self) -> &DashMap<String, u64> {
        &self.idempotency_cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use solana_sdk::{
        signature::{Signature, Keypair, Signer},
        pubkey::Pubkey,
        hash::Hash,
        transaction::Transaction,
        instruction::Instruction,
        account::Account,
    };
    use solana_client::client_error::ClientError;
    use solana_client::rpc_response::{Response, RpcResponseContext, RpcSimulateTransactionResult, RpcPrioritizationFee};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // -------------------------------------------------------------------
    // Helper: minimal mock provider that tracks sends
    // -------------------------------------------------------------------
    struct MockProvider {
        send_count: AtomicUsize,
    }

    #[async_trait]
    impl crate::api::SolanaProvider for MockProvider {
        async fn simulate_transaction(&self, _tx: &Transaction) -> Result<Response<RpcSimulateTransactionResult>, ClientError> {
            Ok(Response {
                context: RpcResponseContext { slot: 100, api_version: None },
                value: RpcSimulateTransactionResult {
                    err: None, logs: Some(vec!["ok".to_string()]), accounts: None,
                    units_consumed: Some(200), return_data: None,
                    inner_instructions: None, loaded_accounts_data_size: None,
                    replacement_blockhash: None,
                },
            })
        }
        async fn send_transaction(&self, _tx: &Transaction) -> Result<Signature, ClientError> {
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Ok(Signature::from([1u8; 64]))
        }
        async fn get_latest_blockhash(&self) -> Result<(Hash, u64), ClientError> {
            Ok((Hash::default(), 1000))
        }
        async fn get_balance(&self, _pubkey: &Pubkey) -> Result<u64, ClientError> { Ok(0) }
        async fn get_account(&self, _pubkey: &Pubkey) -> Result<Account, ClientError> {
            Ok(Account { lamports: 0, data: vec![], owner: Pubkey::default(), executable: false, rent_epoch: 0 })
        }
        async fn get_recent_prioritization_fees(&self, _pubkeys: &[Pubkey]) -> Result<Vec<RpcPrioritizationFee>, ClientError> { Ok(vec![]) }
        async fn get_token_account_balance(&self, _pubkey: &Pubkey) -> Result<serde_json::Value, ClientError> { Ok(serde_json::json!({})) }
        async fn get_signature_statuses(&self, _signatures: &[Signature]) -> Result<Response<Vec<Option<serde_json::Value>>>, ClientError> {
            Ok(Response { context: RpcResponseContext { slot: 0, api_version: None }, value: vec![] })
        }
        async fn get_slot(&self) -> Result<u64, ClientError> { Ok(0) }
        async fn request_airdrop(&self, _pubkey: &Pubkey, _lamports: u64) -> Result<Signature, ClientError> { Ok(Signature::default()) }
        async fn get_transaction(&self, _signature: &Signature) -> Result<serde_json::Value, ClientError> { Ok(serde_json::json!({})) }
        async fn get_epoch_info(&self) -> Result<solana_sdk::epoch_info::EpochInfo, ClientError> {
            Ok(solana_sdk::epoch_info::EpochInfo { absolute_slot: 0, block_height: 0, epoch: 0, slots_in_epoch: 0, slot_index: 0, transaction_count: None })
        }
    }

    // -------------------------------------------------------------------
    // Idempotency cache tests
    // -------------------------------------------------------------------

    #[test]
    fn test_idempotency_cache_insert_and_check() {
        let cache: DashMap<String, u64> = DashMap::new();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        cache.insert("corr-123".to_string(), now_ms + 60_000);

        assert!(cache.contains_key("corr-123"), "Should find inserted key");
        assert!(!cache.contains_key("corr-456"), "Should not find missing key");
    }

    #[test]
    fn test_idempotency_cache_cleanup_removes_expired() {
        let cache: DashMap<String, u64> = DashMap::new();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Insert with past expiry
        cache.insert("expired".to_string(), now_ms - 1);
        cache.insert("valid".to_string(), now_ms + 60_000);

        // Simulate cleanup (same logic as background task)
        cache.retain(|_, &mut expiry| expiry > now_ms);

        assert!(!cache.contains_key("expired"), "Expired entry should be removed");
        assert!(cache.contains_key("valid"), "Valid entry should remain");
    }

    // -------------------------------------------------------------------
    // RBAC checks on message identity
    // -------------------------------------------------------------------

    #[test]
    fn test_nats_rbac_unknown_identity_maps_to_unknown_role() {
        let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/fake-service".to_string());
        let role = ServiceRole::from(&identity);
        assert_eq!(role, ServiceRole::Unknown, "Unknown SPIFFE ID should map to Unknown role");
    }

    #[test]
    fn test_nats_rbac_trading_service_maps_correctly() {
        let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service/matcher".to_string());
        let role = ServiceRole::from(&identity);
        assert_eq!(role, ServiceRole::TradingMatcher);
    }

    #[test]
    fn test_nats_rbac_oracle_bridge_maps_correctly() {
        let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/oracle-bridge".to_string());
        let role = ServiceRole::from(&identity);
        assert_eq!(role, ServiceRole::OracleBridge);
    }

    #[test]
    fn test_nats_rbac_iam_service_maps_correctly() {
        let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/iam-service".to_string());
        let role = ServiceRole::from(&identity);
        assert_eq!(role, ServiceRole::IamService);
    }

    // -------------------------------------------------------------------
    // Staleness check logic
    // -------------------------------------------------------------------

    #[test]
    fn test_staleness_check_fresh_message() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let created_at_ms = now_ms - 10_000; // 10s ago

        let age_ms = now_ms.saturating_sub(created_at_ms);
        assert!(age_ms <= 55_000, "10s old message should be fresh");
    }

    #[test]
    fn test_staleness_check_stale_message() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let created_at_ms = now_ms - 60_000; // 60s ago

        let age_ms = now_ms.saturating_sub(created_at_ms);
        assert!(age_ms > 55_000, "60s old message should be stale");
    }

    // -------------------------------------------------------------------
    // Cancel idempotency logic
    // -------------------------------------------------------------------

    #[test]
    fn test_cancel_idempotency_key_format() {
        let corr_id = "tx-abc-123";
        let cancel_key = format!("cancel:{}", corr_id);
        assert_eq!(cancel_key, "cancel:tx-abc-123");

        // Verify both keys can coexist in the cache
        let cache: DashMap<String, u64> = DashMap::new();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        cache.insert(corr_id.to_string(), now_ms + 60_000);
        cache.insert(cancel_key, now_ms + 60_000);

        assert!(cache.contains_key("tx-abc-123"));
        assert!(cache.contains_key("cancel:tx-abc-123"));
    }

    // -------------------------------------------------------------------
    // Message deserialization
    // -------------------------------------------------------------------

    #[test]
    fn test_tx_submit_message_deserialization() {
        let config = gridtokenx_blockchain_core::config::SolanaProgramsConfig::default();
        let trading_prog: Pubkey = config.trading_program_id.parse().unwrap();
        let payer = Keypair::new();
        let ix = Instruction::new_with_bytes(trading_prog, &[1, 2, 3], vec![]);
        let msg = solana_sdk::message::Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new_unsigned(msg);
        let serialized_tx = bincode::serialize(&tx).unwrap();

        let submit_msg = TxSubmitMessage {
            correlation_id: "test-123".to_string(),
            reply_subject: "chain.tx.result.test-123".to_string(),
            serialized_tx,
            key_id: "platform_admin".to_string(),
            skip_preflight: false,
            retry_count: 0,
            service_identity: "spiffe://gridtokenx.th/prod/trading-service/matcher".to_string(),
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        };

        let json = serde_json::to_vec(&submit_msg).unwrap();
        let parsed: TxSubmitMessage = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.correlation_id, "test-123");
        assert_eq!(parsed.key_id, "platform_admin");
        assert_eq!(parsed.service_identity, "spiffe://gridtokenx.th/prod/trading-service/matcher");
    }

    #[test]
    fn test_tx_submit_message_invalid_json_returns_error() {
        let result: Result<TxSubmitMessage, _> = serde_json::from_slice(b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_tx_simulate_message_deserialization() {
        let sim_msg = TxSimulateMessage {
            correlation_id: "sim-456".to_string(),
            reply_subject: "chain.tx.simresult.sim-456".to_string(),
            serialized_tx: vec![1, 2, 3],
            key_id: "platform_admin".to_string(),
            service_identity: "spiffe://gridtokenx.th/prod/oracle-bridge".to_string(),
            created_at_ms: 1000,
        };

        let json = serde_json::to_vec(&sim_msg).unwrap();
        let parsed: TxSimulateMessage = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.correlation_id, "sim-456");
    }

    #[test]
    fn test_tx_cancel_message_deserialization() {
        let cancel_msg = TxCancelMessage {
            correlation_id: "cancel-789".to_string(),
            reply_subject: "chain.tx.cancelresult.cancel-789".to_string(),
            service_identity: "spiffe://gridtokenx.th/prod/trading-service/matcher".to_string(),
            created_at_ms: 2000,
        };

        let json = serde_json::to_vec(&cancel_msg).unwrap();
        let parsed: TxCancelMessage = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.correlation_id, "cancel-789");
    }

    #[test]
    fn test_tx_result_message_serialization() {
        let result_msg = TxResultMessage {
            correlation_id: "res-001".to_string(),
            success: true,
            signature: Some(Signature::from([1u8; 64]).to_string()),
            error: None,
            slot: 100,
        };

        let json = serde_json::to_vec(&result_msg).unwrap();
        let parsed: TxResultMessage = serde_json::from_slice(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.slot, 100);
    }

    // -------------------------------------------------------------------
    // sign_and_submit integration through the signing service
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_sign_and_submit_via_grpc_service_for_nats_identity() {
        let provider = Arc::new(MockProvider { send_count: AtomicUsize::new(0) });
        let vault = Arc::new(crate::vault::InsecureKeypairProvider::new());
        let cache = Arc::new(crate::api::BlockhashCache::new());
        let service = Arc::new(crate::api::ChainBridgeGrpcService::new(
            provider.clone(), vault, cache,
        ));

        // Use a program ID that is explicitly allowed for trading-service in PolicyEngine
        let trading_prog = "DXxHdUar3pUUKRnt4XAMA8rdYRpAsNY1xk3Zo4crShvY".parse::<Pubkey>().unwrap();
        let payer = Keypair::new();
        let ix = Instruction::new_with_bytes(trading_prog, &[1], vec![]);
        let msg = solana_sdk::message::Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new_unsigned(msg);
        let serialized = bincode::serialize(&tx).unwrap();

        let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service/matcher".to_string());
        let result = service.sign_and_submit(&serialized, "platform_admin", &identity).await;

        assert!(result.is_ok(), "sign_and_submit should succeed for valid trading identity + trading program");
        assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
    }
}
