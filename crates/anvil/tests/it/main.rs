mod abi;
mod ganache;
mod geth;
mod http;
mod ipc;
mod middleware;
pub mod utils;

#[allow(unused)]
pub(crate) fn init_tracing() {
    let _ = tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

fn main() {}
