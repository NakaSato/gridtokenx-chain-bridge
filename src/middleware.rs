use axum::{
    body::Body,
    http::Request,
    response::Response,
};
use std::task::{Context, Poll};
use tower::{Layer, Service};
use tracing::info;
use gridtokenx_blockchain_core::auth::SpiffeIdentity;
use x509_parser::prelude::*;
use std::sync::Arc;

/// Extension type used to store the verified SPIFFE URI from the peer certificate.
#[derive(Clone, Debug)]
pub struct VerifiedSpiffeUri(pub String);

/// The PeerCertLayer is the standard GridTokenX way to propagate mTLS identity.
/// It extracts the SPIFFE URI from the client certificate (injected by the acceptor)
/// and makes it available via extensions and a synthetic header.
#[derive(Clone)]
pub struct PeerCertLayer {
}

impl PeerCertLayer {
    pub fn new() -> Self {
        Self {}
    }
}

impl<S> Layer<S> for PeerCertLayer {
    type Service = PeerCertService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PeerCertService {
            inner,
        }
    }
}

#[derive(Clone)]
pub struct PeerCertService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for PeerCertService<S>
where
    S: Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        // The custom MtlsAcceptor in main.rs will inject the peer certificates
        // into the request extensions.
        if let Some(certs) = req.extensions().get::<Arc<Vec<Vec<u8>>>>() {
            if let Some(leaf) = certs.first() {
                if let Some(spiffe_id) = extract_spiffe_id(leaf) {
                    info!("✅ Verified SPIFFE identity from mTLS: {}", spiffe_id);
                    req.extensions_mut().insert(VerifiedSpiffeUri(spiffe_id.clone()));
                    
                    // Inject into a synthetic header for connectrpc
                    if let Ok(hv) = spiffe_id.parse() {
                        req.headers_mut().insert("z-gridtokenx-spiffe-id", hv);
                    }
                    
                    // Also insert the SpiffeIdentity from gridtokenx_blockchain_core::auth
                    // so that the ServiceRole extractor can find it.
                    req.extensions_mut().insert(gridtokenx_blockchain_core::auth::SpiffeIdentity(spiffe_id));
                }
            }
        }
        
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        
        Box::pin(async move {
            inner.call(req).await
        })
    }
}

