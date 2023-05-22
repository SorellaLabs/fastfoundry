//! Support for forking off another client generic over HTTP, IPC or ethers-reth middleware

use crate::eth::{backend::mem::fork_db::ForkedDatabase, error::BlockchainError};
use anvil_core::eth::{proof::AccountProof, transaction::EthTransactionRequest};
use anvil_rpc::error::RpcError;
use ethers::{
    prelude::BlockNumber,
    types::{
        transaction::eip2930::AccessListWithGasUsed, Address, Block, BlockId, Bytes, FeeHistory,
        Filter, GethDebugTracingOptions, GethTrace, Log, Trace, Transaction, TransactionReceipt,
        TxHash, H256, U256,
    },
};
use ethers_providers::MiddlewareError;
use foundry_common::{ProviderBuilder, RetryProvider};
use foundry_evm::utils::u256_to_h256_be;
use parking_lot::{
    lock_api::{RwLockReadGuard, RwLockWriteGuard},
    RawRwLock, RwLock,
};
use std::{collections::HashMap, fmt::Debug, sync::Arc, time::Duration};
use tokio::sync::RwLock as AsyncRwLock;
use tracing::trace;
use ethers::providers::{Ipc, Middleware, Provider, ProviderError};
use ethers_reth::RethMiddleware;
use async_trait::async_trait;
use std::path::Path;
use ethers_providers::JsonRpcClient;



pub struct ClientForkIpc {
    /// Contains the cached data
    pub storage: Arc<RwLock<ForkedStorage>>,
    /// contains the info how the fork is configured
    // Wrapping this in a lock, ensures we can update this on the fly via additional custom RPC
    // endpoints
    pub config: Arc<RwLock<ClientForkConfigIpc>>,
    /// This also holds a handle to the underlying database
    pub database: Arc<AsyncRwLock<ForkedDatabase>>,
}



#[derive(Debug, Clone)]
pub struct ClientForkConfigIpc {
    pub ipc_path: Option<String>,
    pub db_path: Option<String>,
    pub block_number: u64,
    pub block_hash: H256,
    pub provider: Arc<Provider<Ipc>>,
    pub chain_id: u64,
    pub override_chain_id: Option<u64>,
    /// The timestamp for the forked block
    pub timestamp: u64,
    /// The basefee of the forked block
    pub base_fee: Option<U256>,
    /// request timeout
    pub timeout: Option<Duration>,
    /// request retries for spurious networks
    pub retries: Option<u32>,
    /// request retries for spurious networks
    pub backoff: Option<Duration>,
    /// available CUPS
    pub compute_units_per_second: Option<u64>,
    /// total difficulty of the chain until this block
    pub total_difficulty: U256,
}




impl ClientForkConfigIpc {

    fn provider(&self) -> Arc<Provider<Ipc>> {
        self.provider.clone()
    }

    async fn update_url_or_path(&mut self, url_or_path: String) -> Result<(), BlockchainError> {
        self.provider = Arc::new(
            Provider::connect_ipc(&url_or_path)
                .await
                .map_err(|_| BlockchainError::InvalidUrl(url_or_path.clone()))?,
        );
        trace!(target: "fork", "Updated IPC path  {}", url_or_path);
        self.ipc_path = Some(url_or_path);
        Ok(())
    }

    fn update_block(
        &mut self,
        block_number: u64,
        block_hash: H256,
        timestamp: u64,
        base_fee: Option<U256>,
        total_difficulty: U256,
    ) {
        self.block_number = block_number;
        self.block_hash = block_hash;
        self.timestamp = timestamp;
        self.base_fee = base_fee;
        self.total_difficulty = total_difficulty;
        trace!(target: "fork", "Updated block number={} hash={:?}", block_number, block_hash);
    }
}
