//! Chain Bridge gRPC API surface.
//!
//! Split from the former monolithic `api.rs`:
//! - [`provider`]  — `SolanaProvider` trait + `Real`/`Surfpool` impls + `BlockhashCache`
//! - [`service`]   — `ChainBridgeGrpcService` and its `ChainBridgeService` handlers
//! - `tests`       — unit tests (cfg(test))

// Generated code from proto
#[allow(clippy::module_inception)]
pub mod chain_v1 {
    include!(concat!(env!("OUT_DIR"), "/_chain_bridge_include.rs"));
    pub use gridtokenx::chain::v1::*;
}

mod provider;
mod service;

pub use provider::*;
pub use service::*;

// Shared imports re-exported for submodules, which pull them in via `use super::*;`.
// (This preserves the single-file import surface the code was written against.)
pub(crate) use connectrpc::{ConnectError, Context, ErrorCode};
pub(crate) use buffa::view::OwnedView;
pub(crate) use tracing::{error, info, warn};
pub(crate) use gridtokenx_blockchain_core::auth::ServiceRole;

// Test-only request constructors (handlers consume the `*View`/`*Response` types).
#[cfg(test)]
pub(crate) use chain_v1::{
    GetBalanceRequest, GetAccountDataRequest, SimulateTransactionRequest, SubmitTransactionRequest,
};

pub(crate) use chain_v1::{
    ChainBridgeService,
    GetBalanceRequestView, GetBalanceResponse,
    GetAccountDataRequestView, GetAccountDataResponse,
    GetLatestBlockhashRequestView, GetLatestBlockhashResponse,
    GetRecentPrioritizationFeesRequestView, GetRecentPrioritizationFeesResponse,
    GetTokenAccountBalanceRequestView, GetTokenAccountBalanceResponse,
    GetSignatureStatusRequestView, GetSignatureStatusResponse,
    GetSlotRequestView, GetSlotResponse,
    SimulateTransactionRequestView, SimulateTransactionResponse,
    SubmitTransactionRequestView, SubmitTransactionResponse,
    PrioritizationFee,
};

pub(crate) use solana_client::client_error::{ClientError, ClientErrorKind};
pub(crate) use solana_client::nonblocking::rpc_client::RpcClient;
pub(crate) use solana_client::rpc_response::{Response, RpcPrioritizationFee, RpcSimulateTransactionResult};
pub(crate) use solana_sdk::account::Account;
pub(crate) use solana_sdk::hash::Hash;
pub(crate) use solana_sdk::pubkey::Pubkey;
pub(crate) use solana_sdk::signature::Signature;
pub(crate) use solana_sdk::transaction::Transaction;
pub(crate) use std::str::FromStr;
pub(crate) use std::sync::Arc;
pub(crate) use async_trait::async_trait;
pub(crate) use tokio::sync::RwLock;
pub(crate) use crate::vault::VaultProvider;

#[cfg(test)]
mod tests;
