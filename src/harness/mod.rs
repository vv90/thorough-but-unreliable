//! Deterministic conversation loop and separate effectful adapters.

pub mod async_driver;
pub mod driver;
pub mod inference;
pub mod model_loop;
pub mod types;

#[cfg(test)]
mod tests;
