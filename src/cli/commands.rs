use std::path::PathBuf;
use std::str::FromStr;

use bitcoin::Network as BitcoinNetwork;
use bitcoin::hashes::Hash;
use clap::{Args, Parser, Subcommand};
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};

use super::solver::{
    ReserveRequest, RfqRequest, SolverConfig, SolverService, generate_key_file, read_key_file,
    request_quote, reserve_remote,
};
use crate::core::amount::CanonicalAmount;
use crate::core::error::{CoreError, CoreResult};
use crate::core::rfq::{Intent, PinnedSolverKeys, now_unix_seconds, verify_quote};
use crate::core::routes::RouteRegistry;
use crate::core::storage::SqliteStore;

#[derive(Clone, Debug, Parser)]
#[command(
    name = "xmm",
    version,
    about = "Regtest cross-network market-maker research POC"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    Solver {
        #[command(subcommand)]
        command: SolverCommand,
    },
    Rfq(RfqArgs),
    Routes,
    Swap {
        #[command(subcommand)]
        command: Box<SwapCommand>,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum SolverCommand {
    Keygen(KeygenArgs),
    Serve(Box<ServeArgs>),
}

#[derive(Clone, Debug, Args)]
pub struct KeygenArgs {
    #[arg(long)]
    pub out: PathBuf,
}

#[derive(Clone, Debug, Args)]
pub struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1:37771")]
    pub bind: String,
    #[arg(long)]
    pub solver_id: String,
    #[arg(long)]
    pub key_id: String,
    #[arg(long)]
    pub key_file: PathBuf,
    #[arg(long)]
    pub database: PathBuf,
    #[arg(long)]
    pub asset_hash: String,
    #[arg(long, default_value = "TEST-DEPIX")]
    pub inventory_asset_id: String,
    #[arg(long, default_value = "1000000")]
    pub inventory_amount: String,
    #[arg(long, default_value = "1000")]
    pub fee_lbtc: String,
    #[arg(long, default_value = "1")]
    pub rate_numerator: u64,
    #[arg(long, default_value = "1")]
    pub rate_denominator: u64,
}

#[derive(Clone, Debug, Args)]
pub struct RfqArgs {
    #[arg(long, value_delimiter = ',')]
    pub peer: Vec<String>,
    #[arg(long)]
    pub intent: PathBuf,
    #[arg(long, value_delimiter = ',')]
    pub pinned_key: Vec<String>,
    #[arg(long)]
    pub asset_hash: String,
    #[arg(long)]
    pub reserve: bool,
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum SwapCommand {
    Prepare(PrepareArgs),
    Forward(SwapArgs),
    Resume(SwapArgs),
    Status(StatusArgs),
}

#[derive(Clone, Debug, Args)]
pub struct PrepareArgs {
    #[arg(long)]
    pub session: String,
    #[arg(long)]
    pub intent_out: PathBuf,
    #[arg(long)]
    pub database: PathBuf,
    #[arg(long)]
    pub recovery_key: PathBuf,
    #[arg(long)]
    pub asset_hash: String,
    #[arg(long)]
    pub amount_in: String,
    #[arg(long)]
    pub amount_out: String,
    #[arg(long)]
    pub fee_limit_lbtc: String,
    #[arg(long)]
    pub fee_sats: u64,
    #[arg(long, default_value = "1000")]
    pub source_refund_delta: u64,
    #[arg(long, default_value = "80")]
    pub lightning_max_cltv: u64,
    #[arg(long, default_value = "7200")]
    pub margin_seconds: u64,
    #[arg(long)]
    pub refund_destination: Option<String>,
    #[arg(long)]
    pub receiver_url: String,
    #[arg(long)]
    pub receiver_cert: PathBuf,
    #[arg(long)]
    pub receiver_macaroon: PathBuf,
    #[arg(long, default_value = "40")]
    pub hold_cltv: u32,
    #[arg(long, default_value = "http://127.0.0.1:27051")]
    pub elements_rpc: String,
    #[arg(long, default_value = "cross_network_market_maker")]
    pub elements_user: String,
    #[arg(
        long,
        default_value = "cross_network_market_maker_elements_rpc_password"
    )]
    pub elements_password: String,
}

