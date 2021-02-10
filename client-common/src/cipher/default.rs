use std::{
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    sync::Arc,
};

use parity_scale_codec::{Decode, Encode};

use crate::TransactionObfuscation;
use crate::{
    tendermint::{types::AbciQueryExt, Client},
    Error, ErrorKind, PrivateKey, Result, ResultExt, SignedTransaction, Transaction,
};
use chain_core::tx::{data::TxId, TxAux, TxWithOutputs};
use enclave_macro::{get_mrsigner, get_network_id, get_tqe_mrenclave};
use enclave_protocol::{
    DecryptionRequest, DecryptionResponse, EncryptionRequest, EncryptionResponse,
    TxQueryInitRequest, TxQueryInitResponse,
};
use ra_client::{EnclaveCertVerifier, EnclaveCertVerifierConfig, EnclaveInfo};

fn get_tls_config() -> Arc<rustls::ClientConfig> {
    let mr_signer: [u8; 32] = get_mrsigner!();
    let mr_enclave: Option<[u8; 32]> = Some(get_tqe_mrenclave!());
    let tqe_info = EnclaveInfo {
        mr_enclave,
        mr_signer,
        previous_mr_enclave: None,
        isv_prod_id: get_network_id!(),
        // TODO: it seems there's no global CPU SVN across all CPU models,
        // so one can't really fix it to one at compile-time?
        cpu_svn: [0; 16],
        // TODO: should also be configurable / upgrades should be tested
        isv_svn: 0,
        // TODO: should be configurable, but would need more human-interpretable config to bitvec
        // e.g. which flag is the debug mode launch
        attributes: [0; 16],
    };
    let config = EnclaveCertVerifierConfig::new_with_enclave_info(tqe_info);
    let verifier = EnclaveCertVerifier::new(config).expect("verifier config");
    Arc::new(
        verifier
            .into_client_config(true)
            .expect("Error while creating TLS client configuration"),
    )
}

/// Implementation of transaction obfuscation which directly talks to transaction decryption query and encryption enclaves
/// TODO: querying from multiple nodes / addresses
#[derive(Debug, Clone)]
pub struct DefaultTransactionObfuscation {
    tqe_address: String,
    tqe_hostname: webpki::DNSName,
}

impl DefaultTransactionObfuscation {
    /// tqe_address: connection string <HOST/IP:PORT>
    /// tqe_hostname: expected hostname (e.g. localhost in testing)
    pub fn new(tqe_address: String, tqe_hostname: String) -> Self {
        // one may just write an ip address instead of a domain name, which isn't a valid DNS name
        // so there's a default case
        // TODO: should TQE enforce valid domain names,
        // as some of the infra may not be assigned a domain name
        // and the TLS checking is augmented with attestation anyway?
        let dns_name = webpki::DNSNameRef::try_from_ascii_str(&tqe_hostname)
            .unwrap_or_else(|_| webpki::DNSNameRef::try_from_ascii_str("localhost").unwrap())
            .to_owned();
        DefaultTransactionObfuscation {
            tqe_address,
            tqe_hostname: dns_name,
        }
    }

    /// Get DefaultTransactionObfuscation from txquery call to Tendermint client
    pub fn from_tx_query<C>(tendermint_client: &C) -> Result<DefaultTransactionObfuscation>
    where
        C: Client,
    {
        let result = tendermint_client
            .query("txquery", &[], None, false)?
            .bytes();
        let address = std::str::from_utf8(&result).chain(|| {
            (
                ErrorKind::ConnectionError,
                "Unable to decode txquery address",
            )
        })?;
        DefaultTransactionObfuscation::from_tx_query_address(&address)
    }

    /// Get DefaultTransactionObfuscation from tx query address
    pub fn from_tx_query_address(address: &str) -> Result<DefaultTransactionObfuscation> {
        if let Some(hostname) = address.split(':').next() {
            Ok(DefaultTransactionObfuscation::new(
                address.to_string(),
                hostname.to_string(),
            ))
        } else {
            Err(Error::new(
                ErrorKind::ConnectionError,
                "Unable to decode txquery address",
            ))
        }
    }
}

