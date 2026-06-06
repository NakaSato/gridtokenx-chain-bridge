//! Solana client adapters — relocated from the root crate's `api/provider.rs`.
//!
//! Keeps the rich read-side [`SolanaProvider`] trait (12 methods used by the
//! gRPC read handlers) plus [`RealSolanaProvider`], the LiteSVM-backed
//! [`SurfpoolSolanaProvider`], and [`BlockhashCache`]. Additionally implements
//! the minimal [`chain_bridge_core::ChainClientPort`] (blockhash + submit) so
//! the logic-layer submit saga can depend only on `chain-bridge-core`.

use async_trait::async_trait;
use solana_client::client_error::ClientError;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_response::{Response, RpcPrioritizationFee, RpcSimulateTransactionResult};
use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use chain_bridge_core::{ChainBridgeError, ChainClientPort, SubmitOutcome};

pub struct BlockhashCache {
    current: RwLock<Option<(Hash, u64)>>,
}

impl Default for BlockhashCache {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockhashCache {
    pub fn new() -> Self {
        Self {
            current: RwLock::new(None),
        }
    }

    pub async fn update(&self, hash: Hash, last_valid_block_height: u64) {
        let mut lock = self.current.write().await;
        *lock = Some((hash, last_valid_block_height));
    }

    pub async fn get(&self) -> Option<(Hash, u64)> {
        *self.current.read().await
    }
}

/// Abstraction layer for Solana RPC interactions to enable mocking in tests.
#[async_trait]
pub trait SolanaProvider: Send + Sync {
    async fn simulate_transaction(&self, transaction: &Transaction) -> Result<Response<RpcSimulateTransactionResult>, ClientError>;
    async fn send_transaction(&self, transaction: &Transaction) -> Result<Signature, ClientError>;
    async fn get_latest_blockhash(&self) -> Result<(Hash, u64), ClientError>;
    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ClientError>;
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Account, ClientError>;
    async fn get_recent_prioritization_fees(&self, pubkeys: &[Pubkey]) -> Result<Vec<RpcPrioritizationFee>, ClientError>;
    async fn get_token_account_balance(&self, pubkey: &Pubkey) -> Result<serde_json::Value, ClientError>;
    async fn get_signature_statuses(&self, signatures: &[Signature]) -> Result<Response<Vec<Option<serde_json::Value>>>, ClientError>;
    async fn get_slot(&self) -> Result<u64, ClientError>;
    async fn request_airdrop(&self, pubkey: &Pubkey, lamports: u64) -> Result<Signature, ClientError>;
    async fn get_transaction(&self, signature: &Signature) -> Result<serde_json::Value, ClientError>;
    async fn get_epoch_info(&self) -> Result<solana_sdk::epoch_info::EpochInfo, ClientError>;
}

/// Real implementation of SolanaProvider using RpcClient.
pub struct RealSolanaProvider {
    client: RpcClient,
}

impl RealSolanaProvider {
    pub fn new(rpc_url: String) -> Self {
        Self {
            client: RpcClient::new(rpc_url),
        }
    }
}

#[async_trait]
impl SolanaProvider for RealSolanaProvider {
    async fn simulate_transaction(&self, transaction: &Transaction) -> Result<Response<RpcSimulateTransactionResult>, ClientError> {
        info!("Simulating transaction...");
        for (i, msg) in transaction.message.instructions.iter().enumerate() {
            info!("Instruction {}: program_id_index={}", i, msg.program_id_index);
            for (j, acc_idx) in msg.accounts.iter().enumerate() {
                if let Some(pubkey) = transaction.message.account_keys.get(*acc_idx as usize) {
                    info!("  Account {}: {}", j, pubkey);
                }
            }
        }
        self.client.simulate_transaction(transaction).await
    }
    async fn send_transaction(&self, transaction: &Transaction) -> Result<Signature, ClientError> {
        self.client.send_transaction(transaction).await
    }
    async fn get_latest_blockhash(&self) -> Result<(Hash, u64), ClientError> {
        self.client.get_latest_blockhash_with_commitment(self.client.commitment()).await
    }
    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ClientError> {
        self.client.get_balance(pubkey).await
    }
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Account, ClientError> {
        self.client.get_account(pubkey).await
    }
    async fn get_recent_prioritization_fees(&self, pubkeys: &[Pubkey]) -> Result<Vec<RpcPrioritizationFee>, ClientError> {
        self.client.get_recent_prioritization_fees(pubkeys).await
    }
    async fn get_token_account_balance(&self, pubkey: &Pubkey) -> Result<serde_json::Value, ClientError> {
        self.client.get_token_account_balance(pubkey).await.map(|b| serde_json::to_value(b).unwrap())
    }
    async fn get_signature_statuses(&self, signatures: &[Signature]) -> Result<Response<Vec<Option<serde_json::Value>>>, ClientError> {
        let resp = self.client.get_signature_statuses(signatures).await?;
        let value = resp.value.into_iter().map(|opt| opt.map(|s| serde_json::to_value(s).unwrap())).collect();
        Ok(Response { context: resp.context, value })
    }
    async fn get_slot(&self) -> Result<u64, ClientError> {
        self.client.get_slot().await
    }
    async fn request_airdrop(&self, pubkey: &Pubkey, lamports: u64) -> Result<Signature, ClientError> {
        self.client.request_airdrop(pubkey, lamports).await
    }
    async fn get_transaction(&self, signature: &Signature) -> Result<serde_json::Value, ClientError> {
        self.client.get_transaction_with_config(
            signature,
            solana_client::rpc_config::RpcTransactionConfig {
                encoding: Some(solana_transaction_status::UiTransactionEncoding::Json),
                commitment: Some(self.client.commitment()),
                max_supported_transaction_version: Some(0),
            }
        ).await.map(|tx| serde_json::to_value(tx).unwrap())
    }
    async fn get_epoch_info(&self) -> Result<solana_sdk::epoch_info::EpochInfo, ClientError> {
        let info = self.client.get_epoch_info().await?;
        Ok(solana_sdk::epoch_info::EpochInfo {
            absolute_slot: info.absolute_slot,
            block_height: info.block_height,
            epoch: info.epoch,
            slots_in_epoch: info.slots_in_epoch,
            slot_index: info.slot_index,
            transaction_count: info.transaction_count,
        })
    }
}

use litesvm::LiteSVM;

/// Implementation of SolanaProvider using LiteSVM for in-memory simulation.
/// It can lazily fetch accounts from a real RPC to simulate mainnet state.
pub struct SurfpoolSolanaProvider {
    svm: Arc<RwLock<LiteSVM>>,
    rpc_client: Option<RpcClient>,
}

impl SurfpoolSolanaProvider {
    pub async fn new(rpc_url: Option<String>) -> anyhow::Result<Self> {
        let svm = LiteSVM::new();
        let rpc_client = rpc_url.map(RpcClient::new);

        info!("🌊 Surfpool (LiteSVM) initialized");
        if rpc_client.is_some() {
            info!("🔗 Mainnet state cloning enabled via RPC");
        }

        let mut provider = Self {
            svm: Arc::new(RwLock::new(svm)),
            rpc_client,
        };

        provider.deploy_core_programs().await?;

        Ok(provider)
    }

