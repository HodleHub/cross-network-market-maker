#![deny(unsafe_code)]

pub mod chain;
pub mod cli;
pub mod config;
pub mod core;
pub mod exit;
pub mod lightning;
pub mod rpc;
pub mod swaps;

pub use core::error::{CoreError, CoreResult};
