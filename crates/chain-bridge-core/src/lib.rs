//! `chain-bridge-core` — the hexagonal center of Chain Bridge.
//!
//! Holds the **ports** (traits the outside world implements) and the **domain
//! types** they exchange. Per the platform "Sync Core, Async Edges" rule, the
//! pure logic here is synchronous (policy evaluation, audit hash-chaining);
//! only the ports are `async` because they front network/HSM/DB edges.
//!
//! Dependency direction: persistence and logic depend on this crate; this crate
//! depends on nothing GridTokenX-specific. It must never import HTTP, NATS,
//! Vault, SQLx, or RPC client types.

pub mod audit;
pub mod error;
pub mod nonce;
pub mod ports;

pub use error::ChainBridgeError;
pub use ports::{
    AuditPort, ChainClientPort, NoncePort, PreSignSimulatorPort, SignerPort, SimulationOutcome,
    SubmitOutcome,
};