#[derive(Clone, Debug, Args)]
pub struct SwapArgs {
    #[arg(long)]
    pub intent: PathBuf,
    #[arg(long)]
    pub quote: PathBuf,
    #[arg(long)]
    pub database: PathBuf,
    #[arg(long)]
    pub recovery_key: PathBuf,
    #[arg(long, value_delimiter = ',')]
    pub pinned_key: Vec<String>,
    #[arg(long)]
    pub asset_hash: String,
    #[arg(long)]
    pub solver_database: Option<PathBuf>,
    #[arg(long, default_value = "http://127.0.0.1:27051")]
    pub elements_rpc: String,
    #[arg(long, default_value = "cross_network_market_maker")]
    pub elements_user: String,
    #[arg(
        long,
        default_value = "cross_network_market_maker_elements_rpc_password"
    )]
    pub elements_password: String,
    #[arg(long, default_value = "cross_network_market_maker")]
    pub elements_wallet: String,
    #[arg(long)]
    pub receiver_url: String,
    #[arg(long)]
    pub receiver_cert: PathBuf,
    #[arg(long)]
    pub receiver_macaroon: PathBuf,
    #[arg(long)]
    pub payer_url: String,
    #[arg(long)]
    pub payer_cert: PathBuf,
    #[arg(long)]
    pub payer_macaroon: PathBuf,
    #[arg(long, default_value = "7200")]
    pub margin_seconds: u64,
    #[arg(long, default_value = "80")]
    pub lightning_cltv_limit: u32,
    #[arg(long, default_value = "40")]
    pub hold_cltv: u32,
    #[arg(long, default_value = "20")]
    pub fee_limit_sats: u64,
    #[arg(long, default_value = "20")]
    pub fee_sats: u64,
    #[arg(long)]
    pub stop_after_payment: bool,
}

#[derive(Clone, Debug, Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub database: PathBuf,
    #[arg(long)]
    pub swap_id: String,
}

pub fn run(cli: Cli) -> CoreResult<()> {
    match cli.command {
        Command::Solver { command } => run_solver(command),
        Command::Rfq(args) => run_rfq(args),
        Command::Routes => run_routes(),
        Command::Swap { command } => run_swap(*command),
    }
}

fn run_solver(command: SolverCommand) -> CoreResult<()> {
    match command {
        SolverCommand::Keygen(args) => {
            let public_key = generate_key_file(&args.out)?;
            print_json(&PublicKeyOutput {
                public_key,
                key_file: args.out.display().to_string(),
            })
        }
        SolverCommand::Serve(args) => {
            let secret_key = read_key_file(&args.key_file)?;
            let fee_lbtc = CanonicalAmount::parse(&args.fee_lbtc)?;
            let inventory_amount = CanonicalAmount::parse(&args.inventory_amount)?;
            let service = SolverService::new(SolverConfig {
                solver_id: args.solver_id.clone(),
                key_id: args.key_id.clone(),
                secret_key_hex: secret_key,
                asset_hash: args.asset_hash.clone(),
                inventory_asset_id: args.inventory_asset_id.clone(),
                fee_lbtc,
                rate_numerator: args.rate_numerator,
                rate_denominator: args.rate_denominator,
                database: args.database.clone(),
                inventory_amount,
            })?;
            eprintln!("solver listening on {}", args.bind);
            super::solver::serve(&args.bind, service)
        }
    }
}

