use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use solana_client::{
    client_error::ClientError,
    rpc_response::{Response, RpcPrioritizationFee, RpcResponseContext, RpcSimulateTransactionResult},
};
use solana_sdk::{
    account::Account, hash::Hash, pubkey::Pubkey, signature::{Signature, Keypair, Signer}, transaction::Transaction, message::Message, instruction::Instruction
};

use gridtokenx_chain_bridge::api::{BlockhashCache, ChainBridgeGrpcService, SolanaProvider};
use gridtokenx_chain_bridge::vault::VaultProvider;
use gridtokenx_blockchain_core::auth::SpiffeIdentity;

struct MockSolanaProvider {
    send_count: AtomicUsize,
}

impl MockSolanaProvider {
    fn new() -> Self {
        Self { send_count: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl SolanaProvider for MockSolanaProvider {
    async fn simulate_transaction(&self, _tx: &Transaction) -> Result<Response<RpcSimulateTransactionResult>, ClientError> {
        Ok(Response {
            context: RpcResponseContext { slot: 100, api_version: None },
            value: RpcSimulateTransactionResult { err: None, logs: None, accounts: None, units_consumed: None, return_data: None, inner_instructions: None, loaded_accounts_data_size: None, replacement_blockhash: None },
        })
    }

    async fn send_transaction(&self, _tx: &Transaction) -> Result<Signature, ClientError> {
        self.send_count.fetch_add(1, Ordering::SeqCst);
        Ok(Signature::default())
    }

    async fn get_latest_blockhash(&self) -> Result<(Hash, u64), ClientError> {
        Ok((Hash::default(), 1000))
    }

    async fn get_balance(&self, _pubkey: &Pubkey) -> Result<u64, ClientError> {
        Ok(1_000_000)
    }

    async fn get_account(&self, _pubkey: &Pubkey) -> Result<Account, ClientError> {
        Ok(Account { lamports: 0, data: vec![], owner: Pubkey::default(), executable: false, rent_epoch: 0 })
    }

    async fn get_recent_prioritization_fees(&self, _pubkeys: &[Pubkey]) -> Result<Vec<RpcPrioritizationFee>, ClientError> {
        Ok(vec![])
    }

    async fn get_token_account_balance(&self, _pubkey: &Pubkey) -> Result<serde_json::Value, ClientError> {
        Ok(serde_json::json!({}))
    }

    async fn get_signature_statuses(&self, _signatures: &[Signature]) -> Result<Response<Vec<Option<serde_json::Value>>>, ClientError> {
        Ok(Response { context: RpcResponseContext { slot: 0, api_version: None }, value: vec![] })
    }

    async fn get_slot(&self) -> Result<u64, ClientError> {
        Ok(0)
    }

    async fn request_airdrop(&self, _pubkey: &Pubkey, _lamports: u64) -> Result<Signature, ClientError> {
        Ok(Signature::default())
    }

    async fn get_transaction(&self, _signature: &Signature) -> Result<serde_json::Value, ClientError> {
        Ok(serde_json::json!({}))
    }

    async fn get_epoch_info(&self) -> Result<solana_sdk::epoch_info::EpochInfo, ClientError> {
        Ok(solana_sdk::epoch_info::EpochInfo { absolute_slot: 0, block_height: 0, epoch: 0, slots_in_epoch: 0, slot_index: 0, transaction_count: None })
    }
}

struct MockVaultProvider;

#[async_trait]
impl VaultProvider for MockVaultProvider {
    async fn sign_message(&self, _key_name: &str, _message: &[u8]) -> anyhow::Result<Signature> {
        Ok(Signature::default())
    }

    async fn get_public_key(&self, _key_name: &str) -> anyhow::Result<Pubkey> {
        Ok(Pubkey::default())
    }
}

fn create_mock_tx(program_id: Pubkey) -> Transaction {
    let payer = Keypair::new();
    let instruction = Instruction::new_with_bytes(program_id, &[1, 2, 3], vec![]);
    let message = Message::new(&[instruction], Some(&payer.pubkey()));
    Transaction::new_unsigned(message)
}

// Program IDs that match PolicyEngine's allowlist. Derived from the shared
// SolanaProgramsConfig (env-driven, Anchor.toml defaults) — the PolicyEngine reads the
// same source, so these track the deployed ids instead of hardcoded values.
fn trading_program() -> String {
    gridtokenx_blockchain_core::config::SolanaProgramsConfig::from_env().trading_program_id
}
fn registry_program() -> String {
    gridtokenx_blockchain_core::config::SolanaProgramsConfig::from_env().registry_program_id
}
fn energy_token_program() -> String {
    gridtokenx_blockchain_core::config::SolanaProgramsConfig::from_env().energy_token_program_id
}
fn oracle_program() -> String {
    gridtokenx_blockchain_core::config::SolanaProgramsConfig::from_env().oracle_program_id
}

#[tokio::test]
async fn test_trading_service_can_submit_trading_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let trading_program_id = trading_program().parse().unwrap();
    let tx = create_mock_tx(trading_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service/matcher".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_ok(), "Trading matcher should be able to submit trading tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_trading_service_cannot_submit_oracle_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let oracle_program_id = oracle_program().parse().unwrap();
    let tx = create_mock_tx(oracle_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service".to_string());
    
    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_err(), "Trading matcher should NOT be able to submit oracle tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------
// Expanded invariant test matrix
// ---------------------------------------------------------------

#[tokio::test]
async fn test_oracle_service_can_submit_oracle_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let oracle_program_id = oracle_program().parse().unwrap();
    let tx = create_mock_tx(oracle_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/aggregator-bridge".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_ok(), "Oracle Bridge should be able to submit oracle tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_oracle_service_cannot_submit_trading_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let trading_program_id = trading_program().parse().unwrap();
    let tx = create_mock_tx(trading_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/aggregator-bridge".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_err(), "Oracle Bridge should NOT be able to submit trading tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_iam_service_can_submit_registry_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let registry_program_id = registry_program().parse().unwrap();
    let tx = create_mock_tx(registry_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/iam-service".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_ok(), "IAM Service should be able to submit registry tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_iam_service_cannot_submit_trading_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let trading_program_id = trading_program().parse().unwrap();
    let tx = create_mock_tx(trading_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/iam-service".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_err(), "IAM Service should NOT be able to submit trading tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_unknown_identity_rejected_entirely() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    // Even with the system program (always allowed in policy), unknown identity is rejected
    let sys_program_id = "11111111111111111111111111111111".parse().unwrap();
    let tx = create_mock_tx(sys_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/unknown-service".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_err(), "Unknown SPIFFE identity must be rejected entirely");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_admin_identity_can_submit_any_program_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    // Admin can submit to ANY program — test with oracle (which is restricted for others)
    let oracle_program_id = oracle_program().parse().unwrap();
    let tx = create_mock_tx(oracle_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/admin/superuser".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_ok(), "Admin should bypass all policy checks");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_system_program_always_allowed() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    // System program is allowed for any known service identity
    let sys_program_id: Pubkey = "11111111111111111111111111111111".parse().unwrap();
    let tx = create_mock_tx(sys_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    // Oracle bridge should be able to call system program
    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/aggregator-bridge".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_ok(), "System program should be allowed for all known identities");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_multi_instruction_tx_one_unauthorized_rejects_all() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let trading_prog: Pubkey = trading_program().parse().unwrap();
    let oracle_prog: Pubkey = oracle_program().parse().unwrap();
    let payer = Keypair::new();

    // Two instructions: trading (allowed for trading-service) + oracle (denied)
    let ixs = vec![
        Instruction::new_with_bytes(trading_prog, &[1], vec![]),
        Instruction::new_with_bytes(oracle_prog, &[2], vec![]),
    ];
    let message = Message::new(&ixs, Some(&payer.pubkey()));
    let tx = Transaction::new_unsigned(message);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_err(), "Tx with any unauthorized instruction must be rejected");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_trading_service_can_call_energy_token_program() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let energy_prog: Pubkey = energy_token_program().parse().unwrap();
    let tx = create_mock_tx(energy_prog);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service/matcher".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_ok(), "Trading service should be allowed to call Energy Token program");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_settlement_service_can_submit_trading_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let trading_program_id = trading_program().parse().unwrap();
    let tx = create_mock_tx(trading_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/settlement-service".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_ok(), "Settlement Service should be able to submit trading tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_settlement_service_can_submit_energy_token_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let energy_prog: Pubkey = energy_token_program().parse().unwrap();
    let tx = create_mock_tx(energy_prog);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/settlement-service".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_ok(), "Settlement Service should be able to submit energy token tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_settlement_service_cannot_submit_oracle_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let oracle_program_id = oracle_program().parse().unwrap();
    let tx = create_mock_tx(oracle_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/settlement-service".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_err(), "Settlement Service should NOT be able to submit oracle tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_settlement_service_cannot_submit_registry_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    let registry_program_id = registry_program().parse().unwrap();
    let tx = create_mock_tx(registry_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/settlement-service".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_err(), "Settlement Service should NOT be able to submit registry tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_reporting_service_cannot_submit_any_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    // Even the system program (allowed for every signing role) is rejected:
    // the read-only deny arm fires before the per-instruction allowlist.
    let sys_program_id: Pubkey = "11111111111111111111111111111111".parse().unwrap();
    let tx = create_mock_tx(sys_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/reporting-service".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_err(), "Reporting Service must never submit transactions");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_api_gateway_cannot_submit_any_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault, cache);

    // Defense in depth below the gRPC RBAC layer: this gate is what blocks the
    // gateway identity on the NATS path, where the consumer only screens Unknown.
    let sys_program_id: Pubkey = "11111111111111111111111111111111".parse().unwrap();
    let tx = create_mock_tx(sys_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/apisix".to_string());

    let result = core.sign_and_submit(&serialized, "platform_admin", &identity, "").await;
    assert!(result.is_err(), "API Gateway must never submit transactions");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}
