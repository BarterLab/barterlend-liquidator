//! CreditPool v5 liquidator with local integration tests.
//! Network identities and transaction permissions are explicitly configured.

pub mod liquidation_math;
mod abi_validation;
pub mod planner;
pub mod routes;
pub mod economics;
pub mod executor_abi;
pub mod simulation;
pub mod tx;
pub mod local_config;
pub mod operator_key;
pub mod scheduler;
pub mod audit;
pub mod app;
pub mod diagnostics;
mod diagnostics_io;
pub mod recovery;
pub mod sync_policy;
mod pool_sync;


pub mod server;
pub mod configuration;

/// Automatic pool discovery from reviewed lending factories.
pub mod catalog;

pub mod selection;
