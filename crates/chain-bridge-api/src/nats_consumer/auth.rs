//! Envelope authentication for the NATS write path.
//!
//! The gRPC path proves identity at L4 (mTLS cert → SPIFFE SAN). NATS has no
//! such channel, so publishers attach an [`EnvelopeAuth`]: their mTLS client
//! cert PEM plus an ECDSA P-256 signature over the canonical envelope bytes
//! (shared scheme in `gridtokenx_blockchain_core::rpc::envelope_auth`). This
//! module verifies that chain end-to-end:
//!
//! 1. cert parses and is inside its validity window,
//! 2. cert is signed by the same dev CA the mTLS server trusts
//!    (`CHAIN_BRIDGE_TLS_CA` — single trust root, single-level chain),
//! 3. the cert's SPIFFE URI SAN equals the claimed `service_identity`
//!    (binds the self-asserted string to the cert),
//! 4. the signature verifies against the cert's public key.
//!
//! EKU clientAuth is deliberately not checked: the only CA-issued cert without
//! a SPIFFE SAN is the bridge's own server cert, which step 3 already excludes.
//!
//! Enforcement is rollout-gated: `CHAIN_BRIDGE_REQUIRE_SIGNED_NATS` defaults to
//! false (log-only) so signer/verifier version skew across submodule pins can
//! never halt the write path. All verification logic is pure (explicit flags +
//! CA bytes, no env reads) — the same race-avoidance pattern as
//! `ChainBridgeGrpcService::role_from`. Tests must never mutate process env.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use gridtokenx_blockchain_core::rpc::envelope_auth::{
    ENVELOPE_AUTH_SCHEME_V1, verify_p256_signature,
};
use gridtokenx_blockchain_core::rpc::nats_schema::EnvelopeAuth;
use tracing::warn;
use x509_parser::prelude::*;

use crate::middleware::extract_spiffe_id;

/// Immutable envelope-auth policy, built once at startup.
#[derive(Clone)]
pub struct NatsAuthPolicy {
    pub require_signed: bool,
    /// DER of the dev-CA certificate. None ⇒ verification impossible, so the
    /// constructor forces log-only mode.
    pub ca_der: Option<Vec<u8>>,
}

impl NatsAuthPolicy {
    /// Pure constructor. Enforcement without a CA would reject every message
    /// (nothing could ever verify), so `require_signed` is forced off when
    /// `ca_der` is absent.
    pub fn new(require_signed: bool, ca_der: Option<Vec<u8>>) -> Self {
        if require_signed && ca_der.is_none() {
            warn!(
                "⚠️ CHAIN_BRIDGE_REQUIRE_SIGNED_NATS requested but no CA loaded — forcing log-only mode"
            );
            return Self {
                require_signed: false,
                ca_der: None,
            };
        }
        Self {
            require_signed,
            ca_der,
        }
    }

    /// Thin env wrapper — call ONLY from `main.rs` wiring, never from tests
    /// (env is process-global; tests construct [`NatsAuthPolicy::new`]).
    /// Reads `CHAIN_BRIDGE_REQUIRE_SIGNED_NATS` (default false) and the CA from
    /// `CHAIN_BRIDGE_TLS_CA` (default `infra/certs/ca.crt`) — the same trust
    /// root the mTLS server uses. A missing CA file is tolerated (log-only).
    pub fn from_env() -> Self {
        let require_signed = std::env::var("CHAIN_BRIDGE_REQUIRE_SIGNED_NATS")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);
        let ca_path = std::env::var("CHAIN_BRIDGE_TLS_CA")
            .unwrap_or_else(|_| "infra/certs/ca.crt".to_string());
        let ca_der = match std::fs::read(&ca_path) {
            Ok(pem_bytes) => match parse_x509_pem(&pem_bytes) {
                Ok((_, pem)) => Some(pem.contents),
                Err(e) => {
                    warn!("⚠️ CA at {ca_path} is not parseable PEM ({e}); NATS auth log-only");
                    None
                }
            },
            Err(e) => {
                warn!("⚠️ CA not readable at {ca_path} ({e}); NATS envelope auth log-only");
                None
            }
        };
        Self::new(require_signed, ca_der)
    }
}

/// Outcome of verifying one envelope.
#[derive(Debug)]
pub enum AuthCheck {
    /// Cert chains to the CA, SAN matches the claimed identity, signature valid.
    Verified,
    /// No `auth` attached (legacy publisher / dev mode), or no CA to verify with.
    Unsigned,
    /// `auth` attached but verification failed — never silently ignored.
    Failed(String),
}

