use quinn::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use quinn::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use quinn::rustls::{CertificateError, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::*;

#[derive(Debug)]
pub struct SpkiVerifier {
    expected_spki_hash: Vec<u8>,
}

impl SpkiVerifier {
    pub fn new() -> Result<SpkiVerifier, ()> {
        let cert_der = include_bytes!(env!("SOZO_SERVER_CRT"));
        let (_, cert) = X509Certificate::from_der(cert_der).map_err(|_| ())?;
        let spki = cert.public_key().raw;
        let hash = Sha256::digest(spki).to_vec();

        Ok(SpkiVerifier {
            expected_spki_hash: hash,
        })
    }
}

impl ServerCertVerifier for SpkiVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer,
        _intermediates: &[CertificateDer],
        _server_name: &ServerName,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let (_, cert) = X509Certificate::from_der(end_entity.as_ref())
            .map_err(|_| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;

        let hash = Sha256::digest(cert.public_key().raw).to_vec();

        if hash == self.expected_spki_hash {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::InvalidCertificate(CertificateError::BadSignature))
        }
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer,
        _dls: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer,
        _dls: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}
