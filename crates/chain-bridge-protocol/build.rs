//! Codegen for the `gridtokenx.chain.v1` ChainBridgeService.
//!
//! Relocated verbatim from the root crate's `build.rs`. The proto lives in the
//! sibling `gridtokenx-blockchain-core` submodule; the path is relative to this
//! crate's manifest dir (`crates/chain-bridge-protocol/`).
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "../../../gridtokenx-blockchain-core/proto/chain_bridge.proto";
    let include = "../../../gridtokenx-blockchain-core/proto";

    println!("cargo:rerun-if-changed={proto}");

    // Standard Tonic outputs (kept for inter-service gRPC if needed; not
    // currently `include!`d, so no tonic/prost runtime dep is required).
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(&[proto], &[include])?;

    // ConnectRPC outputs — the surface actually used by the service.
    connectrpc_build::Config::new()
        .files(&[proto])
        .includes(&[include])
        .include_file("_chain_bridge_include.rs")
        .compile()?;

    Ok(())
}
