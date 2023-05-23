//! Support for forking off another client generic over HTTP, IPC or ethers-reth middleware

use crate::eth::{backend::mem::fork_db::ForkedDatabase, error::BlockchainError};
use anvil_core::eth::{proof::AccountProof, transaction::EthTransactionRequest};
use anvil_rpc::error::RpcError;
use async_trait::async_trait;
use ethers::{
    prelude::BlockNumber,
    providers::{Ipc, Middleware, Provider, ProviderError},
    types::{
        transaction::eip2930::AccessListWithGasUsed, Address, Block, BlockId, Bytes, FeeHistory,
        Filter, GethDebugTracingOptions, GethTrace, Log, Trace, Transaction, TransactionReceipt,
        TxHash, H256, U256,
    },
};
use ethers::providers::{JsonRpcClient, MiddlewareError};
use ethers_reth::RethMiddleware;
use foundry_common::{ProviderBuilder, RetryProvider};
use foundry_evm::utils::u256_to_h256_be;
use parking_lot::{
    lock_api::{RwLockReadGuard, RwLockWriteGuard},
    RawRwLock, RwLock,
};
use std::{collections::HashMap, fmt::Debug, path::Path, sync::Arc, time::Duration};
use tokio::sync::RwLock as AsyncRwLock;
use tracing::trace;

pub mod http;
pub mod ipc;
pub mod middleware;

use crate::eth::backend::fork::{http::ClientForkHttp, ipc::ClientForkIpc};
use http::ClientForkConfigHttp;
use ipc::ClientForkConfigIpc;
// use middleware::ClientForkConfigMiddleware;

/// Represents a fork of a remote client
///
/// This type contains a subset of the [`EthApi`](crate::eth::EthApi) functions but will exclusively
/// fetch the requested data from the remote client, if it wasn't already fetched.

#[async_trait]
pub trait ClientForkTrait: Sync + Send {

    /// Reset the fork to a fresh forked state, and optionally update the fork config
    async fn reset(
        &self,
        url_or_path: Option<String>,
        block_number: BlockId,
    ) -> Result<(), BlockchainError>;

    /// Removes all data cached from previous responses
    fn clear_cached_storage(&self);

    /// Returns true whether the block predates the fork
    fn predates_fork(&self, block: u64) -> bool;

    /// Returns true whether the block predates the fork _or_ is the same block as the fork
    fn predates_fork_inclusive(&self, block: u64) -> bool;

    fn backoff(&self) -> Option<Duration>;

    fn timestamp(&self) -> u64;

    fn block_number(&self) -> u64;

    fn total_difficulty(&self) -> U256;

    fn provider_path(&self) -> Option<String>;

    fn base_fee(&self) -> Option<U256>;

    fn block_hash(&self) -> H256;

    fn chain_id(&self) -> u64;

    fn storage_read(&self) -> RwLockReadGuard<'_, RawRwLock, ForkedStorage>;

    fn storage_write(&self) -> RwLockWriteGuard<'_, RawRwLock, ForkedStorage>;

    /// Returns the fee history  `eth_feeHistory`
    async fn fee_history(
        &self,
        block_count: U256,
        newest_block: BlockNumber,
        reward_percentiles: &[f64],
    ) -> Result<FeeHistory, ProviderError>;

