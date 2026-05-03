// tests/invariants.rs
//
// Integration tests for chain-bridge invariants and throughput properties.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use connectrpc::{Context, ErrorCode};
use solana_sdk::{
    account::Account,
    hash::Hash,
    pubkey::Pubkey,
    signature::Signature,
    transaction::Transaction,
};
use solana_client::{
    client_error::ClientError,
    rpc_response::{Response, RpcPrioritizationFee, RpcResponseContext, RpcSimulateTransactionResult},
};
use buffa::view::OwnedView;

// Use the now-available library crate
use gridtokenx_chain_bridge::api::{ChainBridgeGrpcService, SolanaProvider};
use gridtokenx_chain_bridge::api::chain_v1::{
    self, ChainBridgeService, GetBalanceRequest, GetBalanceResponse, SubmitTransactionRequest,
};
use gridtokenx_chain_bridge::vault::VaultTransitClient;

/// A mock provider with configurable latency, failures, and call counts.
struct ConfigurableMockProvider {
    send_count: AtomicUsize,
    get_balance_count: AtomicUsize,
    latency: Duration,
}

impl ConfigurableMockProvider {
    fn new() -> Self {
        Self {
            send_count: AtomicUsize::new(0),
            get_balance_count: AtomicUsize::new(0),
            latency: Duration::ZERO,
        }
    }

    fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    fn sleep(&self) {
        if !self.latency.is_zero() {
            std::thread::sleep(self.latency);
        }
    }
}

#[async_trait]
impl SolanaProvider for ConfigurableMockProvider {
    async fn simulate_transaction(&self, _tx: &Transaction) -> Result<Response<RpcSimulateTransactionResult>, ClientError> {
        self.sleep();
        Ok(Response {
            context: RpcResponseContext { slot: 100, api_version: None },
            value: RpcSimulateTransactionResult {
                err: None,
                logs: Some(vec!["Program log: ok".into()]),
                accounts: None,
                units_consumed: Some(200),
                return_data: None,
                inner_instructions: None,
                loaded_accounts_data_size: None,
                replacement_blockhash: None,
            },
        })
    }

    async fn send_transaction(&self, _tx: &Transaction) -> Result<Signature, ClientError> {
        self.sleep();
        self.send_count.fetch_add(1, Ordering::SeqCst);
        let n = self.send_count.load(Ordering::SeqCst) as u8;
        let mut bytes = [0u8; 64];
        bytes[0] = n;
        Ok(Signature::from(bytes))
    }

    async fn get_latest_blockhash(&self) -> Result<(Hash, u64), ClientError> {
        self.sleep();
        Ok((Hash::default(), 1000))
    }

    async fn get_balance(&self, _pubkey: &Pubkey) -> Result<u64, ClientError> {
        self.sleep();
        self.get_balance_count.fetch_add(1, Ordering::SeqCst);
        Ok(1_000_000_000)
    }

    async fn get_account(&self, _pubkey: &Pubkey) -> Result<Account, ClientError> {
        self.sleep();
        Ok(Account {
            lamports: 1_000_000_000,
            data: vec![1, 2, 3],
            owner: Pubkey::default(),
            executable: false,
            rent_epoch: 0,
        })
    }

    async fn get_recent_prioritization_fees(&self, _p: &[Pubkey]) -> Result<Vec<RpcPrioritizationFee>, ClientError> {
        Ok(vec![])
    }

    async fn get_token_account_balance(&self, _p: &Pubkey) -> Result<serde_json::Value, ClientError> {
        Ok(serde_json::json!({"amount": "0", "decimals": 9, "ui_amount": 0.0}))
    }

    async fn get_signature_statuses(&self, _s: &[Signature]) -> Result<Response<Vec<Option<serde_json::Value>>>, ClientError> {
        Ok(Response {
            context: RpcResponseContext { slot: 101, api_version: None },
            value: vec![None],
        })
    }

    async fn get_slot(&self) -> Result<u64, ClientError> {
        Ok(100)
    }

    async fn request_airdrop(&self, _pubkey: &Pubkey, _lamports: u64) -> Result<Signature, ClientError> {
        Ok(Signature::from([0u8; 64]))
    }

