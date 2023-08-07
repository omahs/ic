pub mod address;
pub mod endpoints;
pub mod eth_logs;
pub mod eth_rpc;
pub mod management;
pub mod numeric;
mod serde_data;
pub mod state;
pub mod transactions;
pub mod tx;

#[cfg(test)]
mod tests;

pub const MAIN_DERIVATION_PATH: Vec<Vec<u8>> = vec![];
