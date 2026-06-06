use super::*;
use anyhow::Context as _;
use chain_bridge_core::audit::{AuditEntry, AuditOutcome};
use chain_bridge_core::AuditPort;
use chain_bridge_persistence::InMemoryAuditStore;

pub struct ChainBridgeGrpcService {
    pub(crate) provider: Arc<dyn SolanaProvider>,
    pub(crate) vault: Arc<dyn VaultProvider>,
    pub(crate) blockhash_cache: Arc<BlockhashCache>,
    pub(crate) transit_key_name: String,
    /// Tamper-evident, hash-chained audit trail (Gap #2). Every signing
    /// decision in `sign_and_submit` (policy/auth reject, submit) appends here.
    /// Defaults to in-memory; `with_audit` swaps in the Postgres adapter.
    pub(crate) audit: Arc<dyn AuditPort>,
}

impl ChainBridgeGrpcService {
    pub fn new(
        provider: Arc<dyn SolanaProvider>,
        vault: Arc<dyn VaultProvider>,
        blockhash_cache: Arc<BlockhashCache>,
    ) -> Self {
        let transit_key_name = std::env::var("CHAIN_BRIDGE_VAULT_KEY_NAME")
            .unwrap_or_else(|_| "gridtokenx-bridge".to_string());

        Self {
            provider,
            vault,
            blockhash_cache,
            transit_key_name,
            audit: Arc::new(InMemoryAuditStore::new()),
        }
    }

