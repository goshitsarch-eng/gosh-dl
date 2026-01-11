//! Protocol-level errors
//!
//! Simplified error types for the engine boundary.
//! These hide internal implementation details while providing
//! enough information for consumers to handle errors appropriately.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Protocol-level error returned from engine operations
///
/// This is a simplified error type that hides internal engine details
/// while providing actionable information to consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProtocolError {
    /// Download was not found
    NotFound {
        /// The ID that was not found
        id: String,
    },

    /// Invalid state for requested operation
    InvalidState {
        /// The action that was attempted
        action: String,
        /// The current state that prevented the action
        current_state: String,
    },

    /// Invalid input provided
    InvalidInput {
        /// The field with invalid input
        field: String,
        /// Description of what's wrong
        message: String,
    },

    /// Network error occurred
    Network {
        /// Error description
        message: String,
        /// Whether the operation can be retried
        retryable: bool,
    },

    /// Storage/filesystem error
    Storage {
        /// Error description
        message: String,
    },

    /// Engine is shutting down
    Shutdown,

    /// Internal engine error (opaque to consumer)
    Internal {
        /// Error description
        message: String,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { id } => write!(f, "Download not found: {}", id),
            Self::InvalidState {
                action,
                current_state,
            } => write!(f, "Cannot {} while {}", action, current_state),
            Self::InvalidInput { field, message } => {
                write!(f, "Invalid input for '{}': {}", field, message)
            }
            Self::Network { message, .. } => write!(f, "Network error: {}", message),
            Self::Storage { message } => write!(f, "Storage error: {}", message),
            Self::Shutdown => write!(f, "Engine is shutting down"),
            Self::Internal { message } => write!(f, "Internal error: {}", message),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl ProtocolError {
    /// Check if this error is retryable
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network { retryable, .. } => *retryable,
            _ => false,
        }
    }
}

/// Result type for protocol operations
pub type ProtocolResult<T> = std::result::Result<T, ProtocolError>;