    async fn deploy_core_programs(&mut self) -> anyhow::Result<()> {
        let mut svm = self.svm.write().await;

        let programs = [
            ("HZR6b8GhzhDowyL6dX58qBjdSDNtFyJHU5dPF3kXDcTS", "gridtokenx-anchor/target/deploy/registry.so"),
            ("GjSjmPt8VSHr49ti4BijWZSu7rwb8o32pod7gNBnTY4U", "gridtokenx-anchor/target/deploy/energy_token.so"),
            ("DXxHdUar3pUUKRnt4XAMA8rdYRpAsNY1xk3Zo4crShvY", "gridtokenx-anchor/target/deploy/trading.so"),
            ("AiWcoPDEk3G4iKrDXj1wCN1ffWxQDEsgtJZKcjauoFJr", "gridtokenx-anchor/target/deploy/oracle.so"),
            ("6FsfuFEg8LHjSiejc8om8Q6iSaAgfEWHCgz78PT8jocw", "gridtokenx-anchor/target/deploy/governance.so"),
        ];

        for (id_str, path) in programs {
            let pubkey = Pubkey::from_str(id_str)?;
            let address = solana_address::Address::from(pubkey.to_bytes());

            if let Ok(program_data) = std::fs::read(path) {
                info!("🚀 Deploying program {} from {} to LiteSVM", id_str, path);
                let _ = svm.add_program(address, &program_data);
            } else {
                warn!("⚠️ Could not find program SO at {}. Skipping deployment for {}.", path, id_str);
            }
        }

        // --- PDA CALCULATIONS ---
        let auth_pubkey = Pubkey::from_str("EzudwoHvNPAc4dpPi5ndU8MEZVHVzq3Pj3Thm9ooKmiJ")?;
        let registry_program_id = Pubkey::from_str("HZR6b8GhzhDowyL6dX58qBjdSDNtFyJHU5dPF3kXDcTS")?;
        let energy_program_id = Pubkey::from_str("GjSjmPt8VSHr49ti4BijWZSu7rwb8o32pod7gNBnTY4U")?;

        let (registry_pda, _) = Pubkey::find_program_address(&[b"registry"], &registry_program_id);
        let (token_info_pda, _) = Pubkey::find_program_address(&[b"token_info_2022"], &energy_program_id);

        // --- TOKEN MINTS ---
        let token_program_id = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")?;
        let token_program_address = solana_address::Address::from(token_program_id.to_bytes());

        // Initialize Energy Token Mint (GpGDVgksF2ivMv3XXR4VZDXRmW9G6agA2AGkKUBQRzk6)
        let mint_pubkey = Pubkey::from_str("GpGDVgksF2ivMv3XXR4VZDXRmW9G6agA2AGkKUBQRzk6")?;
        let mint_address = solana_address::Address::from(mint_pubkey.to_bytes());

        let mut data = vec![0u8; 82];
        data[0..4].copy_from_slice(&1u32.to_le_bytes()); // COption::Some
        data[4..36].copy_from_slice(&token_info_pda.to_bytes()); // Authority MUST be token_info_pda
        data[44] = 9;
        data[45] = 1;

        let mint_account = solana_account::Account {
            lamports: 1_000_000_000,
            data,
            owner: token_program_address,
            executable: false,
            rent_epoch: 0,
        };
        let _ = svm.set_account(mint_address, mint_account);
        info!("🪙 Initialized Energy Token Mint in LiteSVM: {}. Authority set to TokenInfo PDA: {}", mint_pubkey, token_info_pda);

        // Initialize Currency Token Mint (8BGFtQLRaY9Nh5BGUwjJvdeXEsscCgJAi5zTgALk1Vg5)
        let currency_pubkey = Pubkey::from_str("8BGFtQLRaY9Nh5BGUwjJvdeXEsscCgJAi5zTgALk1Vg5")?;
        let currency_address = solana_address::Address::from(currency_pubkey.to_bytes());

        let mut currency_data = vec![0u8; 82];
        currency_data[0..4].copy_from_slice(&1u32.to_le_bytes()); // COption::Some
        currency_data[4..36].copy_from_slice(&auth_pubkey.to_bytes()); // Admin is okay for currency
        currency_data[44] = 6;
        currency_data[45] = 1;

        let currency_account = solana_account::Account {
            lamports: 1_000_000_000,
            data: currency_data,
            owner: token_program_address,
            executable: false,
            rent_epoch: 0,
        };
        let _ = svm.set_account(currency_address, currency_account);
        info!("🪙 Initialized Currency Token Mint in LiteSVM: {}", currency_pubkey);

        // --- REGISTRY ACCOUNTS ---
        let registry_program_address = solana_address::Address::from(registry_program_id.to_bytes());
        let registry_address = solana_address::Address::from(registry_pda.to_bytes());

        let mut registry_data = vec![0u8; 104];
        registry_data[0..8].copy_from_slice(&[47, 174, 110, 246, 184, 182, 252, 218]); // account:Registry
        registry_data[8..40].copy_from_slice(&auth_pubkey.to_bytes());
        let registry_account = solana_account::Account {
            lamports: 1_000_000_000,
            data: registry_data,
            owner: registry_program_address,
            executable: false,
            rent_epoch: 0,
        };
        let _ = svm.set_account(registry_address, registry_account);
        info!("📋 Initialized Registry PDA in LiteSVM: {}", registry_pda);

        let (shard_pda, _) = Pubkey::find_program_address(&[b"registry_shard", &[0u8]], &registry_program_id);
        let shard_address = solana_address::Address::from(shard_pda.to_bytes());
        let mut shard_data = vec![0u8; 32];
        shard_data[0..8].copy_from_slice(&[98, 27, 121, 8, 54, 239, 169, 252]); // account:RegistryShard
        let shard_account = solana_account::Account {
            lamports: 1_000_000_000,
            data: shard_data,
            owner: registry_program_address,
            executable: false,
            rent_epoch: 0,
        };
        let _ = svm.set_account(shard_address, shard_account);
        info!("📋 Initialized RegistryShard 0 in LiteSVM: {}", shard_pda);

        // --- ENERGY TOKEN ACCOUNTS ---
        let energy_program_address = solana_address::Address::from(energy_program_id.to_bytes());
        let token_info_address = solana_address::Address::from(token_info_pda.to_bytes());
        let mut token_info_data = vec![0u8; 320];
        token_info_data[0..8].copy_from_slice(&[109, 162, 52, 125, 77, 166, 37, 202]); // account:TokenInfo
        token_info_data[8..40].copy_from_slice(&auth_pubkey.to_bytes());
        token_info_data[40..72].copy_from_slice(&registry_pda.to_bytes());
        token_info_data[72..104].copy_from_slice(&registry_program_id.to_bytes());
        token_info_data[104..136].copy_from_slice(&mint_pubkey.to_bytes());
        let token_info_account = solana_account::Account {
            lamports: 1_000_000_000,
            data: token_info_data,
            owner: energy_program_address,
            executable: false,
            rent_epoch: 0,
        };
        let _ = svm.set_account(token_info_address, token_info_account);
        info!("📋 Initialized TokenInfo PDA in LiteSVM: {}", token_info_pda);

        Ok(())
    }

