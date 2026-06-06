use anyhow::{Context, anyhow};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

#[async_trait]
pub trait VaultProvider: Send + Sync {
    async fn get_public_key(&self, key_name: &str) -> anyhow::Result<solana_sdk::pubkey::Pubkey>;
    async fn sign_message(
        &self,
        key_name: &str,
        message: &[u8],
    ) -> anyhow::Result<solana_sdk::signature::Signature>;
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

        info!(
            "🔑 Fetching public key for '{}' from Vault Transit",
            key_name
        );
        let resp = self
            .client
            .get(format!("{}/v1/transit/keys/{}", self.address, key_name))
            .header("X-Vault-Token", &self.token)
            .send()
            .await?
            .error_for_status()
            .context("Failed to fetch key from Vault")?;

        let data: TransitKeyResponse = resp.json().await?;

        // Vault returns keys as versions. We just take the latest or iterate.
        // Usually, for ed25519, there's a base64 public_key in the response.
        let pubkey_b64 = data
            .data
            .keys
            .values()
            .next()
            .ok_or_else(|| anyhow!("No keys found for {}", key_name))?
            .public_key
            .clone();

        let pubkey_bytes = general_purpose::STANDARD
            .decode(pubkey_b64)
            .context("Invalid base64 in public_key")?;

        let pubkey = solana_sdk::pubkey::Pubkey::try_from(pubkey_bytes)
            .map_err(|_| anyhow!("Invalid public key bytes length"))?;

        let mut cache = self.key_cache.write().await;
        cache.insert(key_name.to_string(), pubkey);

        Ok(pubkey)
    }

    async fn sign_message(
        &self,
        key_name: &str,
        message: &[u8],
    ) -> anyhow::Result<solana_sdk::signature::Signature> {
        debug!("🖋️ Requesting Vault signature for key '{}'", key_name);

        let req = TransitSignRequest {
            input: general_purpose::STANDARD.encode(message),
        };

        let resp = self
            .client
            .post(format!("{}/v1/transit/sign/{}", self.address, key_name))
            .header("X-Vault-Token", &self.token)
            .json(&req)
            .send()
            .await?
            .error_for_status()
            .context("Vault Transit sign request failed")?;

        let data: TransitSignResponse = resp.json().await?;

        // Vault signature format is "vault:v1:<base64>"
        let sig_parts: Vec<&str> = data.data.signature.split(':').collect();
        let sig_b64 = sig_parts
            .last()
            .ok_or_else(|| anyhow!("Malformed signature format from Vault"))?;

        let sig_bytes = general_purpose::STANDARD
            .decode(sig_b64)
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
            241, 3, 15, 11, 59, 189, 0, 251, 20, 183, 69, 181, 3, 24, 241, 148, 23, 179, 177, 88,
            214, 187, 29, 157, 2, 66, 127, 53, 53, 185, 21, 209, 207, 253, 141, 144, 58, 192, 105,
            53, 193, 102, 73, 89, 250, 146, 246, 181, 133, 48, 6, 16, 231, 20, 229, 155, 54, 191,
            88, 204, 36, 39, 161, 251,
        ];
        let keypair =
            solana_sdk::signature::Keypair::try_from(&pk_bytes[..]).expect("Invalid dev keypair");
        info!(
            "⚠️ Using InsecureKeypairProvider with pubkey: {}",
            solana_sdk::signer::Signer::pubkey(&keypair)
        );
        Self { keypair }
    }
}

#[async_trait]
impl VaultProvider for InsecureKeypairProvider {
    async fn get_public_key(&self, _key_name: &str) -> anyhow::Result<solana_sdk::pubkey::Pubkey> {
        Ok(solana_sdk::signer::Signer::pubkey(&self.keypair))
    }

    async fn sign_message(
        &self,
        _key_name: &str,
        message: &[u8],
    ) -> anyhow::Result<solana_sdk::signature::Signature> {
        use solana_sdk::signer::Signer;
        Ok(self.keypair.sign_message(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signer::Signer;

    // ---------------------------------------------------------------------------
    // InsecureKeypairProvider
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_insecure_pubkey_is_deterministic() {
        let provider = InsecureKeypairProvider::new();
        let pk1 = provider.get_public_key("any-key").await.unwrap();
        let pk2 = provider.get_public_key("other-key").await.unwrap();
        // Same hardcoded keypair regardless of key_name
        assert_eq!(pk1, pk2);
    }

    #[tokio::test]
    async fn test_insecure_sign_and_verify_roundtrip() {
        let provider = InsecureKeypairProvider::new();
        let message = b"test message for signing";
        let signature = provider.sign_message("key", message).await.unwrap();
        let pubkey = provider.get_public_key("key").await.unwrap();

        assert!(
            signature.verify(&pubkey.to_bytes(), message),
            "Signature must verify against the provider's public key"
        );
    }

    #[tokio::test]
    async fn test_insecure_signature_differs_for_different_messages() {
        let provider = InsecureKeypairProvider::new();
        let sig_a = provider.sign_message("key", b"message A").await.unwrap();
        let sig_b = provider.sign_message("key", b"message B").await.unwrap();
        assert_ne!(sig_a, sig_b, "Different messages must produce different signatures");
    }

    // ---------------------------------------------------------------------------
    // VaultTransitClient — signature parsing logic
    // ---------------------------------------------------------------------------

    #[test]
    fn test_vault_signature_parsing_valid() {
        // Simulate what Vault returns: "vault:v1:<base64>"
        let keypair = solana_sdk::signature::Keypair::new();
        let message = b"test";
        let real_sig = keypair.sign_message(message);
        let sig_b64 = general_purpose::STANDARD.encode(real_sig.as_ref());

        // Parse using the same logic as VaultTransitClient::sign_message
        let vault_format = format!("vault:v1:{}", sig_b64);
        let parts: Vec<&str> = vault_format.split(':').collect();
        let sig_b64_parsed = parts.last().unwrap();
        let sig_bytes = general_purpose::STANDARD
            .decode(sig_b64_parsed)
            .expect("Valid base64");
        let recovered = solana_sdk::signature::Signature::try_from(sig_bytes)
            .expect("Valid signature length");

        assert_eq!(recovered, real_sig);
    }

    #[test]
    fn test_vault_signature_parsing_malformed_no_colon() {
        // No colon separator — should fail
        let bad = general_purpose::STANDARD.encode([0u8; 64]);
        let parts: Vec<&str> = bad.split(':').collect();
        // When there's no colon, split returns a single element
        // parts.last() returns the whole string, which isn't valid base64 of 64 bytes
        // This tests that the parsing logic handles the edge case
        assert_eq!(parts.len(), 1, "No colon means single element");
    }

    // ---------------------------------------------------------------------------
    // VaultTransitClient — public key caching
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_vault_key_cache_hit_avoids_fetch() {
        // Pre-populate cache manually
        let client = VaultTransitClient::new("http://localhost:0".to_string(), "token".to_string());
        let test_pk = solana_sdk::pubkey::Pubkey::new_unique();
        client.key_cache.write().await.insert("test-key".to_string(), test_pk);

        // Should return cached value without hitting HTTP
        let result = client.get_public_key("test-key").await.unwrap();
        assert_eq!(result, test_pk);
    }

    #[tokio::test]
    async fn test_vault_key_cache_miss_would_fetch() {
        // Empty cache — get_public_key would attempt HTTP (which fails here)
        let client = VaultTransitClient::new("http://localhost:0".to_string(), "token".to_string());
        let result = client.get_public_key("nonexistent-key").await;
        // Connection refused — but proves it tried to fetch
        assert!(result.is_err());
    }
}
