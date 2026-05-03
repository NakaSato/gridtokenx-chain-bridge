# Test cases for `gridtokenx-chain-bridge-patterns` skill

Three layers of tests, each verifying something different.

## (1) `evals/triggering.json` — does the skill activate on the right prompts?

Fifteen prompts split between **positive** (should trigger the skill) and **negative** (should NOT trigger — catches overtriggering). These are the skill-creator format eval cases: prompts only, no code, graded by whether Claude consults the skill when running the prompt.

Run with the skill-creator's evaluation harness (or manually — load the skill, send each prompt, observe whether Claude's response reflects material from the SKILL.md).

The ten positive cases span:
- Core workflows (adding a gRPC handler, adding a NATS subject)
- Throughput questions (TPS targets, latency spikes)
- Debugging (PermissionDenied in staging, Vault signature length errors)
- Architecture (scaffolding a sibling service)
- Security judgement (refusing escape hatches)
- Scope boundaries (duplicate order fills — adjacent but not in scope)

The five negative cases test specifically against keyword-bait: "What's SPIFFE?" mentions a core term but is a generic question, not a GridTokenX one. A skill that fires on that is overtriggering.

## (2) `evals/behavioural.json` — does Claude follow the patterns when using the skill?

Eight prompts with objective assertions. These assume the skill has triggered, and check whether the *response* conforms to the patterns the SKILL.md prescribes.

Assertion types used:
- `exact-pattern` — the output must contain a specific textual or structural element (e.g. `extract_role` as the first line of handler work)
- `ordering` — sequence matters (e.g. deserialize → RBAC → idempotency → staleness, in that order)
- `negative` — the output must NOT contain something (e.g. must not produce a local keypair escape hatch)
- `judgement` — requires a grader to evaluate (clear, objective criteria, but not a pure string match)

The test on the review-catches-rbac-removal case is the most important one: it verifies the skill makes Claude push back on a plausible-sounding "just remove the RBAC check for speed" diff, which is the exact failure mode the invariants section is designed to prevent.

Run with the skill-creator's graded eval workflow (`scripts.aggregate_benchmark`), or manually by running each prompt and scoring the assertions yourself.

## (3) `tests/rust/invariants.rs` — does the chain-bridge itself enforce what the skill says?

A `#[tokio::test]` suite that exercises the invariants and throughput properties directly in Rust. Drop this into `crates/chain-bridge/tests/invariants.rs` and adjust the `use chain_bridge::*` imports to your crate name.

Five sections:

- **§1 Invariants** — the four core invariants from the SKILL.md, encoded as runnable tests. The most important one (`unknown_spiffe_service_is_denied`) verifies that the RBAC gate runs *before* the provider, by asserting the provider's call count stays at zero when the role is Unknown. If someone removes `role.require_any(...)`, this test fails loudly.

- **§2 RBAC** — service-by-service allowlist verification. api-gateway can read balances but cannot submit transactions. trading-matcher can do both. admin can do everything.

- **§3 NATS consumer** — four-check ordering tests (stale, duplicate, unknown identity, malformed). These are `#[ignore]`'d because they need a running NATS JetStream; run with a dev NATS server as documented in the file header.

- **§4 Throughput** — `concurrent_reads_should_not_serialise` is the key test here. It fires 16 concurrent reads through a 50ms-latency mock provider on a 4-worker Tokio runtime. With the current synchronous `SolanaProvider` trait it will sometimes fail under CI load (this is intentional — it documents the problem). After migrating to `solana_client::nonblocking`, tighten the threshold to <200ms to lock in the improvement.

- **§5 Vault integration** — `#[ignore]`'d, require a real dev-mode Vault. Verify pubkey caching works, 64-byte signature parsing works, and the "wrong key type" error mode from the skill produces a recognisable error string.

### How to run

```bash
# Default suite (no infra required)
cargo test --test invariants

# With NATS and Vault available
vault server -dev -dev-root-token-id=root &
vault secrets enable transit
vault write -f transit/keys/test-bridge-key type=ed25519
nats-server -js &

VAULT_ADDR=http://127.0.0.1:8200 VAULT_TOKEN=root \
  cargo test --test invariants -- --ignored --test-threads=1
```

### What to add to `Cargo.toml`

```toml
[dev-dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time", "test-util"] }
futures = "0.3"
# (most other deps are already in the main [dependencies])
```

## How these fit together

| If this breaks... | ...then |
|---|---|
| Triggering evals (1) fail | The SKILL.md description doesn't match real-world phrasing. Edit the description, not the skill body. |
| Behavioural evals (2) fail | The SKILL.md body doesn't communicate the pattern clearly enough — Claude reads it and still gets it wrong. Edit the body, usually by adding concrete examples or moving the pattern higher in the doc. |
| Rust tests (3) fail | The *code* drifted from what the skill documents. Either fix the code to match the skill, or update the skill if the drift was intentional and justified. |

All three together give you a feedback loop: prompts that reliably trigger, responses that reliably follow the patterns, code that reliably enforces them at runtime.
