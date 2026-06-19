use thiserror::Error;

#[derive(Error, Debug)]
pub enum DbError {
    #[error("SQLite connection error: {0}")]
    ConnectionError(#[from] tokio_rusqlite::Error),

    #[error("SQLite database error: {0}")]
    SqliteError(#[from] rusqlite::Error),

    #[error("Message serialization error: {0}")]
    SerializationError(#[from] rmp_serde::encode::Error),

    #[error("Message deserialization error: {0}")]
    DeserializationError(#[from] rmp_serde::decode::Error),

    #[error("Invalid pubkey format")]
    InvalidPubkey,

    #[error("Invalid message ID format")]
    InvalidMessageId,

    #[error("Invalid relay proof format: {0}")]
    InvalidRelayProof(String),
}
