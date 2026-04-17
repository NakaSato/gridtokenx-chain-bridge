use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
};
use std::task::{Context, Poll};
use tower::{Layer, Service};
use tracing::{warn, info};
use gridtokenx_blockchain_core::auth::SpiffeIdentity;
use x509_parser::prelude::*;
use std::sync::Arc;

/// Extension type used to store the verified SPIFFE URI from the peer certificate.
#[derive(Clone, Debug)]
pub struct VerifiedSpiffeUri(pub String);

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
