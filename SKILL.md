---
name: gridtokenx-chain-bridge-patterns
description: Patterns, gotchas, and high-throughput performance guidance for building and maintaining GridTokenX Rust microservices — specifically the chain-bridge service (the Solana signing protocol bridge) and its siblings that use mTLS + SPIFFE identity, connectrpc, NATS JetStream, HashiCorp Vault Transit signing, and Solana RPC. Use this skill whenever the user is working on chain-bridge (src/api.rs, src/main.rs, src/middleware.rs, src/nats_consumer.rs, src/vault.rs), adding a new gRPC handler, adding a new NATS subject, wiring up a new GridTokenX service, debugging mTLS or SPIFFE identity issues, integrating Vault Transit signing, reasoning about RBAC between platform microservices, OR scaling the bridge to high throughput (questions about TPS targets, Vault signing bottlenecks, Solana RPC pooling, NATS worker concurrency, connection reuse, backpressure, latency budgets, or any question framed as "how do we get to N TPS"). Consult this skill even if the user's question sounds generic ("how do I add auth to this handler", "why is my SPIFFE ID not showing up", "this service feels slow", "can we handle more load") when they're in a GridTokenX repo.
---

# GridTokenX Chain Bridge Patterns

This skill captures the architectural patterns used by the **chain-bridge** service — the one component in GridTokenX that holds Solana signing authority — and generalises them to other microservices (trading-matcher, oracle-bridge, iam-service, api-gateway) that live in the same platform.

The chain-bridge is the canonical reference: when in doubt about how to wire mTLS identity, how to structure a connectrpc handler, how to consume NATS with idempotency, or how to sign with Vault Transit, look at its implementation first and copy the shape.

---

## What the chain-bridge actually is

A narrow, high-trust service. Its only job is:

1. **Hold the Solana signing authority** for platform-admin transactions (never in app memory — always remote-signed by Vault Transit).
2. **Forward Solana RPC reads** (`get_balance`, `get_account_data`, `get_latest_blockhash`, etc.) to caller services with RBAC on top.
3. **Accept transaction submit/simulate requests** over two transports:
   - **gRPC (connectrpc over mTLS)** — synchronous, for interactive callers.
   - **NATS JetStream** — asynchronous, with idempotency, staleness rejection, retries, and a DLQ.

Everything else in the platform talks *to* the chain-bridge. The chain-bridge talks *to* Vault and Solana. That's the whole shape.

---

## The four invariants

These are the non-negotiables. If a change violates any of them, stop and reconsider.

1. **Private keys never leave Vault.** `VaultTransitClient::sign_message` sends message bytes to Vault and gets a `Signature` back. There is no `Keypair` in app memory, no local `.json` keyfile, nothing. If you find yourself reaching for `solana_sdk::signature::Keypair`, you're going the wrong way.

2. **Identity comes from mTLS, not from headers the client set.** The `MtlsAcceptor` extracts peer certificates after the TLS handshake, `PeerCertLayer` parses the SPIFFE URI from the cert's SAN, and *that* is what populates the synthetic `z-gridtokenx-spiffe-id` header. A client cannot forge this by setting the header themselves — the `ConnectionService` injects the verified cert into request extensions, and the middleware derives the header from the cert, overwriting any client-supplied value. Header-based auth is only accepted when `CHAIN_BRIDGE_ALLOW_HEADER_AUTH` is set, which is a dev-only escape hatch.

3. **Every handler does an RBAC check before work.** The pattern is always:
   ```rust
   let role = self.extract_role(&ctx);
   role.require_any(&[ServiceRole::Foo, ServiceRole::Admin])
       .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;
   ```
   Never do work before the check. Never skip the check "because this endpoint is read-only."

4. **`sign_and_submit` is the one signing path.** Both the gRPC `submit_transaction` handler and the NATS `handle_submit` consumer call `ChainBridgeGrpcService::sign_and_submit`. Don't fork this logic. If you need new signing behaviour (e.g. a second key ID), extend that function, don't reimplement it.

---

## The request-path shape (study this before editing handlers)

A gRPC request flows through these layers, and most bugs come from misunderstanding one of them:

