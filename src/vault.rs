use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use anyhow::{Context, anyhow};
use tracing::{info, debug};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

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

impl VaultTransitClient {
    pub fn new(address: String, token: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            address,
            token,
            key_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_public_key(&self, key_name: &str) -> anyhow::Result<solana_sdk::pubkey::Pubkey> {
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

    pub async fn sign_message(&self, key_name: &str, message: &[u8]) -> anyhow::Result<solana_sdk::signature::Signature> {
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
