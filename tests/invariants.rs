use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use solana_client::{
    client_error::ClientError,
    rpc_response::{Response, RpcPrioritizationFee, RpcResponseContext, RpcSimulateTransactionResult},
};
use solana_sdk::{
    account::Account, hash::Hash, pubkey::Pubkey, signature::{Signature, Keypair, Signer}, transaction::Transaction, message::Message, instruction::Instruction
};

use gridtokenx_chain_bridge::api::{BlockhashCache, ChainBridgeCore, SolanaProvider};
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

#[tokio::test]
async fn test_trading_service_can_submit_trading_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeCore::new(provider.clone(), vault, cache);

    // Trading program ID
    let trading_program_id = "DXxHdUar3pUUKRnt4XAMA8rdYRpAsNY1xk3Zo4crShvY".parse().unwrap();
    let tx = create_mock_tx(trading_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service".to_string());
    
    let result = core.sign_and_submit(&serialized, "platform_admin", &identity).await;
    assert!(result.is_ok(), "Trading matcher should be able to submit trading tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_trading_service_cannot_submit_oracle_tx() {
    let provider = Arc::new(MockSolanaProvider::new());
    let vault = Arc::new(MockVaultProvider);
    let cache = Arc::new(BlockhashCache::new());
    let core = ChainBridgeCore::new(provider.clone(), vault, cache);

    // Oracle program ID
    let oracle_program_id = "AiWcoPDEk3G4iKrDXj1wCN1ffWxQDEsgtJZKcjauoFJr".parse().unwrap();
    let tx = create_mock_tx(oracle_program_id);
    let serialized = bincode::serialize(&tx).unwrap();

    let identity = SpiffeIdentity("spiffe://gridtokenx.th/prod/trading-service".to_string());
    
    let result = core.sign_and_submit(&serialized, "platform_admin", &identity).await;
    assert!(result.is_err(), "Trading matcher should NOT be able to submit oracle tx");
    assert_eq!(provider.send_count.load(Ordering::SeqCst), 0);
}