```
TCP → rustls TLS handshake (MtlsAcceptor)
    → peer cert bytes captured into Arc<Vec<Vec<u8>>>
    → ConnectionService injects certs into request.extensions
    → PeerCertLayer parses SPIFFE URI from cert SAN
    → inserts VerifiedSpiffeUri + SpiffeIdentity into extensions
    → inserts synthetic "z-gridtokenx-spiffe-id" header
    → connectrpc router dispatches to handler
    → handler calls extract_role(&ctx) which reads the header
    → role.require_any([...]) gates the work
    → handler calls self.provider.* for Solana RPC
    → (if signing) self.sign_and_submit → vault.sign_message → provider.send_transaction
```

Two things about this that trip people up:

- **The header is synthesised from the cert**, not trusted from the client. `PeerCertLayer` *overwrites* whatever the client sent.
- **`extract_role` reads the header, not the extension directly.** This is a quirk of getting connectrpc's `Context` to carry peer identity — the cert → extension → header → context chain is indirect. If you're adding a new transport (say, a WebSocket gateway), either wire identity through a `Context` extension, or replicate the synthetic-header trick.

---

## Adding a new gRPC handler

Follow the existing shape in `src/api.rs`. The five steps, in order:

1. **Add the RPC to `chain_bridge.proto`** — request/response messages and the service method.
2. **Regenerate** — `buffa` build script emits the `*View` and `*Response` types. If they don't appear in `chain_v1`, check `build.rs` and the `include!(...)` glob.
3. **Implement the handler on `ChainBridgeGrpcService`:**
   ```rust
   async fn your_new_method(
       &self,
       ctx: Context,
       request: OwnedView<YourRequestView<'static>>,
   ) -> Result<(YourResponse, Context), ConnectError> {
       let role = self.extract_role(&ctx);
       role.require_any(&[/* which services may call this */])
           .map_err(|(_, msg)| ConnectError::permission_denied(msg))?;

       info!("🔗 gRPC Received your_new_method for {}", request.some_field);

       // ... real work via self.provider or self.vault ...

       let mut response = YourResponse::default();
       // populate response
       Ok((response, ctx))
   }
   ```
4. **Pick the RBAC allowlist carefully.** Look at the existing handlers:
   - Read-only and broadly useful → `ApiGateway, TradingApi, TradingMatcher, OracleBridge, IamService, Admin` (see `get_balance`, `get_account_data`).
   - Signing-capable → exclude `ApiGateway` (see `submit_transaction`). The api-gateway is the most exposed surface and should never be able to submit transactions directly.
   - Blockhash/slot/fees → no `ApiGateway` either, since these are typically needed only by signers and matchers.
5. **Add a unit test** using `MockSolanaProvider` and `authenticated_ctx()`. If the handler has an RBAC constraint tighter than the default admin role, also add an `unauthenticated_ctx()` test.

**Do not** accept ambient `Result<_, ClientError>` from `solana_client` without wrapping it into a `ConnectError`. Leaking Solana RPC error types across the gRPC boundary leaks internal state.

---

## Adding a new NATS subject

NATS is for fire-and-forget or async reply patterns. The shape in `src/nats_consumer.rs`:

