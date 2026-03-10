use thiserror::Error;

#[derive(Error, Debug)]
pub enum DiscoveryError {
    #[error("mDNS service daemon error: {0}")]
    MdnsError(String),

    #[error("Failed to register mDNS service: {0}")]
    ServiceRegistrationError(String),

    #[error("Failed to browse mDNS services: {0}")]
    ServiceBrowseError(String),

    #[error("Invalid TXT record format: {0}")]
    InvalidTxtRecord(String),

    #[error("Failed to parse pubkey from TXT record")]
    PubkeyParseError,
}