/// Pure envelope verification — no env, no IO, no clock (`now_secs` injected).
pub fn check_envelope_auth(
    ca_der: Option<&[u8]>,
    auth: Option<&EnvelopeAuth>,
    claimed_identity: &str,
    canonical: &[u8],
    now_secs: i64,
) -> AuthCheck {
    let Some(auth) = auth else {
        return AuthCheck::Unsigned;
    };
    let Some(ca_der) = ca_der else {
        return AuthCheck::Unsigned;
    };

    if auth.scheme != ENVELOPE_AUTH_SCHEME_V1 {
        return AuthCheck::Failed(format!("unknown auth scheme: {}", auth.scheme));
    }

    let leaf_der = match parse_x509_pem(auth.cert_pem.as_bytes()) {
        Ok((_, pem)) => pem.contents,
        Err(e) => return AuthCheck::Failed(format!("cert_pem is not valid PEM: {e}")),
    };
    let leaf = match X509Certificate::from_der(&leaf_der) {
        Ok((_, cert)) => cert,
        Err(e) => return AuthCheck::Failed(format!("cert_pem is not a valid certificate: {e}")),
    };

    let now = ASN1Time::from_timestamp(now_secs).unwrap_or(ASN1Time::now());
    if !leaf.validity().is_valid_at(now) {
        return AuthCheck::Failed("certificate outside validity window".to_string());
    }

    let ca = match X509Certificate::from_der(ca_der) {
        Ok((_, cert)) => cert,
        Err(e) => return AuthCheck::Failed(format!("configured CA unparseable: {e}")),
    };
    if leaf.verify_signature(Some(ca.public_key())).is_err() {
        return AuthCheck::Failed("certificate not signed by the trusted CA".to_string());
    }

    match extract_spiffe_id(&leaf_der) {
        Some(san) if san == claimed_identity => {}
        Some(san) => {
            return AuthCheck::Failed(format!(
                "cert SPIFFE SAN '{san}' does not match claimed identity '{claimed_identity}'"
            ));
        }
        None => return AuthCheck::Failed("certificate has no SPIFFE URI SAN".to_string()),
    }

    let sig_der = match BASE64.decode(&auth.signature) {
        Ok(s) => s,
        Err(e) => return AuthCheck::Failed(format!("signature is not valid base64: {e}")),
    };
    let pubkey_point = &leaf.public_key().subject_public_key.data;
    match verify_p256_signature(pubkey_point, canonical, &sig_der) {
        Ok(()) => AuthCheck::Verified,
        Err(e) => AuthCheck::Failed(format!("{e:#}")),
    }
}

