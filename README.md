# GridTokenX Chain Bridge

[![Rust](https://img.shields.io/badge/Rust-2024_Edition-orange.svg)](https://www.rust-lang.org/)
[![Solana](https://img.shields.io/badge/Solana-2.3-blueviolet.svg)](https://solana.com)
[![License](https://img.shields.io/badge/license-Proprietary-red.svg)](LICENSE)

> **Decentralized Signing Authority and Solana Blockchain Interface** for the GridTokenX platform. All microservices route blockchain transactions through Chain Bridge for centralized key management via HashiCorp Vault Transit, SPIFFE/mTLS identity verification, program-level policy enforcement, and high-throughput transaction submission.

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
├──────────────┤                   │  • Policy Engine     │────────▶│    Solana      │
│ NATS         │──────────────────▶│  • Vault Signing     │  RPC    │   Blockchain   │
│ JetStream    │  chain.tx.*       │  • Blockhash Cache   │         └───────────────┘
└──────────────┘                   │  • Tx Simulation     │
                                   └──────────────────────┘
```

---

## Theoretical Foundations & Academic Specification

### 1. Security Model: Reference Monitor Pattern
Chain Bridge implements a strictly mediated **Reference Monitor** pattern. It satisfies the three essential design requirements for secure mediation (Anderson, 1972):
*   **Tamper-proof:** The mediation mechanism is isolated within a hardened network perimeter, with all entry points secured via mutual TLS (mTLS).
*   **Always-invoked:** Every transaction path (synchronous gRPC and asynchronous NATS) converges at the `sign_and_submit()` atomic pipeline, ensuring no bypass exists.
*   **Small enough to be verified:** The core authorization logic (RBAC + Policy Engine) and the signing delegation are concentrated in a minimal, auditable surface area (~10% of the codebase).

### 2. Trust Model & Root of Trust (RoT)
The system operates under a **Delegated Trust Model**, partitioning authority to minimize the blast radius of any single component compromise:
*   **Identity RoT (SPIFFE):** Workload identity is rooted in the platform's SPIRE control plane. The bridge relies on the cryptographic verification of X.509 SVIDs during the TLS handshake to establish the caller's identity.
*   **Key RoT (Vault Transit):** Cryptographic keys are never instantiated in the application's address space. The bridge delegates the **Signing Oracle** function to a TEE/HSM-backed Vault instance, maintaining physical and logical separation between authorization logic and private key material.
*   **Blockchain RoT (Solana):** Finality and state transition truth are rooted in the Solana validator cluster, with the bridge acting as a non-custodial interface.

### 3. System Invariants & Formal Properties
The bridge maintains the following formal properties $(\mathcal{P})$ during operation:

*   **Safety (Authorization Integrity):**
    A signature $\sigma$ for transaction $T$ is generated only if the mediation function $M$ returns true for identity $I$:
    $$\forall \sigma, T: \text{Generate}(\sigma, T) \implies M(I, T) \text{ where } M(I, T) \equiv \text{Verify}(I) \land \text{RBAC}(I) \land \text{Policy}(I, T)$$
*   **Liveness (Transaction Finality):**
    Every valid transaction $T$ submitted by an authorized identity $I$ reaches the blockchain layer within the blockhash validity window $\Delta t \approx 60s$:
    $$\forall T, I: \text{Authorized}(I, T) \implies \exists \text{Submission}(T) \text{ within } \Delta t$$
*   **Identity Non-Repudiation:**
    Identities are derived directly from the verified transport layer ($L_4$) SAN URI, overwriting any application-layer ($L_7$) headers.

### 4. Policy Engine Formalism
The Policy Engine is a mapping function $f: \mathcal{I} \to 2^{\mathcal{P}}$ where $\mathcal{I}$ is the set of SPIFFE identities and $\mathcal{P}$ is the power set of allowed Solana program IDs.
$$f(I) = \{SystemProgram\} \cup \{P_1, P_2, \dots, P_n \mid \text{Allowlist}(I)\}$$
Validation is performed per-instruction: $\forall \text{inst} \in T: \text{ProgramID}(\text{inst}) \in f(I)$.

---

### Dual Ingestion Paths

| Path | Protocol | Use Case |
|------|----------|----------|
| **gRPC** (ConnectRPC) | Synchronous request/response | IAM, Trading, Oracle — real-time tx submission |
| **NATS JetStream** | Async message queue | Batch settlement, retries, dead-letter queue (DLQ) |

### Transaction Submission Flow

Every transaction — whether via gRPC or NATS — passes through the same `sign_and_submit()` pipeline:

```
gRPC Path                              NATS Path
────────                               ─────────
Client mTLS cert                       TxSubmitMessage
  │                                      │
  ▼                                      ▼
MtlsAcceptor                           NatsConsumer.handle_submit()
  │  TLS handshake, extract peer certs    │
  ▼                                      ▼
ConnectionService                       1. ServiceRole RBAC check
  │  inject certs into extensions         2. Idempotency cache check
  ▼                                      3. 55s staleness rejection
PeerCertLayer                           4. Retry (3×, 500ms interval)
  │  parse X.509 SAN → SPIFFE URI         │
  ▼                                      ▼
extract_role()                          sign_and_submit()
  │  SPIFFE → ServiceRole                 │
  ▼                                      ▼
RBAC check (require_any)                1. Deserialize bincode Transaction
  │                                      2. PolicyEngine::validate_transaction()
  ▼                                      3. Blockhash (if empty → cache → RPC fallback)
sign_and_submit()                       4. Vault Transit sign (message_data only)
  │                                      5. Attach signature at fee-payer slot
  ▼                                      6. SolanaProvider::send_transaction()
Solana Blockchain                       Solana Blockchain
```

### Eight-Layer Defense in Depth

| Layer | Mechanism | Where |
|-------|-----------|-------|
| **1. Network** | mTLS with `WebPkiClientVerifier`, CA-pinned certificates | `MtlsAcceptor` |
| **2. Identity** | SPIFFE URI extracted from X.509 SAN, propagated via extensions + synthetic header | `PeerCertLayer` |
| **3. RBAC** | Per-RPC method `require_any()` — 9 distinct service roles, Admin bypasses all | `extract_role()` |
| **4. Policy** | Per-instruction program ID allowlist scoped to caller identity | `PolicyEngine` |
| **5. Key Authority** | Only `platform_admin` key_id authorized; all signing remote via Vault Transit | `sign_and_submit()` |
| **6. Idempotency** | `DashMap` dedup prevents double-submission within 60s windows (NATS path) | `NatsConsumer` |
| **7. Staleness** | 55s TTL rejects expired transactions before signing (NATS path) | `NatsConsumer` |
| **8. Retry Discipline** | Fixed 500ms interval, max 3 attempts for transient RPC errors only | `NatsConsumer` |

### Service-to-Program Authorization Matrix

The Policy Engine enforces that each service can only invoke its designated on-chain programs:

| Service (SPIFFE URI prefix) | Allowed Programs | Source |
|---|---|---|
| `spiffe://gridtokenx.th/prod/admin` | All (bypass) | — |
| `spiffe://gridtokenx.th/prod/trading-service` | System, Trading, Registry, Energy Token | `SolanaProgramsConfig` |
| `spiffe://gridtokenx.th/prod/aggregator-bridge` | System, Oracle, Energy Token, SPL ATA | `SolanaProgramsConfig` |
| `spiffe://gridtokenx.th/prod/iam-service` | System, Registry | `SolanaProgramsConfig` |
| `spiffe://gridtokenx.th/prod/settlement-service` | System, Trading, Energy Token | `SolanaProgramsConfig` |
| `spiffe://gridtokenx.th/prod/apisix` | **Denied all submission** (read-only role) | — |
| `spiffe://gridtokenx.th/prod/reporting-service` | **Denied all submission** (read-only role) | — |
| Unknown identity | **Rejected** | — |

The System Program (`11111111111111111111111111`) is always allowed for signing-capable roles; the read-only roles (`apisix`, `reporting-service`) are denied before per-instruction checks even for System-only transactions. All other programs are validated per-instruction — a transaction with two instructions is rejected if either one calls an unauthorized program.

### SPIFFE Identity Mapping

| SPIFFE URI | ServiceRole | Access Level |
|---|---|---|
| `spiffe://gridtokenx.th/prod/apisix` | `ApiGateway` | Read-only RPCs |
| `spiffe://gridtokenx.th/prod/iam-service` | `IamService` | Read + Submit + Airdrop |
| `spiffe://gridtokenx.th/prod/trading-service/api` | `TradingApi` | Read + Submit |
| `spiffe://gridtokenx.th/prod/trading-service/matcher` | `TradingMatcher` | Read + Submit |
| `spiffe://gridtokenx.th/prod/aggregator-bridge` | `AggregatorBridge` | Read + Submit |
| `spiffe://gridtokenx.th/prod/settlement-service` | `SettlementService` | Read + Submit |
| `spiffe://gridtokenx.th/prod/reporting-service` | `ReportingService` | Read-only RPCs |
| `spiffe://gridtokenx.th/prod/admin` | `Admin` | Full access (all RPCs) |

### Solana Provider Architecture

```
                    ┌─────────────────────────┐
                    │   SolanaProvider trait   │
                    │   (12 async methods)     │
                    └─────────┬───────────────┘
                              │
              ┌───────────────┼───────────────┐
              ▼                               ▼
   RealSolanaProvider               SurfpoolSolanaProvider
   ┌─────────────────┐             ┌──────────────────────┐
   │ RpcClient        │             │ LiteSVM (in-memory)  │
   │ Real Solana RPC  │             │                      │
   │                  │             │ • 5 Anchor programs  │
   │ • Direct proxy   │             │ • Pre-seeded mints   │
   │   to RPC node    │             │   & PDAs             │
   │                  │             │ • Mainnet account    │
   └─────────────────┘             │   cloning (lazy)     │
                                    │ • Auto-airdrop (<1SOL)│
   Selected by:                     │ • sdk → transaction   │
   SOLANA_NETWORK != "simnet"       │   crate conversion    │
                                    └──────────────────────┘
                                    Selected by:
                                    SOLANA_NETWORK == "simnet"
```

### NATS JetStream Topology

```
┌─────────────────┐     publish      ┌───────────────────────────────────┐
│ Trading Service │─────────────────▶│ Stream: CHAIN_TX                  │
│ Oracle Bridge   │                  │ Subjects: chain.tx.*              │
│ IAM Service     │                  └───────────┬───────────────────────┘
└─────────────────┘                              │
                                                 │ pull (batch=128)
                                                 ▼
                                  ┌──────────────────────────────────┐
                                  │ Consumer: chain-bridge-worker    │
                                  │ Concurrency: 32 (for_each_conc)  │
                                  │                                  │
                                  │ chain.tx.submit  → handle_submit │
                                  │ chain.tx.simulate → handle_sim   │
                                  │ chain.tx.cancel  → handle_cancel │
                                  └──────────────────────────────────┘

  Terminated / malformed messages:
                                                 ▼
                                  ┌──────────────────────────────────┐
                                  │ Stream: CHAIN_TX_DLQ             │
                                  │ Subjects: chain.tx.dlq.*         │
                                  │ Consumer: dlq-monitor            │
                                  │ Action: log + ack                │
                                  └──────────────────────────────────┘
```

NATS message schemas (defined in `gridtokenx-blockchain-core`):

| Message | Direction | Fields |
|---------|-----------|--------|
| `TxSubmitMessage` | Publisher → Bridge | `correlation_id`, `reply_subject`, `serialized_tx`, `key_id`, `skip_preflight`, `retry_count`, `service_identity`, `created_at_ms` |
| `TxResultMessage` | Bridge → Publisher | `correlation_id`, `success`, `signature?`, `error?`, `slot` |
| `TxSimulateMessage` | Publisher → Bridge | `correlation_id`, `reply_subject`, `serialized_tx`, `key_id`, `service_identity`, `created_at_ms` |
| `TxSimulateResultMessage` | Bridge → Publisher | `correlation_id`, `success`, `compute_units_consumed`, `error_message`, `logs` |
| `TxCancelMessage` | Publisher → Bridge | `correlation_id`, `reply_subject`, `service_identity`, `created_at_ms` |
| `TxCancelResultMessage` | Bridge → Publisher | `correlation_id`, `success`, `error?` |

---

## Key Features

### Vault Transit Signing
- **No local private keys** — all Ed25519 signing delegated to HashiCorp Vault Transit engine.
- Public key caching via `RwLock<HashMap>` to avoid repeated Vault lookups.
- Signs `message_data()` only (not the whole transaction) — signature attached at the fee-payer slot (index 0).
- Vault signature format parsed: `vault:v1:<base64>` → raw Ed25519 bytes.
- Insecure dev mode (`CHAIN_BRIDGE_INSECURE=true`) with a hardcoded dev keypair for local development.

### SPIFFE/mTLS Identity & RBAC
- Extracts SPIFFE URIs from client X.509 certificates via mutual TLS.
- Maps identities to `ServiceRole` (9 roles: Admin, ApiGateway, IamService, TradingApi, TradingMatcher, AggregatorBridge, SettlementService, ReportingService, Unknown).
- Role-based access control enforced **before** any blockchain operation.
- Three-tier identity resolution: mTLS cert → `z-gridtokenx-spiffe-id` header → `x-gridtokenx-role` header (dev escape hatch).
- ApiGateway role requires `x-gridtokenx-gateway-secret` header matching `GATEWAY_SECRET` env var.

### Policy Engine
- Validates every instruction in a transaction against a program ID allowlist scoped to the caller's SPIFFE identity.
- Applied in both gRPC and NATS ingestion paths via `sign_and_submit()`.
- Unknown SPIFFE identities are rejected entirely — no default allowlist.
- Program IDs sourced from `SolanaProgramsConfig` in `gridtokenx-blockchain-core`.
- Admin/insecure mode bypasses all policy checks.

### Blockhash Cache
- Background task refreshes the latest blockhash every 2 seconds.
- Eliminates per-transaction RPC calls for blockhash retrieval (Wall 1 mitigation).
- Only sets blockhash on transactions that have an empty/default blockhash — preserves caller-supplied values.
- Falls back to slow-path RPC if cache is empty (e.g. at startup).

### NATS JetStream Consumer
- Pull-based consumer with 32-way concurrent message processing (`for_each_concurrent(32, ...)`).
- Batch size 128, max waiting 512 — optimized for high throughput.
- Three handlers: `chain.tx.submit`, `chain.tx.simulate`, `chain.tx.cancel`.
- DLQ monitoring on `chain.tx.dlq.*` for failed transactions.
- Idempotency cache (`DashMap`) with 5s automatic TTL cleanup — prevents double-submission within 60s windows.
- Staleness check (55s) rejects expired transactions before signing.
- Retry with `tokio-retry`: FixedInterval 500ms, max 3 attempts for transient RPC errors.

### Surfpool / LiteSVM Provider
- In-memory Solana VM for simnet mode — deploys all 5 Anchor programs locally.
- Pre-seeded accounts: Energy Token Mint, Currency Token Mint, Registry PDA, RegistryShard PDA, TokenInfo PDA.
- Proper `Transaction` → `VersionedTransaction` conversion via `solana_sdk::VersionedTransaction::from()` for LiteSVM compatibility.
- Lazy account cloning from mainnet RPC for realistic simulation.
- Auto-airdrop: payer accounts with < 1 SOL receive 10 SOL automatically.

---

## gRPC Service Interface

Chain Bridge exposes a ConnectRPC service (`ChainBridgeService`) with 12 RPCs:

| RPC Method | Description | Allowed Roles |
|------------|-------------|---------------|
| `SimulateTransaction` | Dry-run a transaction | TradingApi, TradingMatcher, AggregatorBridge, IamService, SettlementService, Admin |
| `SubmitTransaction` | Sign via Vault + submit to Solana | TradingApi, TradingMatcher, AggregatorBridge, IamService, SettlementService, Admin |
| `GetBalance` | Query SOL balance | **All except Unknown** |
| `GetAccountData` | Fetch raw account data | **All except Unknown** |
| `GetLatestBlockhash` | Get latest blockhash (cached) | All except Unknown and ApiGateway |
| `GetRecentPrioritizationFees` | Query priority fee estimates | All except Unknown and ApiGateway |
| `GetTokenAccountBalance` | SPL token balance query | **All except Unknown** |
| `GetSignatureStatus` | Check transaction confirmation | **All except Unknown** |
| `GetSlot` | Current slot height | All except Unknown and ApiGateway |
| `RequestAirdrop` | Dev/testnet airdrop | **Admin, IamService only** |
| `GetTransactionDetails` | Full transaction details | **All except Unknown** |
| `GetEpochInfo` | Epoch and slot information | **All except Unknown** |

> **Note**: `ApiGateway` is allowed on read operations but **not** on write operations (SimulateTransaction, SubmitTransaction) or infrastructure operations (GetLatestBlockhash, GetSlot, GetRecentPrioritizationFees). `ReportingService` is read-only: it gets every read RPC (including blockhash/slot/fees) but is excluded from SimulateTransaction and SubmitTransaction, and the PolicyEngine denies it (and `ApiGateway`) all transaction submission as defense in depth on the NATS path.

---

## Source Structure

```
gridtokenx-chain-bridge/
├── src/
│   ├── main.rs              # Entry point — server bootstrap, Vault init, NATS setup, DLQ monitor
│   ├── lib.rs               # Library re-exports (api, vault, nats_consumer, harness, middleware)
│   ├── api.rs               # ChainBridgeGrpcService (12 RPCs, ~1250 lines)
│   │                        #   ├── SolanaProvider trait (mockable, 12 async methods)
│   │                        #   ├── RealSolanaProvider (RpcClient delegation)
│   │                        #   ├── SurfpoolSolanaProvider (LiteSVM simnet)
│   │                        #   │     └── deploy_core_programs() — 5 .so files, pre-seeded PDAs
│   │                        #   ├── BlockhashCache (RwLock<Option<(Hash, u64)>>, 2s refresh)
│   │                        #   ├── extract_role() — 3-tier identity resolution
│   │                        #   └── sign_and_submit() — policy + blockhash + Vault signing
│   ├── vault.rs             # VaultProvider trait + VaultTransitClient + InsecureKeypairProvider
│   ├── middleware.rs         # PeerCertLayer — X.509 SAN → SPIFFE URI extraction
│   ├── harness.rs           # MtlsAcceptor — custom TLS acceptor, peer cert injection
│   └── nats_consumer.rs     # NatsConsumer — JetStream pull consumer (submit/simulate/cancel)
├── tests/
│   └── invariants.rs        # Invariant tests (RBAC, service authorization, program allowlisting)
├── build.rs                 # Protobuf code generation (buffa + connectrpc)
└── Cargo.toml               # Dependencies
```

Proto source: `../gridtokenx-blockchain-core/proto/chain_bridge.proto`

---

## Configuration

### Environment Variables

```bash
# Server
CHAIN_BRIDGE_GRPC_PORT=5040              # gRPC listen port (default: 5040)
CHAIN_BRIDGE_INSECURE=true               # Dev mode — no TLS, admin RBAC bypass, policy bypass

# Solana
SOLANA_RPC_URL=http://localhost:8899     # Solana RPC endpoint
SOLANA_NETWORK=localnet                  # localnet | devnet | simnet | mainnet
SOLANA_COMMITMENT=confirmed              # confirmed | finalized | processed

# Vault Transit (production)
VAULT_ADDR=http://localhost:8200         # HashiCorp Vault address
VAULT_TOKEN=root                         # Vault token
CHAIN_BRIDGE_VAULT_KEY_NAME=gridtokenx-bridge  # Transit key name

# mTLS Certificates — server side (generate with `just gen-certs` in the superproject)
CHAIN_BRIDGE_TLS_CERT=infra/certs/server.crt
CHAIN_BRIDGE_TLS_KEY=infra/certs/server.key
CHAIN_BRIDGE_TLS_CA=infra/certs/ca.crt

# mTLS Certificates — client side (read by gridtokenx-blockchain-core BlockchainService)
CHAIN_BRIDGE_CA_CERT=infra/certs/ca.crt          # trust anchor for verifying the bridge
CHAIN_BRIDGE_CLIENT_CERT=infra/certs/client.crt  # per-service cert w/ SPIFFE URI SAN
CHAIN_BRIDGE_CLIENT_KEY=infra/certs/client.key
CHAIN_BRIDGE_TLS_DOMAIN=localhost                # SNI / server-cert hostname (chain-bridge in docker)

# NATS JetStream
NATS_URL=nats://localhost:4222           # NATS server URL

# Dev Escape Hatches
CHAIN_BRIDGE_ALLOW_HEADER_AUTH=1         # Allow header-based auth (dev only)
GATEWAY_SECRET=gridtokenx-gateway-secret-2025  # API Gateway shared secret
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

### Simnet Mode (In-Memory SVM)

```bash
# No Solana validator needed — LiteSVM runs in-process
SOLANA_NETWORK=simnet \
CHAIN_BRIDGE_INSECURE=true \
cargo run
```

### mTLS Mode (default in docker-compose)

```bash
# 1. Generate dev CA + server cert + per-SPIFFE-identity client certs (superproject root)
just gen-certs        # → infra/certs/{ca.crt,server.crt,server.key,clients/*.{crt,key}}

# 2. Start Chain Bridge with TLS
CHAIN_BRIDGE_TLS_CERT=infra/certs/server.crt \
CHAIN_BRIDGE_TLS_KEY=infra/certs/server.key \
CHAIN_BRIDGE_TLS_CA=infra/certs/ca.crt \
cargo run

# 3. Smoke-test with a client cert (identity = SPIFFE URI SAN in the cert)
curl --cacert infra/certs/ca.crt \
     --cert infra/certs/clients/admin.crt --key infra/certs/clients/admin.key \
     -X POST https://localhost:5040/gridtokenx.chain.v1.ChainBridgeService/GetSlot \
     -H 'content-type: application/json' -d '{}'
```

Clients (IAM, Trading, Aggregator Bridge) reuse `gridtokenx-blockchain-core`'s
`BlockchainService`, which picks up `CHAIN_BRIDGE_CA_CERT` / `CHAIN_BRIDGE_CLIENT_CERT` /
`CHAIN_BRIDGE_CLIENT_KEY` and rewrites `http://` → `https://` for the TLS channel.
The dev CA is a static stand-in for SPIRE-issued SVIDs in production. Note the NATS
write path carries a self-asserted `service_identity` (no transport-level proof) —
mTLS secures the gRPC path only; NATS identity hardening is future SPIRE/NATS-auth work.

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
# Run all tests (unit + invariant)
cargo test

# Run only invariant tests
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
| **Unit (api/)** | 41 tests — blockhash cache, submit, simulate, balance, epoch, account data, RBAC (incl. settlement/reporting roles), env overrides | None (mocks) |
| **Unit (nats_consumer/)** | 21 tests — idempotency, staleness, message parsing, identity mapping | None (mocks) |
| **Unit (vault.rs)** | 7 tests — pubkey caching, signature parsing, insecure provider | None (mocks) |
| **Unit (middleware.rs)** | 8 tests — X.509 cert extraction, Tower layer integration | None (mocks) |
| **Invariants (invariants.rs)** | 17 tests — Multi-service program allowlist matrix (incl. settlement allow / reporting & gateway deny), admin bypass, system program | None (mocks) |
| **Policy (blockchain-core)** | 11 tests — per-role program allowlists, read-only role denial, look-alike identity rejection | None |
| **Auth (blockchain-core)** | 8 tests — role mapping, RBAC require/require_any | None |

**Total: 113 tests in these sections (94 in chain-bridge, 19 in blockchain-core)**

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
| **Shared** | `gridtokenx-blockchain-core` | Auth, RBAC, policy engine, metrics, NATS schemas |

---

## Shared Library (`gridtokenx-blockchain-core`)

Chain Bridge depends on `gridtokenx-blockchain-core` for cross-service shared types:

| Module | Types Used |
|--------|-----------|
| `auth` | `SpiffeIdentity`, `ServiceRole` — identity mapping and RBAC |
| `policy` | `PolicyEngine` — per-instruction program ID validation |
| `config` | `SolanaProgramsConfig` — allowlisted program IDs per environment |
| `rpc::nats_schema` | `TxSubmitMessage`, `TxResultMessage`, `TxSimulateMessage`, etc. |
| `rpc::metrics` | `BlockchainMetrics` trait for observability |

---

## License

Part of the GridTokenX Ecosystem — Proprietary

---

_Maintained by the GridTokenX Engineering Team._