    /// Replace the audit sink (e.g. the Postgres hash-chain store wired in
    /// `main`). Builder-style so existing 3-arg `new` callers stay unchanged.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<dyn AuditPort>) -> Self {
        self.audit = audit;
        self
    }

    /// Append one audit entry. Best-effort: a failed append is logged but does
    /// NOT fail the mediated effect (audit is observability, not a gate).
    /// `correlation_id` is empty until NATS envelope-id threading lands.
    async fn record_audit(
        &self,
        identity: &gridtokenx_blockchain_core::auth::SpiffeIdentity,
        outcome: AuditOutcome,
    ) {
        let created_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let entry = AuditEntry::new("", identity.0.clone(), "sign_and_submit", outcome, created_at_ms);
        if let Err(e) = self.audit.append(entry).await {
            warn!("⚠️ audit append failed (effect still applied): {}", e);
        }
    }

    pub fn provider(&self) -> Arc<dyn SolanaProvider> {
        self.provider.clone()
    }

    pub(crate) fn extract_role(&self, ctx: &Context) -> ServiceRole {
        // Primary path: SPIFFE URI from verified TLS client certificate
        // We look for it in headers if we can't get it from extensions easily with connectrpc
        if let Some(spiffe_id) = ctx.headers.get("z-gridtokenx-spiffe-id").and_then(|v| v.to_str().ok()) {
            let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(spiffe_id.to_string());
            let role = ServiceRole::from(&identity);
            info!("mTLS peer verified → identity: {} role: {}", spiffe_id, role);
            return role;
        }

        // Dev-only fallback: trust everything when insecure mode is explicitly enabled
        if std::env::var("CHAIN_BRIDGE_INSECURE").map(|v| v.to_lowercase() == "true").unwrap_or(false) {
            warn!("⚠️ Granting ADMIN role due to CHAIN_BRIDGE_INSECURE mode");
            return ServiceRole::Admin;
        }

        // Dev-only fallback: trust header only when escape hatch env var is set
        if std::env::var("CHAIN_BRIDGE_ALLOW_HEADER_AUTH").is_ok() {
            warn!("⚠️ Using header-based auth (dev mode only)");
            return ServiceRole::from_headers(&ctx.headers);
        }

        ServiceRole::Unknown
    }

    /// Reusable signing and submission logic for both gRPC and NATS
    pub async fn sign_and_submit(&self, serialized_tx: &[u8], key_id: &str, identity: &gridtokenx_blockchain_core::auth::SpiffeIdentity) -> anyhow::Result<(Signature, u64)> {
        let mut transaction: Transaction = bincode::deserialize(serialized_tx)
            .map_err(|e| anyhow::anyhow!("Invalid transaction format: {}", e))?;

        if let Err(e) =
            gridtokenx_blockchain_core::policy::PolicyEngine::validate_transaction(identity, &transaction)
        {
            let reason = e.to_string();
            self.record_audit(
                identity,
                AuditOutcome::Rejected { stage: "policy".to_string(), reason: reason.clone() },
            )
            .await;
            return Err(anyhow::anyhow!("Policy Engine rejection: {}", reason));
        }

        if key_id == "platform_admin" {
            // Only set blockhash if it's empty
            if transaction.message.recent_blockhash == solana_sdk::hash::Hash::default() {
                // 1. Refresh blockhash from cache (Wall 1 mitigation in SKILL.md)
                let (bh, _) = match self.blockhash_cache.get().await {
                    Some(cached) => cached,
                    None => {
                        // Fallback to slow path if cache is empty (e.g. at startup)
                        info!("⚠️ Blockhash cache empty, performing slow-path RPC refresh");
                        self.provider.get_latest_blockhash().await
                            .map_err(|e| anyhow::anyhow!("Failed to refresh blockhash: {}", e))?
                    }
                };
                
                transaction.message.recent_blockhash = bh;
            }

            // 1b. Pre-sign simulation (Gap #3): reject a doomed transaction
            // before spending a Vault signing operation. The tx is still
            // unsigned here; the provider's simulate runs with sig_verify=false.
            // A definitive tx-level failure rejects (audited stage="simulation");
            // an RPC/infra error stays advisory so a simulation outage can't halt
            // every write. Opt out via CHAIN_BRIDGE_PRESIGN_DISABLE for flows
            // (e.g. some simnet paths) that can't meaningfully simulate.
            let presign_disabled = std::env::var("CHAIN_BRIDGE_PRESIGN_DISABLE")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(false);
            if !presign_disabled {
                match self.provider.simulate_transaction(&transaction).await {
                    Ok(sim) if sim.value.err.is_some() => {
                        let reason = format!("pre-sign simulation failed: {:?}", sim.value.err);
                        warn!("🛑 {} — rejecting before Vault sign", reason);
                        self.record_audit(
                            identity,
                            AuditOutcome::Rejected {
                                stage: "simulation".to_string(),
                                reason: reason.clone(),
                            },
                        )
                        .await;
                        return Err(anyhow::anyhow!(reason));
                    }
                    Ok(sim) => {
                        info!(
                            "✅ pre-sign simulation passed ({} compute units)",
                            sim.value.units_consumed.unwrap_or(0)
                        );
                    }
                    Err(e) => {
                        warn!("⚠️ pre-sign simulation RPC error (advisory, proceeding): {}", e);
                    }
                }
            }

            // 2. Sign the message data, not the whole transaction
            let message_data = transaction.message_data();
            let num_required = transaction.message.header.num_required_signatures as usize;
            let account_keys = &transaction.message.account_keys;
            
            info!("📝 Signing transaction for {} required signatures. Payer: {}", num_required, account_keys[0]);
            if num_required > 1 {
                for i in 0..num_required {
                    info!("  Signer {}: {}", i, account_keys[i]);
                }
            }
            
            // 3. Request signature from Vault
            let signature = self.vault.sign_message(&self.transit_key_name, &message_data).await
                .context("Vault Transit signing failed")?;

            // 4. Attach signature at slot 0 (the fee payer slot)
            // Ensure the signatures vector is correctly sized
            if transaction.signatures.len() < num_required {
                transaction.signatures.resize(num_required, Signature::default());
            }
            
            transaction.signatures[0] = signature;

            info!("✅ Transaction signed remotely via Vault Transit for platform_admin. Signature: {}", signature);
        } else if !key_id.is_empty() {
            self.record_audit(
                identity,
                AuditOutcome::Rejected {
                    stage: "auth".to_string(),
                    reason: format!("Key ID not authorized: {}", key_id),
                },
            )
            .await;
            return Err(anyhow::anyhow!("Key ID not authorized: {}", key_id));
        }

        let sig = match self.provider.send_transaction(&transaction).await {
            Ok(s) => s,
            Err(e) => {
                self.record_audit(
                    identity,
                    AuditOutcome::Rejected { stage: "submit".to_string(), reason: e.to_string() },
                )
                .await;
                return Err(anyhow::anyhow!("Solana RPC submission failed: {}", e));
            }
        };

        self.record_audit(
            identity,
            AuditOutcome::Submitted { signature: sig.to_string(), slot: 0 },
        )
        .await;

        Ok((sig, 0))
    }
}

