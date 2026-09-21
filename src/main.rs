fn main() -> Result<(), Box<dyn std::error::Error>> {
    cross_network_market_maker::cli::run_cli().map_err(Into::into)
}
