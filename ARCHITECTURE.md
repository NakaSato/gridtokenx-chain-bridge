# gridtokenx-chain-bridge — Architecture

> The **single gateway** between every GridTokenX backend service and the Solana blockchain — the
> only process that holds signing authority or reaches Solana RPC. A **Reference Monitor**: every
> write converges on one mediated pipeline (RBAC → policy → Vault signing → RPC submit).
>
> This repo is a **git submodule** of the `gridtokenx-coresystem` superproject. Platform-wide rules
> live in the superproject. This doc covers **only** the contents of this folder.

---

## 1. What This Is

A running **service** — a binary backed by a multi-crate Cargo workspace. It exposes two ingress
paths and one exit:

- **Synchronous reads** — ConnectRPC/gRPC over mTLS on `:5040` (`CHAIN_BRIDGE_GRPC_PORT`): balance,
  account data, slot, blockhash, fees, signature status, tx details, epoch.
- **Asynchronous writes** — a NATS JetStream consumer on subjects `chain.tx.submit | simulate |
  cancel`.
- **Signing** — delegated to HashiCorp **Vault Transit** (Ed25519). Private keys never enter the
  process address space.

Workspace root is a **virtual manifest** (`members = ["crates/*"]`, `resolver = "3"`, edition 2024).
The release profile is `lto + codegen-units=1 + panic=abort + strip`. Build/run from this directory —
do **not** `cargo` from the superproject root. The bin name is `gridtokenx-chain-bridge`; the lib is
`gridtokenx_chain_bridge`.

The four member crates form a hexagonal split:

| Crate | Role |
| :--- | :--- |
| `chain-bridge-api` | the **binary + lib `gridtokenx_chain_bridge`** — all driving adapters (gRPC service, NATS consumer, mTLS server, wiring). Holds the live signing path. |
| `chain-bridge-core` | hexagonal **ports** + domain types (`SignerPort`, `ChainClientPort`, `PreSignSimulatorPort`, `AuditPort`, `NoncePort`, audit hash-chain, errors). |
| `chain-bridge-persistence` | **driven adapters** over the core ports (`PostgresAuditStore`, plus the not-yet-live `vault_signer` / `solana_client` / `litesvm_sim` / `nonce_store` forks). |
| `chain-bridge-protocol` | generated ConnectRPC bindings (orphan — `chain-bridge-api` still generates its own via `build.rs`; consolidate when the live path adopts it). |

## 2. Module Layout

```
crates/
├── chain-bridge-api/            # binary + lib — the live driving adapters
│   ├── build.rs                 #   compiles ../../../gridtokenx-blockchain-core/proto/chain_bridge.proto (tonic + ConnectRPC)
│   └── src/
│       ├── lib.rs               #   exposes 6 modules: api, vault, nats_consumer, harness, middleware, agent_memory
│       ├── main.rs              #   wiring: provider/vault selection, blockhash refresh, NATS + DLQ consumers, TLS bind
│       ├── api/
│       │   ├── mod.rs           #     re-exports + `chain_v1` generated proto types
│       │   ├── provider.rs      #     SolanaProvider trait + Real/Surfpool impls + BlockhashCache
│       │   ├── service.rs       #     ChainBridgeGrpcService — gRPC handlers, extract_role, sign_and_submit pipeline
│       │   └── tests.rs
│       ├── nats_consumer/
│       │   ├── mod.rs
│       │   ├── consumer.rs      #     subscribe loop + handle_submit/simulate/cancel + claim_or_replay
│       │   ├── dedup.rs         #     effect-level dedup types (DedupRecord, DedupState)
│       │   └── tests.rs
│       ├── vault.rs             #   VaultProvider trait, VaultTransitClient, dev-only InsecureKeypairProvider
│       ├── harness.rs           #   MtlsAcceptor / MtlsStream — injects peer certs into request extensions
│       ├── middleware.rs        #   PeerCertLayer — extracts SPIFFE URI from peer cert
│       └── agent_memory.rs      #   self-contained embedding / cosine-similarity memory store
├── chain-bridge-core/src/       # hexagonal ports + domain (no I/O)
│   ├── lib.rs
│   ├── ports.rs                 #   SignerPort, ChainClientPort, PreSignSimulatorPort, AuditPort, NoncePort
│   ├── audit.rs                 #   audit hash-chain
│   ├── nonce.rs
│   └── error.rs
├── chain-bridge-persistence/src/  # driven adapters over the core ports
│   ├── lib.rs
│   ├── postgres_audit.rs        #   PostgresAuditStore (live)
│   ├── vault_signer.rs          #   not-yet-live fork
│   ├── solana_client.rs         #   not-yet-live fork
│   ├── litesvm_sim.rs           #   not-yet-live fork
│   └── nonce_store.rs
└── chain-bridge-protocol/src/
    └── lib.rs                   #   orphan generated ConnectRPC bindings

migrations/0001_audit_log.sql    # Postgres audit-log schema
```

