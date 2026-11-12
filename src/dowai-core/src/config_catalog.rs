//! Catálogo único de parâmetros configuráveis (plano §3.4, ENT-012).
//!
//! Cada parâmetro declara tipo, default, unidade, validação e regra de teto administrativo.
//! Defaults, validação e aplicação de tetos são derivados daqui; `specs/config.schema.json` e
//! `docs/CONFIGURATION.md` são conferidos contra este catálogo por testes.
//!
//! Exceções documentadas: `python.interpreter`, `python.timeout_seconds`,
//! `python.package_policy` e `python.allow_unlisted_packages` pertencem ao bloco `python` do
//! `dowai_project.json`, validado em `project.rs`, e não são camadas de configuração.

use serde_json::{json, Map, Value};

/// Tipo e validação de um parâmetro.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bool,
    /// Booleano que deve ser sempre `true` (AUDIT essencial).
    EssentialTrue,
    /// Inteiro não negativo dentro de `[min, max]`.
    Integer {
        min: u64,
        max: u64,
    },
    /// Número decimal finito maior ou igual a zero.
    NonNegativeNumber,
    Enum(&'static [&'static str]),
    NonEmptyText,
    /// Texto não vazio ou `null` (herda a camada anterior).
    NullableText,
    TextList,
    /// Lista que, nesta versão, só pode estar vazia.
    EmptyTextList,
    Object,
}

/// Regra aplicada pelo administrador sobre o valor efetivo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ceiling {
    None,
    /// Valor efetivo não pode exceder o teto administrativo.
    Maximum,
    /// Administrador `false` proíbe `true` em projeto ou usuário.
    DenyIfAdminFalse,
    /// Administrador `true` impõe `true` em projeto ou usuário.
    RequireIfAdminTrue,
    /// Valor efetivo é intersectado com a lista administrativa.
    Allowlist,
}

/// Default de um parâmetro.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DefaultValue {
    Bool(bool),
    Int(u64),
    Number(f64),
    Text(&'static str),
    Null,
    EmptyList,
    EmptyObject,
}