    async fn ensure_account(&self, pubkey: &Pubkey) -> Result<(), ClientError> {
        let mut svm = self.svm.write().await;
        let address = solana_address::Address::from(pubkey.to_bytes());
        if svm.get_account(&address).is_none()
            && let Some(ref client) = self.rpc_client
        {
            info!("🔍 Cloning account {} from mainnet...", pubkey);
            let account = client.get_account(pubkey).await?;
            let svm_account = solana_account::Account {
                lamports: account.lamports,
                data: account.data,
                owner: solana_address::Address::from(account.owner.to_bytes()),
                executable: account.executable,
                rent_epoch: account.rent_epoch,
            };
            svm.set_account(address, svm_account)
                .map_err(|e| ClientError::from(solana_client::client_error::ClientErrorKind::Custom(e.to_string())))?;
        }
        Ok(())
    }
}

#[async_trait]
impl SolanaProvider for SurfpoolSolanaProvider {
    async fn simulate_transaction(&self, transaction: &Transaction) -> Result<Response<RpcSimulateTransactionResult>, ClientError> {
        // Ensure all accounts in the transaction are cloned if needed
        for message in &transaction.message.instructions {
            for key_idx in &message.accounts {
                if let Some(key) = transaction.message.account_keys.get(*key_idx as usize) {
                    self.ensure_account(key).await?;
                }
            }
        }

        let svm = self.svm.write().await;
        let sdk_vtx = solana_sdk::transaction::VersionedTransaction::from(transaction.clone());
        let bytes = bincode::serialize(&sdk_vtx).map_err(|e| ClientError::from(solana_client::client_error::ClientErrorKind::Custom(e.to_string())))?;
        let versioned_tx: solana_transaction::versioned::VersionedTransaction = bincode::deserialize(&bytes)
            .map_err(|e| ClientError::from(solana_client::client_error::ClientErrorKind::Custom(e.to_string())))?;
        let result = svm.simulate_transaction(versioned_tx)
            .map_err(|e| ClientError::from(solana_client::client_error::ClientErrorKind::Custom(format!("{:?}", e))))?;

        Ok(Response {
            context: solana_client::rpc_response::RpcResponseContext { slot: 0, api_version: None },
            value: RpcSimulateTransactionResult {
                err: None, // If we are here, simulation succeeded
                logs: Some(result.meta.logs.clone()),
                accounts: None,
                units_consumed: Some(result.meta.compute_units_consumed),
                return_data: None,
                inner_instructions: None,
                loaded_accounts_data_size: None,
                replacement_blockhash: None,
            },
        })
    }