fn run_rfq(args: RfqArgs) -> CoreResult<()> {
    if args.peer.is_empty() {
        return Err(CoreError::Transport(
            "at least one solver peer is required".to_owned(),
        ));
    }

    let intent: Intent = read_json(&args.intent)?;
    let registry = RouteRegistry::qualified(&args.asset_hash)?;
    let pinned_keys = parse_pinned_keys(&args.pinned_key)?;
    let responses = args
        .peer
        .iter()
        .map(|peer| {
            request_quote(
                peer,
                &RfqRequest {
                    intent: intent.clone(),
                },
                10,
            )
            .map(|response| (peer.clone(), response))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let now = now_unix_seconds()?;
    let mut verified = responses
        .into_iter()
        .map(|(peer, response)| {
            verify_quote(
                &intent,
                &response.signed_quote,
                &pinned_keys,
                &registry,
                &intent.deadline_context,
                now,
            )?;

            validate_quote_payment_request(&intent, &response.signed_quote)?;

            Ok((peer, response))
        })
        .collect::<CoreResult<Vec<_>>>()?;
    verified.sort_by_key(|response| {
        (
            response
                .1
                .signed_quote
                .quote
                .fee_lbtc
                .value()
                .unwrap_or(u64::MAX),
            u64::MAX.saturating_sub(
                response
                    .1
                    .signed_quote
                    .quote
                    .amount_out
                    .value()
                    .unwrap_or_default(),
            ),
        )
    });
    let selected = verified
        .first()
        .ok_or_else(|| CoreError::QuoteRejected("no solver returned a quote".to_owned()))?;
    let reservation_id = format!("reservation-{}", selected.1.signed_quote.quote.quote_id);

    if args.reserve {
        reserve_remote(
            &selected.0,
            &ReserveRequest {
                reservation_id: reservation_id.clone(),
                signed_quote: selected.1.signed_quote.clone(),
                intent: intent.clone(),
            },
            10,
        )?;
    }

    let output = SelectedQuoteOutput {
        selected_peer: selected.0.clone(),
        solver_id: selected.1.solver_id.clone(),
        quote_id: selected.1.signed_quote.quote.quote_id.clone(),
        reservation_id,
        amount_in: selected.1.signed_quote.quote.amount_in.to_string(),
        amount_out: selected.1.signed_quote.quote.amount_out.to_string(),
        fee_lbtc: selected.1.signed_quote.quote.fee_lbtc.to_string(),
        expires_at: selected.1.signed_quote.quote.expires_at,
        signed_quote: selected.1.signed_quote.clone(),
    };

    if let Some(path) = args.out {
        write_json(&path, &output)?;
    }

    print_json(&output)
}

pub(super) fn validate_quote_payment_request(
    intent: &Intent,
    signed_quote: &crate::core::rfq::SignedQuote,
) -> CoreResult<()> {
    let payment_request = signed_quote.quote.payment_request.as_deref();

    if intent.destination.network != crate::core::network::Network::LightningRegtest {
        if payment_request.is_some() {
            return Err(CoreError::QuoteRejected(
                "non-Lightning quote unexpectedly carries a payment request".to_owned(),
            ));
        }

        return Ok(());
    }

    let payment_request = payment_request.ok_or_else(|| {
        CoreError::QuoteRejected("Lightning quote is missing its payment request".to_owned())
    })?;

    if payment_request != intent.destination_destination {
        return Err(CoreError::QuoteRejected(
            "quote payment request differs from the intent invoice".to_owned(),
        ));
    }

    let invoice = Bolt11Invoice::from_str(payment_request)
        .map_err(|error| CoreError::QuoteRejected(format!("invalid BOLT11 invoice: {error}")))?;

    if invoice.network() != BitcoinNetwork::Regtest {
        return Err(CoreError::QuoteRejected(
            "quote invoice is not for regtest".to_owned(),
        ));
    }

    let amount_msat = invoice
        .amount_milli_satoshis()
        .ok_or_else(|| CoreError::QuoteRejected("invoice amount is missing".to_owned()))?;
    let amount_sats = signed_quote.quote.amount_out.value()?;

    let expected_msat = amount_sats.checked_mul(1000).ok_or_else(|| {
        CoreError::QuoteRejected("quote amount overflows millisatoshis".to_owned())
    })?;

    if amount_msat != expected_msat {
        return Err(CoreError::QuoteRejected(
            "quote amount_out does not equal the invoice amount".to_owned(),
        ));
    }

    if invoice.payment_hash().to_byte_array() != intent.hash_commitment.bytes() {
        return Err(CoreError::QuoteRejected(
            "quote invoice hash differs from H".to_owned(),
        ));
    }

    Ok(())
}

fn run_routes() -> CoreResult<()> {
    let registry = RouteRegistry::regtest()?;
    let routes = registry
        .all()
        .iter()
        .map(|route| RouteOutput {
            source: route.source.canonical_id(),
            destination: route.destination.canonical_id(),
            adapter_id: route.adapter_id.clone(),
            status: if route.enabled {
                "ENABLED"
            } else {
                "UNSUPPORTED"
            }
            .to_owned(),
        })
        .collect::<Vec<_>>();

    print_json(&routes)
}

fn run_swap(command: SwapCommand) -> CoreResult<()> {
    match command {
        SwapCommand::Prepare(args) => super::live::run_prepare(args),
        SwapCommand::Forward(args) => run_swap_forward(args),
        SwapCommand::Resume(args) => run_swap_resume(args),
        SwapCommand::Status(args) => run_swap_status(args),
    }
}

fn run_swap_forward(args: SwapArgs) -> CoreResult<()> {
    super::live::run_forward(args, false)
}

fn run_swap_resume(args: SwapArgs) -> CoreResult<()> {
    super::live::run_forward(args, true)
}

fn run_swap_status(args: StatusArgs) -> CoreResult<()> {
    let store = SqliteStore::open(&args.database)?;
    let record = store
        .get_swap(&args.swap_id)?
        .ok_or_else(|| CoreError::Storage("swap session does not exist".to_owned()))?;

    print_json(&record)
}

pub(super) fn parse_pinned_keys(values: &[String]) -> CoreResult<PinnedSolverKeys> {
    let mut keys = PinnedSolverKeys::default();

    for value in values {
        let (key_id, public_key) = value
            .split_once('=')
            .ok_or_else(|| CoreError::Signature("pinned key must be key-id=hex".to_owned()))?;
        keys.insert(key_id, public_key);
    }

    Ok(keys)
}

pub(super) fn read_json<T: for<'de> serde::Deserialize<'de>>(path: &PathBuf) -> CoreResult<T> {
    let bytes = std::fs::read(path).map_err(|error| CoreError::Storage(error.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|error| CoreError::Serialization(error.to_string()))
}

pub(super) fn print_json<T: Serialize>(value: &T) -> CoreResult<()> {
    let output = serde_json::to_string_pretty(value)
        .map_err(|error| CoreError::Serialization(error.to_string()))?;
    println!("{output}");

    Ok(())
}

#[derive(Clone, Debug, Serialize)]
struct PublicKeyOutput {
    public_key: String,
    key_file: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct SelectedQuoteOutput {
    pub(super) selected_peer: String,
    pub(super) solver_id: String,
    pub(super) quote_id: String,
    pub(super) reservation_id: String,
    pub(super) amount_in: String,
    pub(super) amount_out: String,
    pub(super) fee_lbtc: String,
    pub(super) expires_at: u64,
    pub(super) signed_quote: crate::core::rfq::SignedQuote,
}

#[derive(Clone, Debug, Serialize)]
struct RouteOutput {
    source: String,
    destination: String,
    adapter_id: String,
    status: String,
}

pub(super) fn write_json<T: Serialize>(path: &PathBuf, value: &T) -> CoreResult<()> {
    let data = serde_json::to_vec_pretty(value)
        .map_err(|error| CoreError::Serialization(error.to_string()))?;
    std::fs::write(path, data).map_err(|error| CoreError::Storage(error.to_string()))
}
