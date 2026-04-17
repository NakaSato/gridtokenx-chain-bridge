mod middleware;

use std::sync::Arc;
use tracing::{info, warn, error};
use anyhow::Context as _;
use std::pin::Pin;
use std::task::{Context as StdContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use axum_server::accept::Accept;
use tokio_rustls::server::TlsStream;
use futures::future::BoxFuture;
use tower::Service;
use axum::http::Request;
use futures::StreamExt;

mod api;
mod nats_consumer;
pub mod vault;

use api::{ChainBridgeGrpcService, chain_v1::ChainBridgeServiceExt};
use middleware::PeerCertLayer;

// Simulate Vault integration for the Security Plane
async fn load_keys_from_vault() -> anyhow::Result<String> {
    info!("🔐 Bootstrapping from HashiCorp Vault...");
    let vault_url = std::env::var("VAULT_ADDR").unwrap_or_else(|_| "http://localhost:8200".to_string());
    let vault_token = std::env::var("VAULT_TOKEN").unwrap_or_else(|_| "root".to_string());

    let client = reqwest::Client::new();
    
    match client.get(format!("{}/v1/sys/health", vault_url))
        .header("X-Vault-Token", vault_token)
        .send()
        .await 
    {
        Ok(resp) if resp.status().is_success() => {
            info!("✅ Successfully authenticated with HashiCorp Vault.");
            Ok("master_key_secure_mock".to_string())
        }
        _ => {
            warn!("⚠️ Could not reach local Vault instance! Falling back to env vars.");
            Ok("dev_key".to_string())
        }
    }
}

/// A custom stream that carries the peer certificates
pub struct MtlsStream<S> {
    inner: TlsStream<S>,
    certs: Arc<Vec<Vec<u8>>>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for MtlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut StdContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for MtlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut StdContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Helper service to inject certs into request extensions
#[derive(Clone)]
pub struct ConnectionService<S> {
    pub inner: S,
    pub certs: Arc<Vec<Vec<u8>>>,
}

impl<S, ReqBody> Service<Request<ReqBody>> for ConnectionService<S>
where
    S: Service<Request<ReqBody>> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut StdContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        req.extensions_mut().insert(self.certs.clone());
        self.inner.call(req)
    }
}

/// A custom acceptor that extracts client certificates after the TLS handshake
#[derive(Clone)]
pub struct MtlsAcceptor {
    config: Arc<rustls::ServerConfig>,
}

impl MtlsAcceptor {
    pub fn new(config: Arc<rustls::ServerConfig>) -> Self {
        Self { config }
    }
}

