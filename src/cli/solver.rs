use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use bitcoin::Network as BitcoinNetwork;
use bitcoin::hashes::Hash;
use lightning_invoice::Bolt11Invoice;
use rand::rngs::OsRng;
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::core::amount::CanonicalAmount;
use crate::core::error::{CoreError, CoreResult};
use crate::core::rfq::{
    Intent, PinnedSolverKeys, Quote, SignedQuote, now_unix_seconds, sign_quote, verify_quote,
};
use crate::core::routes::RouteRegistry;
use crate::core::storage::{Reservation, ReservationState, SqliteStore};

const MAX_HTTP_BODY_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RfqRequest {
    pub intent: Intent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RfqResponse {
    pub solver_id: String,
    pub signed_quote: SignedQuote,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReserveRequest {
    pub reservation_id: String,
    pub signed_quote: SignedQuote,
    pub intent: Intent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReserveResponse {
    pub reservation_id: String,
    pub state: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HealthResponse {
    pub solver_id: String,
    pub key_id: String,
    pub capabilities: Vec<String>,
}

pub struct SolverConfig {
    pub solver_id: String,
    pub key_id: String,
    pub secret_key_hex: String,
    pub asset_hash: String,
    pub inventory_asset_id: String,
    pub fee_lbtc: CanonicalAmount,
    pub rate_numerator: u64,
    pub rate_denominator: u64,
    pub database: std::path::PathBuf,
    pub inventory_amount: CanonicalAmount,
}

pub struct SolverService {
    solver_id: String,
    key_id: String,
    secret_key: SecretKey,
    asset_hash: String,
    inventory_asset_id: String,
    fee_lbtc: CanonicalAmount,
    rate_numerator: u64,
    rate_denominator: u64,
    store: SqliteStore,
}

impl SolverService {
    pub fn new(config: SolverConfig) -> CoreResult<Self> {
        if config.rate_numerator == 0 || config.rate_denominator == 0 {
            return Err(CoreError::QuoteRejected(
                "solver rate must be positive".to_owned(),
            ));
        }

        let secret_key = parse_secret_key(&config.secret_key_hex)?;
        let mut store = SqliteStore::open(&config.database)?;
        store.ensure_solver_ledger(
            &config.solver_id,
            &config.inventory_asset_id,
            &config.inventory_amount,
        )?;

        Ok(Self {
            solver_id: config.solver_id,
            key_id: config.key_id,
            secret_key,
            asset_hash: config.asset_hash,
            inventory_asset_id: config.inventory_asset_id,
            fee_lbtc: config.fee_lbtc,
            rate_numerator: config.rate_numerator,
            rate_denominator: config.rate_denominator,
            store,
        })
    }

    pub fn public_key_hex(&self) -> String {
        let public_key = PublicKey::from_secret_key(&Secp256k1::new(), &self.secret_key);
        hex::encode(public_key.serialize())
    }

    pub fn health(&self) -> HealthResponse {
        HealthResponse {
            solver_id: self.solver_id.clone(),
            key_id: self.key_id.clone(),
            capabilities: vec![
                "TEST-DEPIX@liquid-regtest".to_owned(),
                "BTC@lightning-regtest".to_owned(),
            ],
        }
    }

    pub fn quote(&self, intent: &Intent) -> CoreResult<SignedQuote> {
        let registry = RouteRegistry::qualified(&self.asset_hash)?;

        if !registry.is_enabled(&intent.source, &intent.destination) {
            return Err(CoreError::UnsupportedRoute {
                source_endpoint: intent.source.canonical_id(),
                destination_endpoint: intent.destination.canonical_id(),
            });
        }

        if intent.destination.asset_id != self.inventory_asset_id {
            return Err(CoreError::QuoteRejected(
                "solver inventory asset does not match destination asset".to_owned(),
            ));
        }

        let amount_in = intent.amount_in.value()?;
        let amount_out = amount_in
            .checked_mul(self.rate_numerator)
            .and_then(|value| value.checked_div(self.rate_denominator))
            .ok_or_else(|| CoreError::QuoteRejected("rate overflows amount".to_owned()))?;
        let amount_out = CanonicalAmount::parse(&amount_out.to_string())?;
        validate_destination_invoice(intent, &amount_out)?;
        let now = now_unix_seconds()?;
        let expires_at = intent.expires_at.min(now.saturating_add(300));
        let adapter_id = registry
            .route(&intent.source, &intent.destination)
            .map(|route| route.adapter_id.clone())
            .ok_or_else(|| CoreError::QuoteRejected("route missing".to_owned()))?;
        let quote = Quote {
            version: 1,
            quote_id: format!("{}-{}", self.solver_id, intent.intent_id),
            intent_id: intent.intent_id.clone(),
            source: intent.source.clone(),
            destination: intent.destination.clone(),
            amount_in: intent.amount_in.clone(),
            min_amount_out: intent.min_amount_out.clone(),
            amount_out,
            fee_lbtc: self.fee_lbtc.clone(),
            fee_limit_lbtc: intent.fee_limit_lbtc.clone(),
            hash_commitment: intent.hash_commitment.clone(),
            client_claim_pubkey: intent.client_claim_pubkey.clone(),
            client_refund_pubkey: intent.client_refund_pubkey.clone(),
            source_destination: intent.source_destination.clone(),
            destination_destination: intent.destination_destination.clone(),
            refund_destination: intent.refund_destination.clone(),
            source_refund_height: intent.deadline_context.source_refund_height,
            destination_refund_height: intent.deadline_context.destination_refund_height,
            lightning_expiry_height: intent.deadline_context.lightning_expiry_height,
            deadline_context: intent.deadline_context.clone(),
            expires_at,
            nonce: intent.nonce.clone(),
            solver_key_id: self.key_id.clone(),
            adapter_id,
            payment_request: if intent.destination.network
                == crate::core::network::Network::LightningRegtest
            {
                Some(intent.destination_destination.clone())
            } else {
                None
            },
        };

        sign_quote(&quote, &hex::encode(self.secret_key.secret_bytes()))
    }

    pub fn reserve(&mut self, request: ReserveRequest) -> CoreResult<ReserveResponse> {
        let mut keys = PinnedSolverKeys::default();
        keys.insert(self.key_id.clone(), self.public_key_hex());
        let registry = RouteRegistry::qualified(&self.asset_hash)?;
        verify_quote(
            &request.intent,
            &request.signed_quote,
            &keys,
            &registry,
            &request.intent.deadline_context,
            now_unix_seconds()?,
        )?;
        let reservation = Reservation {
            reservation_id: request.reservation_id.clone(),
            solver_id: self.solver_id.clone(),
            quote_id: request.signed_quote.quote.quote_id.clone(),
            swap_id: None,
            amount: request.signed_quote.quote.amount_out.clone(),
            hash_commitment: request.signed_quote.quote.hash_commitment.as_hex(),
            state: ReservationState::Held,
            expires_at: request.signed_quote.quote.expires_at,
        };
        self.store.reserve(&reservation, &self.inventory_asset_id)?;

        Ok(ReserveResponse {
            reservation_id: reservation.reservation_id,
            state: "HELD".to_owned(),
        })
    }
}

fn validate_destination_invoice(intent: &Intent, amount_out: &CanonicalAmount) -> CoreResult<()> {
    if intent.destination.network != crate::core::network::Network::LightningRegtest {
        return Ok(());
    }

    let invoice_text = intent.destination_destination.as_str();
    let invoice = Bolt11Invoice::from_str(invoice_text)
        .map_err(|error| CoreError::QuoteRejected(format!("invalid BOLT11 invoice: {error}")))?;

    if invoice.network() != BitcoinNetwork::Regtest {
        return Err(CoreError::QuoteRejected(
            "Lightning invoice is not for regtest".to_owned(),
        ));
    }

    let amount_msat = invoice
        .amount_milli_satoshis()
        .ok_or_else(|| CoreError::QuoteRejected("invoice must have an exact amount".to_owned()))?;

    if amount_msat % 1000 != 0 || amount_msat / 1000 != amount_out.value()? {
        return Err(CoreError::QuoteRejected(
            "quote amount_out must equal the BOLT11 amount exactly".to_owned(),
        ));
    }

    if invoice.payment_hash().to_byte_array() != intent.hash_commitment.bytes() {
        return Err(CoreError::QuoteRejected(
            "quote invoice hash does not equal H".to_owned(),
        ));
    }

    Ok(())
}

pub fn serve(bind: &str, mut service: SolverService) -> CoreResult<()> {
    let listener =
        TcpListener::bind(bind).map_err(|error| CoreError::Transport(error.to_string()))?;

    for stream in listener.incoming() {
        let mut stream = stream.map_err(|error| CoreError::Transport(error.to_string()))?;

        if let Err(error) = handle_connection(&mut stream, &mut service) {
            let _ = write_error(&mut stream, 400, &error.to_string());
        }
    }

    Ok(())
}

pub fn request_quote(
    peer: &str,
    request: &RfqRequest,
    timeout_seconds: u64,
) -> CoreResult<RfqResponse> {
    post_json(peer, "/rfq", request, timeout_seconds)
}

pub fn reserve_remote(
    peer: &str,
    request: &ReserveRequest,
    timeout_seconds: u64,
) -> CoreResult<ReserveResponse> {
    post_json(peer, "/reserve", request, timeout_seconds)
}

pub fn generate_key_file(path: impl AsRef<Path>) -> CoreResult<String> {
    let secret_key = SecretKey::new(&mut OsRng);
    let hex_key = hex::encode(secret_key.secret_bytes());
    let target = path.as_ref();

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| CoreError::Storage(error.to_string()))?;
    }

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(target)
        .map_err(|error| CoreError::Storage(error.to_string()))?;
    file.write_all(hex_key.as_bytes())
        .map_err(|error| CoreError::Storage(error.to_string()))?;
    file.sync_all()
        .map_err(|error| CoreError::Storage(error.to_string()))?;
    let public_key = PublicKey::from_secret_key(&Secp256k1::new(), &secret_key);

    Ok(hex::encode(public_key.serialize()))
}

pub fn read_key_file(path: impl AsRef<Path>) -> CoreResult<String> {
    let value = fs::read_to_string(path).map_err(|error| CoreError::Storage(error.to_string()))?;
    parse_secret_key(value.trim())?;

    Ok(value.trim().to_owned())
}

fn handle_connection(stream: &mut TcpStream, service: &mut SolverService) -> CoreResult<()> {
    let request = read_request(stream)?;
    let body = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => serde_json::to_vec(&service.health()).map_err(serialization_error)?,
        ("POST", "/rfq") => {
            let request: RfqRequest =
                serde_json::from_slice(&request.body).map_err(serialization_error)?;
            serde_json::to_vec(&RfqResponse {
                solver_id: service.solver_id.clone(),
                signed_quote: service.quote(&request.intent)?,
            })
            .map_err(serialization_error)?
        }
        ("POST", "/reserve") => {
            let request: ReserveRequest =
                serde_json::from_slice(&request.body).map_err(serialization_error)?;
            serde_json::to_vec(&service.reserve(request)?).map_err(serialization_error)?
        }
        ("POST", "/consume") => {
            return Err(CoreError::Adapter(
                "reservation consumption is runner-owned and is not an HTTP operation".to_owned(),
            ));
        }
        _ => return write_error(stream, 404, "route not found"),
    };

    write_response(stream, 200, &body)
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> CoreResult<HttpRequest> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .map_err(|error| CoreError::Transport(error.to_string()))?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|error| CoreError::Transport(error.to_string()))?;

        if read == 0 {
            return Err(CoreError::Transport(
                "HTTP request ended before headers".to_owned(),
            ));
        }

        bytes.extend_from_slice(&chunk[..read]);

        if let Some(position) = find_header_end(&bytes) {
            break position;
        }

        if bytes.len() > 64 * 1024 {
            return Err(CoreError::Transport(
                "HTTP headers are too large".to_owned(),
            ));
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| CoreError::Transport("HTTP headers are not UTF-8".to_owned()))?;
    let mut lines = headers.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| CoreError::Transport("HTTP request line is missing".to_owned()))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| CoreError::Transport("HTTP method is missing".to_owned()))?
        .to_owned();
    let path = request_parts
        .next()
        .ok_or_else(|| CoreError::Transport("HTTP path is missing".to_owned()))?
        .to_owned();
    let mut content_lengths = lines
        .clone()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CoreError::Transport("invalid content length".to_owned()))?;

    if content_lengths.len() > 1 {
        return Err(CoreError::Transport(
            "duplicate content-length headers are rejected".to_owned(),
        ));
    }

    if lines.clone().any(|line| {
        line.split_once(':')
            .is_some_and(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
    }) {
        return Err(CoreError::Transport(
            "transfer-encoding is rejected by the bounded HTTP server".to_owned(),
        ));
    }

    let content_length = content_lengths.pop().unwrap_or(0);

    if content_length > MAX_HTTP_BODY_BYTES {
        return Err(CoreError::Transport("HTTP body is too large".to_owned()));
    }

    let body_start = header_end
        .checked_add(4)
        .ok_or_else(|| CoreError::Transport("HTTP body offset overflow".to_owned()))?;
    let body_end = body_start
        .checked_add(content_length)
        .ok_or_else(|| CoreError::Transport("HTTP body length overflow".to_owned()))?;

    while bytes.len() < body_end {
        let read = stream
            .read(&mut chunk)
            .map_err(|error| CoreError::Transport(error.to_string()))?;

        if read == 0 {
            return Err(CoreError::Transport("HTTP body ended early".to_owned()));
        }

        bytes.extend_from_slice(&chunk[..read]);
    }

    Ok(HttpRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        body: bytes[body_start..body_end].to_vec(),
    })
}

fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) -> CoreResult<()> {
    let reason = if status == 200 { "OK" } else { "Error" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|error| CoreError::Transport(error.to_string()))
}

fn write_error(stream: &mut TcpStream, status: u16, message: &str) -> CoreResult<()> {
    let body = serde_json::json!({ "error": message }).to_string();
    write_response(stream, status, body.as_bytes())
}

fn post_json<T: Serialize, R: for<'de> Deserialize<'de>>(
    peer: &str,
    path: &str,
    request: &T,
    timeout_seconds: u64,
) -> CoreResult<R> {
    let url = Url::parse(&format!("http://{peer}{path}"))
        .map_err(|error| CoreError::Transport(error.to_string()))?;

    if !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
        || url.username() != ""
        || url.password().is_some()
    {
        return Err(CoreError::Transport(
            "solver peers must be loopback HTTP endpoints without credentials".to_owned(),
        ));
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_seconds))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| CoreError::Transport(error.to_string()))?;
    client
        .post(url)
        .json(request)
        .send()
        .map_err(|error| CoreError::Transport(error.to_string()))?
        .error_for_status()
        .map_err(|error| CoreError::Transport(error.to_string()))?
        .json::<R>()
        .map_err(|error| CoreError::Serialization(error.to_string()))
}

fn parse_secret_key(value: &str) -> CoreResult<SecretKey> {
    let bytes = hex::decode(value)
        .map_err(|_| CoreError::Signature("solver key is not hexadecimal".to_owned()))?;

    SecretKey::from_slice(&bytes).map_err(|error| CoreError::Signature(error.to_string()))
}

fn serialization_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::Serialization(error.to_string())
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}
