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


/// Contains all fork metadata
#[derive(Debug, Clone)]
pub struct ClientForkConfigHttp {
    pub eth_rpc_url: Option<String>,
    pub block_number: u64,
    pub block_hash: H256,
    pub provider: Arc<RetryProvider>,
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



impl ClientForkConfigHttp {
    // Can unwrap because it should always have default values
    async fn update_url_or_path(&mut self, url_or_path: String) -> Result<(), BlockchainError> {
        let interval = self.provider.get_interval();
        self.provider = Arc::new(
            ProviderBuilder::new(&url_or_path)
                .timeout(self.timeout.unwrap())
                .timeout_retry(self.retries.unwrap())
                .max_retry(10)
                .initial_backoff(self.backoff.unwrap().as_millis() as u64)
                .compute_units_per_second(self.compute_units_per_second.unwrap())
                .build()
                .map_err(|_| BlockchainError::InvalidUrl(url_or_path.clone()))?
                .interval(interval),
        );
        trace!(target: "fork", "Updated rpc url  {}", url_or_path);
        self.eth_rpc_url = Some(url_or_path);
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

