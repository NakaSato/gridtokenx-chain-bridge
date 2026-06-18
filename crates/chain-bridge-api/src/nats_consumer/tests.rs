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
    // Effect-level dedup store (idempotency_key) state machine
    // -------------------------------------------------------------------

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    // These exercise the real decision fn `NatsConsumer::claim_or_replay` — the
    // exact logic `handle_submit` runs at step 4.5 — not a hand-rolled
    // simulation of it. `handle_submit` itself is untestable in-process (its
    // `async_nats::Message` has no public constructor); extracting the pure
    // decision lets the production code path be asserted directly.

    fn inflight(now: u64) -> DedupRecord {
        DedupRecord { expiry_ms: now + 120_000, state: DedupState::InFlight }
    }

    #[test]
    fn test_dedup_none_key_skips_and_leaves_store_untouched() {
        // Empty/legacy key -> dedup_key is None -> proceed, no claim recorded.
        let store: DashMap<String, DedupRecord> = DashMap::new();
        let out = NatsConsumer::claim_or_replay(&store, None, "corr-1", now_ms());
        assert!(out.is_none(), "no key must proceed (None)");
        assert!(store.is_empty(), "no key must not write a claim");
    }

    #[test]
    fn test_dedup_absent_key_claims_inflight_and_proceeds() {
        let store: DashMap<String, DedupRecord> = DashMap::new();
        let now = now_ms();
        let out = NatsConsumer::claim_or_replay(&store, Some("settle:1"), "corr-1", now);
        assert!(out.is_none(), "first sight of a key must proceed");
        let rec = store.get("settle:1").expect("claim must be recorded");
        assert!(matches!(rec.state, DedupState::InFlight), "must be claimed InFlight");
        assert!(rec.expiry_ms > now, "claim must carry a future expiry");
    }

    #[test]
    fn test_dedup_done_is_replayed_with_dedup_flag() {
        let store: DashMap<String, DedupRecord> = DashMap::new();
        let now = now_ms();
        store.insert(
            "mint:abc".to_string(),
            DedupRecord {
                expiry_ms: now + 120_000,
                state: DedupState::Done { signature: Some("SIG".to_string()), slot: 9 },
            },
        );

        let out = NatsConsumer::claim_or_replay(&store, Some("mint:abc"), "corr-2", now)
            .expect("a Done record must short-circuit with a replay");
        assert!(out.success, "replay of a Done effect is a success");
        assert!(out.deduplicated, "replay must be flagged deduplicated");
        assert_eq!(out.signature.as_deref(), Some("SIG"));
        assert_eq!(out.slot, 9);
        assert_eq!(out.correlation_id, "corr-2", "result carries THIS attempt's correlation_id");
        // The stored effect is untouched — no second on-chain submit.
        assert!(matches!(store.get("mint:abc").unwrap().state, DedupState::Done { .. }));
    }

    #[test]
    fn test_dedup_keys_are_namespaced_by_effect_type() {
        // F4 regression: handle_submit and handle_mint share one dedup_store but
        // prefix the stable idempotency_key with "submit:" / "mint:". A submit's
        // Done record must therefore NOT short-circuit a mint carrying the same
        // raw idempotency_key (which would forge a mint success without minting).
        let store: DashMap<String, DedupRecord> = DashMap::new();
        let now = now_ms();
        // A submit completed and recorded its effect under the submit namespace.
        store.insert(
            "submit:shared-key".to_string(),
            DedupRecord {
                expiry_ms: now + 120_000,
                state: DedupState::Done { signature: Some("SUBMIT_SIG".to_string()), slot: 7 },
            },
        );

        // A mint with the SAME raw idempotency_key queries under "mint:" and must
        // see no prior effect — it claims InFlight and proceeds to a real mint.
        let out = NatsConsumer::claim_or_replay(&store, Some("mint:shared-key"), "corr-mint", now);
        assert!(
            out.is_none(),
            "a submit Done must not replay as a mint result across the shared store"
        );
        assert!(
            matches!(store.get("mint:shared-key").unwrap().state, DedupState::InFlight),
            "the mint must claim its own namespaced InFlight slot"
        );
        // The submit's record is untouched.
        assert!(matches!(
            store.get("submit:shared-key").unwrap().state,
            DedupState::Done { .. }
        ));
    }

    #[test]
    fn test_dedup_inflight_collision_is_rejected_not_resubmitted() {
        let store: DashMap<String, DedupRecord> = DashMap::new();
        let now = now_ms();
        store.insert("settle:1".to_string(), inflight(now));

        let out = NatsConsumer::claim_or_replay(&store, Some("settle:1"), "corr-3", now)
            .expect("an in-flight collision must short-circuit");
        assert!(!out.success, "a concurrent submit must not report success");
        assert!(out.deduplicated, "collision is a dedup hit");
        assert!(out.signature.is_none());
        assert!(
            out.error.as_deref().unwrap_or_default().contains("already in flight"),
            "error must name the in-flight collision, got: {:?}", out.error
        );
    }

    #[test]
    fn test_dedup_inflight_then_failure_releases_claim() {
        let store: DashMap<String, DedupRecord> = DashMap::new();
        let now = now_ms();

        // First attempt claims InFlight and proceeds.
        assert!(NatsConsumer::claim_or_replay(&store, Some("settle:1"), "a", now).is_none());
        assert!(store.contains_key("settle:1"));

        // handle_submit removes the claim on submit failure so a retry can run.
        store.remove("settle:1");

        // The retry sees no claim and proceeds again (re-claims InFlight).
        assert!(
            NatsConsumer::claim_or_replay(&store, Some("settle:1"), "b", now_ms()).is_none(),
            "after a released claim a genuine retry must proceed"
        );
        assert!(matches!(store.get("settle:1").unwrap().state, DedupState::InFlight));
    }

    #[test]
    fn test_dedup_expired_record_treated_as_absent() {
        // A record past its window must not be replayed — it falls through to the
        // claim branch and the stale Done is overwritten with a fresh InFlight.
        let store: DashMap<String, DedupRecord> = DashMap::new();
        let now = now_ms();
        store.insert(
            "settle:old".to_string(),
            DedupRecord {
                expiry_ms: now - 1,
                state: DedupState::Done { signature: Some("OLD".to_string()), slot: 1 },
            },
        );

        let out = NatsConsumer::claim_or_replay(&store, Some("settle:old"), "corr-4", now);
        assert!(out.is_none(), "an expired record must not replay — must proceed");
        assert!(
            matches!(store.get("settle:old").unwrap().state, DedupState::InFlight),
            "expired Done must be overwritten by a fresh InFlight claim"
        );
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
    fn test_nats_rbac_aggregator_bridge_maps_correctly() {
        let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/aggregator-bridge".to_string());
        let role = ServiceRole::from(&identity);
        assert_eq!(role, ServiceRole::AggregatorBridge);
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
            idempotency_key: String::new(),
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
            auth: None,
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
            service_identity: "spiffe://gridtokenx.th/prod/aggregator-bridge".to_string(),
            created_at_ms: 1000,
            auth: None,
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
            auth: None,
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
            deduplicated: false,
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

        // Use a program ID that is explicitly allowed for trading-service in PolicyEngine.
        // Derived from the shared SolanaProgramsConfig so it tracks the deployed id rather
        // than a hardcoded value (the PolicyEngine allowlist reads the same config).
        let trading_prog = gridtokenx_blockchain_core::config::SolanaProgramsConfig::from_env()
            .trading_program_id
            .parse::<Pubkey>()
            .unwrap();
        let payer = Keypair::new();
        let ix = Instruction::new_with_bytes(trading_prog, &[1], vec![]);
        let msg = solana_sdk::message::Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new_unsigned(msg);
        let serialized = bincode::serialize(&tx).unwrap();

        let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service/matcher".to_string());
        let result = service.sign_and_submit(&serialized, "platform_admin", &identity, "").await;

        assert!(result.is_ok(), "sign_and_submit should succeed for valid trading identity + trading program");
        assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
    }
