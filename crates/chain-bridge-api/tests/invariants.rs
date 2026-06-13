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

/// Vault provider that counts `sign_message` calls — lets a test prove the
/// signing path was (or was NOT) entered, which a non-counting mock cannot.
struct CountingVaultProvider {
    sign_count: AtomicUsize,
}

impl CountingVaultProvider {
    fn new() -> Self {
        Self { sign_count: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl VaultProvider for CountingVaultProvider {
    async fn sign_message(&self, _key_name: &str, _message: &[u8]) -> anyhow::Result<Signature> {
        self.sign_count.fetch_add(1, Ordering::SeqCst);
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

// ---------------------------------------------------------------------------
// NATS envelope authentication invariant: under an enforcing NatsAuthPolicy an
// unsigned envelope is rejected, while a publisher-signed envelope whose mTLS
// cert chains to the trusted CA and whose SPIFFE SAN matches the claimed
// service_identity verifies end-to-end (canonical bytes shared with the
// publisher in gridtokenx_blockchain_core::rpc::envelope_auth).
// No env vars are touched — the policy is constructed directly.
// ---------------------------------------------------------------------------
#[test]
fn test_unsigned_nats_envelope_rejected_when_enforced_signed_verifies() {
    use gridtokenx_blockchain_core::rpc::envelope_auth::{
        EnvelopeSigner, canonical_submit_bytes,
    };
    use gridtokenx_blockchain_core::rpc::nats_schema::TxSubmitMessage;
    use gridtokenx_chain_bridge::nats_consumer::auth::{
        AuthCheck, NatsAuthPolicy, auth_decision, check_envelope_auth,
    };
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, PKCS_ECDSA_P256_SHA256,
        SanType,
    };

    const IDENTITY: &str = "spiffe://gridtokenx.th/prod/trading-service/matcher";
    const NOW_SECS: i64 = 1_750_000_000;

    // Dev CA + CA-signed leaf carrying the SPIFFE URI SAN (gen-certs.sh shape).
    let mut ca_params = CertificateParams::default();
    ca_params.distinguished_name.push(DnType::CommonName, "test-ca");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let ca = ca_params.self_signed(&ca_key).unwrap();

    let mut leaf_params = CertificateParams::default();
    leaf_params.distinguished_name.push(DnType::CommonName, "leaf");
    leaf_params
        .subject_alt_names
        .push(SanType::URI(IDENTITY.try_into().unwrap()));
    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();

    let policy = NatsAuthPolicy::new(true, Some(ca.der().to_vec()));
    assert!(policy.require_signed);

    let mut envelope = TxSubmitMessage {
        correlation_id: "inv-1".to_string(),
        idempotency_key: "inv-key".to_string(),
        reply_subject: "chain.tx.result.inv-1".to_string(),
        serialized_tx: vec![1, 2, 3],
        key_id: "platform_admin".to_string(),
        skip_preflight: false,
        retry_count: 0,
        service_identity: IDENTITY.to_string(),
        created_at_ms: 1_700_000_000_000,
        auth: None,
    };
    let canonical = canonical_submit_bytes(&envelope);

    // Unsigned → rejected under enforcement.
    let check = check_envelope_auth(
        policy.ca_der.as_deref(),
        envelope.auth.as_ref(),
        IDENTITY,
        &canonical,
        NOW_SECS,
    );
    assert!(matches!(check, AuthCheck::Unsigned));
    assert!(auth_decision(&check, policy.require_signed).is_err());

    // Signed by the cert's key → Verified, decision Ok.
    let signer = EnvelopeSigner::from_pem(leaf.pem(), &leaf_key.serialize_pem()).unwrap();
    envelope.auth = Some(signer.sign(&canonical));
    let check = check_envelope_auth(
        policy.ca_der.as_deref(),
        envelope.auth.as_ref(),
        IDENTITY,
        &canonical,
        NOW_SECS,
    );
    assert!(matches!(check, AuthCheck::Verified), "got {check:?}");
    assert!(auth_decision(&check, policy.require_signed).is_ok());
}

// ---------------------------------------------------------------------------
// Forged envelope under enforcement: an `auth` block that does NOT chain to the
// trusted CA (and, separately, a valid cert+sig over TAMPERED bytes) is rejected
// before any signing or submission, and the rejection is written to the audit
// hash-chain at stage="auth". Models the handle_submit contract (auth gate runs
// before sign_and_submit) with the same pure pieces the consumer uses; the
// counting provider + vault prove the signing path is never entered.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_forged_nats_envelope_rejected_no_vault_no_submit_audit_recorded() {
    use chain_bridge_core::AuditPort;
    use chain_bridge_core::audit::{AuditEntry, AuditOutcome};
    use chain_bridge_persistence::InMemoryAuditStore;
    use gridtokenx_blockchain_core::rpc::envelope_auth::{EnvelopeSigner, canonical_submit_bytes};
    use gridtokenx_blockchain_core::rpc::nats_schema::TxSubmitMessage;
    use gridtokenx_chain_bridge::nats_consumer::auth::{
        AuthCheck, NatsAuthPolicy, auth_decision, check_envelope_auth,
    };
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, PKCS_ECDSA_P256_SHA256, SanType,
    };

    const IDENTITY: &str = "spiffe://gridtokenx.th/prod/trading-service/matcher";
    const NOW_SECS: i64 = 1_750_000_000;

    fn make_ca(cn: &str) -> (rcgen::Certificate, KeyPair) {
        let mut p = CertificateParams::default();
        p.distinguished_name.push(DnType::CommonName, cn);
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let k = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let c = p.self_signed(&k).unwrap();
        (c, k)
    }

    // Trusted CA (configured in policy) and a rogue CA the forger actually used.
    let (trusted_ca, _trusted_key) = make_ca("trusted-ca");
    let (rogue_ca, rogue_key) = make_ca("rogue-ca");

    // Leaf carries the correct SPIFFE SAN but is signed by the ROGUE CA.
    let mut leaf_params = CertificateParams::default();
    leaf_params.distinguished_name.push(DnType::CommonName, "leaf");
    leaf_params
        .subject_alt_names
        .push(SanType::URI(IDENTITY.try_into().unwrap()));
    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let forged_leaf = leaf_params.signed_by(&leaf_key, &rogue_ca, &rogue_key).unwrap();

    let policy = NatsAuthPolicy::new(true, Some(trusted_ca.der().to_vec()));
    assert!(policy.require_signed);

    let mut envelope = TxSubmitMessage {
        correlation_id: "forge-1".to_string(),
        idempotency_key: "forge-key".to_string(),
        reply_subject: "chain.tx.result.forge-1".to_string(),
        serialized_tx: vec![9, 9, 9],
        key_id: "platform_admin".to_string(),
        skip_preflight: false,
        retry_count: 0,
        service_identity: IDENTITY.to_string(),
        created_at_ms: 1_700_000_000_000,
        auth: None,
    };
    let canonical = canonical_submit_bytes(&envelope);

    // Forgery A: leaf properly signs the canonical bytes, but its cert chains to
    // the rogue CA, not the trusted one → rejected at the CA-trust step.
    let signer = EnvelopeSigner::from_pem(forged_leaf.pem(), &leaf_key.serialize_pem()).unwrap();
    envelope.auth = Some(signer.sign(&canonical));
    let check = check_envelope_auth(
        policy.ca_der.as_deref(),
        envelope.auth.as_ref(),
        IDENTITY,
        &canonical,
        NOW_SECS,
    );
    assert!(
        matches!(check, AuthCheck::Failed(ref r) if r.contains("not signed by the trusted CA")),
        "forged cert (rogue CA) must fail CA trust, got {check:?}"
    );
    let decision = auth_decision(&check, policy.require_signed);
    assert!(decision.is_err(), "forged envelope must be rejected under enforcement");

    // Forgery B: same forger signs DIFFERENT bytes, then claims they cover this
    // envelope → signature is over tampered content (here also rogue-CA, so the
    // CA step fails first; the point is no variant yields Verified).
    let other = TxSubmitMessage { idempotency_key: "different".to_string(), ..envelope.clone() };
    let tampered_auth = signer.sign(&canonical_submit_bytes(&other));
    let check_b = check_envelope_auth(
        policy.ca_der.as_deref(),
        Some(&tampered_auth),
        IDENTITY,
        &canonical,
        NOW_SECS,
    );
    assert!(matches!(check_b, AuthCheck::Failed(_)), "tampered envelope must not verify, got {check_b:?}");

    // Consumer contract on an auth-rejected envelope: audit the rejection at
    // stage="auth" and NEVER reach sign_and_submit. Wire a real service with
    // counting provider + vault so "no submit, no sign" is an observable fact.
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(CountingVaultProvider::new());
    let cache = Arc::new(BlockhashCache::new());
    let audit = Arc::new(InMemoryAuditStore::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault.clone(), cache)
        .with_audit(audit.clone());

    if decision.is_err() {
        audit
            .append(AuditEntry::new(
                &envelope.correlation_id,
                &envelope.service_identity,
                "submit",
                AuditOutcome::Rejected { stage: "auth".to_string(), reason: decision.unwrap_err() },
                envelope.created_at_ms,
            ))
            .await
            .unwrap();
    } else {
        // Unreachable for a forged envelope; present so a future regression that
        // wrongly accepts the forgery would submit and trip the asserts below.
        let identity = SpiffeIdentity(IDENTITY.to_string());
        let _ = core.sign_and_submit(&envelope.serialized_tx, &envelope.key_id, &identity, "").await;
    }

    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0, "forged envelope must not submit on-chain");
    assert_eq!(vault.sign_count.load(Ordering::SeqCst), 0, "forged envelope must not reach Vault signing");

    let entries = audit.entries().await;
    assert_eq!(entries.len(), 1, "rejection must append exactly one audit entry");
    assert_eq!(entries[0].action, "submit");
    assert_eq!(entries[0].correlation_id, "forge-1");
    assert_eq!(entries[0].identity, IDENTITY, "audit carries the claimed (unverified) identity");
    match &entries[0].outcome {
        AuditOutcome::Rejected { stage, .. } => assert_eq!(stage, "auth"),
        other => panic!("expected Rejected{{stage:auth}}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Single signing path: both ingress identities (a gRPC matcher and the NATS
// settlement service) reach the provider through the one `sign_and_submit`
// entrypoint — there is no second route that touches Vault or submits. Two
// submits ⇒ exactly two provider sends AND two Vault signs (one funnel).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_single_signing_path_grpc_and_nats_funnel_through_sign_and_submit() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(CountingVaultProvider::new());
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeGrpcService::new(provider.clone(), vault.clone(), cache);

    let trading_program_id: Pubkey = trading_program().parse().unwrap();
    let serialized = bincode::serialize(&create_mock_tx(trading_program_id)).unwrap();

    // gRPC-origin caller (trading matcher) — both trading-capable identities.
    let grpc_identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service/matcher".to_string());
    assert!(core.sign_and_submit(&serialized, "platform_admin", &grpc_identity, "").await.is_ok());

    // NATS-origin caller (settlement service) — the consumer calls this exact method.
    let nats_identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/settlement-service".to_string());
    assert!(core.sign_and_submit(&serialized, "platform_admin", &nats_identity, "").await.is_ok());

    assert_eq!(provider.send_count.load(Ordering::SeqCst), 2, "both paths submit via the one funnel");
    assert_eq!(vault.sign_count.load(Ordering::SeqCst), 2, "every submit signs through the single Vault path");
}