impl DefaultValue {
    pub fn to_json(self) -> Value {
        match self {
            Self::Bool(value) => json!(value),
            Self::Int(value) => json!(value),
            Self::Number(value) => json!(value),
            Self::Text(value) => json!(value),
            Self::Null => Value::Null,
            Self::EmptyList => json!([]),
            Self::EmptyObject => json!({}),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Parameter {
    pub section: &'static str,
    pub name: &'static str,
    pub kind: Kind,
    pub default: DefaultValue,
    pub unit: &'static str,
    pub ceiling: Ceiling,
}

const fn p(
    section: &'static str,
    name: &'static str,
    kind: Kind,
    default: DefaultValue,
    unit: &'static str,
    ceiling: Ceiling,
) -> Parameter {
    Parameter {
        section,
        name,
        kind,
        default,
        unit,
        ceiling,
    }
}

const fn int(min: u64, max: u64) -> Kind {
    Kind::Integer { min, max }
}

const DEFAULT_MODES: &[&str] = &["automatic", "manual"];
const LOG_LEVELS: &[&str] = &["ERROR", "WARNING", "INFO", "DEBUG"];
const COMPRESSION: &[&str] = &["none", "gzip"];
const CONSOLE_FORMATS: &[&str] = &["text", "json"];
const EMIT_MODES: &[&str] = &["disabled", "file"];
const CONFLICT_POLICIES: &[&str] = &["error", "wait_process"];
const INSTALL_TOOLS: &[&str] = &["uv"];
const RESOURCE_ENFORCEMENT: &[&str] = &["best_effort", "require"];
const STATE_BACKENDS: &[&str] = &["sqlite"];
const CHECKPOINT_POLICIES: &[&str] = &["after_commit"];

use Ceiling::{Allowlist, DenyIfAdminFalse, Maximum, None as NoCeiling, RequireIfAdminTrue};
use DefaultValue::{Bool, EmptyList, EmptyObject, Int, Null, Number, Text};
use Kind::{Bool as BoolKind, EssentialTrue, NonEmptyText, NonNegativeNumber, NullableText};

/// Todos os parâmetros do catálogo, por grupo.
pub static PARAMETERS: &[Parameter] = &[
    // runtime
    p(
        "runtime",
        "max_parallel_nodes",
        int(1, 256),
        Int(4),
        "nós",
        Maximum,
    ),
    p(
        "runtime",
        "max_worker_processes",
        int(1, 64),
        Int(4),
        "processos",
        Maximum,
    ),
    p(
        "runtime",
        "max_threads",
        int(1, 256),
        Int(8),
        "threads",
        Maximum,
    ),
    p(
        "runtime",
        "max_cpu_percent",
        int(1, 100),
        Int(100),
        "% de CPU",
        Maximum,
    ),
    p(
        "runtime",
        "max_memory_mb",
        int(64, 1_048_576),
        Int(2048),
        "MiB",
        Maximum,
    ),
    p(
        "runtime",
        "max_inflight_bytes",
        int(1, 1_099_511_627_776),
        Int(134_217_728),
        "bytes",
        Maximum,
    ),
    p(
        "runtime",
        "queue_capacity",
        int(1, 1_000_000),
        Int(1000),
        "itens",
        Maximum,
    ),
    p(
        "runtime",
        "shutdown_grace_seconds",
        int(0, 600),
        Int(10),
        "s",
        Maximum,
    ),
    // execution
    p(
        "execution",
        "default_mode",
        Kind::Enum(DEFAULT_MODES),
        Text("automatic"),
        "",
        NoCeiling,
    ),
    p(
        "execution",
        "default_profile",
        NullableText,
        Null,
        "nome do perfil",
        NoCeiling,
    ),
    p(
        "execution",
        "stop_on_error",
        BoolKind,
        Bool(true),
        "",
        NoCeiling,
    ),
    p(
        "execution",
        "allow_manual_mode",
        BoolKind,
        Bool(true),
        "",
        DenyIfAdminFalse,
    ),
    p(
        "execution",
        "max_call_depth",
        int(1, 256),
        Int(16),
        "níveis",
        Maximum,
    ),
    p(
        "execution",
        "max_invocations_per_run",
        int(1, 100_000),
        Int(1000),
        "invocações",
        Maximum,
    ),
    p(
        "execution",
        "node_timeout_seconds",
        int(1, 86_400),
        Int(3600),
        "s",
        Maximum,
    ),
    p(
        "execution",
        "run_timeout_seconds",
        int(1, 604_800),
        Int(86_400),
        "s",
        Maximum,
    ),
    p(
        "execution",
        "retry_max_attempts",
        int(1, 10),
        Int(1),
        "tentativas totais, incluindo a primeira",
        Maximum,
    ),
    p(
        "execution",
        "retry_backoff_ms",
        int(0, 600_000),
        Int(1000),
        "ms",
        Maximum,
    ),
    p(
        "execution",
        "retry_jitter",
        BoolKind,
        Bool(true),
        "",
        NoCeiling,
    ),
    // templates
    p(
        "templates",
        "strict_undefined",
        BoolKind,
        Bool(true),
        "",
        NoCeiling,
    ),
    p(
        "templates",
        "allow_dowai_if_alias",
        BoolKind,
        Bool(true),
        "",
        NoCeiling,
    ),
    p(
        "templates",
        "max_nesting_depth",
        int(1, 64),
        Int(32),
        "níveis",
        Maximum,
    ),
    p(
        "templates",
        "max_render_bytes",
        int(1024, 104_857_600),
        Int(1_048_576),
        "bytes",
        Maximum,
    ),
    p(
        "templates",
        "allowed_macros",
        Kind::EmptyTextList,
        EmptyList,
        "nomes; nesta versão só lista vazia (ADR 0004)",
        NoCeiling,
    ),
    // security
    p(
        "security",
        "allow_external_writes",
        BoolKind,
        Bool(false),
        "",
        DenyIfAdminFalse,
    ),
    p(
        "security",
        "require_approval_for_destructive",
        BoolKind,
        Bool(true),
        "",
        RequireIfAdminTrue,
    ),
    p(
        "security",
        "allow_insecure_tls",
        BoolKind,
        Bool(false),
        "",
        DenyIfAdminFalse,
    ),
    p(
        "security",
        "allowed_paths",
        Kind::TextList,
        EmptyList,
        "caminhos",
        Allowlist,
    ),
    p(
        "security",
        "allowed_hosts",
        Kind::TextList,
        EmptyList,
        "hosts",
        Allowlist,
    ),
    p(
        "security",
        "allowed_profiles",
        Kind::TextList,
        EmptyList,
        "perfis",
        Allowlist,
    ),
    // plugins
    p(
        "plugins",
        "require_manifest",
        BoolKind,
        Bool(true),
        "",
        RequireIfAdminTrue,
    ),
    p(
        "plugins",
        "allow_local_override",
        BoolKind,
        Bool(true),
        "",
        DenyIfAdminFalse,
    ),
    p(
        "plugins",
        "max_parallel_calls",
        int(1, 64),
        Int(4),
        "chamadas",
        Maximum,
    ),
    p(
        "plugins",
        "default_timeout_seconds",
        int(1, 86_400),
        Int(300),
        "s",
        Maximum,
    ),
    p(
        "plugins",
        "allow_network",
        BoolKind,
        Bool(false),
        "",
        DenyIfAdminFalse,
    ),
    p(
        "plugins",
        "allow_filesystem",
        BoolKind,
        Bool(false),
        "",
        DenyIfAdminFalse,
    ),
    p(
        "plugins",
        "require_idempotency_key",
        BoolKind,
        Bool(true),
        "",
        RequireIfAdminTrue,
    ),
    // logging
    p(
        "logging",
        "level",
        Kind::Enum(LOG_LEVELS),
        Text("INFO"),
        "",
        NoCeiling,
    ),
    p(
        "logging",
        "audit_enabled",
        EssentialTrue,
        Bool(true),
        "AUDIT essencial: sempre true",
        NoCeiling,
    ),
    p(
        "logging",
        "retention_days",
        int(1, 3650),
        Int(90),
        "dias",
        Maximum,
    ),
    p(
        "logging",
        "rotation_max_mb",
        int(1, 10_240),
        Int(64),
        "MiB",
        Maximum,
    ),
    p(
        "logging",
        "compression",
        Kind::Enum(COMPRESSION),
        Text("gzip"),
        "",
        NoCeiling,
    ),
    p(
        "logging",
        "redaction_enabled",
        BoolKind,
        Bool(true),
        "",
        RequireIfAdminTrue,
    ),
    p(
        "logging",
        "console_format",
        Kind::Enum(CONSOLE_FORMATS),
        Text("text"),
        "",
        NoCeiling,
    ),
    // lineage
    p("lineage", "enabled", BoolKind, Bool(false), "", NoCeiling),
    p(
        "lineage",
        "emit_mode",
        Kind::Enum(EMIT_MODES),
        Text("disabled"),
        "",
        NoCeiling,
    ),
    p(
        "lineage",
        "output_path",
        NonEmptyText,
        Text("lineage/openlineage.jsonl"),
        "caminho relativo ao projeto",
        NoCeiling,
    ),
    p(
        "lineage",
        "namespace",
        NonEmptyText,
        Text("dowai"),
        "",
        NoCeiling,
    ),
    p(
        "lineage",
        "fail_on_emit_error",
        BoolKind,
        Bool(false),
        "",
        NoCeiling,
    ),
    // ai
    p("ai", "enabled", BoolKind, Bool(false), "", DenyIfAdminFalse),
    p(
        "ai",
        "default_profile",
        NullableText,
        Null,
        "nome do perfil",
        NoCeiling,
    ),
    p(
        "ai",
        "max_tokens",
        int(1, 1_000_000),
        Int(4096),
        "tokens",
        Maximum,
    ),
    p(
        "ai",
        "max_cost_per_run",
        NonNegativeNumber,
        Number(0.0),
        "unidade monetária do provedor; 0 = nenhum custo permitido",
        Maximum,
    ),
    p("ai", "timeout_seconds", int(1, 600), Int(60), "s", Maximum),
    p(
        "ai",
        "allow_data_rows",
        BoolKind,
        Bool(false),
        "",
        DenyIfAdminFalse,
    ),
    p(
        "ai",
        "redaction",
        BoolKind,
        Bool(true),
        "",
        RequireIfAdminTrue,
    ),
    p(
        "ai",
        "profiles",
        Kind::Object,
        EmptyObject,
        "perfis de IA (objeto)",
        NoCeiling,
    ),
    // sessions
    p(
        "sessions",
        "conflict_policy",
        Kind::Enum(CONFLICT_POLICIES),
        Text("error"),
        "",
        NoCeiling,
    ),
    p(
        "sessions",
        "wait_process_timeout_seconds",
        int(1, 86_400),
        Int(300),
        "s",
        Maximum,
    ),
    p(
        "sessions",
        "ttl_seconds",
        int(60, 31_536_000),
        Int(86_400),
        "s",
        Maximum,
    ),
    p(
        "sessions",
        "max_variables",
        int(1, 100_000),
        Int(1000),
        "variáveis",
        Maximum,
    ),
    p(
        "sessions",
        "max_value_bytes",
        int(1, 10_485_760),
        Int(65_536),
        "bytes",
        Maximum,
    ),
    p(
        "sessions",
        "max_context_bytes",
        int(1, 104_857_600),
        Int(1_048_576),
        "bytes",
        Maximum,
    ),
    p(
        "sessions",
        "allow_resume",
        BoolKind,
        Bool(true),
        "",
        NoCeiling,
    ),
    p(
        "sessions",
        "persist_state",
        BoolKind,
        Bool(true),
        "",
        NoCeiling,
    ),
    // database
    p(
        "database",
        "pool_max_connections",
        int(1, 100),
        Int(4),
        "conexões",
        Maximum,
    ),
    p(
        "database",
        "connect_timeout_seconds",
        int(1, 600),
        Int(10),
        "s",
        Maximum,
    ),
    p(
        "database",
        "query_timeout_seconds",
        int(1, 86_400),
        Int(1800),
        "s",
        Maximum,
    ),
    p(
        "database",
        "fetch_batch_rows",
        int(1, 1_000_000),
        Int(10_000),
        "linhas",
        Maximum,
    ),
    p(
        "database",
        "write_batch_rows",
        int(1, 1_000_000),
        Int(10_000),
        "linhas",
        Maximum,
    ),
    p(
        "database",
        "max_transfer_bytes",
        int(1, 1_099_511_627_776),
        Int(1_073_741_824),
        "bytes",
        Maximum,
    ),
    p(
        "database",
        "tls_required",
        BoolKind,
        Bool(true),
        "",
        RequireIfAdminTrue,
    ),
    // python (ver exceções no cabeçalho do módulo)
    p(
        "python",
        "dependency_lock_required",
        BoolKind,
        Bool(true),
        "",
        RequireIfAdminTrue,
    ),
    p(
        "python",
        "install_tool",
        Kind::Enum(INSTALL_TOOLS),
        Text("uv"),
        "",
        NoCeiling,
    ),
    p(
        "python",
        "venv_path",
        NonEmptyText,
        Text(".venv"),
        "caminho relativo ao projeto",
        NoCeiling,
    ),
    p(
        "python",
        "max_threads",
        int(1, 256),
        Int(4),
        "threads",
        Maximum,
    ),
    p(
        "python",
        "resource_enforcement",
        Kind::Enum(RESOURCE_ENFORCEMENT),
        Text("best_effort"),
        "",
        NoCeiling,
    ),
    // state
    p(
        "state",
        "backend",
        Kind::Enum(STATE_BACKENDS),
        Text("sqlite"),
        "",
        NoCeiling,
    ),
    p(
        "state",
        "path",
        NonEmptyText,
        Text("state/dowai.sqlite"),
        "caminho relativo ao projeto",
        NoCeiling,
    ),
    p(
        "state",
        "busy_timeout_ms",
        int(0, 600_000),
        Int(5000),
        "ms",
        NoCeiling,
    ),
    p(
        "state",
        "checkpoint_policy",
        Kind::Enum(CHECKPOINT_POLICIES),
        Text("after_commit"),
        "",
        NoCeiling,
    ),
    p(
        "state",
        "retention_days",
        int(1, 3650),
        Int(90),
        "dias",
        Maximum,
    ),
];

/// Grupos do catálogo, na ordem de declaração.
pub fn sections() -> Vec<&'static str> {
    let mut sections: Vec<&'static str> = Vec::new();
    for parameter in PARAMETERS {
        if !sections.contains(&parameter.section) {
            sections.push(parameter.section);
        }
    }
    sections
}

/// Busca um parâmetro pelo grupo e nome.
pub fn find(section: &str, name: &str) -> Option<&'static Parameter> {
    PARAMETERS
        .iter()
        .find(|parameter| parameter.section == section && parameter.name == name)
}

/// Defaults aninhados por grupo, idênticos aos do catálogo.
pub fn defaults() -> Value {
    let mut root = Map::new();
    for section in sections() {
        let mut values = Map::new();
        for parameter in PARAMETERS.iter().filter(|item| item.section == section) {
            values.insert(parameter.name.to_owned(), parameter.default.to_json());
        }
        root.insert(section.to_owned(), Value::Object(values));
    }
    Value::Object(root)
}

impl Parameter {
    /// Chave `grupo.nome`, usada em origens e mensagens.
    pub fn key(&self) -> String {
        format!("{}.{}", self.section, self.name)
    }

