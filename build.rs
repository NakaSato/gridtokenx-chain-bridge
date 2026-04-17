fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../gridtokenx-blockchain-core/proto/chain_bridge.proto");

    let protos = &["../gridtokenx-blockchain-core/proto/chain_bridge.proto"];
    let includes = &["../gridtokenx-blockchain-core/proto"];

    // Build standard Tonic outputs (for inter-service gRPC if needed)
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(protos, includes)?;

    // And ConnectRPC outputs
    connectrpc_build::Config::new()
        .files(protos)
        .includes(includes)
        .include_file("_chain_bridge_include.rs")
        .compile()?;

    Ok(())
}
