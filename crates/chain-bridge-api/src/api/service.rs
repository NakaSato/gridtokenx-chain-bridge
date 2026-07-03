use super::*;
use anyhow::Context as _;
use chain_bridge_core::audit::{AuditEntry, AuditOutcome};
use chain_bridge_core::AuditPort;
use chain_bridge_persistence::InMemoryAuditStore;

/// Outcome of polling a just-submitted tx for confirmation. Lets the mint path
/// gate `success` on finality: only `Confirmed` is an `Ok` reply; `Errored` and
/// `Pending` become `Err` so the aggregator outbox keeps + retries the mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmOutcome {
    /// Tx landed without error at `confirmed` commitment; carries the slot.
    Confirmed(u64),
    /// Tx landed but with an on-chain error (no second mint occurred).
    Errored,
    /// Tx not confirmed within the bounded poll window (unknown — retry).
    Pending,
}

/// Which stage of `sign_and_submit` failed. Callers that need to map a
/// failure to a specific status code (e.g. `submit_transaction`'s gRPC
/// response) should `downcast_ref::<SignSubmitStage>()` the returned
/// `anyhow::Error` rather than pattern-matching its `Display` text — the
/// text is for logs/audit, not for classification, and free-text matching
/// silently breaks the moment a message is reworded.
#[derive(Debug)]
pub(crate) enum SignSubmitStage {
    /// `bincode::deserialize` couldn't parse the transaction.
    InvalidFormat(String),
    /// `PolicyEngine::validate_transaction` denied the request (unauthorized
    /// role/program, dev-bypass excluded).
    PolicyRejected(String),
    /// Blockhash cache empty and the slow-path RPC refresh also failed.
    BlockhashRefreshFailed(String),
    /// Pre-sign simulation found a definitive on-chain failure (not an RPC
    /// error, which is advisory and doesn't reach this variant).
    SimulationRejected(String),
    /// Vault Transit couldn't produce a signature.
    VaultSigningFailed(String),
    /// `key_id` isn't the recognized `platform_admin` signer.
    KeyNotAuthorized(String),
    /// The signed transaction was rejected or failed to reach the cluster.
    SubmissionFailed(String),
}

impl std::fmt::Display for SignSubmitStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFormat(e) => write!(f, "Invalid transaction format: {e}"),
            Self::PolicyRejected(reason) => write!(f, "Policy Engine rejection: {reason}"),
            Self::BlockhashRefreshFailed(e) => write!(f, "Failed to refresh blockhash: {e}"),
            Self::SimulationRejected(reason) => write!(f, "{reason}"),
            Self::VaultSigningFailed(e) => write!(f, "Vault Transit signing failed: {e}"),
            Self::KeyNotAuthorized(key_id) => write!(f, "Key ID not authorized: {key_id}"),
            Self::SubmissionFailed(e) => write!(f, "Solana RPC submission failed: {e}"),
        }
    }
}

