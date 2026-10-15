//! Command failures carrying stable error codes.

use std::fmt;

use ghf_proto::ErrorCode;

#[derive(Debug)]
pub struct Failure {
    pub code: ErrorCode,
    pub message: String,
    /// Structured detail for the JSON envelope (for example, apply conflicts).
    pub data: Option<serde_json::Value>,
}

pub type Result<T> = std::result::Result<T, Failure>;

impl Failure {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// `fail!(Code, "fmt", args...)` returns early with a [`Failure`].
#[macro_export]
macro_rules! fail {
    ($code:ident, $($arg:tt)*) => {
        return Err($crate::error::Failure::new(ghf_proto::ErrorCode::$code, format!($($arg)*)))
    };
}

/// `err!(Code, "fmt", args...)` builds a [`Failure`].
#[macro_export]
macro_rules! err {
    ($code:ident, $($arg:tt)*) => {
        $crate::error::Failure::new(ghf_proto::ErrorCode::$code, format!($($arg)*))
    };
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Failure::new(ErrorCode::Internal, format!("I/O error: {e}"))
    }
}

impl From<rusqlite::Error> for Failure {
    fn from(e: rusqlite::Error) -> Self {
        Failure::new(ErrorCode::Internal, format!("local store error: {e}"))
    }
}

impl From<serde_json::Error> for Failure {
    fn from(e: serde_json::Error) -> Self {
        Failure::new(ErrorCode::Internal, format!("JSON error: {e}"))
    }
}

/// Attach context to I/O results.
pub trait Context<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T>;
}

impl<T> Context<T> for std::io::Result<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| Failure::new(ErrorCode::Internal, format!("{}: {e}", what())))
    }
}