impl<S, T> Accept<S, T> for MtlsAcceptor
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    T: Service<Request<axum::body::Body>> + Clone + Send + 'static,
{
    type Stream = MtlsStream<S>;
    type Service = ConnectionService<T>;
    type Future = BoxFuture<'static, std::io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: S, service: T) -> Self::Future {
        let config = self.config.clone();
        Box::pin(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(config);
            let tls_stream = acceptor.accept(stream).await?;
            
            let certs = tls_stream.get_ref().1.peer_certificates()
                .map(|c| c.iter().map(|cert| cert.as_ref().to_vec()).collect::<Vec<_>>())
                .unwrap_or_default();
            let certs = Arc::new(certs);
            
            let service = ConnectionService { inner: service, certs: certs.clone() };

            Ok((MtlsStream { inner: tls_stream, certs }, service))
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider().install_default().expect("Failed to install rustls crypto provider");
    tracing_subscriber::fmt::init();
    info!("🚀 GridTokenX Chain Bridge Protocol v1.2 starting...");

    // 1. Core security Plane Bootstrap
    let vault_addr = std::env::var("VAULT_ADDR").unwrap_or_else(|_| "http://localhost:8200".to_string());
    let vault_token = std::env::var("VAULT_TOKEN").unwrap_or_else(|_| "root".to_string());
    
    let vault_client = Arc::new(vault::VaultTransitClient::new(vault_addr, vault_token));
    info!("🔐 Vault Transit Client initialized.");

    // 2. Resource Initialization
    let solana_rpc_url = std::env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "http://localhost:8899".to_string());
    let provider = Arc::new(api::RealSolanaProvider::new(solana_rpc_url));
    
    let grpc_service = Arc::new(ChainBridgeGrpcService::new(provider, vault_client));
    
    let metrics = Arc::new(gridtokenx_blockchain_core::rpc::metrics::NoopMetrics);

    // NATS JetStream Setup
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    if let Ok(nats_client) = async_nats::connect(&nats_url).await {
        info!("✅ Connected to NATS at {}", nats_url);
        let jetstream = async_nats::jetstream::new(nats_client);
        
        // Main consumer
        let consumer = nats_consumer::NatsConsumer::new(jetstream.clone(), grpc_service.clone(), metrics.clone());
        tokio::spawn(async move {
            info!("🚀 Starting NATS consumer loop");
            if let Err(e) = consumer.start().await {
                error!("❌ NATS consumer error: {}", e);
            }
        });

        // DLQ Consumer (alerting/logging)
        let dlq_signing_service = grpc_service.clone();
        let dlq_jetstream = jetstream.clone();
        tokio::spawn(async move {
            info!("🚨 Starting NATS DLQ monitor");
            if let Err(e) = run_dlq_monitor(dlq_jetstream, dlq_signing_service).await {
                error!("❌ DLQ monitor error: {}", e);
            }
        });
    } else {
        warn!("⚠️ Failed to connect to NATS at {}. NATS path will be disabled.", nats_url);
    }

    // mTLS Configuration
    let cert_path = std::env::var("CHAIN_BRIDGE_TLS_CERT")
        .unwrap_or_else(|_| "infra/certs/server.crt".to_string());
    let key_path = std::env::var("CHAIN_BRIDGE_TLS_KEY")
        .unwrap_or_else(|_| "infra/certs/server.key".to_string());
    let ca_path = std::env::var("CHAIN_BRIDGE_TLS_CA")
        .unwrap_or_else(|_| "infra/certs/ca.crt".to_string());

    let cert_chain = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(&cert_path)?))
        .collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(&key_path)?))?
        .context("Missing private key")?;

    let ca_cert = std::fs::read(&ca_path).context("Failed to read CA certificate")?;
    let mut root_store = rustls::RootCertStore::empty();
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(&ca_cert[..]))
        .collect::<Result<Vec<_>, _>>()?;
    for cert in certs {
        root_store.add(cert)?;
    }

    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()?;

    let server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, key)?;
    let server_config = Arc::new(server_config);

    // Initialise the gRPC router and convert to Axum-compatible service
    let grpc_router = grpc_service.register(connectrpc::Router::new());
    
    let app = axum::Router::new()
        .fallback_service(grpc_router.into_axum_service())
        .layer(PeerCertLayer::new());

    let grpc_port = std::env::var("CHAIN_BRIDGE_GRPC_PORT").unwrap_or_else(|_| "5040".to_string());
    let grpc_addr: std::net::SocketAddr = format!("0.0.0.0:{}", grpc_port).parse()?;

    info!("🔐 Chain Bridge listening with mTLS on {}", grpc_addr);
    
    let insecure_mode = std::env::var("CHAIN_BRIDGE_INSECURE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);

    if insecure_mode {
        warn!("⚠️ Chain Bridge starting in INSECURE mode (no TLS)");
        axum_server::bind(grpc_addr)
            .serve(app.into_make_service())
            .await?;
    } else {
        let acceptor = MtlsAcceptor::new(server_config);
        axum_server::bind(grpc_addr)
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await?;
    }
    
    Ok(())
}

async fn run_dlq_monitor(js: async_nats::jetstream::Context, _service: Arc<ChainBridgeGrpcService>) -> anyhow::Result<()> {
    // Ensure DLQ stream exists
    let stream = js.get_or_create_stream(async_nats::jetstream::stream::Config {
        name: "CHAIN_TX_DLQ".to_string(),
        subjects: vec!["chain.tx.dlq.*".to_string()],
        ..Default::default()
    }).await?;

    let consumer = stream.get_or_create_consumer("dlq-monitor", async_nats::jetstream::consumer::pull::Config {
        durable_name: Some("dlq-monitor".to_string()),
        ..Default::default()
    }).await?;

    let mut messages = consumer.messages().await?;
    while let Some(result) = messages.next().await {
        match result {
            Ok(msg) => {
                error!("🔥 DEAD LETTER DETECTED: subject={} payload_size={}", msg.subject, msg.payload.len());
                msg.ack_with(async_nats::jetstream::AckKind::Ack).await.ok();
            }
            Err(e) => error!("DLQ monitor error: {}", e),
        }
    }
    Ok(())
}
