use std::sync::Arc;
use tracing::{info, warn, error};
use anyhow::Context as _;
use futures::StreamExt;

use gridtokenx_chain_bridge::api::{self, ChainBridgeGrpcService, SolanaProvider, chain_v1::ChainBridgeServiceExt};
use gridtokenx_chain_bridge::middleware::PeerCertLayer;
use gridtokenx_chain_bridge::harness::MtlsAcceptor;
use gridtokenx_chain_bridge::vault;
use gridtokenx_chain_bridge::nats_consumer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider().install_default().expect("Failed to install rustls crypto provider");
    let _telemetry_guard = gridtokenx_telemetry::init("gridtokenx-chain-bridge");
    info!("🚀 GridTokenX Chain Bridge Protocol v1.2 starting...");

    let insecure_mode = std::env::var("CHAIN_BRIDGE_INSECURE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);

    // 2. Resource Initialization
    let solana_network = std::env::var("SOLANA_NETWORK").unwrap_or_else(|_| "mainnet".to_string());
    let solana_rpc_url = std::env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "http://localhost:8899".to_string());

    let provider: Arc<dyn SolanaProvider> = if solana_network == "simnet" {
        info!("🚀 Initializing SIMNET provider (Surfpool in-memory SVM)");
        Arc::new(api::SurfpoolSolanaProvider::new(Some(solana_rpc_url)).await?)
    } else {
        Arc::new(api::RealSolanaProvider::new(solana_rpc_url))
    };
    
    let blockhash_cache = Arc::new(api::BlockhashCache::new());
    
    // Background blockhash refresh task (Wall 1 mitigation)
    let bh_cache = blockhash_cache.clone();
    let bh_provider = provider.clone();
    tokio::spawn(async move {
        info!("⏳ Starting background blockhash refresh task (2s interval)");
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            match bh_provider.get_latest_blockhash().await {
                Ok((hash, height)) => {
                    bh_cache.update(hash, height).await;
                }
                Err(e) => warn!("⚠️ Failed to refresh blockhash cache: {}", e),
            }
        }
    });

    let vault_provider: Arc<dyn vault::VaultProvider> = if insecure_mode {
        Arc::new(vault::InsecureKeypairProvider::new())
    } else {
        let vault_addr = std::env::var("VAULT_ADDR").unwrap_or_else(|_| "http://localhost:8200".to_string());
        let vault_token = std::env::var("VAULT_TOKEN").unwrap_or_else(|_| "root".to_string());
        let client = Arc::new(vault::VaultTransitClient::new(vault_addr, vault_token));
        info!("🔐 Vault Transit Client initialized.");
        client
    };

    // Audit hash-chain sink (Gap #2): Postgres when DATABASE_URL is set and
    // reachable, else an in-memory fallback so the binary runs without a DB.
    let audit: Arc<dyn chain_bridge_core::AuditPort> = match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => {
            match chain_bridge_persistence::PostgresAuditStore::connect(&url).await {
                Ok(store) => {
                    info!("📓 Audit trail → Postgres (hash-chained)");
                    Arc::new(store)
                }
                Err(e) => {
                    warn!("⚠️ Audit Postgres connect failed ({e}); falling back to in-memory audit");
                    Arc::new(chain_bridge_persistence::InMemoryAuditStore::new())
                }
            }
        }
        _ => {
            info!("📓 Audit trail → in-memory (DATABASE_URL unset)");
            Arc::new(chain_bridge_persistence::InMemoryAuditStore::new())
        }
    };

    let grpc_service = Arc::new(
        ChainBridgeGrpcService::new(provider, vault_provider, blockhash_cache).with_audit(audit),
    );
    
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

    // Initialise the gRPC router and convert to Axum-compatible service
    let grpc_router = grpc_service.register(connectrpc::Router::new());
    
    let app = axum::Router::new()
        .fallback_service(grpc_router.into_axum_service())
        .layer(PeerCertLayer::new());

    let grpc_port = std::env::var("CHAIN_BRIDGE_GRPC_PORT").unwrap_or_else(|_| "5040".to_string());
    let grpc_addr: std::net::SocketAddr = format!("0.0.0.0:{}", grpc_port).parse()?;

    let insecure_mode = std::env::var("CHAIN_BRIDGE_INSECURE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);

    if insecure_mode {
        warn!("⚠️ Chain Bridge starting in INSECURE mode (no TLS)");
        info!("🚀 Chain Bridge listening on {}", grpc_addr);
        axum_server::bind(grpc_addr)
            .serve(app.into_make_service())
            .await?;
    } else {
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

        info!("🔐 Chain Bridge listening with mTLS on {}", grpc_addr);
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

