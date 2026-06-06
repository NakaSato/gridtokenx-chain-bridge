//! Generated ConnectRPC bindings for `gridtokenx.chain.v1`.
//!
//! Re-exports the codegen so dependents write `chain_bridge_protocol::chain_v1::*`
//! instead of juggling `OUT_DIR`/`include!`. This mirrors the `api::chain_v1`
//! module the code was originally written against.

#[allow(clippy::module_inception)]
pub mod chain_v1 {
    include!(concat!(env!("OUT_DIR"), "/_chain_bridge_include.rs"));
    pub use gridtokenx::chain::v1::*;
}

pub use chain_v1::*;
