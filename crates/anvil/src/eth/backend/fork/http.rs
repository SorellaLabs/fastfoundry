//! Support for forking off another client generic over HTTP, IPC or ethers-reth middleware

use crate::{
    eth::{
        backend::{fork::ClientForkTrait, mem::fork_db::ForkedDatabase},
        error::BlockchainError,
    },
    NodeConfig,
};
use anvil_core::eth::{proof::AccountProof, transaction::EthTransactionRequest};

use async_trait::async_trait;
use ethers::{
    prelude::BlockNumber,
    providers::{Middleware, ProviderError},
    types::{
        transaction::eip2930::AccessListWithGasUsed, Address, Block, BlockId, Bytes, FeeHistory,
        Filter, GethDebugTracingOptions, GethTrace, Log, Trace, Transaction, TransactionReceipt,
        TxHash, H256, U256,
    },
};

use foundry_common::{ProviderBuilder, RetryProvider};
use foundry_evm::utils::u256_to_h256_be;
use parking_lot::{
    lock_api::{RwLockReadGuard, RwLockWriteGuard},
    RawRwLock, RwLock,
};
use std::{fmt::Debug, sync::Arc, time::Duration};
use tokio::{runtime::Handle, sync::RwLock as AsyncRwLock};
use tracing::trace;

use super::{ConfigAsProvider, ForkedStorage};

pub struct ClientForkHttp {
    /// Contains the cached data
    pub storage: Arc<RwLock<ForkedStorage>>,
    /// contains the info how the fork is configured
    // Wrapping this in a lock, ensures we can update this on the fly via additional custom RPC
    // endpoints
    pub config: Arc<RwLock<ClientForkConfigHttp>>,
    /// This also holds a handle to the underlying database
    pub database: Arc<AsyncRwLock<ForkedDatabase>>,
}

impl ClientForkHttp {
    pub fn new_http(
        config: ClientForkConfigHttp,
        database: Arc<AsyncRwLock<ForkedDatabase>>,
    ) -> ClientForkHttp {
        Self { storage: Default::default(), config: Arc::new(RwLock::new(config)), database }
    }

    fn provider(&self) -> Arc<RetryProvider> {
        self.config.read().provider.clone()
    }
}

#[async_trait]
impl ClientForkTrait for ClientForkHttp {
    /// Reset the fork to a fresh forked state, and optionally update the fork config1
    async fn reset(
        &self,
        path: Option<String>,
        block_number: BlockId,
    ) -> Result<(), BlockchainError> {
        {
            let mut db_write = self.database.write().await;
            db_write.reset(block_number).map_err(BlockchainError::Internal)?;
        }

        if let Some(path) = path {
            {
                let mut config_write = self.config.write();
                config_write.update_url(path)?;
            }

            let override_chain_id;
            {
                let config_read = self.config.read();
                override_chain_id = config_read.override_chain_id;
            }
            let chain_id = if let Some(chain_id) = override_chain_id {
                chain_id.into()
            } else {
                self.provider().get_chainid().await?
            };
            {
                let mut config_write = self.config.write();
                config_write.chain_id = chain_id.as_u64();
            }
        }

        let provider = self.provider();
        let block =
            provider.get_block(block_number).await?.ok_or(BlockchainError::BlockNotFound)?;
        let block_hash = block.hash.ok_or(BlockchainError::BlockNotFound)?;
        let timestamp = block.timestamp.as_u64();
        let base_fee = block.base_fee_per_gas;
        let total_difficulty = block.total_difficulty.unwrap_or_default();

        {
            let mut config_write = self.config.write();
            config_write.update_block(
                block.number.ok_or(BlockchainError::BlockNotFound)?.as_u64(),
                block_hash,
                timestamp,
                base_fee,
                total_difficulty,
            );
        }

        self.clear_cached_storage();
        Ok(())
    }

    async fn update_url(&self, url: &str) -> Result<(), anyhow::Error> {
        self.config.write().update_url(url.to_string())?;
        Ok(())
    }

    fn database(&self) -> Arc<AsyncRwLock<ForkedDatabase>> {
        self.database.clone()
    }

    /// Removes all data cached from previous responses
    fn clear_cached_storage(&self) {
        self.storage.write().clear()
    }

    /// Returns true whether the block predates the fork
    fn predates_fork(&self, block: u64) -> bool {
        block < self.block_number()
    }

    /// Returns true whether the block predates the fork _or_ is the same block as the fork
    fn predates_fork_inclusive(&self, block: u64) -> bool {
        block <= self.block_number()
    }

