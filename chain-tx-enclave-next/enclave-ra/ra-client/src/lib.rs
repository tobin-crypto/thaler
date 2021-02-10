//! This crate exposes types for connecting to an enclave over attested TLS connection.
//!
//! # Usage
//!
//! ```rust,no_run
//! use std::sync::Arc;
//!
//! use ra_client::{EnclaveCertVerifier, EnclaveCertVerifierConfig};
//! use rustls::ClientConfig;
//!
//! let verifier_config = EnclaveCertVerifierConfig {
//!     //signing_ca_cert_pem: include_bytes!("./path/to/Intel_SGX_Attestation_RootCA.pem"),
//!     valid_enclave_quote_statuses: vec!["OK".into(), "GROUP_OUT_OF_DATE".into()].into(),
//!     ..Default::default()
//! };
//! let verifier = EnclaveCertVerifier::new(verifier_config).unwrap();
//!
//! let tls_client_config: Arc<ClientConfig> = Arc::new(verifier.into_client_config(true).unwrap());
//!
//! // This `tls_client_config` can now be used to create a `rustls::Stream`.
//! ```
mod config;
mod verifier;

pub use self::{
    config::{EnclaveCertVerifierConfig, EnclaveInfo},
    verifier::{
        AttestedCertVerifier, CertVerifyResult, EnclaveCertVerifier, EnclaveCertVerifierError,
        ENCLAVE_CERT_VERIFIER,
    },
};
pub use ra_common::DEFAULT_EXPIRATION_SECS;
