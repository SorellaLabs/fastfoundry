mod abi;
mod ganache;
mod geth;
pub mod utils;
mod http;
mod ipc;
mod middleware;

#[allow(unused)]
pub(crate) fn init_tracing() {
    let _ = tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

fn main() {}