/// Enforcement decision: a `Failed`/`Unsigned` envelope is rejected only when
/// signing is required; in log-only mode everything proceeds (callers log).
pub fn auth_decision(check: &AuthCheck, require_signed: bool) -> Result<(), String> {
    if !require_signed {
        return Ok(());
    }
    match check {
        AuthCheck::Verified => Ok(()),
        AuthCheck::Unsigned => Err("unsigned envelope (signing required)".to_string()),
        AuthCheck::Failed(reason) => Err(reason.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridtokenx_blockchain_core::rpc::envelope_auth::{
        EnvelopeSigner, canonical_submit_bytes,
    };
    use gridtokenx_blockchain_core::rpc::nats_schema::TxSubmitMessage;
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, PKCS_ECDSA_P256_SHA256,
        SanType,
    };

    // Fixed "now" inside rcgen's default validity window (1975..4096).
    const NOW_SECS: i64 = 1_750_000_000;

    fn make_test_ca() -> (rcgen::Certificate, KeyPair) {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "test-ca");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert, key)
    }

    /// CA-signed P-256 leaf with a SPIFFE URI SAN; returns (cert_pem, key_pair).
    fn make_leaf(
        spiffe_uri: &str,
        ca: &rcgen::Certificate,
        ca_key: &KeyPair,
    ) -> (String, KeyPair) {
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, "leaf");
        params.is_ca = IsCa::NoCa;
        params
            .subject_alt_names
            .push(SanType::URI(spiffe_uri.try_into().unwrap()));
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = params.signed_by(&key, ca, ca_key).unwrap();
        (cert.pem(), key)
    }

    fn submit_fixture(identity: &str) -> TxSubmitMessage {
        TxSubmitMessage {
            correlation_id: "c1".to_string(),
            idempotency_key: "ik".to_string(),
            reply_subject: "chain.tx.result.c1".to_string(),
            serialized_tx: vec![9, 9, 9],
            key_id: "platform_admin".to_string(),
            skip_preflight: false,
            retry_count: 0,
            service_identity: identity.to_string(),
            created_at_ms: 1_700_000_000_000,
            auth: None,
        }
    }

    /// Sign an envelope with a leaf cert/key (rcgen keys serialize as PKCS#8,
    /// which `EnvelopeSigner::from_pem` accepts alongside prod's SEC1).
    fn signed_auth(
        envelope: &TxSubmitMessage,
        cert_pem: &str,
        key: &KeyPair,
    ) -> gridtokenx_blockchain_core::rpc::nats_schema::EnvelopeAuth {
        let signer = EnvelopeSigner::from_pem(cert_pem.to_string(), &key.serialize_pem()).unwrap();
        signer.sign(&canonical_submit_bytes(envelope))
    }

    const IDENTITY: &str = "spiffe://gridtokenx.th/prod/iam-service";

    #[test]
    fn verified_happy_path() {
        let (ca, ca_key) = make_test_ca();
        let (leaf_pem, leaf_key) = make_leaf(IDENTITY, &ca, &ca_key);
        let env = submit_fixture(IDENTITY);
        let auth = signed_auth(&env, &leaf_pem, &leaf_key);

        let check = check_envelope_auth(
            Some(ca.der()),
            Some(&auth),
            IDENTITY,
            &canonical_submit_bytes(&env),
            NOW_SECS,
        );
        assert!(matches!(check, AuthCheck::Verified), "got {check:?}");
    }

    #[test]
    fn san_mismatch_rejected() {
        let (ca, ca_key) = make_test_ca();
        // Attacker holds a VALID cert for trading, claims to be iam-service.
        let (leaf_pem, leaf_key) =
            make_leaf("spiffe://gridtokenx.th/prod/trading-service/api", &ca, &ca_key);
        let env = submit_fixture(IDENTITY);
        let auth = signed_auth(&env, &leaf_pem, &leaf_key);

        let check = check_envelope_auth(
            Some(ca.der()),
            Some(&auth),
            IDENTITY,
            &canonical_submit_bytes(&env),
            NOW_SECS,
        );
        assert!(
            matches!(&check, AuthCheck::Failed(r) if r.contains("does not match")),
            "got {check:?}"
        );
    }

    #[test]
    fn wrong_ca_rejected() {
        let (trusted_ca, _) = make_test_ca();
        let (rogue_ca, rogue_ca_key) = make_test_ca();
        let (leaf_pem, leaf_key) = make_leaf(IDENTITY, &rogue_ca, &rogue_ca_key);
        let env = submit_fixture(IDENTITY);
        let auth = signed_auth(&env, &leaf_pem, &leaf_key);

        let check = check_envelope_auth(
            Some(trusted_ca.der()),
            Some(&auth),
            IDENTITY,
            &canonical_submit_bytes(&env),
            NOW_SECS,
        );
        assert!(
            matches!(&check, AuthCheck::Failed(r) if r.contains("not signed by the trusted CA")),
            "got {check:?}"
        );
    }

    #[test]
    fn expired_cert_rejected() {
        let (ca, ca_key) = make_test_ca();
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, "leaf");
        params
            .subject_alt_names
            .push(SanType::URI(IDENTITY.try_into().unwrap()));
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2021, 1, 1); // long expired vs NOW_SECS
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let leaf_pem = params.signed_by(&key, &ca, &ca_key).unwrap().pem();

        let env = submit_fixture(IDENTITY);
        let auth = signed_auth(&env, &leaf_pem, &key);
        let check = check_envelope_auth(
            Some(ca.der()),
            Some(&auth),
            IDENTITY,
            &canonical_submit_bytes(&env),
            NOW_SECS,
        );
        assert!(
            matches!(&check, AuthCheck::Failed(r) if r.contains("validity window")),
            "got {check:?}"
        );
    }

    #[test]
    fn tampered_canonical_rejected() {
        let (ca, ca_key) = make_test_ca();
        let (leaf_pem, leaf_key) = make_leaf(IDENTITY, &ca, &ca_key);
        let mut env = submit_fixture(IDENTITY);
        let auth = signed_auth(&env, &leaf_pem, &leaf_key);

        env.reply_subject = "chain.tx.result.attacker".to_string();
        let check = check_envelope_auth(
            Some(ca.der()),
            Some(&auth),
            IDENTITY,
            &canonical_submit_bytes(&env),
            NOW_SECS,
        );
        assert!(
            matches!(&check, AuthCheck::Failed(r) if r.contains("signature verification failed")),
            "got {check:?}"
        );
    }

    #[test]
    fn unknown_scheme_rejected() {
        let (ca, ca_key) = make_test_ca();
        let (leaf_pem, leaf_key) = make_leaf(IDENTITY, &ca, &ca_key);
        let env = submit_fixture(IDENTITY);
        let mut auth = signed_auth(&env, &leaf_pem, &leaf_key);
        auth.scheme = "hmac-md5-v0".to_string();

        let check = check_envelope_auth(
            Some(ca.der()),
            Some(&auth),
            IDENTITY,
            &canonical_submit_bytes(&env),
            NOW_SECS,
        );
        assert!(
            matches!(&check, AuthCheck::Failed(r) if r.contains("unknown auth scheme")),
            "got {check:?}"
        );
    }

    #[test]
    fn unsigned_rejected_when_enforced_accepted_otherwise() {
        let check = AuthCheck::Unsigned;
        assert!(auth_decision(&check, true).is_err());
        assert!(auth_decision(&check, false).is_ok());
        // Failed is also tolerated in log-only mode (rollout skew protection)…
        let failed = AuthCheck::Failed("boom".to_string());
        assert!(auth_decision(&failed, false).is_ok());
        // …and rejected under enforcement.
        assert!(auth_decision(&failed, true).is_err());
        assert!(auth_decision(&AuthCheck::Verified, true).is_ok());
    }

    #[test]
    fn policy_new_forces_log_only_without_ca() {
        let policy = NatsAuthPolicy::new(true, None);
        assert!(!policy.require_signed);

        let policy = NatsAuthPolicy::new(true, Some(vec![1, 2, 3]));
        assert!(policy.require_signed);
    }
}