    /// Valida um valor não nulo. Em caso de erro, retorna a expectativa em texto.
    pub fn validate(&self, value: &Value) -> std::result::Result<(), String> {
        let valid = match self.kind {
            Kind::Bool => value.is_boolean(),
            Kind::EssentialTrue => value == &Value::Bool(true),
            Kind::Integer { min, max } => value
                .as_u64()
                .is_some_and(|number| (min..=max).contains(&number)),
            Kind::NonNegativeNumber => value
                .as_f64()
                .is_some_and(|number| number.is_finite() && number >= 0.0),
            Kind::Enum(allowed) => value.as_str().is_some_and(|text| allowed.contains(&text)),
            Kind::NonEmptyText => value.as_str().is_some_and(|text| !text.trim().is_empty()),
            Kind::NullableText => {
                value.is_null() || value.as_str().is_some_and(|text| !text.trim().is_empty())
            }
            Kind::TextList => value
                .as_array()
                .is_some_and(|values| values.iter().all(Value::is_string)),
            Kind::EmptyTextList => value.as_array().is_some_and(Vec::is_empty),
            Kind::Object => value.is_object(),
        };
        if valid {
            return Ok(());
        }
        Err(self.expectation())
    }

    fn expectation(&self) -> String {
        let base = match self.kind {
            Kind::Bool => "boolean true ou false".to_owned(),
            Kind::EssentialTrue => "true (AUDIT essencial não pode ser desativado)".to_owned(),
            Kind::Integer { min, max } => format!("inteiro entre {min} e {max}"),
            Kind::NonNegativeNumber => "número maior ou igual a zero".to_owned(),
            Kind::Enum(allowed) => format!("um de: {}", allowed.join(", ")),
            Kind::NonEmptyText => "texto não vazio".to_owned(),
            Kind::NullableText => "texto não vazio ou null".to_owned(),
            Kind::TextList => "lista de textos".to_owned(),
            Kind::EmptyTextList => "lista vazia (não suportado nesta versão)".to_owned(),
            Kind::Object => "objeto".to_owned(),
        };
        if self.unit.is_empty() {
            base
        } else {
            format!("{base} (unidade: {})", self.unit)
        }
    }
}
