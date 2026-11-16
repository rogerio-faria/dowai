use serde_json::Value;
use std::fmt;

pub type Result<T> = std::result::Result<T, DowaiError>;

/// Contrato público de exit codes (plano §6.4). Único ponto de definição dos valores;
/// `DowaiError::exit_code` e a CLI usam estas constantes.
pub mod exit_codes {
    /// Sucesso.
    pub const SUCCESS: i32 = 0;
    /// Configuração, parser, plano ou entrada inválida (também usado por `clap` para uso incorreto).
    pub const INVALID_INPUT: i32 = 2;
    /// Falha de execução de modelo.
    pub const EXECUTION_FAILED: i32 = 3;
    /// Falha de conexão externa.
    pub const CONNECTION_FAILED: i32 = 4;
    /// Política, permissão ou segurança negada.
    pub const POLICY_DENIED: i32 = 5;
    /// Funcionalidade não implementada ou indisponível.
    pub const NOT_IMPLEMENTED: i32 = 6;
    /// Cancelamento pelo usuário (SIGINT/SIGTERM ou API).
    pub const CANCELLED: i32 = 130;
}

/// Plugin refusals caused by the caller's input, not by the plugin run (exit code 2).
const INVALID_INPUT_PLUGIN_CODES: [&str; 2] = [
    "DOWAI_PLUGIN_INPUT_INVALID",
    "DOWAI_PLUGIN_IDEMPOTENCY_KEY_REQUIRED",
];