1. **Declare the subject under `chain.*.*`** (the stream binds `chain.tx.*`; if your subject lives outside that prefix, extend the `subjects` vec in `start()`).
2. **Define the envelope in `gridtokenx_blockchain_core::rpc::nats_schema`** — it must carry `correlation_id`, `service_identity` (SPIFFE URI string), `reply_subject`, and `created_at_ms` at minimum. Everything you add should follow that shape.
3. **Match on `msg.subject.as_str()` in `NatsConsumer::start`** and dispatch to a new `handle_*` method.
4. **Inside the handler, do the four checks in this order:**
   1. Deserialize payload → `Term` ack on failure (no retry — it's a bad message).
   2. RBAC: `ServiceRole::from(&SpiffeIdentity(envelope.service_identity))`. `Unknown` → `Term` ack.
   3. Idempotency: check `self.idempotency_cache`. If hit, plain `ack` and return — the caller already got a reply last time.
   4. Staleness: reject if `now_ms - envelope.created_at_ms > 55_000`. The Solana blockhash window is ~60s, so anything older than 55s is likely to fail anyway; fail fast with a clear error instead of burning RPC.
5. **Use `tokio_retry` only for transient failures** (Solana RPC flakes, node-behind). Don't retry deserialization failures, auth failures, or stale messages.
6. **Publish the result to `envelope.reply_subject`.** Both success and failure paths must publish — the caller is awaiting it.
7. **Always `ack` the message** once you've published a result (success or failure). `Term` only for malformed/unauthenticated messages that should never be retried.

### The DLQ is currently advisory, not recovery

`run_dlq_monitor` in `main.rs` consumes `chain.tx.dlq.*` and logs `🔥 DEAD LETTER DETECTED`. It does not retry, does not alert externally, does not page anyone. If you need actual recovery, wire it into the metrics system and a pager. Until then, treat DLQ hits as something a human investigates via logs.

---

## Vault Transit signing — what to know

`VaultTransitClient` in `src/vault.rs` is deliberately tiny. Two methods:

- `get_public_key(key_name)` — fetches + caches the ed25519 public key. Used when you need the Solana pubkey of the platform-admin key (e.g. to derive PDAs or set as a fee payer).
- `sign_message(key_name, message_bytes)` — sends bytes, receives a `solana_sdk::signature::Signature`.

The flow inside `sign_and_submit`:

```rust
// 1. Refresh blockhash (Vault signatures take ~100ms, so old blockhash = failed tx)
transaction.message.recent_blockhash = self.provider.get_latest_blockhash()?.0;

// 2. Sign the *message data*, not the whole transaction
let message_data = transaction.message_data();
let signature = self.vault.sign_message(&self.transit_key_name, &message_data).await?;

// 3. Attach at slot 0 (the fee payer slot for num_required_signatures=1)
if transaction.signatures.is_empty() {
    transaction.signatures.push(signature);
} else {
    transaction.signatures[0] = signature;
}
```

**Gotchas:**

- **Vault's signature format is `vault:v1:<base64>`** — the parsing in `sign_message` splits on `:` and takes the last segment. If Vault changes format across versions, this will break; the error will be "Invalid signature length from Vault" since the base64 of a version string won't decode to 64 bytes.
- **Only `key_id == "platform_admin"` triggers Vault signing.** Any other non-empty `key_id` returns an error. An empty `key_id` passes through *unsigned*, which is correct when the caller already produced a fully-signed transaction (e.g. a user-signed tx from the frontend).
- **The fee payer must be slot 0.** If you construct multi-signer transactions, Vault signs the *fee payer* slot only; other signers must have signed before the tx reaches chain-bridge.
- **`transit_key_name` comes from `CHAIN_BRIDGE_VAULT_KEY_NAME`**, defaulting to `gridtokenx-bridge`. Dev and prod must have different key names — never reuse.

---

## RBAC: how `ServiceRole` is decided

In order of precedence inside `extract_role`:

1. **mTLS SPIFFE URI from the verified cert** (via the synthetic header). This is the only path that should matter in production.
2. **`CHAIN_BRIDGE_INSECURE=true`** → grants `Admin` unconditionally. **Never set this in prod.** It's for local dev without certs.
3. **`CHAIN_BRIDGE_ALLOW_HEADER_AUTH` set (any value)** → trusts client-supplied headers via `ServiceRole::from_headers`. **Never set this in prod.** It's for integration tests where standing up a cert chain is painful.
4. Fallthrough → `ServiceRole::Unknown`, which fails every `require_any` check.

Both escape hatches log a `⚠️` warning on every call. If you see that warning stream in a prod log, someone has misconfigured the deployment and the service is effectively unauthenticated — treat as an incident.

### SPIFFE URI → ServiceRole mapping

This is done by `gridtokenx_blockchain_core::auth::ServiceRole::from(&SpiffeIdentity)`. The SPIFFE URIs follow `spiffe://gridtokenx.th/<env>/<service-name>` (e.g. `spiffe://gridtokenx.th/prod/trading-matcher`). Keep new service names consistent with whatever that function recognises — if you add a new service, you must also extend that enum and its `From` impl in the core crate.

---

## The `SolanaProvider` trait — why it exists

All Solana RPC goes through an `Arc<dyn SolanaProvider>`, not directly through `RpcClient`. This exists for exactly one reason: **tests need a mock**. `MockSolanaProvider` in the test module returns deterministic responses so handler logic can be tested without a running validator.

When adding a new Solana RPC call:

1. Add the method to the `SolanaProvider` trait.
2. Implement it on `RealSolanaProvider` by delegating to `self.client.<method>`.
3. Implement it on `MockSolanaProvider` (in the test module) with a fixed response.
4. Use `self.provider.<method>` in the handler — **never `RpcClient` directly**.

If you find yourself wanting to construct an `RpcClient` inside a handler, stop — that call won't be testable and will bypass the trait boundary.

---

## Running and debugging locally

**Fully insecure local mode** (no certs, no Vault):
```bash
export CHAIN_BRIDGE_INSECURE=true
export SOLANA_RPC_URL=http://localhost:8899   # solana-test-validator
export NATS_URL=nats://localhost:4222         # optional; warns and skips if absent
cargo run
```
Expect: `⚠️ Chain Bridge starting in INSECURE mode (no TLS)` in the first few log lines. If you don't see it, you're hitting the mTLS path and will need certs.

**mTLS local mode:**
```bash
export CHAIN_BRIDGE_TLS_CERT=infra/certs/server.crt
export CHAIN_BRIDGE_TLS_KEY=infra/certs/server.key
export CHAIN_BRIDGE_TLS_CA=infra/certs/ca.crt
export VAULT_ADDR=http://localhost:8200
export VAULT_TOKEN=root
cargo run
```
The client certs must carry a SPIFFE URI in the SAN — otherwise `extract_spiffe_id` returns `None`, no identity gets injected, `extract_role` returns `Unknown`, and every call gets `PermissionDenied`. If you're seeing blanket permission-denied in local dev, that's usually the cause. Generate certs with a SAN URI like `spiffe://gridtokenx.th/dev/trading-matcher`.

**NATS is optional at startup.** The `if let Ok(nats_client) = async_nats::connect(...)` means the service starts even if NATS is down — you just won't have the async path. Look for `✅ Connected to NATS` vs `⚠️ Failed to connect to NATS` in the logs.

### Common symptoms → likely causes

| Symptom | Usually means |
|---|---|
| Every gRPC call returns `PermissionDenied` | Client cert has no SPIFFE URI in SAN, OR `extract_spiffe_id` is failing to parse the SAN |
| `Vault Transit sign request failed` | `VAULT_ADDR` unreachable, or `VAULT_TOKEN` lacks `transit/sign/<key>` capability, or the key doesn't exist yet (`vault write -f transit/keys/gridtokenx-bridge type=ed25519`) |
| NATS tx results never arrive | Caller isn't subscribed to `reply_subject`, or `created_at_ms` is more than 55s old and message is being rejected as stale |
| `Invalid signature length from Vault` | Vault returned a non-ed25519 signature — check the key type (`vault read transit/keys/<name>`) |
| Transaction succeeds locally but fails on-chain with `BlockhashNotFound` | Blockhash refresh in `sign_and_submit` is racing with Vault latency — unusual but possible on a slow Vault; add a retry around `send_transaction` with a fresh blockhash |
| Handler works in tests but 500s in prod | You're probably using `RpcClient` directly somewhere instead of `self.provider` |

---

## Building a sibling service in this style

If you're scaffolding a new service (say, `oracle-bridge` or a new `grid-reliability` service), copy the chain-bridge shape:

1. **`main.rs`** — keep the `MtlsAcceptor` + `ConnectionService` + `PeerCertLayer` trio verbatim. They're not specific to chain-bridge; they're the platform mTLS harness. Factor them into a shared crate if you're doing this more than twice.
2. **A `*GrpcService` struct** holding whatever clients you need (Solana provider, Vault client, a DB pool, etc.) behind `Arc`s. Put an `extract_role(&ctx)` method on it and call it first in every handler.
3. **A `SolanaProvider`-equivalent trait** if you touch external systems — DB, RPC, HTTP APIs, anything non-deterministic. Mockability is cheap to add upfront and expensive to retrofit.
4. **NATS consumer** if you need async — reuse the envelope shape (`correlation_id`, `service_identity`, `reply_subject`, `created_at_ms`), the four checks (deserialize, RBAC, idempotency, staleness), and the DLQ pattern.
5. **`CHAIN_BRIDGE_INSECURE` / `CHAIN_BRIDGE_ALLOW_HEADER_AUTH` equivalents** — give the service its own env var names (not `CHAIN_BRIDGE_*`), and log a loud `⚠️` warning whenever either is active.

Things that are chain-bridge-specific and should *not* be copied into sibling services:

- Vault Transit wiring (only chain-bridge signs; other services should call chain-bridge over gRPC to submit transactions).
- The `platform_admin` key_id branch in `sign_and_submit` — this authority lives in exactly one service by design.
- The specific RBAC allowlists — each service has its own callers.

---

## Things that are easy to break

A non-exhaustive list of places where a well-meaning change will cause subtle breakage:

- **Removing the `role.require_any(...)` line from a handler "because it's read-only."** Even reads leak information; the api-gateway should not be reading arbitrary account data without mediation.
- **Changing the SPIFFE header name (`z-gridtokenx-spiffe-id`).** `extract_role` and `PeerCertLayer` must stay in sync. If you rename it, grep both files.
- **Adding a `.expect()` or `.unwrap()` inside a handler.** Handlers must return `ConnectError`, never panic. Panics kill the Tokio task and can leave connections half-open.
- **Calling `bincode::deserialize` without length bounds.** A malicious or buggy client can send a huge payload and OOM you. If this becomes a real concern, add a size check before `deserialize`.
- **Forgetting to `ack` a NATS message.** It'll redeliver forever and you'll see the same `correlation_id` pile up in the idempotency cache.
- **Setting `CHAIN_BRIDGE_INSECURE=true` in a non-local `.env` file.** This has happened. Make it a checklist item in deployment review.

---

## High-throughput guide (designing for 10k+ TPS)

The chain-bridge as originally written handles maybe 100–500 TPS on a good day. Getting to 10k+ TPS is not a tuning exercise — it forces structural choices. This section names the walls, in the order you'll hit them, and the options for getting past each one.

### Honest baselines first

Before optimising anything, know the per-hop latency budget. At 10k TPS a single blocking call of 10ms on a 200-worker Tokio runtime saturates the pool. Rough numbers to carry in your head:

| Hop | Realistic latency | Hard ceiling of a single pipeline |
|---|---|---|
| Vault Transit `sign` (local network) | 5–20 ms | ~50–200 signs/sec per pipeline |
| Vault Transit `sign` (across AZ) | 15–50 ms | ~20–65 signs/sec |
| Solana `sendTransaction` (RPC) | 20–100 ms | ~10–50 txs/sec per client |
| Solana `getLatestBlockhash` | 10–40 ms | — (but every `sign_and_submit` does one) |
| mTLS handshake (cold) | 5–15 ms | — (dominant if not pooled) |
| NATS JetStream publish + ack | <1 ms | not the bottleneck |

**Implication:** the signing path is the ceiling. Everything else is either cheap (NATS) or parallelisable (Solana RPC via client pool). Plan your architecture around that fact.

### Wall 1 — Vault Transit is the signing ceiling

**The problem:** `sign_and_submit` does a serial `get_latest_blockhash → vault.sign_message → send_transaction`. Vault signing alone caps a single pipeline at ~100 TPS. Adding workers helps, but Vault itself becomes the bottleneck around 500–2000 signs/sec depending on its deployment.

**The real question to ask:** *does this transaction actually need to be signed by `platform_admin`?* At 10k TPS, the answer is almost always no. User-initiated trades should be signed by the user's own keypair on the frontend, and chain-bridge just relays them (the `key_id.is_empty()` branch in `sign_and_submit` already handles this). Reserve `platform_admin` for genuinely admin operations: REC minting, epoch settlements, oracle updates, NFT issuance. Those are low-TPS by nature.

**If you really do need high-TPS admin signing**, options in order of preference:

1. **Partition authority.** Mint N sibling keys (`settlement_signer_0`, `settlement_signer_1`, ...) each with scoped permission on-chain, and shard traffic across them. 16 parallel signing pipelines × 100 TPS each = 1600 TPS of admin signing, still fully Vault-backed.
2. **Pipeline Vault calls with bounded concurrency.** Use `futures::stream::buffer_unordered(N)` around Vault calls with N tuned to Vault's capacity. Measure Vault's saturation point — once request latency starts climbing, back off.
3. **Co-locate Vault with chain-bridge** in the same AZ / pod — cuts signing latency by 3–5x.
4. **Batch blockhash refreshes.** Currently every `sign_and_submit` calls `get_latest_blockhash`. Cache the latest blockhash and refresh it every 2–3 seconds in a background task; hand it to signing calls from the cache. Solana blockhashes are valid ~60 seconds, so a 2–3s cache costs nothing.
5. **Move to an HSM + local ed25519 signing** as a last resort. Loses the "keys never leave Vault" invariant and re-introduces key material into the bridge's trust boundary — do not do this lightly, and document the trade-off.

**Avoid:** building a local hot-path keypair "just for throughput." That breaks invariant #1 (keys never leave Vault). If throughput forces this choice, you've exhausted the other options and need a security review, not a code change.

### Wall 2 — `SolanaProvider` is synchronous inside an async runtime

**The problem:** `solana_client::rpc_client::RpcClient` is a blocking client. Every call (`send_transaction`, `get_balance`, `simulate_transaction`) blocks the Tokio worker thread it's running on. At 10k TPS this starves the runtime — handlers that should be cheap start queuing behind blocked workers.

**Fixes:**

1. **Switch to `solana_client::nonblocking::rpc_client::RpcClient`.** The async variant exists and has an identical surface. Update the `SolanaProvider` trait methods from sync `fn` to `async fn`. This alone is often a 3–5x throughput improvement because it stops starving Tokio.
2. **Pool clients.** A single `RpcClient` has one underlying HTTP connection. Behind the trait, hold a `Vec<Arc<RpcClient>>` and round-robin across them. Typical useful pool size: 8–32 clients.
3. **Consider multiple RPC endpoints.** Route reads (`get_balance`, `get_account_data`) to read replicas, writes (`send_transaction`) to a primary. Jito/Helius/Triton-style RPC providers support this natively.
4. **For submission specifically, use `sendTransactionWithConfig { skip_preflight: true }`** on paths where you've already simulated. Preflight adds a full round-trip. Every TPS saved here compounds.
5. **TPU-direct submission** (via `solana_tpu_client`) bypasses RPC and forwards to the current leader directly. This is what high-throughput Solana DEXs (Jupiter aggregator, Phoenix) use. Much more operationally complex — only reach for this above ~5k TPS.

When you change `SolanaProvider` to async, remember: `MockSolanaProvider` in tests also needs to become async. `#[async_trait]` is already in use so the trait signature change is mechanical.

### Wall 3 — NATS consumer is single-threaded

**The problem:** `NatsConsumer::start` has one loop that pulls a message, awaits `handle_submit`, then pulls the next. If `handle_submit` takes 30ms (realistic with Vault + Solana), that's ~33 msgs/sec from one consumer. The `durable_name = "chain-bridge-worker"` ensures JetStream redelivers to *this* consumer name — multiple processes with the same durable name will share the work (pull queue group semantics).

**Fixes:**

1. **Horizontal scale first.** Run N replicas of chain-bridge with the same `durable_name`. JetStream distributes messages across them via the pull consumer group. This is the simplest scaling lever and often the only one needed.
2. **Concurrent per-replica processing.** Inside `start()`, replace the serial `while let Some(result) = messages.next().await { handle(msg).await }` with a bounded worker pool:
   ```rust
   use futures::stream::StreamExt;
   messages
       .for_each_concurrent(32, |result| async {
           if let Ok(msg) = result { self.handle_submit(msg).await }
       })
       .await;
   ```
   Bound the concurrency (don't use unlimited `buffer_unordered`) so you have a predictable ceiling on in-flight Vault calls. 16–64 is a reasonable range.
3. **Tune the pull batch size.** JetStream pull consumers fetch N messages per request; the default is often too conservative. Set `max_batch` on the pull config explicitly (128–512 is a good range for high throughput).
4. **Separate streams per workload class.** `chain.tx.submit` (slow, signs) and `chain.tx.simulate` (faster, read-only) have different latency profiles and saturate different resources. Put them on separate streams with separate consumers so a flood of simulate traffic doesn't starve real submissions, or vice versa.

**Watch out for:** per-message `Retry::spawn(retry_strategy, ...)` inside `handle_submit`. With FixedInterval(500ms) × 3, a single flaky Solana call holds that worker for 1.5s. At 10k TPS that's devastating. Either (a) cut retries and push retry responsibility to the caller, or (b) make retry a background task that acks the original message and re-publishes the failed one on a separate `chain.tx.retry` subject.

### Wall 4 — Idempotency cache contention

**The problem:** `DashMap<String, u64>` is concurrent, but the `cleanup_cache` method does a full `retain` scan on *every message*. At 10k TPS with 600k entries, that's a full-map scan 10k times per second. It will show up in flamegraphs as the hottest line in the service.

**Fixes:**

1. **Move cleanup to a background task.** Spawn once at startup, `tokio::time::interval` every 5 seconds, single scan. Remove the per-message call.
2. **Use TTL-native storage.** `moka::future::Cache` with `time_to_live(Duration::from_secs(60))` does expiry for free and is sharded internally. Drop-in replacement for the `DashMap` for this use case and significantly faster under load.
3. **Persist the cache** if replicas need to share idempotency state. A shared Redis with `SET NX EX 60` is a simple cross-replica dedup primitive — but it adds a round-trip per message, so only do it if replicas can genuinely see duplicate correlation IDs (which depends on your NATS partitioning scheme).

### Wall 5 — mTLS handshake amortisation

**The problem:** A full TLS handshake is 5–15ms. If each gRPC call opens a new connection, you're spending more time on TLS than on work.

**Fixes (mostly on the client side, but the server config has to cooperate):**

1. **HTTP/2 connection reuse.** connectrpc runs over HTTP/2 by default, which already multiplexes. The server side is fine. Clients must reuse a single `Channel` / `Client` per process — *not* construct one per request. This is the single most common client bug.
2. **Tune `max_concurrent_streams`** on the rustls server config upward from the default 100 to something like 1000 if clients are multiplexing heavily. In rustls this is on the HTTP/2 layer, not the TLS layer directly — you set it via axum / hyper server builder options.
3. **Session resumption / 0-RTT** cuts handshake cost on reconnects. rustls supports session tickets; make sure the client side caches and reuses them.
4. **Cap client count.** 10k TPS from 10 clients at 1k TPS each is much friendlier than 10k clients at 1 TPS each. Push clients to coalesce — e.g. the trading-matcher batches orders before calling the bridge.

### Wall 6 — Observability becomes mandatory

At 10k TPS you cannot debug by reading logs. You need:

1. **Per-stage latency histograms** — handshake, auth, Vault sign, Solana RPC, total. Prometheus histograms, not counters. `BlockchainMetrics::track_operation` is the hook; make sure every critical path calls it with an accurate stage label.
2. **In-flight gauges** — number of open gRPC streams, number of in-flight Vault calls, NATS consumer lag. These are leading indicators; latency is lagging.
3. **Saturation signals** — Tokio runtime worker count vs. blocked-on-IO count (via `tokio-metrics`). When these converge you're about to tip over.
4. **Per-caller breakdown** — tag metrics with the SPIFFE service name. "TPS is up" means nothing; "trading-matcher is doing 8k TPS and oracle-bridge is doing 2k TPS" is actionable.

### Load-test plan before you ship any of this

Do not guess. Before claiming any target, run:

1. **Isolate each hop.** Benchmark Vault signing alone (loop calling `vault.sign_message`), Solana RPC alone (loop calling `send_transaction` with a pre-signed tx), NATS alone (publish + ack). Know each ceiling independently.
2. **Integration test at ramp.** Ramp from 100 → 500 → 1k → 2k TPS with 10-minute holds. Watch for latency cliffs — throughput numbers hide them, p99 latency surfaces them.
3. **Fault injection.** Kill Vault for 30 seconds mid-load. Observe: does the consumer DLQ fill? Does the idempotency cache leak? Does the retry loop pathologically hold workers?
4. **Publish the result next to the target.** "Chain-bridge sustains 3200 TPS at p99 < 250ms with 8 replicas, Vault co-located" is useful. "Chain-bridge can do 10k TPS" without conditions is marketing, not engineering.

### What NOT to do at high TPS

- **Do not** disable RBAC "for performance." The check is a few microseconds. Removing it buys nothing and opens a hole.
- **Do not** drop staleness checks. A 55s-old tx that lands on-chain is a bug; avoiding it is cheaper than recovering from it.
- **Do not** remove the mTLS layer even in trusted networks. Replacing mTLS with a network-perimeter-only model makes every future breach a lateral-movement disaster.
- **Do not** hold Vault credentials in a shared global without connection pooling. One `reqwest::Client` per Vault call will open a new TCP+TLS connection each time and throttle yourself.
- **Do not** cache signed transactions. Blockhashes expire; a "cached" signed tx from 70s ago is invalid on-chain and wastes an RPC round-trip to discover that.

### Progression path — what to do in what order

If you're currently handling ~200 TPS and targeting 10k, do these in order. Stop as soon as you hit your target — you probably will well before 10k.

1. Switch `SolanaProvider` to async (`solana_client::nonblocking`). Expect 3–5x.
2. Cache `get_latest_blockhash` in a 2-second background refresh. Expect 1.5–2x.
3. Parallelise NATS consumer with `for_each_concurrent(32)`. Expect 10–30x on the NATS path.
4. Swap `DashMap` idempotency for `moka` with TTL. Expect latency stability, not raw TPS.
5. Horizontally scale: 4–8 chain-bridge replicas behind the NATS pull group. Expect near-linear.
6. Partition admin signing authority (multiple Vault keys). Expect linear with key count, up to Vault's ceiling.
7. Move to TPU-direct submission. Expect 2–3x on the submission path, lots of operational pain.
8. Co-locate Vault. Expect a latency cut that lets all previous changes breathe.

Steps 1–5 typically get you to 2–5k TPS. 6–8 get you past 10k. Past step 5, budget real engineering time — weeks, not days.

---

## AI Agent Memory System Patterns

We have introduced a multi-layered AI Agent memory framework ([agent_memory.rs](crates/chain-bridge-api/src/agent_memory.rs)) designed to provide structured cognitive layers for agents processing transactions or executing tasks within the GridTokenX bridge ecosystem.

### 1. Core Invariants & Rules
*   **Thread Safety First:** All layers (Short-Term, Long-Term, Episodic) must be wrapped in thread-safe locks (`Arc<RwLock<T>>`) to support concurrent read/write access from multi-threaded Tokio runtime tasks.
*   **Bounded Context Window:** Short-term memory must enforce a strict capacity ceiling (`max_messages`). Do not let active dialog buffers grow indefinitely. Trigger `consolidate()` when limits are met to digest and move older turns into semantic long-term memory.
*   **Decoupled Vector Search:** Never couple the long-term memory layer to a specific third-party provider or cloud database. Always interact through the `EmbeddingProvider` trait.
*   **Offline Testability:** All unit/integration tests must use `LocalEmbeddingProvider` (deterministic hash-based vector mapping) to ensure builds are fast, offline-capable, and do not fail due to missing API keys.

### 2. Design Patterns for Memory Integration
*   **Auto-Consolidation:** Always specify a retention window when flushing. For example, if max capacity is 10, keep the 2 most recent messages when consolidating, converting the remaining 8 into a semantic summary.
*   **Recency/Access Weighting:** Long-term vector searches query entries semantically using cosine similarity. Always trigger `touch_entry` upon retrieval to log statistics (`access_count` and `last_accessed_at_ms`).
*   **Structured Trace Logging:** Use the `EpisodicMemory` layer to record precise tool names, arguments, results, and outcome success/rewards for learning or auditing agent actions.

---

Update this file when:

- A new invariant emerges (e.g. a new env var that must never be set in prod).
- The SPIFFE URI scheme changes.
- A new transport is added (e.g. WebSocket, HTTP/JSON for a public endpoint).
- The Vault Transit key rotation story is formalised.
- A new `ServiceRole` variant is added.
- The throughput target changes, or a new wall is discovered during load testing (update the "Honest baselines" table with measured numbers, replacing the rough estimates).
- The AI Agent memory system invariants or structure changes.

Keep the chain-bridge's actual source as the source of truth for the *code*; this skill is the source of truth for the *intent* and the *gotchas*.
