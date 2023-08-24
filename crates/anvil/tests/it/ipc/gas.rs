//! Gas related tests

use anvil::{eth::fees::INITIAL_BASE_FEE, spawn, NodeConfig};
use ethers::{
    prelude::Middleware,
    types::{
        transaction::eip2718::TypedTransaction, Address, BlockNumber, Eip1559TransactionRequest,
        TransactionRequest,
    },
};
use serial_test::serial;
const GAS_TRANSFER: u64 = 21_000u64;

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_basefee_full_block() {
    let (_api, handle) = spawn(
        NodeConfig::test_ipc()
            .with_base_fee(Some(INITIAL_BASE_FEE))
            .with_gas_limit(Some(GAS_TRANSFER))
    )
    .await;
    let provider = handle.http_provider();
    let tx = TransactionRequest::new().to(Address::random()).value(1337u64);
    provider.send_transaction(tx.clone(), None).await.unwrap().await.unwrap().unwrap();
    let base_fee =
        provider.get_block(BlockNumber::Latest).await.unwrap().unwrap().base_fee_per_gas.unwrap();
    let tx = TransactionRequest::new().to(Address::random()).value(1337u64);
    provider.send_transaction(tx.clone(), None).await.unwrap().await.unwrap().unwrap();
    let next_base_fee =
        provider.get_block(BlockNumber::Latest).await.unwrap().unwrap().base_fee_per_gas.unwrap();

    assert!(next_base_fee > base_fee);
    // max increase, full block
    assert_eq!(next_base_fee.as_u64(), INITIAL_BASE_FEE + 125_000_000);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_basefee_half_block() {
    let block_num = BlockNumber::Latest.as_number().unwrap()-5;
    let (_api, handle) = spawn(
        NodeConfig::test_ipc()
            .with_base_fee(Some(INITIAL_BASE_FEE))
            .with_gas_limit(Some(GAS_TRANSFER * 2))
            .with_fork_block_number(Some(block_num.as_u64())),
    )
    .await;
    let provider = handle.http_provider();
    let next_base_fee =
    provider.get_block(block_num).await.unwrap().unwrap().base_fee_per_gas.unwrap();
    println!("{:?}", next_base_fee);
    let tx = TransactionRequest::new().to(Address::random()).value(1337u64);
    println!("{:?}", tx);
    provider.send_transaction(tx.clone(), None).await.unwrap().await.unwrap().unwrap();
    let tx = TransactionRequest::new().to(Address::random()).value(1337u64);
    println!("{:?}", tx);
    provider.send_transaction(tx.clone(), None).await.unwrap().await.unwrap().unwrap();
    let next_base_fee =
        provider.get_block(block_num).await.unwrap().unwrap().base_fee_per_gas.unwrap();
    println!("{:?}", next_base_fee);


    // unchanged, half block
    assert_eq!(next_base_fee.as_u64(), INITIAL_BASE_FEE);
}
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_basefee_empty_block() {
    let (api, handle) = spawn(NodeConfig::test_ipc().with_base_fee(Some(INITIAL_BASE_FEE))).await;

    let provider = handle.http_provider();
    let tx = TransactionRequest::new().to(Address::random()).value(1337u64);
    provider.send_transaction(tx, None).await.unwrap().await.unwrap().unwrap();
    let base_fee =
        provider.get_block(BlockNumber::Latest).await.unwrap().unwrap().base_fee_per_gas.unwrap();

    // mine empty block
    api.mine_one().await;

    let next_base_fee =
        provider.get_block(BlockNumber::Latest).await.unwrap().unwrap().base_fee_per_gas.unwrap();

    // empty block, decreased base fee
    assert!(next_base_fee < base_fee);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_respect_base_fee() {
    let base_fee = 50u64;
    let (_api, handle) = spawn(NodeConfig::test_ipc().with_base_fee(Some(base_fee))).await;
    let provider = handle.http_provider();
    let mut tx = TypedTransaction::default();
    tx.set_value(100u64);
    tx.set_to(Address::random());

    let mut underpriced = tx.clone();
    underpriced.set_gas_price(base_fee - 1);
    let res = provider.send_transaction(underpriced, None).await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("max fee per gas less than block base fee"));

    tx.set_gas_price(base_fee);
    let tx = provider.send_transaction(tx, None).await.unwrap().await.unwrap().unwrap();
    assert_eq!(tx.status, Some(1u64.into()));
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_tip_above_fee_cap() {
    let base_fee = 50u64;
    let (_api, handle) = spawn(NodeConfig::test_ipc().with_base_fee(Some(base_fee))).await;
    let provider = handle.http_provider();
    let tx = TypedTransaction::Eip1559(
        Eip1559TransactionRequest::new()
            .max_fee_per_gas(base_fee)
            .max_priority_fee_per_gas(base_fee + 1)
            .to(Address::random())
            .value(100u64),
    );
    let res = provider.send_transaction(tx, None).await;
    assert!(res.is_err());
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("max priority fee per gas higher than max fee per gas"));
}
