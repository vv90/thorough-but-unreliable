//! Deterministic conversation loop. Only `driver` invokes effectful dependencies.

pub mod driver;
pub mod model_loop;
pub mod types;

#[cfg(test)]
mod tests;