    fn backoff(&self) -> Option<Duration> {
        self.config.read().backoff
    }

    fn timestamp(&self) -> u64 {
        self.config.read().timestamp
    }

    fn block_number(&self) -> u64 {
        self.config.read().block_number
    }

    fn total_difficulty(&self) -> U256 {
        self.config.read().total_difficulty
    }

    fn provider_path(&self) -> Option<String> {
        self.config.read().eth_rpc_url.clone()
    }

    fn base_fee(&self) -> Option<U256> {
        self.config.read().base_fee
    }

    fn block_hash(&self) -> H256 {
        self.config.read().block_hash
    }

    fn chain_id(&self) -> u64 {
        self.config.read().chain_id
    }

    fn storage_read(&self) -> RwLockReadGuard<'_, RawRwLock, ForkedStorage> {
        self.storage.read()
    }

    fn storage_write(&self) -> RwLockWriteGuard<'_, RawRwLock, ForkedStorage> {
        self.storage.write()
    }
    /// Returns the fee history  `eth_feeHistory`
    async fn fee_history(
        &self,
        block_count: U256,
        newest_block: BlockNumber,
        reward_percentiles: &[f64],
    ) -> Result<FeeHistory, ProviderError> {
        self.provider().clone().fee_history(block_count, newest_block, reward_percentiles).await
    }

    /// Sends `eth_getProof`
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<H256>,
        block_number: Option<BlockId>,
    ) -> Result<AccountProof, ProviderError> {
        self.provider().clone().get_proof(address, keys, block_number).await
    }

    /// Sends `eth_call`
    async fn call(
        &self,
        request: &EthTransactionRequest,
        block: Option<BlockNumber>,
    ) -> Result<Bytes, ProviderError> {
        let request = Arc::new(request.clone());
        let block = block.unwrap_or(BlockNumber::Latest);

        if let BlockNumber::Number(num) = block {
            // check if this request was already been sent
            let key = (request.clone(), num.as_u64());
            if let Some(res) = self.storage_read().eth_call.get(&key).cloned() {
                return Ok(res)
            }
        }

        let tx = ethers::utils::serialize(request.as_ref());
        let block_value = ethers::utils::serialize(&block);
        let res: Bytes = self.provider().request("eth_call", [tx, block_value]).await?;

        if let BlockNumber::Number(num) = block {
            // cache result
            let mut storage = self.storage_write();
            storage.eth_call.insert((request, num.as_u64()), res.clone());
        }
        Ok(res)
    }

    /// Sends `eth_call`
    async fn estimate_gas(
        &self,
        request: &EthTransactionRequest,
        block: Option<BlockNumber>,
    ) -> Result<U256, ProviderError> {
        let request = Arc::new(request.clone());
        let block = block.unwrap_or(BlockNumber::Latest);

        if let BlockNumber::Number(num) = block {
            // check if this request was already been sent
            let key = (request.clone(), num.as_u64());
            if let Some(res) = self.storage_read().eth_gas_estimations.get(&key).cloned() {
                return Ok(res)
            }
        }
        let tx = ethers::utils::serialize(request.as_ref());
        let block_value = ethers::utils::serialize(&block);
        let res = self.provider().request("eth_estimateGas", [tx, block_value]).await?;

        if let BlockNumber::Number(num) = block {
            // cache result
            let mut storage = self.storage_write();
            storage.eth_gas_estimations.insert((request, num.as_u64()), res);
        }

        Ok(res)
    }

    /// Sends `eth_createAccessList`
    async fn create_access_list(
        &self,
        request: &EthTransactionRequest,
        block: Option<BlockNumber>,
    ) -> Result<AccessListWithGasUsed, ProviderError> {
        let tx = ethers::utils::serialize(request);
        let block = ethers::utils::serialize(&block.unwrap_or(BlockNumber::Latest));
        self.provider().request("eth_createAccessList", [tx, block]).await
    }

    async fn storage_at(
        &self,
        address: Address,
        index: U256,
        number: Option<BlockNumber>,
    ) -> Result<H256, ProviderError> {
        let index = u256_to_h256_be(index);
        self.provider().clone().get_storage_at(address, index, number.map(Into::into)).await
    }

    async fn logs(&self, filter: &Filter) -> Result<Vec<Log>, ProviderError> {
        if let Some(logs) = self.storage_read().logs.get(filter).cloned() {
            return Ok(logs)
        }

        let logs = self.provider().get_logs(filter).await?;

        let mut storage = self.storage_write();
        storage.logs.insert(filter.clone(), logs.clone());
        Ok(logs)
    }

    async fn get_code(&self, address: Address, blocknumber: u64) -> Result<Bytes, ProviderError> {
        trace!(target: "backend::fork", "get_code={:?}", address);
        if let Some(code) = self.storage_read().code_at.get(&(address, blocknumber)).cloned() {
            return Ok(code)
        }

        let code = self.provider().get_code(address, Some(blocknumber.into())).await?;
        let mut storage = self.storage_write();
        storage.code_at.insert((address, blocknumber), code.clone());

        Ok(code)
    }

    async fn get_balance(&self, address: Address, blocknumber: u64) -> Result<U256, ProviderError> {
        trace!(target: "backend::fork", "get_balance={:?}", address);
        self.provider().get_balance(address, Some(blocknumber.into())).await
    }

    async fn get_nonce(&self, address: Address, blocknumber: u64) -> Result<U256, ProviderError> {
        trace!(target: "backend::fork", "get_nonce={:?}", address);
        self.provider().get_transaction_count(address, Some(blocknumber.into())).await
    }

    async fn transaction_by_block_number_and_index(
        &self,
        number: u64,
        index: usize,
    ) -> Result<Option<Transaction>, ProviderError> {
        if let Some(block) = self.block_by_number(number).await? {
            if let Some(tx_hash) = block.transactions.get(index) {
                return self.transaction_by_hash(*tx_hash).await
            }
        }
        Ok(None)
    }

    async fn transaction_by_block_hash_and_index(
        &self,
        hash: H256,
        index: usize,
    ) -> Result<Option<Transaction>, ProviderError> {
        if let Some(block) = self.block_by_hash(hash).await? {
            if let Some(tx_hash) = block.transactions.get(index) {
                return self.transaction_by_hash(*tx_hash).await
            }
        }
        Ok(None)
    }

    async fn transaction_by_hash(&self, hash: H256) -> Result<Option<Transaction>, ProviderError> {
        trace!(target: "backend::fork", "transaction_by_hash={:?}", hash);
        if let tx @ Some(_) = self.storage_read().transactions.get(&hash).cloned() {
            return Ok(tx)
        }

        if let Some(tx) = self.provider().get_transaction(hash).await? {
            let mut storage = self.storage_write();
            storage.transactions.insert(hash, tx.clone());
            return Ok(Some(tx))
        }
        Ok(None)
    }

    async fn trace_transaction(&self, hash: H256) -> Result<Vec<Trace>, ProviderError> {
        if let Some(traces) = self.storage_read().transaction_traces.get(&hash).cloned() {
            return Ok(traces)
        }

        let traces = self.provider().trace_transaction(hash).await?;
        let mut storage = self.storage_write();
        storage.transaction_traces.insert(hash, traces.clone());

        Ok(traces)
    }

    async fn debug_trace_transaction(
        &self,
        hash: H256,
        opts: GethDebugTracingOptions,
    ) -> Result<GethTrace, ProviderError> {
        if let Some(traces) = self.storage_read().geth_transaction_traces.get(&hash).cloned() {
            return Ok(traces)
        }

        let trace = self.provider().debug_trace_transaction(hash, opts).await?;
        let mut storage = self.storage_write();
        storage.geth_transaction_traces.insert(hash, trace.clone());

        Ok(trace)
    }

    async fn trace_block(&self, number: u64) -> Result<Vec<Trace>, ProviderError> {
        if let Some(traces) = self.storage_read().block_traces.get(&number).cloned() {
            return Ok(traces)
        }

        let traces = self.provider().trace_block(number.into()).await?;
        let mut storage = self.storage_write();
        storage.block_traces.insert(number, traces.clone());

        Ok(traces)
    }

    async fn transaction_receipt(
        &self,
        hash: H256,
    ) -> Result<Option<TransactionReceipt>, ProviderError> {
        if let Some(receipt) = self.storage_read().transaction_receipts.get(&hash).cloned() {
            return Ok(Some(receipt))
        }

        if let Some(receipt) = self.provider().get_transaction_receipt(hash).await? {
            let mut storage = self.storage_write();
            storage.transaction_receipts.insert(hash, receipt.clone());
            return Ok(Some(receipt))
        }

        Ok(None)
    }

    async fn block_by_hash(&self, hash: H256) -> Result<Option<Block<TxHash>>, ProviderError> {
        if let Some(block) = self.storage_read().blocks.get(&hash).cloned() {
            return Ok(Some(block))
        }
        let block = self.fetch_full_block(BlockId::Hash(hash)).await?.map(Into::into);
        Ok(block)
    }

    async fn block_by_hash_full(
        &self,
        hash: H256,
    ) -> Result<Option<Block<Transaction>>, ProviderError> {
        if let Some(block) = self.storage_read().blocks.get(&hash).cloned() {
            return Ok(Some(self.convert_to_full_block(block)))
        }
        self.fetch_full_block(BlockId::Hash(hash)).await
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
    ) -> Result<Option<Block<Transaction>>, ProviderError> {
        if let Some(block) = self.provider().get_block_with_txs(block_id).await? {
            let hash = block.hash.unwrap();
            let block_number = block.number.unwrap().as_u64();
            let mut storage = self.storage_write();
            // also insert all transactions
            storage.transactions.extend(block.transactions.iter().map(|tx| (tx.hash, tx.clone())));
            storage.hashes.insert(block_number, hash);
            storage.blocks.insert(hash, block.clone().into());
            return Ok(Some(block))
        }

        Ok(None)
    }

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
    ) -> Result<Option<Block<TxHash>>, ProviderError> {
        let block_hash = block
            .hash
            .ok_or_else(|| ProviderError::CustomError("missing block-hash".to_string()))?;
        if let Some(uncles) = self.storage_read().uncles.get(&block_hash) {
            return Ok(uncles.get(index).cloned())
        }

        let mut uncles = Vec::with_capacity(block.uncles.len());
        for (uncle_idx, _) in block.uncles.iter().enumerate() {
            let uncle = match self.provider().get_uncle(block_hash, uncle_idx.into()).await? {
                Some(u) => u,
                None => return Ok(None),
            };
            uncles.push(uncle);
        }
        self.storage_write().uncles.insert(block_hash, uncles.clone());
        Ok(uncles.get(index).cloned())
    }

    /// Converts a block of hashes into a full block
    fn convert_to_full_block(&self, block: Block<TxHash>) -> Block<Transaction> {
        let storage = self.storage.read();
        let mut transactions = Vec::with_capacity(block.transactions.len());
        for tx in block.transactions.iter() {
            if let Some(tx) = storage.transactions.get(tx).cloned() {
                transactions.push(tx);
            }
        }
        block.into_full_block(transactions)
    }
}

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
    fn update_url(&mut self, url_or_path: String) -> Result<(), BlockchainError> {
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

        trace!(target: "backend", "Updated fork rpc from \"{}\" to \"{}\"", self.eth_rpc_url.clone().unwrap(), url_or_path);
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

#[async_trait::async_trait]
impl ConfigAsProvider for ClientForkConfigHttp {
    type ProviderKind = RetryProvider;
    type ForkType = ClientForkHttp;

    async fn into_provider(node_config: &NodeConfig, _handle: Option<Handle>) -> RetryProvider {
        ProviderBuilder::new(node_config.eth_rpc_url.clone().unwrap())
            .timeout(node_config.fork_request_timeout)
            .timeout_retry(node_config.fork_request_retries)
            .initial_backoff(node_config.fork_retry_backoff.as_millis() as u64)
            .compute_units_per_second(node_config.compute_units_per_second)
            .max_retry(10)
            .initial_backoff(1000)
            .build()
            .expect("Failed to establish provider to fork RPC url")
    }

    fn into_fork_config(
        node_config: &NodeConfig,
        block: Block<TxHash>,
        chain_id: u64,
        provider: Arc<RetryProvider>,
    ) -> Self {
        ClientForkConfigHttp {
            eth_rpc_url: Some(node_config.eth_rpc_url.clone().unwrap().to_string()),
            block_number: block.number.unwrap().as_u64(),
            block_hash: block.hash.unwrap_or_default(),
            provider,
            chain_id,
            override_chain_id: node_config.chain_id,
            timestamp: block.timestamp.as_u64(),
            base_fee: block.base_fee_per_gas,
            timeout: Some(node_config.fork_request_timeout),
            retries: Some(node_config.fork_request_retries),
            backoff: Some(node_config.fork_retry_backoff),
            compute_units_per_second: Some(node_config.compute_units_per_second),
            total_difficulty: block.total_difficulty.unwrap_or_default(),
        }
    }

    fn into_client_fork(self, backend_db: Arc<AsyncRwLock<ForkedDatabase>>) -> ClientForkHttp {
        ClientForkHttp::new_http(self.clone(), Arc::clone(&backend_db))
    }
}
