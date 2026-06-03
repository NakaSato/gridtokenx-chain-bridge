# Chain Bridge — Test Checklist

> Comprehensive test inventory and coverage tracker.
> Last updated: 2026-06-03

---

## Legend

| Status | Meaning |
|--------|---------|
| ✅ | Implemented and passing |
| ❌ | Not implemented |
| ⚠️ | Partially implemented (see notes) |

---

## 1. Unit Tests — `src/api.rs`

### 1.1 BlockhashCache

| # | Test | Status | Notes |
|---|------|--------|-------|
| 1.1.1 | `test_blockhash_cache` — update then get returns cached value | ✅ | `api.rs:1464` |
| 1.1.2 | Cache returns `None` before first update | ✅ | `api.rs:1464` |
| 1.1.3 | Concurrent update + get does not panic | ✅ | `api.rs:1493` |
| 1.1.4 | Update overwrites previous value | ✅ | `api.rs:1480` |

### 1.2 gRPC RPC Handlers (mocked provider)

| # | Test | Status | Notes |
|---|------|--------|-------|
| 1.2.1 | `test_get_balance` — returns 1 SOL | ✅ | `api.rs:1156` |
| 1.2.2 | `test_simulate_transaction` — returns CU + logs | ✅ | `api.rs:1168` |
| 1.2.3 | `test_submit_transaction_with_platform_admin` — signs + submits | ✅ | `api.rs:1187` |
| 1.2.4 | `test_submit_transaction_unauthorized` — unknown SPIFFE → denied | ✅ | `api.rs:1211` |
| 1.2.5 | `test_get_account_data_exists` — returns account data | ✅ | `api.rs:1223` |
| 1.2.6 | `test_get_epoch_info` — returns epoch + slot | ✅ | `api.rs:1235` |
| 1.2.7 | `get_latest_blockhash` — returns cached blockhash | ✅ | `api.rs:1379` |
| 1.2.8 | `get_recent_prioritization_fees` — returns fee list | ✅ | `api.rs:1389` |
| 1.2.9 | `get_token_account_balance` — returns amount/decimals | ✅ | `api.rs:1400` |
| 1.2.10 | `get_signature_status` — returns confirmed status | ✅ | `api.rs:1411` |
| 1.2.11 | `get_slot` — returns current slot | ✅ | `api.rs:1422` |
| 1.2.12 | `request_airdrop` — Admin/IamService only | ✅ | `api.rs:1432` and `api.rs:1444` |
| 1.2.13 | `get_transaction_details` — returns found=false on missing | ✅ | `api.rs:1459` |

### 1.3 `extract_role()` — Identity Resolution

| # | Test | Status | Notes |
|---|------|--------|-------|
| 1.3.1 | SPIFFE header → correct ServiceRole | ✅ | `api.rs:1473` |
| 1.3.2 | `CHAIN_BRIDGE_INSECURE=true` → Admin | ✅ | `api.rs:1614` |
| 1.3.3 | `CHAIN_BRIDGE_ALLOW_HEADER_AUTH` → header-based role | ✅ | `api.rs:1622` |
| 1.3.4 | Unknown SPIFFE → `ServiceRole::Unknown` | ✅ | `api.rs:1211` |
| 1.3.5 | ApiGateway identity | ✅ | `api.rs:1501` |

### 1.4 `sign_and_submit()` — Core Signing Pipeline

| # | Test | Status | Notes |
|---|------|--------|-------|
| 1.4.1 | Happy path — platform_admin, empty blockhash, Vault signs | ✅ | `api.rs:1187` |
| 1.4.2 | Non-empty blockhash preserved (not overwritten) | ✅ | `api.rs:1266` |
| 1.4.3 | Policy Engine rejects unauthorized program | ✅ | `api.rs:1312` |
| 1.4.4 | Policy Engine allows authorized program | ✅ | `tests/invariants.rs` |
| 1.4.5 | Invalid `serialized_tx` → deserialization error | ✅ | `api.rs:1287` |
| 1.4.6 | Unauthorized `key_id` (not "platform_admin") → error | ✅ | `api.rs:1298` |
| 1.4.7 | Blockhash cache empty → slow-path RPC fallback | ✅ | `api.rs:1515` |
| 1.4.8 | Multi-signature transaction — signature at correct slot | ✅ | `api.rs:1534` |

---

## 2. Unit Tests — `src/vault.rs`

