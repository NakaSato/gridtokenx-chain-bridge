# GridTokenX Chain Bridge

[![Rust](https://img.shields.io/badge/Rust-2024_Edition-orange.svg)](https://www.rust-lang.org/)
[![Solana](https://img.shields.io/badge/Solana-2.3-blueviolet.svg)](https://solana.com)
[![License](https://img.shields.io/badge/license-Proprietary-red.svg)](LICENSE)

> **Decentralized Signing Authority and Solana Blockchain Interface** for the GridTokenX platform. All microservices route blockchain transactions through Chain Bridge for centralized key management via HashiCorp Vault Transit, SPIFFE/mTLS identity verification, and high-throughput transaction submission.

---

## Architecture

Chain Bridge is the **single gateway** between GridTokenX backend services and the Solana blockchain. No service holds private keys — all signing is delegated to Vault Transit via Chain Bridge.

```
                             ┌──────────────────────────┐
                             │     HashiCorp Vault       │
                             │   Transit Engine (Ed25519)│
                             └────────────┬─────────────┘
                                          │ sign / pubkey
┌──────────────┐                          │
│ IAM Service  │──┐                ┌──────▼──────────────┐
├──────────────┤  │  ConnectRPC    │                      │
│ Trading Svc  │──┼───(mTLS)──────▶│   Chain Bridge :5040 │
├──────────────┤  │  + NATS        │                      │
│ Oracle Bridge│──┘  JetStream     │  • RBAC (SPIFFE)     │         ┌───────────────┐
├──────────────┤                   │  • Vault Signing     │────────▶│    Solana      │
│ NATS         │──────────────────▶│  • Blockhash Cache   │  RPC    │   Blockchain   │
│ JetStream    │  chain.tx.*       │  • Tx Simulation     │         └───────────────┘
└──────────────┘                   └──────────────────────┘
```

### Dual Ingestion Paths

| Path | Protocol | Use Case |
|------|----------|----------|
| **gRPC** (ConnectRPC) | Synchronous request/response | IAM, Trading, Oracle — real-time tx submission |
| **NATS JetStream** | Async message queue | Batch settlement, retries, dead-letter queue (DLQ) |

---

## Key Features

### Vault Transit Signing
- **No local private keys** — all Ed25519 signing delegated to HashiCorp Vault Transit engine.
- Public key caching with `DashMap` to avoid repeated Vault lookups.
- Insecure dev mode (`CHAIN_BRIDGE_INSECURE=true`) with a hardcoded dev keypair for local development.

### SPIFFE/mTLS Identity & RBAC
- Extracts SPIFFE URIs from client X.509 certificates via mutual TLS.
- Maps identities to `ServiceRole` (Admin, ApiGateway, TradingMatcher, OracleBridge, etc.).
- Role-based access control enforced **before** any blockchain operation.

### Blockhash Cache
- Background task refreshes the latest blockhash every 2 seconds.
- Eliminates per-transaction RPC calls for blockhash retrieval (Wall 1 mitigation).

### NATS JetStream Consumer
- Pull-based consumer with 32-way concurrent message processing.
- Subjects: `chain.tx.submit`, `chain.tx.simulate`, `chain.tx.cancel`.
- DLQ monitoring on `chain.tx.dlq.*` for failed transactions.
- Idempotency cache with automatic TTL cleanup (Wall 4 mitigation).

### Surfpool / LiteSVM Provider
- In-memory Solana VM for simnet mode — deploys all 5 Anchor programs locally.
- Lazy account cloning from mainnet RPC for realistic simulation.

---

## gRPC Service Interface

Chain Bridge exposes a ConnectRPC service (`ChainBridgeService`) with 12 RPCs:

| RPC Method | Description | RBAC |
|------------|-------------|------|
| `SimulateTransaction` | Dry-run a transaction | Read |
| `SubmitTransaction` | Sign via Vault + submit to Solana | Write |
| `GetBalance` | Query SOL balance | Read |
| `GetAccountData` | Fetch raw account data | Read |
| `GetLatestBlockhash` | Get latest blockhash (cached) | Read |
| `GetRecentPrioritizationFees` | Query priority fee estimates | Read |
| `GetTokenAccountBalance` | SPL token balance query | Read |
| `GetSignatureStatus` | Check transaction confirmation | Read |
| `GetSlot` | Current slot height | Read |
| `RequestAirdrop` | Dev/testnet airdrop | Admin |
| `GetTransactionDetails` | Full transaction details | Read |
| `GetEpochInfo` | Epoch and slot information | Read |

---

## Source Structure

```
gridtokenx-chain-bridge/
├── src/
│   ├── main.rs              # Entry point — server bootstrap, Vault init, NATS setup
│   ├── lib.rs               # Library re-exports
│   ├── api.rs               # ChainBridgeGrpcService (12 RPCs, ~1600 lines)
│   │                        #   ├── SolanaProvider trait (mockable)
│   │                        #   ├── RealSolanaProvider (RpcClient)
│   │                        #   ├── SurfpoolSolanaProvider (LiteSVM simnet)
│   │                        #   ├── BlockhashCache
│   │                        #   └── sign_and_submit() reusable signing logic
│   ├── vault.rs             # VaultProvider trait + VaultTransitClient + InsecureKeypairProvider
│   ├── middleware.rs         # PeerCertLayer — SPIFFE extraction from mTLS certificates
│   ├── harness.rs           # MtlsAcceptor — custom TLS acceptor injecting peer certs
│   └── nats_consumer.rs     # NatsConsumer — JetStream pull consumer (submit/simulate/cancel)
├── tests/
│   └── invariants.rs        # Invariant tests (RBAC, throughput, Vault integration)
├── build.rs                 # Protobuf code generation (buffa + connectrpc)
└── Cargo.toml               # Dependencies
```

---

## Configuration

### Environment Variables

```bash
# Server
CHAIN_BRIDGE_GRPC_PORT=5040              # gRPC listen port (default: 5040)
CHAIN_BRIDGE_INSECURE=true               # Dev mode — no TLS, admin RBAC bypass

# Solana
SOLANA_RPC_URL=http://localhost:8899     # Solana RPC endpoint
SOLANA_NETWORK=localnet                  # localnet | devnet | simnet | mainnet
SOLANA_COMMITMENT=confirmed              # confirmed | finalized | processed

# Vault Transit (production)
VAULT_ADDR=http://localhost:8200         # HashiCorp Vault address
VAULT_TOKEN=root                         # Vault token
CHAIN_BRIDGE_VAULT_KEY_NAME=gridtokenx-bridge  # Transit key name

# mTLS Certificates (production)
CHAIN_BRIDGE_TLS_CERT=infra/certs/server.crt
CHAIN_BRIDGE_TLS_KEY=infra/certs/server.key
CHAIN_BRIDGE_TLS_CA=infra/certs/ca.crt

# NATS JetStream
NATS_URL=nats://localhost:4222           # NATS server URL

# Dev Escape Hatch
CHAIN_BRIDGE_ALLOW_HEADER_AUTH=1         # Allow header-based auth (dev only)
```

---

## Quick Start

### Development (Insecure Mode)

```bash
# Start with local Solana validator
CHAIN_BRIDGE_INSECURE=true \
SOLANA_RPC_URL=http://localhost:8899 \
cargo run

# Or via the platform manager
./scripts/app.sh start
```

### Production (mTLS + Vault)

```bash
# 1. Start Vault and configure Transit
vault server -dev -dev-root-token-id=root &
vault secrets enable transit
vault write -f transit/keys/gridtokenx-bridge type=ed25519

# 2. Start Chain Bridge with TLS
VAULT_ADDR=http://127.0.0.1:8200 \
VAULT_TOKEN=root \
CHAIN_BRIDGE_TLS_CERT=infra/certs/server.crt \
CHAIN_BRIDGE_TLS_KEY=infra/certs/server.key \
CHAIN_BRIDGE_TLS_CA=infra/certs/ca.crt \
cargo run --release
```

---

## Testing

```bash
# Run invariant tests (no infrastructure required)
cargo test --test invariants

# Run with NATS + Vault available
vault server -dev -dev-root-token-id=root &
vault secrets enable transit
vault write -f transit/keys/test-bridge-key type=ed25519
nats-server -js &

VAULT_ADDR=http://127.0.0.1:8200 VAULT_TOKEN=root \
  cargo test --test invariants -- --ignored --test-threads=1
```

### Test Coverage

| Section | Tests | Infrastructure |
|---------|-------|---------------|
| **§1 Invariants** | RBAC gate runs before provider | None |
| **§2 RBAC** | Per-service allowlist verification | None |
| **§3 NATS** | Stale/duplicate/unknown/malformed checks | NATS JetStream |
| **§4 Throughput** | 16 concurrent reads, <200ms target | None |
| **§5 Vault** | Pubkey caching, signature parsing | Vault dev mode |

---

## Dependencies

| Category | Crate | Purpose |
|----------|-------|---------|
| **Web** | `axum` 0.8, `tower`, `tower-http` | HTTP/gRPC server |
| **gRPC** | `connectrpc` 0.2, `buffa` 0.2 | ConnectRPC protocol |
| **Solana** | `solana-sdk` 2.3, `solana-client` 2.3, `anchor-client` 1.0 | Blockchain interaction |
| **Async** | `tokio` 1.48, `futures` 0.3 | Multi-threaded runtime |
| **Messaging** | `async-nats` 0.37 | NATS JetStream consumer |
| **TLS** | `rustls` 0.23, `tokio-rustls` 0.26, `x509-parser` 0.16 | mTLS + SPIFFE |
| **Vault** | `reqwest` 0.12 | Vault Transit HTTP API |
| **Testing** | `litesvm` 0.10 | In-memory SVM for simnet |
| **Shared** | `gridtokenx-blockchain-core` | Auth, RBAC, metrics |

---

## License

Part of the GridTokenX Ecosystem — Proprietary

---

_Maintained by the GridTokenX Engineering Team._