    /// Sends `eth_getProof`
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<H256>,
        block_number: Option<BlockId>,
    ) -> Result<AccountProof, ProviderError>;

    /// Sends `eth_call`
    async fn call(
        &self,
        request: &EthTransactionRequest,
        block: Option<BlockNumber>,
    ) -> Result<Bytes, ProviderError>;

    /// Sends `eth_call`
    async fn estimate_gas(
        &self,
        request: &EthTransactionRequest,
        block: Option<BlockNumber>,
    ) -> Result<U256, ProviderError>;

    /// Sends `eth_createAccessList`
    async fn create_access_list(
        &self,
        request: &EthTransactionRequest,
        block: Option<BlockNumber>,
    ) -> Result<AccessListWithGasUsed, ProviderError>;

    async fn storage_at(
        &self,
        address: Address,
        index: U256,
        number: Option<BlockNumber>,
    ) -> Result<H256, ProviderError>;

    async fn logs(&self, filter: &Filter) -> Result<Vec<Log>, ProviderError>;

    async fn get_code(&self, address: Address, blocknumber: u64) -> Result<Bytes, ProviderError>;

    async fn get_balance(&self, address: Address, blocknumber: u64) -> Result<U256, ProviderError>;

    async fn get_nonce(&self, address: Address, blocknumber: u64) -> Result<U256, ProviderError>;

    async fn transaction_by_block_number_and_index(
        &self,
        number: u64,
        index: usize,
    ) -> Result<Option<Transaction>, ProviderError>;

    async fn transaction_by_block_hash_and_index(
        &self,
        hash: H256,
        index: usize,
    ) -> Result<Option<Transaction>, ProviderError>;

    async fn transaction_by_hash(&self, hash: H256) -> Result<Option<Transaction>, ProviderError>;

    async fn trace_transaction(&self, hash: H256) -> Result<Vec<Trace>, ProviderError>;

    async fn debug_trace_transaction(
        &self,
        hash: H256,
        opts: GethDebugTracingOptions,
    ) -> Result<GethTrace, ProviderError>;

    async fn trace_block(&self, number: u64) -> Result<Vec<Trace>, ProviderError>;

    async fn transaction_receipt(
        &self,
        hash: H256,
    ) -> Result<Option<TransactionReceipt>, ProviderError>;

    async fn block_by_hash(&self, hash: H256) -> Result<Option<Block<TxHash>>, ProviderError> {
        if let Some(block) = self.storage_read().blocks.get(&hash).cloned() {
            return Ok(Some(block))
        }
        let block = self.fetch_full_block(hash.into()).await?.map(Into::into);
        Ok(block)
    }

    async fn block_by_hash_full(
        &self,
        hash: H256,
    ) -> Result<Option<Block<Transaction>>, ProviderError> {
        if let Some(block) = self.storage_read().blocks.get(&hash).cloned() {
            return Ok(Some(self.convert_to_full_block(block)))
        }
        self.fetch_full_block(hash.into()).await
    }

    async fn block_by_number(
        &self,
        block_number: u64,
    ) -> Result<Option<Block<TxHash>>, ProviderError> {
        if let Some(block) = self
            .storage_read()
            .hashes
            .get(&block_number)
            .copied()
            .and_then(|hash| self.storage_read().blocks.get(&hash).cloned())
        {
            return Ok(Some(block))
        }

        let block = self.fetch_full_block(block_number.into()).await?.map(Into::into);
        Ok(block)
    }

    async fn block_by_number_full(
        &self,
        block_number: u64,
    ) -> Result<Option<Block<Transaction>>, ProviderError> {
        if let Some(block) = self
            .storage_read()
            .hashes
            .get(&block_number)
            .copied()
            .and_then(|hash| self.storage_read().blocks.get(&hash).cloned())
        {
            return Ok(Some(self.convert_to_full_block(block)))
        }

        self.fetch_full_block(block_number.into()).await
    }

    async fn fetch_full_block(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Block<Transaction>>, ProviderError>;

    async fn uncle_by_block_hash_and_index(
        &self,
        hash: H256,
        index: usize,
    ) -> Result<Option<Block<TxHash>>, ProviderError> {
        if let Some(block) = self.block_by_hash(hash).await? {
            return self.uncles_by_block_and_index(block, index).await
        }
        Ok(None)
    }

    async fn uncle_by_block_number_and_index(
        &self,
        number: u64,
        index: usize,
    ) -> Result<Option<Block<TxHash>>, ProviderError> {
        if let Some(block) = self.block_by_number(number).await? {
            return self.uncles_by_block_and_index(block, index).await
        }
        Ok(None)
    }

    async fn uncles_by_block_and_index(
        &self,
        block: Block<H256>,
        index: usize,
    ) -> Result<Option<Block<TxHash>>, ProviderError>;

    /// Converts a block of hashes into a full block
    fn convert_to_full_block(&self, block: Block<TxHash>) -> Block<Transaction>;
}

/// Contains cached state fetched to serve EthApi requests
#[derive(Debug, Clone, Default)]
pub struct ForkedStorage {
    pub uncles: HashMap<H256, Vec<Block<TxHash>>>,
    pub blocks: HashMap<H256, Block<TxHash>>,
    pub hashes: HashMap<u64, H256>,
    pub transactions: HashMap<H256, Transaction>,
    pub transaction_receipts: HashMap<H256, TransactionReceipt>,
    pub transaction_traces: HashMap<H256, Vec<Trace>>,
    pub logs: HashMap<Filter, Vec<Log>>,
    pub geth_transaction_traces: HashMap<H256, GethTrace>,
    pub block_traces: HashMap<u64, Vec<Trace>>,
    pub eth_gas_estimations: HashMap<(Arc<EthTransactionRequest>, u64), U256>,
    pub eth_call: HashMap<(Arc<EthTransactionRequest>, u64), Bytes>,
    pub code_at: HashMap<(Address, u64), Bytes>,
}

// === impl ForkedStorage ===

impl ForkedStorage {
    /// Clears all data
    pub fn clear(&mut self) {
        // simply replace with a completely new, empty instance
        *self = Self::default()
    }
}
