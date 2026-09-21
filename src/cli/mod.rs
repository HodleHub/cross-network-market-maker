mod commands;
mod live;
pub mod solver;

use clap::Parser;

use crate::config::RuntimeConfig;
use crate::core::error::CoreResult;

pub use commands::Cli;

pub fn run_cli() -> CoreResult<()> {
    RuntimeConfig::from_env()?;

    let cli = Cli::parse();

    commands::run(cli)
}
