mod abi;
mod ganache;
mod geth;
mod anvil;
mod anvil_api;
mod api;
pub mod fork;
pub mod fork_ipc;
pub mod fork_middleware;
mod gas;
mod genesis;
mod ipc;
mod logs;
pub mod proof;
mod pubsub;
// mod revert; // TODO uncomment <https://github.com/gakonst/ethers-rs/issues/2186>
mod otterscan;
mod sign;
mod traces;
mod transaction;
mod txpool;
mod wsapi;
pub mod utils;

#[allow(unused)]
pub(crate) fn init_tracing() {
    let _ = tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

fn main() {}
