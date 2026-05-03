use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use anyhow::{Context, anyhow};
use tracing::{info, debug};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use async_trait::async_trait;

#[async_trait]
pub trait VaultProvider: Send + Sync {
    async fn get_public_key(&self, key_name: &str) -> anyhow::Result<solana_sdk::pubkey::Pubkey>;
    async fn sign_message(&self, key_name: &str, message: &[u8]) -> anyhow::Result<solana_sdk::signature::Signature>;
}

#[derive(Clone)]
pub struct VaultTransitClient {
    client: reqwest::Client,
    address: String,
    token: String,
    key_cache: Arc<RwLock<HashMap<String, solana_sdk::pubkey::Pubkey>>>,
}

#[derive(Serialize)]
struct TransitSignRequest {
    input: String, // base64
}

#[derive(Deserialize)]
struct TransitSignResponse {
    data: TransitSignData,
}

#[derive(Deserialize)]
struct TransitSignData {
    signature: String,
}

#[derive(Deserialize)]
struct TransitKeyResponse {
    data: TransitKeyData,
}

#[derive(Deserialize)]
struct TransitKeyData {
    keys: HashMap<String, TransitSubKey>,
}

#[derive(Deserialize)]
struct TransitSubKey {
    public_key: String, // base64
}

#[async_trait]
impl VaultProvider for VaultTransitClient {
    async fn get_public_key(&self, key_name: &str) -> anyhow::Result<solana_sdk::pubkey::Pubkey> {
        {
            let cache = self.key_cache.read().await;
            if let Some(pk) = cache.get(key_name) {
                return Ok(*pk);
            }
        }

        info!("🔑 Fetching public key for '{}' from Vault Transit", key_name);
        let resp = self.client.get(format!("{}/v1/transit/keys/{}", self.address, key_name))
            .header("X-Vault-Token", &self.token)
            .send()
            .await?
            .error_for_status()
            .context("Failed to fetch key from Vault")?;

        let data: TransitKeyResponse = resp.json().await?;
        
        // Vault returns keys as versions. We just take the latest or iterate.
        // Usually, for ed25519, there's a base64 public_key in the response.
        let pubkey_b64 = data.data.keys.values()
            .next()
            .ok_or_else(|| anyhow!("No keys found for {}", key_name))?
            .public_key.clone();

        let pubkey_bytes = general_purpose::STANDARD.decode(pubkey_b64)
            .context("Invalid base64 in public_key")?;
        
        let pubkey = solana_sdk::pubkey::Pubkey::try_from(pubkey_bytes)
            .map_err(|_| anyhow!("Invalid public key bytes length"))?;

        let mut cache = self.key_cache.write().await;
        cache.insert(key_name.to_string(), pubkey);
        
        Ok(pubkey)
    }

    async fn sign_message(&self, key_name: &str, message: &[u8]) -> anyhow::Result<solana_sdk::signature::Signature> {
        debug!("🖋️ Requesting Vault signature for key '{}'", key_name);
        
        let req = TransitSignRequest {
            input: general_purpose::STANDARD.encode(message),
        };

        let resp = self.client.post(format!("{}/v1/transit/sign/{}", self.address, key_name))
            .header("X-Vault-Token", &self.token)
            .json(&req)
            .send()
            .await?
            .error_for_status()
            .context("Vault Transit sign request failed")?;

        let data: TransitSignResponse = resp.json().await?;
        
        // Vault signature format is "vault:v1:<base64>"
        let sig_parts: Vec<&str> = data.data.signature.split(':').collect();
        let sig_b64 = sig_parts.last()
            .ok_or_else(|| anyhow!("Malformed signature format from Vault"))?;

        let sig_bytes = general_purpose::STANDARD.decode(sig_b64)
            .context("Invalid base64 in signature")?;

        let signature = solana_sdk::signature::Signature::try_from(sig_bytes)
            .map_err(|_| anyhow!("Invalid signature length from Vault"))?;

        Ok(signature)
    }
}

impl VaultTransitClient {
    pub fn new(address: String, token: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            address,
            token,
            key_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

/// A development-only provider that uses a local Ed25519 keypair.
/// Used when CHAIN_BRIDGE_INSECURE=true.
pub struct InsecureKeypairProvider {
    keypair: solana_sdk::signature::Keypair,
}

impl InsecureKeypairProvider {
    pub fn new() -> Self {
        // We use the infra/solana/dev-wallet.json keypair for local simulation
        let pk_bytes: [u8; 64] = [
            241, 3, 15, 11, 59, 189, 0, 251, 20, 183, 69, 181, 3, 24, 241, 148, 
            23, 179, 177, 88, 214, 187, 29, 157, 2, 66, 127, 53, 53, 185, 21, 209, 
            207, 253, 141, 144, 58, 192, 105, 53, 193, 102, 73, 89, 250, 146, 246, 181, 
            133, 48, 6, 16, 231, 20, 229, 155, 54, 191, 88, 204, 36, 39, 161, 251
        ];
        let keypair = solana_sdk::signature::Keypair::from_bytes(&pk_bytes).expect("Invalid dev keypair");
        info!("⚠️ Using InsecureKeypairProvider with pubkey: {}", solana_sdk::signer::Signer::pubkey(&keypair));
        Self { keypair }
    }
}

#[async_trait]
impl VaultProvider for InsecureKeypairProvider {
    async fn get_public_key(&self, _key_name: &str) -> anyhow::Result<solana_sdk::pubkey::Pubkey> {
        Ok(solana_sdk::signer::Signer::pubkey(&self.keypair))
    }

    async fn sign_message(&self, _key_name: &str, message: &[u8]) -> anyhow::Result<solana_sdk::signature::Signature> {
        use solana_sdk::signer::Signer;
        Ok(self.keypair.sign_message(message))
    }
}