    async fn send_transaction(&self, transaction: &Transaction) -> Result<Signature, ClientError> {
        // Ensure payer exists and has SOL before sending in simnet
        if let Some(payer) = transaction.message.account_keys.first() {
            let mut svm = self.svm.write().await;
            let address = solana_address::Address::from(payer.to_bytes());
            let balance = svm.get_balance(&address).unwrap_or(0);
            if balance < 1_000_000_000 {
                info!("💸 Ensuring payer {} has SOL (current balance: {}). Airdropping 10 SOL...", payer, balance);
                svm.airdrop(&address, 10_000_000_000)
                    .map_err(|e| ClientError::from(solana_client::client_error::ClientErrorKind::Custom(format!("{:?}", e))))?;
            }
        }

        let mut svm = self.svm.write().await;
        let signature = transaction.signatures.first().cloned().unwrap_or_default();

        let sdk_vtx = solana_sdk::transaction::VersionedTransaction::from(transaction.clone());
        let bytes = bincode::serialize(&sdk_vtx).map_err(|e| ClientError::from(solana_client::client_error::ClientErrorKind::Custom(e.to_string())))?;
        let versioned_tx: solana_transaction::versioned::VersionedTransaction = bincode::deserialize(&bytes)
            .map_err(|e| ClientError::from(solana_client::client_error::ClientErrorKind::Custom(e.to_string())))?;

        svm.send_transaction(versioned_tx)
            .map_err(|e| ClientError::from(solana_client::client_error::ClientErrorKind::Custom(format!("{:?}", e))))?;

        Ok(signature)
    }

