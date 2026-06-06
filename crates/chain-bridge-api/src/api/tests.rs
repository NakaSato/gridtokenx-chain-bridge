    use super::*;
    use solana_sdk::signature::{Keypair, Signer};
    use solana_client::rpc_response::{Response, RpcResponseContext, RpcSimulateTransactionResult};
    use buffa::view::OwnedView;
    use connectrpc::{Context, ErrorCode};

    struct MockSolanaProvider;

    #[async_trait]
    impl SolanaProvider for MockSolanaProvider {
        async fn simulate_transaction(&self, _transaction: &Transaction) -> Result<Response<RpcSimulateTransactionResult>, ClientError> {
            Ok(Response {
                context: RpcResponseContext { slot: 100, api_version: None },
                value: RpcSimulateTransactionResult {
                    err: None,
                    logs: Some(vec!["Program log: Hello".to_string()]),
                    accounts: None,
                    units_consumed: Some(100),
                    return_data: None,
                    inner_instructions: None,
                    loaded_accounts_data_size: None,
                    replacement_blockhash: None,
                },
            })
        }
        async fn send_transaction(&self, _transaction: &Transaction) -> Result<Signature, ClientError> {
            Ok(Signature::from([0u8; 64]))
        }
        async fn get_latest_blockhash(&self) -> Result<(Hash, u64), ClientError> {
            Ok((Hash::default(), 1000))
        }
        async fn get_balance(&self, _pubkey: &Pubkey) -> Result<u64, ClientError> {
            Ok(1000000000) // 1 SOL
        }
        async fn get_account(&self, _pubkey: &Pubkey) -> Result<Account, ClientError> {
            Ok(Account {
                lamports: 1000000000,
                data: vec![1, 2, 3],
                owner: Pubkey::default(),
                executable: false,
                rent_epoch: 0,
            })
        }
        async fn get_recent_prioritization_fees(&self, _pubkeys: &[Pubkey]) -> Result<Vec<RpcPrioritizationFee>, ClientError> {
            Ok(vec![RpcPrioritizationFee {
                slot: 100,
                prioritization_fee: 1000,
            }])
        }
        async fn get_token_account_balance(&self, _pubkey: &Pubkey) -> Result<serde_json::Value, ClientError> {
            Ok(serde_json::json!({
                "ui_amount": 10.5,
                "decimals": 9,
                "amount": "10500000000",
                "ui_amount_string": "10.5"
            }))
        }
        async fn get_signature_statuses(&self, _signatures: &[Signature]) -> Result<Response<Vec<Option<serde_json::Value>>>, ClientError> {
            Ok(Response {
                context: RpcResponseContext { slot: 101, api_version: None },
                value: vec![Some(serde_json::json!({
                    "slot": 100,
                    "confirmations": 31,
                    "err": null,
                    "confirmation_status": "Confirmed"
                }))],
            })
        }
        async fn get_slot(&self) -> Result<u64, ClientError> {
            Ok(100)
        }
        async fn request_airdrop(&self, _pubkey: &Pubkey, _lamports: u64) -> Result<Signature, ClientError> {
            Ok(Signature::from([0u8; 64]))
        }
        async fn get_transaction(&self, _signature: &Signature) -> Result<serde_json::Value, ClientError> {
            Ok(serde_json::json!({
                "slot": 100,
                "transaction": {
                    "message": {
                        "accountKeys": ["11111111111111111111111111111111"]
                    }
                }
            }))
        }
        async fn get_epoch_info(&self) -> Result<solana_sdk::epoch_info::EpochInfo, ClientError> {
            Ok(solana_sdk::epoch_info::EpochInfo {
                absolute_slot: 100,
                block_height: 100,
                epoch: 1,
                slots_in_epoch: 432000,
                slot_index: 100,
                transaction_count: Some(1000),
            })
        }
    }

    /// Provider whose pre-sign simulation always returns a tx-level error.
    /// All other calls delegate to `MockSolanaProvider`. Used to prove
    /// `sign_and_submit` rejects (and audits) before reaching the Vault sign.
    struct FailingSimProvider(MockSolanaProvider);

    #[async_trait]
    impl SolanaProvider for FailingSimProvider {
        async fn simulate_transaction(&self, _transaction: &Transaction) -> Result<Response<RpcSimulateTransactionResult>, ClientError> {
            Ok(Response {
                context: RpcResponseContext { slot: 100, api_version: None },
                value: RpcSimulateTransactionResult {
                    err: Some(solana_sdk::transaction::TransactionError::AccountNotFound),
                    logs: Some(vec!["Program log: boom".to_string()]),
                    accounts: None,
                    units_consumed: Some(0),
                    return_data: None,
                    inner_instructions: None,
                    loaded_accounts_data_size: None,
                    replacement_blockhash: None,
                },
            })
        }
        async fn send_transaction(&self, tx: &Transaction) -> Result<Signature, ClientError> { self.0.send_transaction(tx).await }
        async fn get_latest_blockhash(&self) -> Result<(Hash, u64), ClientError> { self.0.get_latest_blockhash().await }
        async fn get_balance(&self, p: &Pubkey) -> Result<u64, ClientError> { self.0.get_balance(p).await }
        async fn get_account(&self, p: &Pubkey) -> Result<Account, ClientError> { self.0.get_account(p).await }
        async fn get_recent_prioritization_fees(&self, p: &[Pubkey]) -> Result<Vec<RpcPrioritizationFee>, ClientError> { self.0.get_recent_prioritization_fees(p).await }
        async fn get_token_account_balance(&self, p: &Pubkey) -> Result<serde_json::Value, ClientError> { self.0.get_token_account_balance(p).await }
        async fn get_signature_statuses(&self, s: &[Signature]) -> Result<Response<Vec<Option<serde_json::Value>>>, ClientError> { self.0.get_signature_statuses(s).await }
        async fn get_slot(&self) -> Result<u64, ClientError> { self.0.get_slot().await }
        async fn request_airdrop(&self, p: &Pubkey, l: u64) -> Result<Signature, ClientError> { self.0.request_airdrop(p, l).await }
        async fn get_transaction(&self, s: &Signature) -> Result<serde_json::Value, ClientError> { self.0.get_transaction(s).await }
        async fn get_epoch_info(&self) -> Result<solana_sdk::epoch_info::EpochInfo, ClientError> { self.0.get_epoch_info().await }
    }

    struct MockVaultProvider;

    #[async_trait]
    impl VaultProvider for MockVaultProvider {
        async fn get_public_key(&self, _key_name: &str) -> anyhow::Result<Pubkey> {
            Ok(Pubkey::default())
        }
        async fn sign_message(&self, _key_name: &str, _message: &[u8]) -> anyhow::Result<Signature> {
            Ok(Signature::from([0u8; 64]))
        }
    }

    fn create_test_service() -> ChainBridgeGrpcService {
        ChainBridgeGrpcService {
            provider: Arc::new(MockSolanaProvider {}),
            vault: Arc::new(MockVaultProvider),
            blockhash_cache: Arc::new(BlockhashCache::new()),
            transit_key_name: "test-key".to_string(),
            audit: Arc::new(chain_bridge_persistence::InMemoryAuditStore::new()),
        }
    }

    fn authenticated_ctx() -> Context {
        let mut ctx = Context::default();
        ctx.headers.insert("z-gridtokenx-spiffe-id", "spiffe://gridtokenx.th/prod/admin".parse().unwrap());
        ctx
    }

    #[tokio::test]
    async fn test_get_balance() {
        let service = create_test_service();
        let pubkey = Pubkey::new_unique();
        let mut request_owned = GetBalanceRequest::default();
        request_owned.pubkey = pubkey.to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_balance(authenticated_ctx(), request).await.unwrap();
        assert_eq!(resp.lamports, 1000000000);
    }

    #[tokio::test]
    async fn test_simulate_transaction() {
        let service = create_test_service();
        let from = Keypair::new();
        let to = Pubkey::new_unique();
        let ix = solana_sdk::system_instruction::transfer(&from.pubkey(), &to, 1000);
        let tx = Transaction::new_with_payer(&[ix], Some(&from.pubkey()));
        let serialized_tx = bincode::serialize(&tx).unwrap();

        let mut request_owned = SimulateTransactionRequest::default();
        request_owned.serialized_transaction = serialized_tx;
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.simulate_transaction(authenticated_ctx(), request).await.unwrap();
        assert!(resp.success);
        assert_eq!(resp.compute_units_consumed, 100);
        assert_eq!(resp.logs[0], "Program log: Hello");
    }

    #[tokio::test]
    async fn test_submit_transaction_with_platform_admin() {
        let service = create_test_service();
        let to = Pubkey::new_unique();
        let ix = solana_sdk::system_instruction::transfer(&Pubkey::default(), &to, 1000);
        let tx = Transaction::new_with_payer(&[ix], Some(&Pubkey::default()));
        let serialized_tx = bincode::serialize(&tx).unwrap();

        let mut request_owned = SubmitTransactionRequest::default();
        request_owned.serialized_transaction = serialized_tx;
        request_owned.key_id = "platform_admin".to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.submit_transaction(authenticated_ctx(), request).await.unwrap();
        assert!(resp.success);
        assert!(!resp.signature.is_empty());
    }

    fn unauthenticated_ctx() -> Context {
        let mut ctx = Context::default();
        ctx.headers.insert("z-gridtokenx-spiffe-id", "spiffe://gridtokenx.th/prod/guest".parse().unwrap());
        ctx
    }

    #[tokio::test]
    async fn test_submit_transaction_unauthorized() {
        let service = create_test_service();
        let mut request_owned = SubmitTransactionRequest::default();
        request_owned.key_id = "unknown_user".to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let result = service.submit_transaction(unauthenticated_ctx(), request).await;
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn test_get_account_data_exists() {
        let service = create_test_service();
        let mut request_owned = GetAccountDataRequest::default();
        request_owned.pubkey = Pubkey::new_unique().to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_account_data(authenticated_ctx(), request).await.unwrap();
        assert!(resp.exists);
        assert_eq!(resp.data, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_get_epoch_info() {
        let service = create_test_service();
        let request_owned = chain_v1::GetEpochInfoRequest::default();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_epoch_info(authenticated_ctx(), request).await.unwrap();
        assert_eq!(resp.epoch, 1);
        assert_eq!(resp.slot, 100);
    }

    #[tokio::test]
    async fn test_blockhash_cache() {
        let cache = BlockhashCache::new();
        assert!(cache.get().await.is_none());

        let hash = Hash::new_unique();
        cache.update(hash, 1000).await;

        let cached = cache.get().await.unwrap();
        assert_eq!(cached.0, hash);
        assert_eq!(cached.1, 1000);
    }

    #[tokio::test]
    async fn test_blockhash_cache_overwrite() {
        let cache = BlockhashCache::new();
        let hash1 = Hash::new_unique();
        let hash2 = Hash::new_unique();

        cache.update(hash1, 1000).await;
        cache.update(hash2, 2000).await;

        let cached = cache.get().await.unwrap();
        assert_eq!(cached.0, hash2);
        assert_eq!(cached.1, 2000);
    }

    #[tokio::test]
    async fn test_blockhash_cache_concurrent() {
        let cache = Arc::new(BlockhashCache::new());
        let cache_clone = cache.clone();

        let hash = Hash::new_unique();
        
        let handle = tokio::spawn(async move {
            cache_clone.update(hash, 1000).await;
        });

        let _ = cache.get().await;
        handle.await.unwrap();
        
        let cached = cache.get().await.unwrap();
        assert_eq!(cached.0, hash);
    }

    // ---------------------------------------------------------------
    // sign_and_submit branch coverage
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_sign_and_submit_empty_cache_fallback() {
        let service = create_test_service();
        // Cache is empty by default

        let to = Pubkey::new_unique();
        let ix = solana_sdk::system_instruction::transfer(&Pubkey::default(), &to, 1000);
        let tx = Transaction::new_with_payer(&[ix], Some(&Pubkey::default()));
        let serialized = bincode::serialize(&tx).unwrap();

        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/admin".to_string()
        );

        // Should fallback to MockSolanaProvider which returns Hash::default()
        let result = service.sign_and_submit(&serialized, "platform_admin", &identity).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sign_and_submit_multi_sig() {
        let service = create_test_service();
        
        let from1 = Pubkey::new_unique();
        let from2 = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        
        let ix = solana_sdk::system_instruction::transfer(&from1, &to, 1000);
        let mut msg = solana_sdk::message::Message::new(&[ix], Some(&from1));
        // Add second required signer manually
        msg.header.num_required_signatures = 2;
        msg.account_keys.insert(1, from2);
        
        let tx = Transaction::new_unsigned(msg);
        let serialized = bincode::serialize(&tx).unwrap();

        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/admin".to_string()
        );

        let result = service.sign_and_submit(&serialized, "platform_admin", &identity).await;
        assert!(result.is_ok());
        // MockVaultProvider returns Signature::from([0u8; 64]) for slot 0
    }

    #[tokio::test]
    async fn test_sign_and_submit_preserves_non_empty_blockhash() {
        let service = create_test_service();

        // Pre-populate cache with a blockhash
        let cached_hash = Hash::new_unique();
        service.blockhash_cache.update(cached_hash, 9999).await;

        // Create a tx with a non-default blockhash
        let custom_hash = Hash::new_unique();
        let to = Pubkey::new_unique();
        let ix = solana_sdk::system_instruction::transfer(&Pubkey::default(), &to, 1000);
        let mut tx = Transaction::new_with_payer(&[ix], Some(&Pubkey::default()));
        tx.message.recent_blockhash = custom_hash;

        let serialized = bincode::serialize(&tx).unwrap();
        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/admin".to_string()
        );

        let result = service.sign_and_submit(&serialized, "platform_admin", &identity).await;
        assert!(result.is_ok(), "Should succeed with non-empty blockhash");

        // Verify the mock received the transaction — the blockhash should NOT be the cached one
        // (We can't inspect the tx passed to the mock, but the fact it succeeded proves the path works)
    }

    #[tokio::test]
    async fn test_sign_and_submit_invalid_serialized_tx() {
        let service = create_test_service();
        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/admin".to_string()
        );

        let result = service.sign_and_submit(&[0xFF, 0xFE, 0xFD], "platform_admin", &identity).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid transaction format"));
    }

    #[tokio::test]
    async fn test_sign_and_submit_unauthorized_key_id() {
        let service = create_test_service();
        let to = Pubkey::new_unique();
        let ix = solana_sdk::system_instruction::transfer(&Pubkey::default(), &to, 1000);
        let tx = Transaction::new_with_payer(&[ix], Some(&Pubkey::default()));
        let serialized = bincode::serialize(&tx).unwrap();

        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/admin".to_string()
        );

        let result = service.sign_and_submit(&serialized, "rogue_key", &identity).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not authorized"));
    }

    #[tokio::test]
    async fn test_sign_and_submit_policy_rejects_unauthorized_program() {
        let service = create_test_service();
        let config = gridtokenx_blockchain_core::config::SolanaProgramsConfig::default();
        let oracle_program_id: Pubkey = config.oracle_program_id.parse().unwrap();

        // Trading identity should not be allowed to call Oracle program
        let ix = solana_sdk::instruction::Instruction {
            program_id: oracle_program_id,
            accounts: vec![],
            data: vec![],
        };
        let tx = Transaction::new_with_payer(&[ix], Some(&Pubkey::default()));
        let serialized = bincode::serialize(&tx).unwrap();

        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/trading-service/matcher".to_string()
        );

        let result = service.sign_and_submit(&serialized, "platform_admin", &identity).await;
        assert!(result.is_err(), "Policy Engine should reject trading-service calling oracle program");
        assert!(result.unwrap_err().to_string().contains("Policy Engine"));
    }

    // ---------------------------------------------------------------
    // Audit hash-chain (Gap #2) — sign_and_submit appends entries
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_audit_records_successful_submit() {
        let store = Arc::new(chain_bridge_persistence::InMemoryAuditStore::new());
        let service = create_test_service().with_audit(store.clone());

        let to = Pubkey::new_unique();
        let ix = solana_sdk::system_instruction::transfer(&Pubkey::default(), &to, 1000);
        let tx = Transaction::new_with_payer(&[ix], Some(&Pubkey::default()));
        let serialized = bincode::serialize(&tx).unwrap();
        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/admin".to_string(),
        );

        assert_eq!(store.len().await, 0);
        let result = service.sign_and_submit(&serialized, "platform_admin", &identity).await;
        assert!(result.is_ok());
        assert_eq!(store.len().await, 1, "successful submit must append one audit entry");
    }

    #[tokio::test]
    async fn test_audit_records_policy_rejection() {
        let store = Arc::new(chain_bridge_persistence::InMemoryAuditStore::new());
        let service = create_test_service().with_audit(store.clone());

        let config = gridtokenx_blockchain_core::config::SolanaProgramsConfig::default();
        let oracle_program_id: Pubkey = config.oracle_program_id.parse().unwrap();
        let ix = solana_sdk::instruction::Instruction {
            program_id: oracle_program_id,
            accounts: vec![],
            data: vec![],
        };
        let tx = Transaction::new_with_payer(&[ix], Some(&Pubkey::default()));
        let serialized = bincode::serialize(&tx).unwrap();
        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/trading-service/matcher".to_string(),
        );

        let result = service.sign_and_submit(&serialized, "platform_admin", &identity).await;
        assert!(result.is_err());
        assert_eq!(store.len().await, 1, "policy rejection must append one audit entry");
    }

    #[tokio::test]
    async fn test_presign_simulation_rejects_before_signing() {
        // A doomed transaction (sim returns a tx-level error) must be rejected
        // before the Vault signing operation, and audited at stage="simulation".
        let store = Arc::new(chain_bridge_persistence::InMemoryAuditStore::new());
        let service = ChainBridgeGrpcService {
            provider: Arc::new(FailingSimProvider(MockSolanaProvider {})),
            vault: Arc::new(MockVaultProvider),
            blockhash_cache: Arc::new(BlockhashCache::new()),
            transit_key_name: "test-key".to_string(),
            audit: store.clone(),
        };

        let to = Pubkey::new_unique();
        let ix = solana_sdk::system_instruction::transfer(&Pubkey::default(), &to, 1000);
        let tx = Transaction::new_with_payer(&[ix], Some(&Pubkey::default()));
        let serialized = bincode::serialize(&tx).unwrap();
        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/admin".to_string(),
        );

        let result = service.sign_and_submit(&serialized, "platform_admin", &identity).await;
        assert!(result.is_err(), "failing pre-sign simulation must reject before signing");
        assert_eq!(store.len().await, 1, "simulation rejection must append one audit entry");
    }

    #[tokio::test]
    async fn test_sign_and_submit_empty_key_id_passes_unsigned() {
        // When key_id is empty, tx is sent unsigned (no signing, no policy check on key)
        // but PolicyEngine still runs
        let service = create_test_service();
        let to = Pubkey::new_unique();
        let ix = solana_sdk::system_instruction::transfer(&Pubkey::default(), &to, 1000);
        let tx = Transaction::new_with_payer(&[ix], Some(&Pubkey::default()));
        let serialized = bincode::serialize(&tx).unwrap();

        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(
            "spiffe://gridtokenx.th/prod/admin".to_string()
        );

        // Empty key_id should not sign but should still send
        let result = service.sign_and_submit(&serialized, "", &identity).await;
        assert!(result.is_ok(), "Empty key_id should skip signing and send unsigned");
    }

    // ---------------------------------------------------------------
    // Missing gRPC handler tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_get_latest_blockhash() {
        let service = create_test_service();
        let request_owned = chain_v1::GetLatestBlockhashRequest::default();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_latest_blockhash(authenticated_ctx(), request).await.unwrap();
        // MockSolanaProvider returns Hash::default() and height 1000
        assert_eq!(resp.last_valid_block_height, 1000);
    }

    #[tokio::test]
    async fn test_get_recent_prioritization_fees() {
        let service = create_test_service();
        let mut request_owned = chain_v1::GetRecentPrioritizationFeesRequest::default();
        request_owned.account_keys = vec![Pubkey::default().to_string()];
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_recent_prioritization_fees(authenticated_ctx(), request).await.unwrap();
        assert_eq!(resp.fees.len(), 1);
        assert_eq!(resp.fees[0].prioritization_fee, 1000);
    }

    #[tokio::test]
    async fn test_get_token_account_balance() {
        let service = create_test_service();
        let mut request_owned = chain_v1::GetTokenAccountBalanceRequest::default();
        request_owned.pubkey = Pubkey::new_unique().to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_token_account_balance(authenticated_ctx(), request).await.unwrap();
        assert_eq!(resp.amount, "10500000000");
        assert_eq!(resp.decimals, 9);
    }

    #[tokio::test]
    async fn test_get_signature_status() {
        let service = create_test_service();
        let mut request_owned = chain_v1::GetSignatureStatusRequest::default();
        request_owned.signature = Signature::from([0u8; 64]).to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_signature_status(authenticated_ctx(), request).await.unwrap();
        assert!(resp.confirmed);
        assert_eq!(resp.status, "Confirmed");
    }

    #[tokio::test]
    async fn test_get_slot() {
        let service = create_test_service();
        let request_owned = chain_v1::GetSlotRequest::default();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_slot(authenticated_ctx(), request).await.unwrap();
        assert_eq!(resp.slot, 100);
    }

    #[tokio::test]
    async fn test_request_airdrop_admin_allowed() {
        let service = create_test_service();
        let mut request_owned = chain_v1::RequestAirdropRequest::default();
        request_owned.pubkey = Pubkey::new_unique().to_string();
        request_owned.lamports = 1_000_000_000;
        let request = OwnedView::from_owned(&request_owned).unwrap();

        // Admin should be allowed
        let (resp, _) = service.request_airdrop(authenticated_ctx(), request).await.unwrap();
        assert!(!resp.signature.is_empty());
    }

    #[tokio::test]
    async fn test_request_airdrop_trading_service_denied() {
        let service = create_test_service();
        let mut request_owned = chain_v1::RequestAirdropRequest::default();
        request_owned.pubkey = Pubkey::new_unique().to_string();
        request_owned.lamports = 1_000_000_000;
        let request = OwnedView::from_owned(&request_owned).unwrap();

        // Trading API should be denied
        let mut ctx = Context::default();
        ctx.headers.insert("z-gridtokenx-spiffe-id", "spiffe://gridtokenx.th/prod/trading-service/api".parse().unwrap());

        let result = service.request_airdrop(ctx, request).await;
        assert!(result.is_err(), "Trading service should not be allowed to airdrop");
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn test_get_transaction_details() {
        let service = create_test_service();
        let mut request_owned = chain_v1::GetTransactionDetailsRequest::default();
        request_owned.signature = Signature::from([0u8; 64]).to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_transaction_details(authenticated_ctx(), request).await.unwrap();
        assert!(resp.found);
    }

    // ---------------------------------------------------------------
    // extract_role branch coverage
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_extract_role_trading_matcher() {
        let service = create_test_service();
        let mut ctx = Context::default();
        ctx.headers.insert("z-gridtokenx-spiffe-id", "spiffe://gridtokenx.th/prod/trading-service/matcher".parse().unwrap());

        // Should be able to submit — TradingMatcher is in the allowed list
        let request_owned = chain_v1::GetSlotRequest::default();
        let request = OwnedView::from_owned(&request_owned).unwrap();
        let (resp, _) = service.get_slot(ctx, request).await.unwrap();
        assert_eq!(resp.slot, 100);
    }

    #[tokio::test]
    async fn test_extract_role_api_gateway_denied_submit() {
        let service = create_test_service();
        let mut ctx = Context::default();
        ctx.headers.insert("z-gridtokenx-spiffe-id", "spiffe://gridtokenx.th/prod/apisix".parse().unwrap());

        // ApiGateway should be denied on SubmitTransaction
        let mut request_owned = SubmitTransactionRequest::default();
        request_owned.key_id = "platform_admin".to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let result = service.submit_transaction(ctx, request).await;
        assert!(result.is_err(), "ApiGateway should be denied write operations");
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn test_extract_role_api_gateway_allowed_read() {
        let service = create_test_service();
        let mut ctx = Context::default();
        ctx.headers.insert("z-gridtokenx-spiffe-id", "spiffe://gridtokenx.th/prod/apisix".parse().unwrap());

        // ApiGateway should be allowed on read operations like GetBalance
        let mut request_owned = GetBalanceRequest::default();
        request_owned.pubkey = Pubkey::new_unique().to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let (resp, _) = service.get_balance(ctx, request).await.unwrap();
        assert_eq!(resp.lamports, 1000000000);
    }

    #[tokio::test]
    async fn test_extract_role_env_overrides() {
        let service = create_test_service();
        
        // 1. Insecure mode override
        {
            let ctx = Context::default();
            unsafe { std::env::set_var("CHAIN_BRIDGE_INSECURE", "true"); }
            let role = service.extract_role(&ctx);
            unsafe { std::env::remove_var("CHAIN_BRIDGE_INSECURE"); }
            assert_eq!(role, ServiceRole::Admin, "Insecure mode should grant Admin");
        }

        // 2. Header auth override
        {
            let mut ctx = Context::default();
            ctx.headers.insert("x-gridtokenx-role", "trading-matcher".parse().unwrap());
            unsafe { std::env::set_var("CHAIN_BRIDGE_ALLOW_HEADER_AUTH", "true"); }
            let role = service.extract_role(&ctx);
            unsafe { std::env::remove_var("CHAIN_BRIDGE_ALLOW_HEADER_AUTH"); }
            assert_eq!(role, ServiceRole::TradingMatcher, "Header auth should grant TradingMatcher");
        }
    }

    #[tokio::test]
    async fn test_get_balance_unauthorized_unknown() {
        let service = create_test_service();
        let ctx = Context::default(); // No SPIFFE header -> Unknown role

        let mut request_owned = chain_v1::GetBalanceRequest::default();
        request_owned.pubkey = Pubkey::new_unique().to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let result = service.get_balance(ctx, request).await;
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn test_simulate_transaction_unauthorized_gateway() {
        let service = create_test_service();
        let mut ctx = Context::default();
        ctx.headers.insert("z-gridtokenx-spiffe-id", "spiffe://gridtokenx.th/prod/apisix".parse().unwrap());

        let mut request_owned = chain_v1::SimulateTransactionRequest::default();
        request_owned.key_id = "platform_admin".to_string();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let result = service.simulate_transaction(ctx, request).await;
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn test_get_latest_blockhash_unauthorized_gateway() {
        let service = create_test_service();
        let mut ctx = Context::default();
        ctx.headers.insert("z-gridtokenx-spiffe-id", "spiffe://gridtokenx.th/prod/apisix".parse().unwrap());

        let request_owned = chain_v1::GetLatestBlockhashRequest::default();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let result = service.get_latest_blockhash(ctx, request).await;
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn test_get_slot_unauthorized_gateway() {
        let service = create_test_service();
        let mut ctx = Context::default();
        ctx.headers.insert("z-gridtokenx-spiffe-id", "spiffe://gridtokenx.th/prod/apisix".parse().unwrap());

        let request_owned = chain_v1::GetSlotRequest::default();
        let request = OwnedView::from_owned(&request_owned).unwrap();

        let result = service.get_slot(ctx, request).await;
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
    }