#[derive(Debug)]
pub enum DowaiError {
    Config(String),
    /// Two configuration formats for one scope: never merged (AC032).
    ConfigConflict(String),
    /// A legacy YAML configuration without its JSON counterpart; not read (AC032).
    LegacyConfigFormat(String),
    Path(String),
    Model(String),
    /// Model file changed between planning and execution (ENT-024).
    ModelChanged(String),
    Template(String),
    /// `model()` does not match the `(env)` or `(env, origem)` contract (ENT-125).
    ModelContract(String),
    Cycle(String),
    Session(String),
    /// Concurrent writes to the same session variable (AC040, `conflict_policy = "error"`).
    SessionConflict(String),
    /// Two nodes of the same concurrency wave write the same session variable (ENT-120, ADR 0008 §4).
    VariableConflict(String),
    State(String),
    Connection(String),
    Execution(String),
    PythonFailure(String),
    PythonTimeout(String),
    PythonProtocol(String),
    Policy(String),
    /// A resource limit that the platform cannot enforce, under `python.resource_enforcement = require` (ENT-038, D06).
    ResourceUnsupported(String),
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
    /// `dowai retry` refusals, each with its own stable code (B-05c).
    Retry {
        code: &'static str,
        message: String,
    },
    /// `env.write` refusals and failures, each with its own stable `DOWAI_WRITE_*` code (ENT-048).
    Write {
        code: &'static str,
        message: String,
    },
    /// Plugin execution refusals and failures, each with its own stable `DOWAI_PLUGIN_*` code
    /// (ENT-107). The message never carries free text produced by the plugin.
    Plugin {
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
            Self::ConfigConflict(_) => "DOWAI_CONFIG_CONFLICT",
            Self::LegacyConfigFormat(_) => "DOWAI_CONFIG_LEGACY_YAML",
            Self::Path(_) => "DOWAI_POLICY_PATH",
            Self::Model(_) => "DOWAI_PLAN_INVALID",
            Self::ModelChanged(_) => "DOWAI_PLAN_MODEL_CHANGED",
            Self::Template(_) => "DOWAI_TEMPLATE_INVALID",
            Self::ModelContract(_) => "DOWAI_MODEL_SIGNATURE_INVALID",
            Self::Cycle(_) => "DOWAI_PLAN_CYCLE",
            Self::Session(_) => "DOWAI_SESSION_INVALID",
            Self::SessionConflict(_) => "DOWAI_SESSION_CONFLICT",
            Self::VariableConflict(_) => "DOWAI_VARIABLE_CONFLICT",
            Self::State(_) => "DOWAI_STATE_ERROR",
            Self::Connection(_) => "DOWAI_CONNECTION_FAILED",
            Self::Execution(_) => "DOWAI_EXECUTION_FAILED",
            Self::PythonFailure(_) => "DOWAI_PYTHON_FAILED",
            Self::PythonTimeout(_) => "DOWAI_PYTHON_TIMEOUT",
            Self::PythonProtocol(_) => "DOWAI_PYTHON_PROTOCOL",
            Self::Policy(_) => "DOWAI_POLICY_DENIED",
            Self::ResourceUnsupported(_) => "DOWAI_RESOURCE_UNSUPPORTED",
            Self::Cancelled(_) => "DOWAI_CANCELLED",
            Self::Validation { .. } => "DOWAI_CONFIG_INVALID",
            Self::NotImplemented(_) => "DOWAI_NOT_IMPLEMENTED",
            Self::DatasetAccess { code, .. } => code,
            Self::Retry { code, .. } => code,
            Self::Write { code, .. } => code,
            Self::Plugin { code, .. } => code,
            Self::Io(_) => "DOWAI_IO_ERROR",
        }
    }

    /// Exit code público do erro (plano §6.4). Ponto único de mapeamento.
    pub fn exit_code(&self) -> i32 {
        use exit_codes as ec;
        match self {
            Self::Config(_)
            | Self::ConfigConflict(_)
            | Self::LegacyConfigFormat(_)
            | Self::Model(_)
            | Self::Template(_)
            | Self::ModelContract(_)
            | Self::Cycle(_)
            | Self::Session(_)
            | Self::State(_)
            | Self::Validation { .. }
            | Self::Retry { .. }
            | Self::Io(_) => ec::INVALID_INPUT,
            Self::Plugin { code, .. } if INVALID_INPUT_PLUGIN_CODES.contains(code) => {
                ec::INVALID_INPUT
            }
            Self::Plugin { .. } => ec::EXECUTION_FAILED,
            Self::Path(_) | Self::Policy(_) => ec::POLICY_DENIED,
            Self::Connection(_) => ec::CONNECTION_FAILED,
            Self::Execution(_)
            // A run that lost a write race is a failed run execution (plan 6.4: exit 3).
            | Self::SessionConflict(_)
            | Self::VariableConflict(_)
            | Self::PythonFailure(_)
            | Self::PythonTimeout(_)
            | Self::PythonProtocol(_)
            | Self::ModelChanged(_)
            | Self::ResourceUnsupported(_)
            | Self::DatasetAccess { .. }
            | Self::Write { .. } => ec::EXECUTION_FAILED,
            Self::NotImplemented(_) => ec::NOT_IMPLEMENTED,
            Self::Cancelled(_) => ec::CANCELLED,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Config(message)
            | Self::ConfigConflict(message)
            | Self::LegacyConfigFormat(message)
            | Self::Path(message)
            | Self::Model(message)
            | Self::ModelChanged(message)
            | Self::Template(message)
            | Self::ModelContract(message)
            | Self::Cycle(message)
            | Self::Session(message)
            | Self::SessionConflict(message)
            | Self::VariableConflict(message)
            | Self::State(message)
            | Self::Connection(message)
            | Self::Execution(message)
            | Self::PythonFailure(message)
            | Self::PythonTimeout(message)
            | Self::PythonProtocol(message)
            | Self::Policy(message)
            | Self::ResourceUnsupported(message)
            | Self::Cancelled(message)
            | Self::NotImplemented(message)
            | Self::DatasetAccess { message, .. }
            | Self::Plugin { message, .. }
            | Self::Retry { message, .. }
            | Self::Write { message, .. } => message.clone(),
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

#[cfg(test)]
mod tests {
    use super::{exit_codes, DowaiError};

    #[test]
    fn every_variant_maps_to_one_contract_code() {
        let cases: Vec<(DowaiError, i32)> = vec![
            (DowaiError::Config(String::new()), exit_codes::INVALID_INPUT),
            (
                DowaiError::ConfigConflict(String::new()),
                exit_codes::INVALID_INPUT,
            ),
            (
                DowaiError::LegacyConfigFormat(String::new()),
                exit_codes::INVALID_INPUT,
            ),
            (DowaiError::Path(String::new()), exit_codes::POLICY_DENIED),
            (DowaiError::Model(String::new()), exit_codes::INVALID_INPUT),
            (
                DowaiError::Template(String::new()),
                exit_codes::INVALID_INPUT,
            ),
            (
                DowaiError::ModelContract(String::new()),
                exit_codes::INVALID_INPUT,
            ),
            (DowaiError::Cycle(String::new()), exit_codes::INVALID_INPUT),
            (
                DowaiError::Session(String::new()),
                exit_codes::INVALID_INPUT,
            ),
            (
                DowaiError::SessionConflict(String::new()),
                exit_codes::EXECUTION_FAILED,
            ),
            (DowaiError::State(String::new()), exit_codes::INVALID_INPUT),
            (
                DowaiError::Connection(String::new()),
                exit_codes::CONNECTION_FAILED,
            ),
            (
                DowaiError::Execution(String::new()),
                exit_codes::EXECUTION_FAILED,
            ),
            (
                DowaiError::PythonFailure(String::new()),
                exit_codes::EXECUTION_FAILED,
            ),
            (
                DowaiError::PythonTimeout(String::new()),
                exit_codes::EXECUTION_FAILED,
            ),
            (
                DowaiError::PythonProtocol(String::new()),
                exit_codes::EXECUTION_FAILED,
            ),
            (DowaiError::Policy(String::new()), exit_codes::POLICY_DENIED),
            (DowaiError::cancelled(), exit_codes::CANCELLED),
            (
                DowaiError::Validation {
                    message: String::new(),
                    report: serde_json::Value::Null,
                },
                exit_codes::INVALID_INPUT,
            ),
            (
                DowaiError::NotImplemented(String::new()),
                exit_codes::NOT_IMPLEMENTED,
            ),
            (
                DowaiError::DatasetAccess {
                    code: "DOWAI_DATASET_TEST",
                    message: String::new(),
                },
                exit_codes::EXECUTION_FAILED,
            ),
            (
                DowaiError::Retry {
                    code: "DOWAI_RETRY_TEST",
                    message: String::new(),
                },
                exit_codes::INVALID_INPUT,
            ),
            (
                DowaiError::Plugin {
                    code: "DOWAI_PLUGIN_INPUT_INVALID",
                    message: String::new(),
                },
                exit_codes::INVALID_INPUT,
            ),
            (
                DowaiError::Plugin {
                    code: "DOWAI_PLUGIN_TIMEOUT",
                    message: String::new(),
                },
                exit_codes::EXECUTION_FAILED,
            ),
            (
                DowaiError::Io(std::io::Error::other("test")),
                exit_codes::INVALID_INPUT,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.exit_code(), expected, "{}", error.code());
        }
    }

    #[test]
    fn contract_values_match_plan_section_6_4() {
        assert_eq!(
            [
                exit_codes::SUCCESS,
                exit_codes::INVALID_INPUT,
                exit_codes::EXECUTION_FAILED,
                exit_codes::CONNECTION_FAILED,
                exit_codes::POLICY_DENIED,
                exit_codes::NOT_IMPLEMENTED,
                exit_codes::CANCELLED,
            ],
            [0, 2, 3, 4, 5, 6, 130]
        );
    }
}