impl TransactionObfuscation for DefaultTransactionObfuscation {
    fn decrypt(
        &self,
        transaction_ids: &[TxId],
        private_key: &PrivateKey,
    ) -> Result<Vec<Transaction>> {
        if transaction_ids.is_empty() {
            return Ok(vec![]);
        }

        let client_config = get_tls_config();
        let dns_name = self.tqe_hostname.as_ref();
        // FIXME: better response from enclave and retry mechanism
        for attempt in 0..3 {
            let mut sess = rustls::ClientSession::new(&client_config, dns_name);

            let mut conn = TcpStream::connect(&self.tqe_address).chain(|| {
                (
                    ErrorKind::ConnectionError,
                    format!("Unable to connect to TQE address: {}", self.tqe_address),
                )
            })?;

            let mut tls = rustls::Stream::new(&mut sess, &mut conn);
            tls.write_all(&TxQueryInitRequest::DecryptChallenge.encode())
                .chain(|| {
                    (
                        ErrorKind::IoError,
                        "Unable to write to TQE connection stream (init decrypt)",
                    )
                })?;
            tls.flush().chain(|| {
                (
                    ErrorKind::IoError,
                    "Unable to write to TQE connection stream (init decrypt flush)",
                )
            })?;
            let mut challenge = [0u8; 33];
            tls.read_exact(&mut challenge).chain(|| {
                (
                    ErrorKind::IoError,
                    "Unable to read from TQE connection stream",
                )
            })?;
            let resp = TxQueryInitResponse::decode(&mut challenge.as_ref());
            let ch = match resp {
                Ok(TxQueryInitResponse::DecryptChallenge(challenge)) => challenge,
                _ => {
                    return Err(Error::new(
                        ErrorKind::IoError,
                        "unexpected response from TQE connection stream",
                    ))
                }
            };

            let request = DecryptionRequest::create(
                secp256k1::SECP256K1,
                transaction_ids.to_owned(),
                ch,
                &private_key.into(),
            );
            tls.write_all(&request.encode()).chain(|| {
                (
                    ErrorKind::IoError,
                    "Unable to write to TQE connection stream (decrypt request)",
                )
            })?;
            tls.flush().chain(|| {
                (
                    ErrorKind::IoError,
                    "Unable to write to TQE connection stream (decrypt request flush)",
                )
            })?;
            let mut plaintext = Vec::new();
            let result = match tls.read_to_end(&mut plaintext) {
                Ok(_) => {
                    let mresp = DecryptionResponse::decode(&mut plaintext.as_slice());
                    if let Ok(resp) = mresp {
                        let txs = resp.txs;

                        let transactions = txs
                            .into_iter()
                            .map(|tx| match tx {
                                TxWithOutputs::Transfer(t) => Transaction::TransferTransaction(t),
                                TxWithOutputs::StakeWithdraw(t) => {
                                    Transaction::WithdrawUnbondedStakeTransaction(t)
                                }
                            })
                            .collect::<Vec<Transaction>>();

                        Ok(transactions)
                    } else {
                        Err(Error::new(
                            ErrorKind::DeserializationError,
                            "Unable to deserialize decryption response from enclave",
                        ))
                    }
                }
                Err(_) => Err(Error::new(
                    ErrorKind::IoError,
                    "Unable to read from TQE connection stream",
                )),
            };
            if result.is_ok() || attempt == 2 {
                return result;
            } else {
                let _ = conn.shutdown(Shutdown::Both);
                log::info!("Decrypt request failed, retrying");
                std::thread::sleep(std::time::Duration::from_millis(3000));
            }
        }
        unreachable!()
    }

    fn encrypt(&self, transaction: SignedTransaction) -> Result<TxAux> {
        let client_config = get_tls_config();
        let dns_name = self.tqe_hostname.as_ref();
        let mut sess = rustls::ClientSession::new(&client_config, dns_name);

        let mut conn = TcpStream::connect(&self.tqe_address).chain(|| {
            (
                ErrorKind::ConnectionError,
                format!("Unable to connect to TQE address: {}", self.tqe_address),
            )
        })?;
        let mut tls = rustls::Stream::new(&mut sess, &mut conn);
        let request = match transaction {
            SignedTransaction::TransferTransaction(tx, witness) => {
                TxQueryInitRequest::Encrypt(Box::new(EncryptionRequest::TransferTx(tx, witness)))
            }
            SignedTransaction::DepositStakeTransaction(tx, witness) => {
                TxQueryInitRequest::Encrypt(Box::new(EncryptionRequest::DepositStake(tx, witness)))
            }

            SignedTransaction::WithdrawUnbondedStakeTransaction(tx, witness) => {
                TxQueryInitRequest::Encrypt(Box::new(EncryptionRequest::WithdrawStake(tx, witness)))
            }
        };
        tls.write_all(&request.encode()).chain(|| {
            (
                ErrorKind::IoError,
                "Unable to write to TQE connection stream (encrypt request)",
            )
        })?;
        tls.flush().chain(|| {
            (
                ErrorKind::IoError,
                "Unable to write to TQE connection stream (encrypt request flush)",
            )
        })?;
        let mut plaintext = Vec::new();
        match tls.read_to_end(&mut plaintext) {
            Ok(_) => {
                let tx = EncryptionResponse::decode(&mut plaintext.as_slice())
                    .chain(|| {
                        (
                            ErrorKind::DeserializationError,
                            "Unable to deserialize encryption response from enclave",
                        )
                    })?
                    .resp
                    .map_err(|e| {
                        Error::new(
                            ErrorKind::InvalidInput,
                            format!("Invalid transaction was submitted: {}", e),
                        )
                    })?;
                Ok(TxAux::EnclaveTx(tx))
            }
            Err(_) => Err(Error::new(
                ErrorKind::IoError,
                "Unable to read from TQE connection stream",
            )),
        }
    }
}
