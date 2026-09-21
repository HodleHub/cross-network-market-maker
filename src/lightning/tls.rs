//! Exact-certificate TLS configuration for the dedicated local LND nodes.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, SignatureScheme};

/// Builds a Rustls client configuration pinned to one PEM certificate.
pub fn pinned_client_config(certificate_pem: &[u8]) -> Result<rustls::ClientConfig, String> {
    let certificate = CertificateDer::from_pem_slice(certificate_pem)
        .map_err(|error| format!("invalid pinned LND certificate: {error}"))?;
    let provider = rustls::crypto::ring::default_provider();
    let verifier = ExactCertificateVerifier {
        certificate_der: certificate.as_ref().to_vec(),
        algorithms: provider.signature_verification_algorithms,
    };
    let builder = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("invalid TLS protocol configuration: {error}"))?;

    Ok(builder
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}

#[derive(Debug)]
struct ExactCertificateVerifier {
    certificate_der: Vec<u8>,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ExactCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if end_entity.as_ref() != self.certificate_der.as_slice() {
            return Err(Error::General(
                "LND TLS certificate does not match the pinned certificate".to_owned(),
            ));
        }

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