    async fn get_latest_blockhash(&self) -> Result<(Hash, u64), ClientError> {
        let svm = self.svm.read().await;
        let hash = svm.latest_blockhash();
        Ok((Hash::new_from_array(hash.to_bytes()), 1000))
    }

    async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, ClientError> {
        self.ensure_account(pubkey).await?;
        let svm = self.svm.read().await;
        let address = solana_address::Address::from(pubkey.to_bytes());
        Ok(svm.get_balance(&address).unwrap_or(0))
    }

    async fn get_account(&self, pubkey: &Pubkey) -> Result<Account, ClientError> {
        self.ensure_account(pubkey).await?;
        let svm = self.svm.read().await;
        let address = solana_address::Address::from(pubkey.to_bytes());
        let account = svm.get_account(&address)
            .ok_or_else(|| ClientError::from(solana_client::client_error::ClientErrorKind::Custom("Account not found".to_string())))?;

        Ok(Account {
            lamports: account.lamports,
            data: account.data,
            owner: Pubkey::new_from_array(account.owner.to_bytes()),
            executable: account.executable,
            rent_epoch: account.rent_epoch,
        })
    }

    async fn get_recent_prioritization_fees(&self, _pubkeys: &[Pubkey]) -> Result<Vec<RpcPrioritizationFee>, ClientError> {
        Ok(vec![])
    }

    async fn get_token_account_balance(&self, _pubkey: &Pubkey) -> Result<serde_json::Value, ClientError> {
        Ok(serde_json::json!({"amount": "1000000000", "decimals": 9, "uiAmount": 1.0}))
    }

    async fn get_signature_statuses(&self, signatures: &[Signature]) -> Result<Response<Vec<Option<serde_json::Value>>>, ClientError> {
        let values = signatures.iter().map(|_| Some(serde_json::json!({"slot": 0, "confirmations": 1, "err": null, "confirmationStatus": "finalized"}))).collect();
        Ok(Response {
            context: solana_client::rpc_response::RpcResponseContext { slot: 0, api_version: None },
            value: values,
        })
    }

    async fn get_slot(&self) -> Result<u64, ClientError> {
        Ok(0)
    }

    async fn request_airdrop(&self, pubkey: &Pubkey, lamports: u64) -> Result<Signature, ClientError> {
        let mut svm = self.svm.write().await;
        let address = solana_address::Address::from(pubkey.to_bytes());
        svm.airdrop(&address, lamports)
            .map_err(|e| ClientError::from(solana_client::client_error::ClientErrorKind::Custom(format!("{:?}", e))))?;
        Ok(Signature::default())
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

// --- ChainClientPort bridge -------------------------------------------------
// Minimal write/blockhash surface the submit saga depends on. Implemented per
// concrete provider (the orphan rule forbids a blanket impl of core's foreign
// trait over an uncovered generic). `slot` is 0 to match the existing submit
// path (send_transaction returns no slot).

async fn port_latest_blockhash<P: SolanaProvider + ?Sized>(p: &P) -> Result<(Hash, u64), ChainBridgeError> {
    SolanaProvider::get_latest_blockhash(p)
        .await
        .map_err(|e| ChainBridgeError::ChainClient(e.to_string()))
}

async fn port_send_transaction<P: SolanaProvider + ?Sized>(p: &P, tx: &Transaction) -> Result<SubmitOutcome, ChainBridgeError> {
    let signature = SolanaProvider::send_transaction(p, tx)
        .await
        .map_err(|e| ChainBridgeError::ChainClient(e.to_string()))?;
    Ok(SubmitOutcome { signature, slot: 0 })
}

#[async_trait]
impl ChainClientPort for RealSolanaProvider {
    async fn latest_blockhash(&self) -> Result<(Hash, u64), ChainBridgeError> {
        port_latest_blockhash(self).await
    }
    async fn send_transaction(&self, tx: &Transaction) -> Result<SubmitOutcome, ChainBridgeError> {
        port_send_transaction(self, tx).await
    }
}

#[async_trait]
impl ChainClientPort for SurfpoolSolanaProvider {
    async fn latest_blockhash(&self) -> Result<(Hash, u64), ChainBridgeError> {
        port_latest_blockhash(self).await
    }
    async fn send_transaction(&self, tx: &Transaction) -> Result<SubmitOutcome, ChainBridgeError> {
        port_send_transaction(self, tx).await
    }
}