## 3. Architecture

### Layering

`chain-bridge-api` is the only crate with a live driving path. `main.rs` is the wiring entry point:
it selects the Solana provider and Vault provider, spawns the background blockhash refresh, starts the
NATS + DLQ consumers, and binds the mTLS gRPC server. Domain ports (`chain-bridge-core`) are pure and
I/O-free; driven adapters (`chain-bridge-persistence`) implement them.

`build.rs` codegen depends on a **sibling submodule**: it compiles
`../../../gridtokenx-blockchain-core/proto/chain_bridge.proto` into both tonic and ConnectRPC outputs,
re-exported as `api::chain_v1`. If `gridtokenx-blockchain-core` is un-inited, the build fails before
compiling any source.

### The Central Split — Reads vs Writes

- **Reads** → synchronous **gRPC/ConnectRPC** over mTLS on `:5040`. Served from `ChainBridgeGrpcService`
  handlers in `api/service.rs`.
- **Writes** → asynchronous **NATS JetStream** consumer (`chain.tx.submit | simulate | cancel`) in
  `nats_consumer/consumer.rs`.

Both paths funnel into a single mediated pipeline, `ChainBridgeGrpcService::sign_and_submit`:
RBAC (`extract_role`) → PolicyEngine validation → Vault signing → RPC submit.

### Signing (Vault Transit)

Signing is delegated to HashiCorp **Vault Transit** (Ed25519) via `VaultTransitClient`. Keys never
enter the process. The Vault key name comes from `CHAIN_BRIDGE_VAULT_KEY_NAME` (default
`gridtokenx-bridge`), **not** from the request. A dev-only `InsecureKeypairProvider` substitutes a
local keypair when running without Vault.

### Solana provider selection

`SurfpoolSolanaProvider` (in-memory SVM via `litesvm`) is selected when `SOLANA_NETWORK=simnet`;
otherwise `RealSolanaProvider` runs against `SOLANA_RPC_URL`.

### Bind address

`main.rs` binds **`0.0.0.0:<CHAIN_BRIDGE_GRPC_PORT>`** (`crates/chain-bridge-api/src/main.rs:159`).
The superproject doc claims "127.0.0.1 only"; the code binds `0.0.0.0`. Confirm intent before changing
the bind.

## 4. Load-Bearing Invariants

1. **One signing path.** All writes (gRPC *and* NATS) must funnel through `sign_and_submit`. Never add
   a second place that touches Vault or submits a transaction. PolicyEngine validation lives inside it.
2. **Signing is gated on `key_id == "platform_admin"`.** Any other non-empty `key_id` is rejected
   (`Key ID not authorized`). The Vault key name comes from `CHAIN_BRIDGE_VAULT_KEY_NAME`, not the
   request.