/// Helper to parse SPIFFE ID from X.509 certificate SAN (Subject Alternative Name)
pub fn extract_spiffe_id(cert_der: &[u8]) -> Option<String> {
    let (_, cert) = X509Certificate::from_der(cert_der).ok()?;
    
    // SPIFFE ID is in the Subject Alternative Name (SAN) extension as a URI
    for ext in cert.extensions() {
        if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for name in &san.general_names {
                if let GeneralName::URI(uri) = name {
                    if uri.starts_with("spiffe://") {
                        return Some(uri.to_string());
                    }
                }
            }
        }
    }
    
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, DnType, IsCa, KeyPair, SanType};

    /// Generate a DER-encoded self-signed certificate with a SPIFFE URI SAN.
    fn generate_cert_with_spiffe(spiffe_uri: &str) -> Vec<u8> {
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, "test");
        params.is_ca = IsCa::NoCa;
        params.subject_alt_names.push(SanType::URI(
            spiffe_uri.try_into().expect("valid URI"),
        ));
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        cert.der().to_vec()
    }

    /// Generate a DER-encoded certificate with NO SAN extension.
    fn generate_cert_without_san() -> Vec<u8> {
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, "test");
        params.is_ca = IsCa::NoCa;
        // No subject_alt_names
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        cert.der().to_vec()
    }

    // ---------------------------------------------------------------------------
    // extract_spiffe_id
    // ---------------------------------------------------------------------------

    #[test]
    fn test_extract_spiffe_id_valid() {
        let cert_der = generate_cert_with_spiffe("spiffe://gridtokenx.th/prod/trading-service/matcher");
        let result = extract_spiffe_id(&cert_der);
        assert_eq!(
            result,
            Some("spiffe://gridtokenx.th/prod/trading-service/matcher".to_string())
        );
    }

    #[test]
    fn test_extract_spiffe_id_no_san() {
        let cert_der = generate_cert_without_san();
        let result = extract_spiffe_id(&cert_der);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_spiffe_id_non_spiffe_uri() {
        let cert_der = generate_cert_with_spiffe("https://example.com/not-spiffe");
        let result = extract_spiffe_id(&cert_der);
        assert_eq!(result, None, "Non-spiffe:// URI should be ignored");
    }

    #[test]
    fn test_extract_spiffe_id_garbage_input() {
        let result = extract_spiffe_id(&[0xFF, 0xFE, 0xFD]);
        assert_eq!(result, None, "Garbage bytes should return None");
    }

    #[test]
    fn test_extract_spiffe_id_empty_input() {
        let result = extract_spiffe_id(&[]);
        assert_eq!(result, None);
    }

    // ---------------------------------------------------------------------------
    // PeerCertLayer — tower service integration
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_peer_cert_layer_injects_identity() {
        use axum::http::Request;
        use tower::ServiceExt;

        let cert_der = generate_cert_with_spiffe("spiffe://gridtokenx.th/prod/iam-service");

        let layer = PeerCertLayer::new();
        let inner = tower::service_fn(|req: Request<Body>| async move {
            // Verify all three identity artifacts were injected
            let spiffe_ext = req.extensions().get::<SpiffeIdentity>().cloned();
            let uri_ext = req.extensions().get::<VerifiedSpiffeUri>().cloned();
            let header = req.headers().get("z-gridtokenx-spiffe-id").cloned();

            assert_eq!(
                spiffe_ext.map(|s| s.0),
                Some("spiffe://gridtokenx.th/prod/iam-service".to_string())
            );
            assert_eq!(
                uri_ext.map(|u| u.0),
                Some("spiffe://gridtokenx.th/prod/iam-service".to_string())
            );
            assert_eq!(
                header.map(|v| v.to_str().unwrap().to_string()),
                Some("spiffe://gridtokenx.th/prod/iam-service".to_string())
            );

            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        });

        let mut service = layer.layer(inner);
        let mut req = Request::builder()
            .body(Body::empty())
            .unwrap();
        // Inject peer certs as the MtlsAcceptor would
        req.extensions_mut()
            .insert(Arc::new(vec![cert_der]));

        let _ = service.ready().await.unwrap().call(req).await.unwrap();
    }

    #[tokio::test]
    async fn test_peer_cert_layer_no_certs() {
        use axum::http::Request;
        use tower::ServiceExt;

        let layer = PeerCertLayer::new();
        let inner = tower::service_fn(|req: Request<Body>| async move {
            // No identity should be injected
            assert!(req.extensions().get::<SpiffeIdentity>().is_none());
            assert!(req.extensions().get::<VerifiedSpiffeUri>().is_none());
            assert!(req.headers().get("z-gridtokenx-spiffe-id").is_none());
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        });

        let mut service = layer.layer(inner);
        let req = Request::builder().body(Body::empty()).unwrap();
        let _ = service.ready().await.unwrap().call(req).await.unwrap();
    }

    #[tokio::test]
    async fn test_peer_cert_layer_empty_cert_list() {
        use axum::http::Request;
        use tower::ServiceExt;

        let layer = PeerCertLayer::new();
        let inner = tower::service_fn(|req: Request<Body>| async move {
            assert!(req.extensions().get::<SpiffeIdentity>().is_none());
            Ok::<_, std::convert::Infallible>(Response::new(Body::empty()))
        });

        let mut service = layer.layer(inner);
        let mut req = Request::builder().body(Body::empty()).unwrap();
        // Inject empty certs vector
        req.extensions_mut().insert(Arc::new(Vec::<Vec<u8>>::new()));

        let _ = service.ready().await.unwrap().call(req).await.unwrap();
    }
}