    async fn get_transaction(&self, _signature: &Signature) -> Result<serde_json::Value, ClientError> {
        Ok(serde_json::json!({}))
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

fn make_service(provider: Arc<dyn SolanaProvider>) -> ChainBridgeGrpcService {
    let vault = Arc::new(VaultTransitClient::new(
        "http://127.0.0.1:1".into(),
        "test-token".into(),
    ));
    ChainBridgeGrpcService::new(provider, vault, Arc::new(gridtokenx_chain_bridge::api::BlockhashCache::new()))
}

fn ctx_with_spiffe(uri: &str) -> Context {
    let mut ctx = Context::default();
    ctx.headers.insert("z-gridtokenx-spiffe-id", uri.parse().unwrap());
    ctx
}

fn api_gateway_ctx() -> Context {
    ctx_with_spiffe("spiffe://gridtokenx.th/prod/apisix")
}

fn admin_ctx() -> Context {
    ctx_with_spiffe("spiffe://gridtokenx.th/prod/admin")
}

fn no_identity_ctx() -> Context {
    Context::default()
}

// =============================================================================
// §1. Invariant tests
// =============================================================================

mod invariants {
    use super::*;

    #[tokio::test]
    async fn no_spiffe_header_means_unknown_role() {
        let service = make_service(Arc::new(ConfigurableMockProvider::new()));
        let mut req_owned = GetBalanceRequest::default();
        req_owned.pubkey = Pubkey::new_unique().to_string();
        req_owned.force_refresh = false;
        let request = OwnedView::from_owned(&req_owned).unwrap();

        let result = service.get_balance(no_identity_ctx(), request).await;
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn unknown_spiffe_service_is_denied() {
        let provider = Arc::new(ConfigurableMockProvider::new());
        let service = make_service(provider.clone());

        let mut req_owned = GetBalanceRequest::default();
        req_owned.pubkey = Pubkey::new_unique().to_string();
        req_owned.force_refresh = false;
        let request = OwnedView::from_owned(&req_owned).unwrap();

        let ctx = ctx_with_spiffe("spiffe://gridtokenx.th/prod/this-service-does-not-exist");
        let result = service.get_balance(ctx, request).await;

        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
        assert_eq!(provider.get_balance_count.load(Ordering::SeqCst), 0);
    }
}

// =============================================================================
// §2. RBAC tests
// =============================================================================

mod rbac {
    use super::*;

    #[tokio::test]
    async fn api_gateway_cannot_submit_transaction() {
        let provider = Arc::new(ConfigurableMockProvider::new());
        let service = make_service(provider);

        let req_owned = SubmitTransactionRequest::default();
        let request = OwnedView::from_owned(&req_owned).unwrap();

        let result = service.submit_transaction(api_gateway_ctx(), request).await;
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn api_gateway_can_read_balance() {
        let provider = Arc::new(ConfigurableMockProvider::new());
        let service = make_service(provider);

        let mut req_owned = GetBalanceRequest::default();
        req_owned.pubkey = Pubkey::new_unique().to_string();
        req_owned.force_refresh = false;
        let request = OwnedView::from_owned(&req_owned).unwrap();

        let (resp, _) = service.get_balance(api_gateway_ctx(), request).await.unwrap();
        assert_eq!(resp.lamports, 1_000_000_000);
    }
}

// =============================================================================
// §4. Throughput tests
// =============================================================================

mod throughput {
    use super::*;
    use futures::future::join_all;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reads_should_not_serialise() {
        let provider = Arc::new(
            ConfigurableMockProvider::new().with_latency(Duration::from_millis(10))
        );
        let service = Arc::new(make_service(provider));

        let start = Instant::now();
        let handles: Vec<_> = (0..8).map(|_| {
            let service = service.clone();
            tokio::spawn(async move {
                let mut req_owned = GetBalanceRequest::default();
                req_owned.pubkey = Pubkey::new_unique().to_string();
                req_owned.force_refresh = false;
                let request = OwnedView::from_owned(&req_owned).unwrap();
                let _: (GetBalanceResponse, Context) = service.get_balance(admin_ctx(), request).await.unwrap();
            })
        }).collect();

        join_all(handles).await;
        let elapsed = start.elapsed();
        println!("concurrent_reads elapsed: {:?}", elapsed);
        
        assert!(elapsed < Duration::from_millis(100));
    }
}