impl std::error::Error for SignSubmitStage {}

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
    /// `correlation_id` ties the entry to the originating NATS envelope; the
    /// gRPC path has no envelope id and passes "". `action` is
    /// `sign_and_submit` for the signing pipeline; the NATS consumer passes its
    /// subject kind (`submit`/`simulate`/`cancel`) for pre-pipeline rejections.
    pub(crate) async fn record_audit(
        &self,
        correlation_id: &str,
        identity: &gridtokenx_blockchain_core::auth::SpiffeIdentity,
        action: &str,
        outcome: AuditOutcome,
    ) {
        let created_at_ms = gridtokenx_telemetry::time::now().timestamp_millis() as u64;
        let entry = AuditEntry::new(correlation_id, identity.0.clone(), action, outcome, created_at_ms);
        if let Err(e) = self.audit.append(entry).await {
            warn!("⚠️ audit append failed (effect still applied): {}", e);
        }
    }

    pub fn provider(&self) -> Arc<dyn SolanaProvider> {
        self.provider.clone()
    }

    /// Look up the on-chain confirmation status of a submitted transaction.
    ///
    /// Shared read used by both the gRPC `get_signature_status` and the NATS
    /// `chain.tx.status` handler. Returns `(found, confirmed, status, slot)`
    /// where `status` is the Solana commitment string
    /// (`processed`/`confirmed`/`finalized`) and `confirmed` means the tx landed
    /// without an execution error.
    ///
    /// # Errors
    /// Returns an error if the signature is malformed or the RPC call fails.
    pub async fn signature_status(
        &self,
        signature: &str,
    ) -> anyhow::Result<(bool, bool, String, u64)> {
        let sig = Signature::from_str(signature)
            .map_err(|e| anyhow::anyhow!("invalid signature: {e}"))?;
        let resp = self
            .provider
            .get_signature_statuses(&[sig])
            .await
            .map_err(|e| anyhow::anyhow!("get_signature_statuses failed: {e}"))?;
        if let Some(Some(status)) = resp.value.first() {
            let confirmed = status["err"].is_null();
            let conf_status = status["confirmation_status"]
                .as_str()
                .unwrap_or("processed")
                .to_string();
            let slot = status["slot"].as_u64().unwrap_or(0);
            Ok((true, confirmed, conf_status, slot))
        } else {
            Ok((false, false, "NotFound".to_string(), 0))
        }
    }

    /// Build, sign (Vault `platform_admin`), and submit an energy-token
    /// generation mint. The bridge owns issuance — the caller sends only a
    /// semantic intent (recipient, kWh, meter, window) and carries no Solana
    /// types. Reuses [`sign_and_submit`](Self::sign_and_submit) for the
    /// policy → pre-sign simulate → Vault sign → submit pipeline.
    ///
    /// Exactly-once is enforced on-chain: the energy-token mint-record PDA is
    /// keyed by `(meter_id, window_start_ms)`, so a replay of the same window is
    /// a no-op mint regardless of bridge-side state.
    pub async fn build_and_submit_generation_mint(
        &self,
        recipient_wallet: &str,
        energy_kwh: f64,
        meter_id: &[u8; 16],
        window_start_ms: i64,
        identity: &gridtokenx_blockchain_core::auth::SpiffeIdentity,
        correlation_id: &str,
    ) -> anyhow::Result<(Signature, u64)> {
        use gridtokenx_blockchain_core::config::SolanaProgramsConfig;
        use gridtokenx_blockchain_core::rpc::instructions::InstructionBuilder;

        // Validate + scale kWh → 9-decimal base units through the shared core
        // helper, which also enforces the MAX_MINT_KWH cap. A bare
        // `(kwh * 1e9) as u64` would *saturate* to u64::MAX for a huge value, so
        // the cap is the load-bearing guard against an over-large mint.
        let amount_atomic =
            gridtokenx_blockchain_core::rpc::nats_schema::energy_kwh_to_base_units(energy_kwh)?;

        let recipient = Pubkey::from_str(recipient_wallet)
            .map_err(|e| anyhow::anyhow!("invalid recipient wallet '{recipient_wallet}': {e}"))?;

        // Fee payer + mint authority = the bridge's Vault platform_admin pubkey,
        // the same key sign_and_submit signs slot 0 with.
        let payer = self
            .vault
            .get_public_key(&self.transit_key_name)
            .await
            .context("resolving bridge signer pubkey from Vault")?;

        let builder = InstructionBuilder::new(payer, SolanaProgramsConfig::from_env());
        let mut instructions = builder
            .build_generation_mint_instructions(
                payer,
                recipient,
                amount_atomic,
                meter_id,
                window_start_ms,
            )
            .map_err(|e| anyhow::anyhow!("building generation-mint instructions: {e:#}"))?;

        // Size the compute budget for create-ATA (idempotent) + mint_to of one
        // recipient; the flat token-minting default only marginally covers a
        // first-time recipient.
        instructions.insert(
            0,
            solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        );

        // Unsigned: sign_and_submit fills the blockhash and Vault-signs slot 0.
        let message = solana_sdk::message::Message::new(&instructions, Some(&payer));
        let transaction = Transaction::new_unsigned(message);
        let serialized =
            bincode::serialize(&transaction).map_err(|e| anyhow::anyhow!("serializing mint tx: {e}"))?;

        let (sig, _) = self
            .sign_and_submit(&serialized, "platform_admin", identity, correlation_id)
            .await?;

        // `sign_and_submit` submits fire-and-forget (returns slot 0, never waits
        // for confirmation). The mint is provenance-critical and low-rate, so —
        // unlike the generic submit path — GATE success on confirmation: only a
        // `Confirmed(slot)` is reported `Ok` to the caller. An errored or
        // unconfirmed-in-window mint returns `Err`, so `handle_mint` replies
        // `success=false` and the aggregator-bridge's durable outbox KEEPS the
        // entry and retries. Retry is safe and convergent: the on-chain
        // `(meter_id, window_start_ms)` `gen_mint` PDA no-ops a replay of a mint
        // that actually landed (energy-token `mint_generation`: `if minted
        // { return Ok(()) }`), so this can never double-mint — it only closes the
        // window where an accepted-but-unfinalized mint was previously dropped.
        match self.resolve_confirmation(&sig).await {
            ConfirmOutcome::Confirmed(slot) => Ok((sig, slot)),
            ConfirmOutcome::Errored => Err(anyhow::anyhow!(
                "generation mint {sig} landed with an on-chain error; not confirmed"
            )),
            ConfirmOutcome::Pending => Err(anyhow::anyhow!(
                "generation mint {sig} unconfirmed within window; outbox will retry"
            )),
        }
    }

    /// Polls signature status to resolve the confirmation outcome of a
    /// just-submitted transaction at `confirmed` commitment. Distinguishes a
    /// confirmed landing (with its slot) from an on-chain error and from a
    /// not-yet-confirmed (pending) tx, so the mint path can gate success on
    /// finality. Localnet/`confirmed` typically resolves in ~1-2s.
    async fn resolve_confirmation(&self, sig: &Signature) -> ConfirmOutcome {
        // ~6s budget (24 × 250ms) — comfortably above confirmed-commitment latency
        // without blocking the mint reply indefinitely on a stalled validator.
        self.resolve_confirmation_bounded(sig, 24, std::time::Duration::from_millis(250))
            .await
    }

    /// Bounded core of [`resolve_confirmation`] — `attempts` polls spaced by
    /// `delay`. Split out so unit tests can drive it fast (small attempts/delay)
    /// without the production 6s budget.
    pub(crate) async fn resolve_confirmation_bounded(
        &self,
        sig: &Signature,
        attempts: u32,
        delay: std::time::Duration,
    ) -> ConfirmOutcome {
        for _ in 0..attempts {
            if let Ok(resp) = self
                .provider
                .get_signature_statuses(std::slice::from_ref(sig))
                .await
            {
                if let Some(Some(status)) = resp.value.first() {
                    // A landed-but-failed tx is a terminal on-chain error.
                    let errored = status.get("err").map(|e| !e.is_null()).unwrap_or(false);
                    if errored {
                        return ConfirmOutcome::Errored;
                    }
                    if let Some(slot) = status.get("slot").and_then(serde_json::Value::as_u64) {
                        return ConfirmOutcome::Confirmed(slot);
                    }
                }
            }
            tokio::time::sleep(delay).await;
        }
        ConfirmOutcome::Pending
    }

    pub(crate) fn extract_role(&self, ctx: &Context) -> ServiceRole {
        let insecure = std::env::var("CHAIN_BRIDGE_INSECURE")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);
        let header_auth = std::env::var("CHAIN_BRIDGE_ALLOW_HEADER_AUTH").is_ok();
        Self::role_from(ctx, insecure, header_auth)
    }

    /// Pure role resolution — env flags are passed in so tests can exercise the
    /// dev-mode branches without mutating process-global env (which races with
    /// parallel tests that read `CHAIN_BRIDGE_INSECURE`, e.g. via the PolicyEngine).
    pub(crate) fn role_from(ctx: &Context, insecure: bool, header_auth: bool) -> ServiceRole {
        // Primary path: SPIFFE URI from verified TLS client certificate
        // We look for it in headers if we can't get it from extensions easily with connectrpc
        if let Some(spiffe_id) = ctx.headers.get("z-gridtokenx-spiffe-id").and_then(|v| v.to_str().ok()) {
            let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(spiffe_id.to_string());
            let role = ServiceRole::from(&identity);
            info!("mTLS peer verified → identity: {} role: {}", spiffe_id, role);
            return role;
        }

        // Dev-only fallback: trust everything when insecure mode is explicitly enabled
        if insecure {
            warn!("⚠️ Granting ADMIN role due to CHAIN_BRIDGE_INSECURE mode");
            return ServiceRole::Admin;
        }

        // Dev-only fallback: trust header only when escape hatch env var is set
        if header_auth {
            warn!("⚠️ Using header-based auth (dev mode only)");
            return ServiceRole::from_headers(&ctx.headers);
        }

        ServiceRole::Unknown
    }

    /// Reusable signing and submission logic for both gRPC and NATS
    pub async fn sign_and_submit(&self, serialized_tx: &[u8], key_id: &str, identity: &gridtokenx_blockchain_core::auth::SpiffeIdentity, correlation_id: &str) -> anyhow::Result<(Signature, u64)> {
        let mut transaction: Transaction = bincode::deserialize(serialized_tx)
            .map_err(|e| SignSubmitStage::InvalidFormat(e.to_string()))?;

        if let Err(e) =
            gridtokenx_blockchain_core::policy::PolicyEngine::validate_transaction(identity, &transaction)
        {
            let reason = e.to_string();
            self.record_audit(
                correlation_id,
                identity,
                "sign_and_submit",
                AuditOutcome::Rejected { stage: "policy".to_string(), reason: reason.clone() },
            )
            .await;
            return Err(SignSubmitStage::PolicyRejected(reason).into());
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
                            .map_err(|e| SignSubmitStage::BlockhashRefreshFailed(e.to_string()))?
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
                            correlation_id,
                            identity,
                            "sign_and_submit",
                            AuditOutcome::Rejected {
                                stage: "simulation".to_string(),
                                reason: reason.clone(),
                            },
                        )
                        .await;
                        return Err(SignSubmitStage::SimulationRejected(reason).into());
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
                .map_err(|e| SignSubmitStage::VaultSigningFailed(e.to_string()))?;

            // 4. Attach signature at slot 0 (the fee payer slot)
            // Ensure the signatures vector is correctly sized
            if transaction.signatures.len() < num_required {
                transaction.signatures.resize(num_required, Signature::default());
            }
            
            transaction.signatures[0] = signature;

            info!("✅ Transaction signed remotely via Vault Transit for platform_admin. Signature: {}", signature);
        } else if !key_id.is_empty() {
            self.record_audit(
                correlation_id,
                identity,
                "sign_and_submit",
                AuditOutcome::Rejected {
                    stage: "auth".to_string(),
                    reason: format!("Key ID not authorized: {}", key_id),
                },
            )
            .await;
            return Err(SignSubmitStage::KeyNotAuthorized(key_id.to_string()).into());
        }

        let sig = match self.provider.send_transaction(&transaction).await {
            Ok(s) => s,
            Err(e) => {
                self.record_audit(
                    correlation_id,
                    identity,
                    "sign_and_submit",
                    AuditOutcome::Rejected { stage: "submit".to_string(), reason: e.to_string() },
                )
                .await;
                return Err(SignSubmitStage::SubmissionFailed(e.to_string()).into());
            }
        };

        self.record_audit(
            correlation_id,
            identity,
            "sign_and_submit",
            AuditOutcome::Submitted { signature: sig.to_string(), slot: 0 },
        )
        .await;

        Ok((sig, 0))
    }
}

