//! Error types for the collator monitor.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum CollatorError {
    #[error("Failed to connect to chain: {0}")]
    ConnectionFailed(String),

    #[error("Failed to query storage: {0}")]
    StorageQueryFailed(String),

    #[error("Failed to submit transaction: {0}")]
    TransactionFailed(String),

    #[error("Account not found: {0}")]
    AccountNotFound(String),

    #[error("Insufficient funds: have {have}, need {need}")]
    InsufficientFunds { have: u128, need: u128 },

    #[error("Invalid address format: {0}")]
    InvalidAddress(String),

    #[error("Slack notification failed: {0}")]
    SlackNotificationFailed(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Chain {chain} is not available on {network}")]
    ChainNotAvailable { chain: String, network: String },
}