| # | Test | Status | Notes |
|---|------|--------|-------|
| 2.1 | `InsecureKeypairProvider::get_public_key` — returns consistent pubkey | ✅ | `vault.rs:207` |
| 2.2 | `InsecureKeypairProvider::sign_message` — returns valid signature | ✅ | `vault.rs:215` |
| 2.3 | `InsecureKeypairProvider` — sign then verify roundtrip | ✅ | `vault.rs:215` |
| 2.4 | `VaultTransitClient::get_public_key` — cache hit skips HTTP | ✅ | `vault.rs:278` |
| 2.5 | `VaultTransitClient::get_public_key` — cache miss fetches from Vault | ✅ | `vault.rs:288` |
| 2.6 | `VaultTransitClient::sign_message` — parses `vault:v1:` format | ✅ | `vault.rs:241` |
| 2.7 | `VaultTransitClient::sign_message` — malformed signature → error | ✅ | `vault.rs:262` |

---

## 3. Unit Tests — `src/nats_consumer.rs`

### 3.1 `handle_submit`

| # | Test | Status | Notes |
|---|------|--------|-------|
| 3.1.1 | Happy path — valid tx → success + correlation_id cached | ✅ | `nats_consumer.rs:600` |
| 3.1.2 | Invalid payload → `Term` ack | ✅ | `nats_consumer.rs:549` |
| 3.1.3 | Unknown SPIFFE identity → `Term` ack | ✅ | `nats_consumer.rs:439` |
| 3.1.5 | Staleness guard (55s) | ✅ | `nats_consumer.rs:500` |

### 3.2 `handle_simulate`

| # | Test | Status | Notes |
|---|------|--------|-------|
| 3.2.1 | Happy path — returns CU + logs | ✅ | `nats_consumer.rs:564` |

### 3.3 `handle_cancel`

| # | Test | Status | Notes |
|---|------|--------|-------|
| 3.3.1 | Happy path — marks correlation_id in cache | ✅ | `nats_consumer.rs:528` |

### 3.4 Idempotency Cache

| # | Test | Status | Notes |
|---|------|--------|-------|
| 3.4.1 | Cleanup task removes expired entries | ✅ | `nats_consumer.rs:418` |
| 3.4.2 | Cleanup task preserves valid entries | ✅ | `nats_consumer.rs:418` |

---

## 4. Unit Tests — `src/middleware.rs`

| # | Test | Status | Notes |
|---|------|--------|-------|
| 4.1 | SPIFFE URI extracted from valid X.509 cert | ✅ | `middleware.rs:143` |
| 4.2 | No peer certs → no SPIFFE identity | ✅ | `middleware.rs:226` |
| 4.3 | Cert without SAN → no SPIFFE identity | ✅ | `middleware.rs:152` |
| 4.5 | `z-gridtokenx-spiffe-id` header set correctly | ✅ | `middleware.rs:176` |
| 4.6 | `SpiffeIdentity` extension set correctly | ✅ | `middleware.rs:176` |

---

## 6. Invariant Tests — `tests/invariants.rs`

| # | Test | Status | Notes |
|---|------|--------|-------|
| 6.1 | Trading service can submit trading tx | ✅ | `invariants.rs:104` |
| 6.2 | Trading service cannot submit oracle tx | ✅ | `invariants.rs:123` |
| 6.3 | Oracle service can submit oracle tx | ✅ | `invariants.rs:147` |
| 6.4 | IAM service can submit registry tx | ✅ | `invariants.rs:185` |
| 6.5 | IAM service cannot submit trading tx | ✅ | `invariants.rs:204` |
| 6.6 | Unknown identity → rejected entirely | ✅ | `invariants.rs:223` |
| 6.7 | Admin identity can submit any program tx | ✅ | `invariants.rs:241` |
| 6.8 | System program always allowed regardless of service | ✅ | `invariants.rs:259` |
| 6.9 | Multi-instruction tx — one unauthorized → whole tx rejected | ✅ | `invariants.rs:277` |
| 6.10 | Trading service can call Energy Token program | ✅ | `invariants.rs:305` |

---

## Summary

### Current State

| Category | Implemented | Missing | Coverage |
|----------|------------|---------|----------|
| **api.rs unit tests** | 33 | 1 | 97% |
| **vault.rs unit tests** | 7 | 0 | 100% |
| **nats_consumer.rs unit tests** | 12 | 1 | 92% |
| **middleware.rs unit tests** | 5 | 0 | 100% |
| **invariants.rs** | 10 | 0 | 100% |
| **TOTAL** | **67** | **2** | **97%** |
