mod audit;
mod cancel;
pub mod compatibility;
mod config;
pub mod config_catalog;
mod error;
mod outbox;
pub mod package_policy;
mod planner;
pub mod plugin_manifest;
pub mod plugin_registry;
pub mod plugin_stubs;
mod project;
mod python_ast;
mod runs;
mod sessions;
pub mod state;
pub mod state_backup;
mod templates;

pub use audit::{default_audit_log_path, sha256_hex, AuditEvent, AuditLog, AUDIT_SCHEMA_VERSION};
pub use cancel::CancellationToken;
pub use compatibility::{
    check_compatibility, CompatibilityFinding, CompatibilityReport, CompatibilityStatus,
    CompatibilitySummary, COMPATIBILITY_SCHEMA_VERSION,
};
pub use config::{
    admin_config_path, check_configuration_formats, load_workspace_profile, migrate_legacy_yaml,
    resolve_effective_configuration, user_config_path, MigrationReport, ProfileTlsMode,
    WorkspaceProfile,
};
pub use error::{exit_codes, DowaiError, Result};
pub use outbox::{
    DeliveryOutcome, DispatchReport, EffectSink, OutboxEntry, Probe, NOT_DELIVERED_CODE,
};
pub use planner::{
    plan, read_planned_model, validate_plan_templates, verify_planned_model, ExecutionMode,
    ExecutionPlan, PlanNode,
};
pub use plugin_registry::{
    discover, find_plugins, plugin_json, plugin_list_json, plugin_roots_from, resolve_call,
    resolved_call_json, PluginCatalog, PluginScope, ResolvedCall,
};
pub use project::{
    create_project, load_project, validate_project, validate_project_with_configuration,
    DiagnosticSeverity, Project, ProjectPython, ProjectSource, ValidationDiagnostic,
    ValidationReport,
};
pub use runs::{
    current_time_ms, default_run_store_path, filter_invocations, invocation_depths, ActivityGuard,
    InvocationEntry, LogLevel, NewInvocation, NewRun, RecoveryReport, RetrySource, RunDetail,
    RunLease, RunStart, RunStore, RunSummary, RUN_INTERRUPTED_CODE, RUN_SCHEMA_VERSION,
};
pub use sessions::{
    current_owner, default_session_store_path, session_limits_from_config, session_ttl_from_config,
    RevisionSnapshot, SessionLimits, SessionLock, SessionMetadata, SessionStore, VariableOrigin,
    VersionedSnapshot,
};
pub use templates::{
    render_sql_model, render_template, source_relation_key, Materialization, RenderedSqlModel,
    TemplateOptions, TemplateRender,
};