impl ChainBridgeService for ChainBridgeGrpcService {
    async fn submit_transaction(
        &self,
        ctx: Context,
        request: OwnedView<SubmitTransactionRequestView<'static>>,
    ) -> Result<(SubmitTransactionResponse, Context), ConnectError> {
        let role = self.extract_role(&ctx);
        role.require_any(&[ServiceRole::TradingApi, ServiceRole::TradingMatcher, ServiceRole::AggregatorBridge, ServiceRole::IamService, ServiceRole::SettlementService, ServiceRole::Admin])
            .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

        info!("🔗 gRPC Received submit_transaction request with key_id: {}", request.key_id);
        
        let identity_str = ctx.headers.get("z-gridtokenx-spiffe-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown");
        let identity = gridtokenx_blockchain_core::auth::SpiffeIdentity(identity_str.to_string());

        // gRPC has no NATS envelope id; audit correlation_id is empty for this path.
        let result = self.sign_and_submit(&request.serialized_transaction, &request.key_id, &identity, "").await;

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
                error!("Transaction submission failed: {:#}", e);

                // Classify by the typed stage marker, not by matching Display
                // text — free-text matching silently breaks the moment a
                // message is reworded (see SignSubmitStage's doc comment).
                let (code, msg) = match e.downcast_ref::<SignSubmitStage>() {
                    Some(SignSubmitStage::InvalidFormat(_)) => {
                        (ErrorCode::InvalidArgument, "Invalid transaction format")
                    }
                    Some(SignSubmitStage::PolicyRejected(_)) => {
                        (ErrorCode::PermissionDenied, "Policy Engine rejection")
                    }
                    Some(SignSubmitStage::KeyNotAuthorized(_)) => {
                        (ErrorCode::PermissionDenied, "Key ID not authorized")
                    }
                    Some(SignSubmitStage::SimulationRejected(_)) => {
                        (ErrorCode::FailedPrecondition, "Transaction would fail simulation")
                    }
                    Some(
                        SignSubmitStage::BlockhashRefreshFailed(_)
                        | SignSubmitStage::VaultSigningFailed(_)
                        | SignSubmitStage::SubmissionFailed(_),
                    ) => (ErrorCode::Unavailable, "Transaction submission failed"),
                    None => (ErrorCode::Internal, "Transaction submission failed"),
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
            ServiceRole::AggregatorBridge,
            ServiceRole::IamService,
            ServiceRole::SettlementService,
            ServiceRole::ReportingService,
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
            ServiceRole::AggregatorBridge,
            ServiceRole::IamService,
            ServiceRole::SettlementService,
            ServiceRole::ReportingService,
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
            ServiceRole::AggregatorBridge,
            ServiceRole::IamService,
            ServiceRole::SettlementService,
            ServiceRole::ReportingService,
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
            ServiceRole::AggregatorBridge,
            ServiceRole::IamService,
            ServiceRole::SettlementService,
            ServiceRole::ReportingService,
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
            ServiceRole::AggregatorBridge,
            ServiceRole::IamService,
            ServiceRole::SettlementService,
            ServiceRole::ReportingService,
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
            ServiceRole::AggregatorBridge,
            ServiceRole::IamService,
            ServiceRole::SettlementService,
            ServiceRole::ReportingService,
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
            ServiceRole::AggregatorBridge,
            ServiceRole::IamService,
            ServiceRole::SettlementService,
            ServiceRole::ReportingService,
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
            ServiceRole::AggregatorBridge,
            ServiceRole::IamService,
            ServiceRole::SettlementService,
            ServiceRole::ReportingService,
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
            ServiceRole::AggregatorBridge,
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
