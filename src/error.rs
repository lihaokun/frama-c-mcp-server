use rmcp::ErrorData as McpError;
use std::borrow::Cow;

#[derive(thiserror::Error, Debug)]
pub enum FramaCError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid frame: {0}")]
    InvalidFrame(String),

    #[error("Frama-C server error [{id}]: {msg}")]
    ServerError { id: String, msg: String },

    #[error("Request rejected [{id}]")]
    Rejected { id: String },

    #[error("Request killed [{id}]")]
    Killed { id: String },

    #[error("Connection timeout: waiting for CMDLINEOFF")]
    ConnectTimeout,

    #[error("Operation timeout after {0:?}")]
    Timeout(std::time::Duration),

    #[error("Unexpected response: {0}")]
    UnexpectedResponse(String),

    #[error("Function not found: {0}")]
    FunctionNotFound(String),

    #[error("Global variable not found: {0}")]
    GlobalNotFound(String),

    #[error("Symbol not found: {0}")]
    SymbolNotFound(String),
}

impl From<FramaCError> for McpError {
    fn from(e: FramaCError) -> Self {
        match e {
            FramaCError::ServerError { msg, .. } => {
                McpError::internal_error(msg, None)
            }
            FramaCError::Rejected { id } => {
                McpError::invalid_request(
                    Cow::Owned(format!("rejected: {id}")),
                    None,
                )
            }
            FramaCError::Killed { id } => {
                McpError::internal_error(
                    Cow::Owned(format!("killed: {id}")),
                    None,
                )
            }
            FramaCError::ConnectTimeout => {
                McpError::internal_error("connection timeout", None)
            }
            FramaCError::Timeout(d) => {
                McpError::internal_error(
                    Cow::Owned(format!("timeout after {d:?}")),
                    None,
                )
            }
            FramaCError::FunctionNotFound(name) => {
                McpError::invalid_params(
                    Cow::Owned(format!("function not found: {name}")),
                    None,
                )
            }
            FramaCError::GlobalNotFound(name) => {
                McpError::invalid_params(
                    Cow::Owned(format!("global variable not found: {name}")),
                    None,
                )
            }
            FramaCError::SymbolNotFound(name) => {
                McpError::invalid_params(
                    Cow::Owned(format!("symbol not found: {name}")),
                    None,
                )
            }
            other => {
                McpError::internal_error(
                    Cow::Owned(other.to_string()),
                    None,
                )
            }
        }
    }
}
