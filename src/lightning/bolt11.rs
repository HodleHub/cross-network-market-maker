use std::time::{SystemTime, UNIX_EPOCH};

use bitcoin::hashes::Hash as _;
use lightning_invoice::{Bolt11Invoice, Currency};

use super::error::LightningError;
use super::types::InvoiceBinding;

pub(crate) fn validate_payment_request(
    payment_request: &str,
    expected_hash: [u8; 32],
    expected_amount_sats: u64,
) -> Result<InvoiceBinding, LightningError> {
    let invoice = payment_request
        .parse::<Bolt11Invoice>()
        .map_err(|error| LightningError::InvalidInvoice(error.to_string()))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| LightningError::InvalidInvoice(error.to_string()))?;

    if invoice.expires_at().is_some_and(|expiry| now >= expiry) {
        return Err(LightningError::InvoiceExpired);
    }

    if invoice.currency() != Currency::Regtest {
        return Err(LightningError::InvoiceWrongNetwork);
    }

    let invoice_hash = invoice.payment_hash().to_byte_array();

    if invoice_hash != expected_hash {
        return Err(LightningError::InvoiceHashMismatch);
    }

    let expected_msat = expected_amount_sats
        .checked_mul(1_000)
        .ok_or(LightningError::ParameterOutOfRange)?;
    let amount_msat = invoice
        .amount_milli_satoshis()
        .ok_or(LightningError::InvoiceAmountMismatch)?;

    if amount_msat != expected_msat {
        return Err(LightningError::InvoiceAmountMismatch);
    }

    Ok(InvoiceBinding {
        payment_hash: invoice_hash,
        amount_sats: expected_amount_sats,
    })
}