3. **Identity comes from L4, not L7.** `extract_role` trusts the SPIFFE URI from the verified mTLS cert
   (`z-gridtokenx-spiffe-id` header injected by the acceptor) → `ServiceRole`. Header-based auth is a
   dev-only escape hatch behind `CHAIN_BRIDGE_ALLOW_HEADER_AUTH`; `CHAIN_BRIDGE_INSECURE=true` grants
   blanket `Admin`. Unverified callers get `ServiceRole::Unknown`.
4. **Blockhash is served from cache.** A background task refreshes `BlockhashCache` every 2s.
   `sign_and_submit` reads the cache and only falls back to a slow RPC call if empty. Don't add a
   synchronous `get_latest_blockhash` to the hot path.
5. **Effect-level idempotency.** Submit/cancel dedup on `idempotency_key` via `claim_or_replay`: a
   successful submit is recorded `Done` and replayed to later re-sends; an `InFlight` claim blocks a
   duplicate on-chain tx; a failure releases the claim so a genuine retry can run. This is distinct
   from the per-`correlation_id` `idempotency_cache` that guards JetStream redelivery.
6. **mTLS is default.** Server runs mTLS unless `CHAIN_BRIDGE_INSECURE=true`. Certs read from
   `CHAIN_BRIDGE_TLS_{CERT,KEY,CA}` (default `infra/certs/*`). The custom `MtlsAcceptor` is what makes
   peer certs visible to `PeerCertLayer` — a plain rustls acceptor would drop them.
7. **NATS is optional at startup.** If `async_nats::connect` fails, the service logs a warning and runs
   gRPC-only. A "NATS path disabled" warning is not a crash.
8. **DLQ is advisory, not recovery.** `run_dlq_monitor` logs dead letters and acks them — it does not
   replay or reprocess. Failed txs don't auto-recover.

## 5. Commands

`cd` into this directory first; `cargo check`/`test`/`build` from here cover all member crates.

```bash
cargo check
cargo test                                  # cfg(test) submodules + tests/invariants.rs
cargo test test_name -- --nocapture         # single test
cargo build --release                       # profile: lto + panic=abort + strip
cargo build --release --bin gridtokenx-chain-bridge
cargo run --bin gridtokenx-chain-bridge
```

`build.rs` codegen requires the sibling `gridtokenx-blockchain-core` submodule to be inited, or the
build fails before any source compiles.

### Key environment variables

| Var | Default | Purpose |
| :--- | :--- | :--- |
| `CHAIN_BRIDGE_GRPC_PORT` | `5040` | gRPC/ConnectRPC listen port |
| `CHAIN_BRIDGE_INSECURE` | `false` | disable TLS, grant `Admin` to all callers (dev only) |
| `CHAIN_BRIDGE_ALLOW_HEADER_AUTH` | unset | trust role from headers instead of mTLS (dev only) |
| `CHAIN_BRIDGE_TLS_CERT/KEY/CA` | `infra/certs/*` | mTLS material |
| `CHAIN_BRIDGE_VAULT_KEY_NAME` | `gridtokenx-bridge` | Vault Transit key used for signing |
| `VAULT_ADDR` / `VAULT_TOKEN` | `http://localhost:8200` / `root` | Vault Transit endpoint |
| `SOLANA_NETWORK` | `mainnet` | `simnet` selects the in-memory Surfpool provider |
| `SOLANA_RPC_URL` | `http://localhost:8899` | Solana RPC endpoint |
| `NATS_URL` | `nats://localhost:4222` | JetStream connection |

## 6. Further Reading (in this repo)

| File | Covers |
| :--- | :--- |
| `CLAUDE.md` | Fast orientation + LLM working rules |
| `SKILL.md` | Deep reference — request path, invariants, change recipes, scaling "walls", AI agent-memory |
| `README.md` | Architecture diagram, security/trust model, formal invariants |
| `crates/chain-bridge-api/tests/invariants.rs` | Integration tests asserting the safety/liveness properties |
| `migrations/0001_audit_log.sql` | Postgres audit-log schema |
| `../gridtokenx-blockchain-core/proto/chain_bridge.proto` | gRPC contract compiled by `build.rs` (sibling submodule) |
