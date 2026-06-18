# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> This is the **Chain Bridge** service — one submodule of the `gridtokenx-coresystem` superproject.
> The superproject `CLAUDE.md` (one level up) holds platform-wide rules; this file is Chain-Bridge-specific.
> **Before editing handlers or consumers, read [SKILL.md](SKILL.md)** — it documents the request path, the
> four invariants, and the high-TPS "walls". This file is the quick map; SKILL.md is the deep reference.

---

## What this service is

The **single gateway** between every GridTokenX backend service and the Solana blockchain. No other service
holds private keys or calls Solana RPC directly. Chain Bridge is a **Reference Monitor**: every write
converges on one mediated pipeline (`ChainBridgeGrpcService::sign_and_submit`) that does RBAC → PolicyEngine →
Vault signing → RPC submit.

Two ingress paths, one exit:
- **Synchronous reads** — ConnectRPC/gRPC over mTLS on `:5040` (`CHAIN_BRIDGE_GRPC_PORT`). Balance, account
  data, slot, blockhash, fees, signature status, tx details, epoch.
- **Asynchronous writes** — NATS JetStream consumer on subjects `chain.tx.submit | simulate | cancel`.
- **Signing** — delegated to HashiCorp **Vault Transit** (Ed25519). Keys never enter the process address space.

## Build & test

Multi-crate Cargo workspace (root = virtual manifest, `members = ["crates/*"]`), **edition 2024**. `cd` into
this dir first — do **not** `cargo` from the superproject root. `cargo check`/`test`/`build` from here cover
all member crates.

```bash
cargo check
cargo test                                  # unit tests live in cfg(test) submodules + tests/invariants.rs
cargo test test_name -- --nocapture         # single test
cargo build --release                       # profile: lto + panic=abort + strip
```

**`build.rs` codegen depends on a sibling submodule**: `crates/chain-bridge-api/build.rs` compiles
`../../../gridtokenx-blockchain-core/proto/chain_bridge.proto` into both tonic and ConnectRPC outputs. If
`gridtokenx-blockchain-core` is missing (un-inited submodule), the build fails before compiling any source.
The generated proto types are re-exported through `api::chain_v1`.

## Workspace layout (post crate split)

The root `Cargo.toml` is a **virtual manifest** (no package) — `members = ["crates/*"]`. Build/run from the root;
the target dir and bin name are unchanged (`cargo build --release --bin gridtokenx-chain-bridge`).

| Crate | Role |
| --- | --- |
| `crates/chain-bridge-api` | the **binary + lib `gridtokenx_chain_bridge`** — all driving adapters (gRPC service, NATS consumer, mTLS server, wiring). Holds the live signing path. |
| `crates/chain-bridge-core` | hexagonal **ports** + domain types (`AuditPort`, `SignerPort`, audit hash-chain, errors). |
| `crates/chain-bridge-persistence` | **driven adapters** over the core ports (`PostgresAuditStore`, plus the not-yet-live `vault_signer`/`solana_client`/`litesvm_sim` forks). |
| `crates/chain-bridge-protocol` | generated ConnectRPC bindings (orphan — `chain-bridge-api` still generates its own via `build.rs`; consolidate when the live path adopts it). |

### `chain-bridge-api` module map (post god-file split)

`src/lib.rs` exposes six modules. The `api` and `nats_consumer` mods were split out of former monolithic files;
each `mod.rs` re-exports shared `use` imports that submodules pull in via `use super::*;` — keep that pattern when
adding submodules. Paths below are under `crates/chain-bridge-api/src/`.

| Path | Role |
| --- | --- |
| `api/provider.rs` | `SolanaProvider` trait + `RealSolanaProvider` / `SurfpoolSolanaProvider` impls + `BlockhashCache` |
| `api/service.rs` | `ChainBridgeGrpcService` — all gRPC handlers, `extract_role`, the `sign_and_submit` pipeline |
| `nats_consumer/consumer.rs` | `NatsConsumer` subscribe loop + `handle_submit/simulate/cancel` + `claim_or_replay` |
| `nats_consumer/auth.rs` | envelope authentication — `NatsAuthPolicy`, `check_envelope_auth` (cert→CA→SAN→signature) |
| `nats_consumer/dedup.rs` | effect-level dedup types (`DedupRecord`, `DedupState`) |
| `vault.rs` | `VaultProvider` trait, `VaultTransitClient`, dev-only `InsecureKeypairProvider` |
| `harness.rs` | `MtlsAcceptor` / `MtlsStream` — custom axum-server acceptor injecting peer certs into request extensions |
| `middleware.rs` | `PeerCertLayer` — extracts SPIFFE URI from the peer cert, propagates mTLS identity |
| `agent_memory.rs` | self-contained embedding/cosine-similarity memory store (see SKILL.md §AI Agent Memory) |
| `main.rs` | wiring: provider/vault selection, background blockhash refresh, NATS + DLQ consumers, TLS server bind |

## Key invariants — don't break these

- **One signing path.** All writes (gRPC *and* NATS) must funnel through `sign_and_submit`. Never add a
  second place that touches Vault or submits a transaction. PolicyEngine validation happens inside it.
