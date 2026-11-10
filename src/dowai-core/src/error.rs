use serde_json::Value;
use std::fmt;

pub type Result<T> = std::result::Result<T, DowaiError>;

#[derive(Debug)]
pub enum DowaiError {
    Config(String),
    Path(String),
    Model(String),
    Template(String),
    Cycle(String),
    Session(String),
    /// Concurrent writes to the same session variable (AC040, `conflict_policy = "error"`).
    SessionConflict(String),
    State(String),
    Connection(String),
    Execution(String),
    PythonFailure(String),
    PythonTimeout(String),
    PythonProtocol(String),
    Policy(String),
    /// The run was cancelled by the user (Ctrl+C, SIGTERM) or through the API.
    Cancelled(String),
    Validation {
        message: String,
        report: Value,
    },
    NotImplemented(String),
    /// Dataset access requested by a Python model (`env.ref`), with a stable code (AC002).
    DatasetAccess {
        code: &'static str,
        message: String,
    },
    Io(std::io::Error),
}

impl DowaiError {
    /// Cancellation with the fixed, free-text-free message used everywhere.
    pub fn cancelled() -> Self {
        Self::Cancelled("The run was cancelled".to_owned())
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "DOWAI_CONFIG_INVALID",
            Self::Path(_) => "DOWAI_POLICY_PATH",
            Self::Model(_) => "DOWAI_PLAN_INVALID",
            Self::Template(_) => "DOWAI_TEMPLATE_INVALID",
            Self::Cycle(_) => "DOWAI_PLAN_CYCLE",
            Self::Session(_) => "DOWAI_SESSION_INVALID",
            Self::SessionConflict(_) => "DOWAI_SESSION_CONFLICT",
            Self::State(_) => "DOWAI_STATE_ERROR",
            Self::Connection(_) => "DOWAI_CONNECTION_FAILED",
            Self::Execution(_) => "DOWAI_EXECUTION_FAILED",
            Self::PythonFailure(_) => "DOWAI_PYTHON_FAILED",
            Self::PythonTimeout(_) => "DOWAI_PYTHON_TIMEOUT",
            Self::PythonProtocol(_) => "DOWAI_PYTHON_PROTOCOL",
            Self::Policy(_) => "DOWAI_POLICY_DENIED",
            Self::Cancelled(_) => "DOWAI_CANCELLED",
            Self::Validation { .. } => "DOWAI_CONFIG_INVALID",
            Self::NotImplemented(_) => "DOWAI_NOT_IMPLEMENTED",
            Self::DatasetAccess { code, .. } => code,
            Self::Io(_) => "DOWAI_IO_ERROR",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_)
            | Self::Model(_)
            | Self::Template(_)
            | Self::Cycle(_)
            | Self::Session(_)
            | Self::State(_)
            | Self::Validation { .. } => 2,
            Self::Path(_) => 5,
            Self::Policy(_) => 5,
            Self::Connection(_) => 4,
            Self::Execution(_) => 3,
            // A run that lost a write race is a failed run execution (plan 6.4: exit 3).
            Self::SessionConflict(_) => 3,
            Self::PythonFailure(_) | Self::PythonTimeout(_) | Self::PythonProtocol(_) => 3,
            Self::NotImplemented(_) => 6,
            Self::DatasetAccess { .. } => 3,
            Self::Cancelled(_) => 130,
            Self::Io(_) => 2,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Config(message)
            | Self::Path(message)
            | Self::Model(message)
            | Self::Template(message)
            | Self::Cycle(message)
            | Self::Session(message)
            | Self::SessionConflict(message)
            | Self::State(message)
            | Self::Connection(message)
            | Self::Execution(message)
            | Self::PythonFailure(message)
            | Self::PythonTimeout(message)
            | Self::PythonProtocol(message)
            | Self::Policy(message)
            | Self::Cancelled(message)
            | Self::NotImplemented(message)
            | Self::DatasetAccess { message, .. } => message.clone(),
            Self::Validation { message, .. } => message.clone(),
            Self::Io(error) => error.to_string(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }

    pub fn details(&self) -> Option<&Value> {
        match self {
            Self::Validation { report, .. } => Some(report),
            _ => None,
        }
    }
}

impl fmt::Display for DowaiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for DowaiError {}

impl From<std::io::Error> for DowaiError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for DowaiError {
    fn from(error: serde_json::Error) -> Self {
        Self::Config(error.to_string())
    }
}

impl From<rusqlite::Error> for DowaiError {
    fn from(error: rusqlite::Error) -> Self {
        Self::State(format!("Session storage operation failed: {error}"))
    }
}
