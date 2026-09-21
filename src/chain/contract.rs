//! Contract creation and deterministic metadata validation.

use bitcoin::Network;
use bitcoin::address::Address;

use super::htlc_script::build_htlc_witness_script;
use super::types::{AssetId, Chain, ChainError, HtlcContract};

/// Creates a Bitcoin or Liquid regtest P2WSH HTLC contract.
pub fn create_htlc_contract(
    chain: Chain,
    asset_id: AssetId,
    hash_lock: [u8; 32],
    claim_pubkey: bitcoin::secp256k1::PublicKey,
    refund_pubkey: bitcoin::secp256k1::PublicKey,
    refund_lock_height: u64,
) -> Result<HtlcContract, ChainError> {
    validate_asset(chain, asset_id)?;
    let witness_script =
        build_htlc_witness_script(hash_lock, &claim_pubkey, &refund_pubkey, refund_lock_height)?;
    let output_script = witness_script.to_p2wsh();
    let address = contract_address(chain, &witness_script, &output_script)?;

    Ok(HtlcContract {
        chain,
        asset_id,
        hash_lock,
        claim_pubkey,
        refund_pubkey,
        refund_lock_height,
        witness_script_hex: hex::encode(witness_script.as_bytes()),
        output_script_hex: hex::encode(output_script.as_bytes()),
        address: address.to_string(),
    })
}

/// Rebuilds a contract and rejects altered metadata before settlement.
pub fn validate_htlc_contract(contract: &HtlcContract) -> Result<(), ChainError> {
    let rebuilt = create_htlc_contract(
        contract.chain,
        contract.asset_id,
        contract.hash_lock,
        contract.claim_pubkey,
        contract.refund_pubkey,
        contract.refund_lock_height,
    )?;

    if rebuilt != *contract {
        return Err(ChainError::Validation(
            "saved HTLC metadata does not match its deterministic contract".to_owned(),
        ));
    }

    Ok(())
}

fn validate_asset(chain: Chain, asset_id: AssetId) -> Result<(), ChainError> {
    match (chain, asset_id) {
        (Chain::BitcoinRegtest, AssetId::Bitcoin) => Ok(()),
        (Chain::BitcoinRegtest, AssetId::Explicit(_)) => Err(ChainError::InvalidInput(
            "Bitcoin HTLC must use native BTC asset".to_owned(),
        )),
        (Chain::LiquidRegtest, AssetId::Explicit(_)) => Ok(()),
        (Chain::LiquidRegtest, AssetId::Bitcoin) => Err(ChainError::InvalidInput(
            "Liquid HTLC must use an explicit asset id".to_owned(),
        )),
    }
}

fn contract_address(
    chain: Chain,
    witness_script: &bitcoin::ScriptBuf,
    output_script: &bitcoin::ScriptBuf,
) -> Result<String, ChainError> {
    match chain {
        Chain::BitcoinRegtest => {
            Ok(Address::p2wsh(witness_script.as_script(), Network::Regtest).to_string())
        }
        Chain::LiquidRegtest => {
            let elements_witness =
                elements::Script::from_hex_no_prefix(&hex::encode(witness_script.as_bytes()))
                    .map_err(|error| ChainError::Serialization(error.to_string()))?;
            let expected_output = elements_witness.to_v0_p2wsh();

            if expected_output.as_bytes() != output_script.as_bytes() {
                return Err(ChainError::Validation(
                    "Liquid P2WSH output differs from deterministic witness script".to_owned(),
                ));
            }

            Ok(elements::Address::p2wsh(
                &elements_witness,
                None,
                &elements::address::AddressParams::ELEMENTS,
            )
            .to_string())
        }
    }
}