impl ChainBridgeService for ChainBridgeGrpcService {
    async fn simulate_transaction(
        &self,
        ctx: Context,
        request: OwnedView<SimulateTransactionRequestView<'static>>,
    ) -> Result<(SimulateTransactionResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[ServiceRole::TradingApi, ServiceRole::TradingMatcher, ServiceRole::OracleBridge, ServiceRole::IamService, ServiceRole::Admin])
            .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received simulate_transaction request for key_id: {}", request.key_id);
        
        let tx: Transaction = bincode::deserialize(&request.serialized_transaction)
            .map_err(|e| ConnectError::new(ErrorCode::InvalidArgument, format!("Invalid transaction format: {}", e)))?;

        match self.provider.simulate_transaction(&tx).await {
            Ok(resp) => {
                let mut response = SimulateTransactionResponse::default();
                response.success = resp.value.err.is_none();
                response.compute_units_consumed = resp.value.units_consumed.unwrap_or(0);
                response.logs = resp.value.logs.unwrap_or_default();
                Ok((response, ctx))
            }
            Err(e) => {
                error!("Solana RPC simulate error: {}", e);
                Err(ConnectError::new(ErrorCode::Internal, "Solana RPC simulation failed"))
            }
        }
    }

    async fn submit_transaction(
        &self,
        ctx: Context,
        request: OwnedView<SubmitTransactionRequestView<'static>>,
    ) -> Result<(SubmitTransactionResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[ServiceRole::TradingApi, ServiceRole::TradingMatcher, ServiceRole::OracleBridge, ServiceRole::IamService, ServiceRole::Admin])
            .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received submit_transaction request with key_id: {}", request.key_id);
        
        let identity_str = ctx.headers.get("z-gridtokenx-spiffe-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown");
        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(identity_str.to_string());

        let result = self.sign_and_submit(&request.serialized_transaction, &request.key_id, &identity).await;

        match result {
            Ok((sig, slot)) => {
                let mut response = SubmitTransactionResponse::default();
                response.success = true;
                response.signature = sig.to_string();
                response.error_message = String::new();
                response.slot = slot;
                Ok((response, ctx))
            }
            Err(e) => {
                let err_msg = format!("{:#}", e);
                error!("Transaction submission failed: {}", err_msg);
                
                let (code, msg) = if err_msg.contains("authorized") {
                    (ErrorCode::PermissionDenied, "Key ID not authorized")
                } else if err_msg.contains("format") {
                    (ErrorCode::InvalidArgument, "Invalid transaction format")
                } else {
                    (ErrorCode::Internal, "Transaction submission failed")
                };
                Err(ConnectError::new(code, msg))
            }
        }
    }

    async fn get_balance(
        &self,
        ctx: Context,
        request: OwnedView<GetBalanceRequestView<'static>>,
    ) -> Result<(GetBalanceResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[
            ServiceRole::ApiGateway, 
            ServiceRole::TradingApi, 
            ServiceRole::TradingMatcher, 
            ServiceRole::OracleBridge, 
            ServiceRole::IamService, 
            ServiceRole::Admin
        ])
        .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received get_balance for {}", request.pubkey);
        
        let pubkey = Pubkey::from_str(&request.pubkey)
            .map_err(|e| ConnectError::new(ErrorCode::InvalidArgument, format!("Invalid pubkey: {}", e)))?;

        match self.provider.get_balance(&pubkey).await {
            Ok(lamports) => {
                let mut response = GetBalanceResponse::default();
                response.lamports = lamports;
                Ok((response, ctx))
            }
            Err(e) => {
                error!("Solana RPC get_balance error for {}: {}", request.pubkey, e);
                Err(ConnectError::new(ErrorCode::Internal, "Failed to fetch balance"))
            }
        }
    }

    async fn get_account_data(
        &self,
        ctx: Context,
        request: OwnedView<GetAccountDataRequestView<'static>>,
    ) -> Result<(GetAccountDataResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[
            ServiceRole::ApiGateway, 
            ServiceRole::TradingApi, 
            ServiceRole::TradingMatcher, 
            ServiceRole::OracleBridge, 
            ServiceRole::IamService, 
            ServiceRole::Admin
        ])
        .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received get_account_data for {}", request.pubkey);
        
        let pubkey = Pubkey::from_str(&request.pubkey)
            .map_err(|e| ConnectError::new(ErrorCode::InvalidArgument, format!("Invalid pubkey: {}", e)))?;

        match self.provider.get_account(&pubkey).await {
            Ok(account) => {
                let mut response = GetAccountDataResponse::default();
                response.exists = true;
                response.data = account.data;
                Ok((response, ctx))
            }
            Err(e) => {
                if e.to_string().contains("AccountNotFound") {
                     let mut response = GetAccountDataResponse::default();
                     response.exists = false;
                     response.data = vec![];
                     Ok((response, ctx))
                } else {
                    error!("Solana RPC get_account error for {}: {}", request.pubkey, e);
                    Err(ConnectError::new(ErrorCode::Internal, "Failed to fetch account data"))
                }
            }
        }
    }

    async fn get_latest_blockhash(
        &self,
        ctx: Context,
        _request: OwnedView<GetLatestBlockhashRequestView<'static>>,
    ) -> Result<(GetLatestBlockhashResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[
            ServiceRole::TradingApi, 
            ServiceRole::TradingMatcher, 
            ServiceRole::OracleBridge, 
            ServiceRole::IamService, 
            ServiceRole::Admin
        ])
        .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received get_latest_blockhash");
        
        match self.provider.get_latest_blockhash().await {
            Ok((blockhash, last_valid_block_height)) => {
                let mut response = GetLatestBlockhashResponse::default();
                response.blockhash = blockhash.to_string();
                response.last_valid_block_height = last_valid_block_height;
                Ok((response, ctx))
            }
            Err(e) => {
                error!("Solana RPC get_latest_blockhash error: {}", e);
                Err(ConnectError::new(ErrorCode::Internal, "Failed to fetch blockhash"))
            }
        }
    }

    async fn get_recent_prioritization_fees(
        &self,
        ctx: Context,
        request: OwnedView<GetRecentPrioritizationFeesRequestView<'static>>,
    ) -> Result<(GetRecentPrioritizationFeesResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[
            ServiceRole::TradingApi, 
            ServiceRole::TradingMatcher, 
            ServiceRole::OracleBridge, 
            ServiceRole::IamService, 
            ServiceRole::Admin
        ])
        .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received get_recent_prioritization_fees for {} accounts", request.account_keys.len());
        
        let account_keys: Vec<Pubkey> = request.account_keys.iter()
            .map(|k| Pubkey::from_str(k).map_err(|e| ConnectError::new(ErrorCode::InvalidArgument, format!("Invalid account key '{}': {}", k, e))))
            .collect::<Result<Vec<_>, _>>()?;

        match self.provider.get_recent_prioritization_fees(&account_keys).await {
            Ok(fees) => {
                let mut response = GetRecentPrioritizationFeesResponse::default();
                response.fees = fees.into_iter().map(|f| {
                    let mut fee = PrioritizationFee::default();
                    fee.slot = f.slot;
                    fee.prioritization_fee = f.prioritization_fee;
                    fee
                }).collect();
                Ok((response, ctx))
            }
            Err(e) => {
                error!("Solana RPC get_recent_prioritization_fees error: {}", e);
                Err(ConnectError::new(ErrorCode::Internal, "Failed to fetch prioritization fees"))
            }
        }
    }

    async fn get_token_account_balance(
        &self,
        ctx: Context,
        request: OwnedView<GetTokenAccountBalanceRequestView<'static>>,
    ) -> Result<(GetTokenAccountBalanceResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[
            ServiceRole::ApiGateway, 
            ServiceRole::TradingApi, 
            ServiceRole::TradingMatcher, 
            ServiceRole::OracleBridge, 
            ServiceRole::IamService, 
            ServiceRole::Admin
        ])
        .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received get_token_account_balance for {}", request.pubkey);
        
        let pubkey = Pubkey::from_str(&request.pubkey)
            .map_err(|e| ConnectError::new(ErrorCode::InvalidArgument, format!("Invalid pubkey: {}", e)))?;

        match self.provider.get_token_account_balance(&pubkey).await {
            Ok(balance) => {
                let mut response = GetTokenAccountBalanceResponse::default();
                response.amount = balance["amount"].as_str().unwrap_or("0").to_string();
                response.decimals = balance["decimals"].as_u64().unwrap_or(0) as u32;
                response.ui_amount = balance["ui_amount"].as_f64().unwrap_or(0.0);
                Ok((response, ctx))
            }
            Err(e) => {
                error!("Solana RPC get_token_account_balance error for {}: {}", request.pubkey, e);
                Err(ConnectError::new(ErrorCode::Internal, "Failed to fetch token balance"))
            }
        }
    }

    async fn get_signature_status(
        &self,
        ctx: Context,
        request: OwnedView<GetSignatureStatusRequestView<'static>>,
    ) -> Result<(GetSignatureStatusResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[
            ServiceRole::ApiGateway, 
            ServiceRole::TradingApi, 
            ServiceRole::TradingMatcher, 
            ServiceRole::OracleBridge, 
            ServiceRole::IamService, 
            ServiceRole::Admin
        ])
        .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received get_signature_status for {}", request.signature);
        
        let signature = Signature::from_str(&request.signature)
            .map_err(|e: solana_sdk::signature::ParseSignatureError| ConnectError::new(ErrorCode::InvalidArgument, format!("Invalid signature: {}", e)))?;

        match self.provider.get_signature_statuses(&[signature]).await {
            Ok(resp) => {
                if let Some(Some(status)) = resp.value.first() {
                    let mut response = GetSignatureStatusResponse::default();
                    response.confirmed = status["confirmations"].as_u64().is_some() || status["err"].is_null();
                    response.status = status["confirmation_status"].as_str().unwrap_or("Unknown").to_string();
                    response.error = if status["err"].is_null() { "".to_string() } else { status["err"].to_string() };
                    Ok((response, ctx))
                } else {
                    let mut response = GetSignatureStatusResponse::default();
                    response.confirmed = false;
                    response.status = "NotFound".to_string();
                    Ok((response, ctx))
                }
            }
            Err(e) => {
                error!("Solana RPC get_signature_statuses error: {}", e);
                Err(ConnectError::new(ErrorCode::Internal, "Failed to fetch signature status"))
            }
        }
    }

    async fn get_slot(
        &self,
        ctx: Context,
        _request: OwnedView<GetSlotRequestView<'static>>,
    ) -> Result<(GetSlotResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[
            ServiceRole::TradingApi, 
            ServiceRole::TradingMatcher, 
            ServiceRole::OracleBridge, 
            ServiceRole::IamService, 
            ServiceRole::Admin
        ])
        .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received get_slot");
        
        match self.provider.get_slot().await {
            Ok(slot) => {
                let mut response = GetSlotResponse::default();
                response.slot = slot;
                Ok((response, ctx))
            }
            Err(e) => {
                error!("Solana RPC get_slot error: {}", e);
                Err(ConnectError::new(ErrorCode::Internal, "Failed to fetch current slot"))
            }
        }
    }

    async fn request_airdrop(
        &self,
        ctx: Context,
        request: OwnedView<chain_v1::RequestAirdropRequestView<'static>>,
    ) -> Result<(chain_v1::RequestAirdropResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[ServiceRole::Admin, ServiceRole::IamService])
            .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received request_airdrop for {} ({} lamports)", request.pubkey, request.lamports);
        
        let pubkey = Pubkey::from_str(&request.pubkey)
            .map_err(|e| ConnectError::new(ErrorCode::InvalidArgument, format!("Invalid pubkey: {}", e)))?;

        match self.provider.request_airdrop(&pubkey, request.lamports).await {
            Ok(sig) => {
                let mut response = chain_v1::RequestAirdropResponse::default();
                response.success = true;
                response.signature = sig.to_string();
                Ok((response, ctx))
            }
            Err(e) => {
                error!("Solana RPC request_airdrop error: {}", e);
                Err(ConnectError::new(ErrorCode::Internal, "Airdrop request failed"))
            }
        }
    }

    async fn get_transaction_details(
        &self,
        ctx: Context,
        request: OwnedView<chain_v1::GetTransactionDetailsRequestView<'static>>,
    ) -> Result<(chain_v1::GetTransactionDetailsResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[
            ServiceRole::ApiGateway, 
            ServiceRole::TradingApi, 
            ServiceRole::TradingMatcher, 
            ServiceRole::OracleBridge, 
            ServiceRole::IamService, 
            ServiceRole::Admin
        ])
        .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received get_transaction_details for {}", request.signature);
        
        let signature = Signature::from_str(&request.signature)
            .map_err(|e| ConnectError::new(ErrorCode::InvalidArgument, format!("Invalid signature: {}", e)))?;

        match self.provider.get_transaction(&signature).await {
            Ok(tx_val) => {
                let mut response = chain_v1::GetTransactionDetailsResponse::default();
                response.found = true;
                response.slot = tx_val["slot"].as_u64().unwrap_or(0);
                // Extract account keys if possible
                if let Some(accounts) = tx_val["transaction"]["message"]["accountKeys"].as_array() {
                    response.account_keys = accounts.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect();
                }
                Ok((response, ctx))
            }
            Err(e) => {
                if e.to_string().contains("TransactionNotFound") || e.to_string().contains("not found") {
                    let mut response = chain_v1::GetTransactionDetailsResponse::default();
                    response.found = false;
                    Ok((response, ctx))
                } else {
                    error!("Solana RPC get_transaction error: {}", e);
                    Err(ConnectError::new(ErrorCode::Internal, "Failed to fetch transaction details"))
                }
            }
        }
    }

    async fn get_epoch_info(
        &self,
        ctx: Context,
        _request: OwnedView<chain_v1::GetEpochInfoRequestView<'static>>,
    ) -> Result<(chain_v1::GetEpochInfoResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[
            ServiceRole::ApiGateway,
            ServiceRole::TradingApi,
            ServiceRole::TradingMatcher,
            ServiceRole::OracleBridge,
            ServiceRole::IamService,
            ServiceRole::Admin
        ])
        .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received get_epoch_info");

        match self.provider.get_epoch_info().await {
            Ok(info) => {
                let mut response = chain_v1::GetEpochInfoResponse::default();
                response.epoch = info.epoch;
                response.slot = info.absolute_slot;
                Ok((response, ctx))
            }
            Err(e) => {
                error!("Solana RPC get_epoch_info error: {}", e);
                Err(ConnectError::new(ErrorCode::Internal, "Failed to fetch epoch info"))
            }
        }
    }

}
