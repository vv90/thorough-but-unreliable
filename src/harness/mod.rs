//! Deterministic conversation loop and separate effectful adapters.

pub mod async_driver;
pub mod command;
pub mod driver;
pub mod inference;
pub mod model_loop;
pub mod presentation;
pub mod report;
pub mod runner;
pub mod types;

#[cfg(test)]
mod tests;