- **Signing is gated on `key_id == "platform_admin"`.** Any other non-empty `key_id` is rejected
  (`Key ID not authorized`). The Vault key name itself comes from `CHAIN_BRIDGE_VAULT_KEY_NAME`
  (default `gridtokenx-bridge`), not from the request.
- **Identity comes from L4, not L7.** `extract_role` trusts the SPIFFE URI from the verified mTLS cert
  (`z-gridtokenx-spiffe-id` header injected by the acceptor) → `ServiceRole`. Header-based auth
  (`ServiceRole::from_headers`) is a dev-only escape hatch behind `CHAIN_BRIDGE_ALLOW_HEADER_AUTH`;
  `CHAIN_BRIDGE_INSECURE=true` grants blanket `Admin`. Unverified callers get `ServiceRole::Unknown`.
- **NATS identity is cryptographically bound to the mTLS cert.** Publishers sign the canonical envelope
  bytes (`gridtokenx_blockchain_core::rpc::envelope_auth`) with their mTLS client key; the consumer's
  `nats_consumer/auth.rs` verifies cert → CA (`CHAIN_BRIDGE_TLS_CA`) → SPIFFE SAN == `service_identity` →
  signature, **before** RBAC and any dedup claim. Rejection only when `CHAIN_BRIDGE_REQUIRE_SIGNED_NATS=true`
  (default log-only with `nats_auth_*` metrics). Verification logic is pure (explicit flags + CA bytes) —
  tests must never mutate env (the `role_from` race rule).
- **Blockhash is served from cache.** A background task refreshes `BlockhashCache` every 2s (Wall 1
  mitigation). `sign_and_submit` reads the cache and only falls back to a slow RPC call if empty. Don't add
  a synchronous `get_latest_blockhash` to the hot path.
- **Effect-level idempotency.** Submit/cancel dedup on `idempotency_key` via `claim_or_replay`: a successful
  submit is recorded `Done` and replayed to later re-sends; an `InFlight` claim blocks a duplicate on-chain tx;
  a failure releases the claim so a genuine retry can run. This is distinct from the per-`correlation_id`
  `idempotency_cache` that guards JetStream redelivery.

## Gotchas

- **DLQ is advisory, not recovery.** `run_dlq_monitor` logs dead letters and acks them — it does not replay or
  reprocess. Don't assume failed txs auto-recover.
- **NATS is optional at startup.** If `async_nats::connect` fails, the service logs a warning and runs
  gRPC-only. A "NATS path disabled" warning is not a crash.
- **mTLS by default.** Server runs mTLS unless `CHAIN_BRIDGE_INSECURE=true`. Certs read from
  `CHAIN_BRIDGE_TLS_{CERT,KEY,CA}` (default `infra/certs/*`). The custom `MtlsAcceptor` is what makes peer
  certs visible to `PeerCertLayer` — a plain rustls acceptor would drop them.
- **`SurfpoolSolanaProvider`** (in-memory SVM via `litesvm`) is selected when `SOLANA_NETWORK=simnet`;
  otherwise `RealSolanaProvider` against `SOLANA_RPC_URL`.
- **Bind address:** `main.rs` binds `0.0.0.0:<port>`. The superproject doc claims "127.0.0.1 only" — if you
  touch the bind, confirm which is intended before changing it.

## Environment variables

| Var | Default | Purpose |
| --- | --- | --- |
| `CHAIN_BRIDGE_GRPC_PORT` | `5040` | gRPC/ConnectRPC listen port |
| `CHAIN_BRIDGE_INSECURE` | `false` | disable TLS, grant `Admin` to all callers (dev only) |
| `CHAIN_BRIDGE_ALLOW_HEADER_AUTH` | unset | trust role from headers instead of mTLS (dev only) |
| `CHAIN_BRIDGE_TLS_CERT/KEY/CA` | `infra/certs/*` | mTLS material |
| `CHAIN_BRIDGE_VAULT_KEY_NAME` | `gridtokenx-bridge` | Vault Transit key used for signing |
| `CHAIN_BRIDGE_REQUIRE_SIGNED_NATS` | `false` | reject NATS envelopes without a valid `EnvelopeAuth` (false = log-only) |
| `VAULT_ADDR` / `VAULT_TOKEN` | `http://localhost:8200` / `root` | Vault Transit endpoint |
| `SOLANA_NETWORK` | `mainnet` | `simnet` selects the in-memory Surfpool provider |
| `SOLANA_RPC_URL` | `http://localhost:8899` | Solana RPC endpoint |
| `NATS_URL` | `nats://localhost:4222` | JetStream connection |

## Where to read more

- **[SKILL.md](SKILL.md)** — request-path walkthrough, "adding a new gRPC handler / NATS subject" recipes,
  Vault signing details, the six scaling "walls" (Vault throughput, sync provider, single-threaded consumer,
  cache contention, mTLS amortisation, observability), and the AI agent-memory patterns.
- **[README.md](README.md)** — architecture diagram, security/trust model, formal invariants.
- **`tests/invariants.rs`** — integration tests asserting the safety/liveness properties.

## Search Tooling

> **Use `rg` (ripgrep), never `grep`.** When shelling out to search files, run `rg` —
> it respects `.gitignore`, skips binaries, and is far faster than `grep`/`find -exec grep`.
> Reserve plain `grep` only for piping non-file streams.
