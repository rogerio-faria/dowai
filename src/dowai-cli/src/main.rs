use clap::{Args, Parser, Subcommand};
use dowai_core::{
    admin_config_path, create_project, default_run_store_path, default_session_store_path,
    filter_invocations, invocation_depths, plan, resolve_effective_configuration, user_config_path,
    validate_project_with_configuration, CancellationToken, DowaiError, ExecutionMode,
    InvocationEntry, LogLevel, Result, RevisionSnapshot, RunDetail, RunStore, RunSummary,
    SessionMetadata, SessionStore, TemplateOptions,
};
use dowai_runtime::{
    check_postgres_connection, check_python_runtime, check_python_sdk, declared_default_profile,
    prepare_postgres_plan, retry_run_detailed_with_cancel, run_postgres_detailed_with_cancel,
    RetryRequest, RunRequest, RunResult,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const DEFAULT_SESSION_TTL_SECONDS: u64 = 86_400;
const DEFAULT_RUNS_LIMIT: usize = 20;

#[derive(Debug, Parser)]
#[command(
    name = "dowai",
    version,
    about = "Data Objects With AI — local workflow engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init(InitArgs),
    Doctor(JsonArgs),
    Validate(ValidateArgs),
    Plan(PlanArgs),
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Run(RunArgs),
    /// Run again a FAILED or CANCELLED run as a new run (B-05c). Variables are required
    /// again when the original run used any; their values are not stored.
    Retry(RetryArgs),
    /// Show the invocation tree of one run from the journal (no free-form messages).
    ///
    /// Levels: ERROR = FAILED; WARN = FAILED and SKIPPED; INFO (default), DEBUG and
    /// TRACE = every invocation (the journal records nothing more detailed).
    Logs {
        run_id: String,
        /// Only invocations whose target (or its file name) equals this value.
        #[arg(long)]
        node: Option<String>,
        #[arg(long, default_value = "INFO")]
        level: String,
        #[arg(long)]
        json: bool,
    },
    Runs {
        #[command(subcommand)]
        command: RunsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RunsCommand {
    /// List recent runs, newest first.
    List {
        #[arg(long, default_value_t = DEFAULT_RUNS_LIMIT)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show one run with its invocations.
    Show {
        run_id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    Start {
        #[arg(long)]
        project_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Show {
        session_id: String,
        #[arg(long)]
        json: bool,
    },
    End {
        session_id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
struct InitArgs {
    name: String,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct JsonArgs {
    #[arg(long)]
    check_connections: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ValidateArgs {
    path: Option<PathBuf>,
    #[arg(long)]
    strict: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PlanArgs {
    #[arg(required = true, num_args = 1..)]
    targets: Vec<String>,
    #[arg(long)]
    manual: bool,
    #[arg(long)]
    vars: Option<String>,
    #[arg(long)]
    project_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Show {
        #[arg(long)]
        effective: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        project_dir: Option<PathBuf>,
    },
    /// Convert a legacy YAML configuration to native JSON (never overwrites; AC032).
    Migrate {
        /// Legacy YAML file; it is read and never modified.
        path: PathBuf,
        /// New JSON file to create; it must not exist.
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
struct RetryArgs {
    run_id: String,
    /// Re-executes only the models that finished FAILED, without their predecessors.
    #[arg(long)]
    failed_only: bool,
    /// Required again when the original run used variables (values are not journaled).
    #[arg(long)]
    vars: Option<String>,
    #[arg(long, help = "Confirm replacement of existing PostgreSQL relations")]
    yes: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(required = true, num_args = 1..)]
    targets: Vec<String>,
    #[arg(long)]
    manual: bool,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    vars: Option<String>,
    #[arg(long)]
    full_refresh: bool,
    #[arg(long, help = "Confirm replacement of existing PostgreSQL relations")]
    yes: bool,
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json_output = wants_json(&cli.command);
    match execute(cli.command) {
        Ok(Some(value)) => match serde_json::to_string_pretty(&value) {
            Ok(output) => {
                println!("{output}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("DOWAI_OUTPUT_ERROR: {error}");
                ExitCode::from(dowai_core::exit_codes::INVALID_INPUT as u8)
            }
        },
        Ok(None) => ExitCode::SUCCESS,
        Err(failure) => {
            let CliFailure { error, run_id } = failure;
            if json_output {
                let mut envelope = json!({
                    "schema_version": 2,
                    "status": if error.is_cancelled() { "CANCELLED" } else { "FAILED" },
                    "error_code": error.code(),
                    "message": error.message(),
                    "details": error.details()
                });
                // Only a run that started has an identifier (ENT-015).
                if let Some(run_id) = run_id {
                    envelope["run_id"] = json!(run_id);
                }
                println!("{envelope}");
            } else {
                eprintln!("{error}");
            }
            ExitCode::from(error.exit_code() as u8)
        }
    }
}

/// A failed CLI command. `run_id` is set only when the failure belongs to a run that started.
#[derive(Debug)]
struct CliFailure {
    error: DowaiError,
    run_id: Option<String>,
}

impl From<DowaiError> for CliFailure {
    fn from(error: DowaiError) -> Self {
        Self {
            error,
            run_id: None,
        }
    }
}

/// Entry point for `main`: `run` and `retry` keep the run identifier of a failed run.
fn execute(command: Command) -> std::result::Result<Option<Value>, CliFailure> {
    match command {
        Command::Run(args) => run_command(args),
        Command::Retry(args) => retry_command(args),
        other => dispatch(other).map_err(CliFailure::from),
    }
}

fn dispatch(command: Command) -> Result<Option<Value>> {
    match command {
        Command::Init(args) => {
            let workspace = match args.workspace {
                Some(path) => path,
                None => std::env::current_dir()?,
            };
            let root = create_project(&workspace, &args.name)?;
            if args.json {
                Ok(Some(json!({
                    "schema_version": 2,
                    "status": "CREATED",
                    "project_dir": root,
                    "message": "Project initialized"
                })))
            } else {
                println!("Project initialized at {}", root.display());
                Ok(None)
            }
        }
        Command::Doctor(args) => {
            let current = find_project_root(&std::env::current_dir()?)?;
            let report = validate_project_with_configuration(
                &current,
                user_config_path().as_deref(),
                admin_config_path().as_deref(),
            );
            // ENT-016: every problem becomes a diagnostic with a stable code. Messages never
            // carry driver output, which may contain connection data. Warnings (ENT-018) are
            // advisory and do not degrade the health report.
            let mut diagnostics: Vec<Value> = report
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == dowai_core::DiagnosticSeverity::Error)
                .map(|diagnostic| json!({"code": diagnostic.code, "message": diagnostic.message}))
                .collect();
            let mut python_runtime = "not_checked";
            let mut python_sdk = "not_checked";
            if report.project.is_some() {
                match check_python_runtime(&current) {
                    Ok(None) => python_runtime = "not_configured",
                    Ok(Some(interpreter)) => {
                        python_runtime = "configured";
                        match check_python_sdk(&interpreter, &current) {
                            Ok(()) => python_sdk = "available",
                            Err(error) => {
                                python_sdk = "unavailable";
                                diagnostics.push(
                                    json!({"code": error.code(), "message": error.message()}),
                                );
                            }
                        }
                    }
                    Err(error) => {
                        python_runtime = "invalid";
                        diagnostics.push(json!({"code": error.code(), "message": error.message()}));
                    }
                }
            }
            let mut sql_database = "not_checked";
            if args.check_connections && report.project.is_some() {
                match declared_default_profile(&current) {
                    Ok(None) => sql_database = "not_declared",
                    Ok(Some(_)) => match check_postgres_connection(&current, None) {
                        Ok(_) => sql_database = "connected",
                        Err(error @ DowaiError::Connection(_)) => {
                            sql_database = "failed";
                            diagnostics.push(json!({
                                "code": error.code(),
                                "message": "Could not connect with the declared default profile"
                            }));
                        }
                        Err(error) => {
                            sql_database = "failed";
                            diagnostics
                                .push(json!({"code": error.code(), "message": error.message()}));
                        }
                    },
                    Err(error) => {
                        sql_database = "failed";
                        diagnostics.push(json!({"code": error.code(), "message": error.message()}));
                    }
                }
            }
            let valid = diagnostics.is_empty();
            let result = json!({
                "schema_version": 2,
                "status": if valid { "HEALTHY" } else { "DEGRADED" },
                "project": report.project,
                "diagnostics": diagnostics,
                "checks": {
                    "project_config": report.valid,
                    "sql_database": sql_database,
                    "python_runtime": python_runtime,
                    "python_sdk": python_sdk
                }
            });
            if !valid {
                return Err(DowaiError::Validation {
                    message: "Project health check failed".to_owned(),
                    report: result,
                });
            }
            if args.json {
                Ok(Some(result))
            } else {
                println!("DOWAI project checks passed.");
                if sql_database == "connected" {
                    println!("PostgreSQL connection check passed.");
                }
                if python_runtime == "configured" {
                    println!("Python runtime and dowai SDK checks passed.");
                }
                Ok(None)
            }
        }
        Command::Validate(args) => {
            let project_dir = match args.path {
                Some(path) => path,
                None => std::env::current_dir()?,
            };
            let mut report = validate_project_with_configuration(
                &project_dir,
                user_config_path().as_deref(),
                admin_config_path().as_deref(),
            );
            if args.strict {
                report.apply_strict();
            }
            if !args.json {
                for diagnostic in &report.diagnostics {
                    let label = match diagnostic.severity {
                        dowai_core::DiagnosticSeverity::Error => "Error",
                        dowai_core::DiagnosticSeverity::Warning => "Warning",
                    };
                    let location = diagnostic
                        .path
                        .as_deref()
                        .map(|path| format!(" ({path})"))
                        .unwrap_or_default();
                    eprintln!(
                        "{label} [{}]{location}: {}",
                        diagnostic.code, diagnostic.message
                    );
                }
            }
            let value = serde_json::to_value(&report)
                .map_err(|error| DowaiError::Config(error.to_string()))?;
            if !report.valid {
                return Err(DowaiError::Validation {
                    message: format!(
                        "Project validation failed with {} diagnostic(s)",
                        report.diagnostics.len()
                    ),
                    report: value,
                });
            }
            let execution_plan = plan(&project_dir, &["all".to_owned()], ExecutionMode::Automatic)?;
            let mut validation_options = template_options(&project_dir)?;
            validation_options.strict_undefined = false;
            prepare_postgres_plan(
                &project_dir,
                &execution_plan,
                &BTreeMap::new(),
                &validation_options,
            )?;
            if args.json {
                Ok(Some(value))
            } else {
                if report.diagnostics.is_empty() {
                    println!("Project is valid.");
                } else {
                    println!(
                        "Project is valid with {} warning(s).",
                        report.diagnostics.len()
                    );
                }
                Ok(None)
            }
        }
        Command::Plan(args) => {
            let variables = parse_vars(args.vars.as_deref())?;
            let project_dir = match args.project_dir {
                Some(path) => path,
                None => find_project_root(&std::env::current_dir()?)?,
            };
            let mode = if args.manual {
                ExecutionMode::Manual
            } else {
                ExecutionMode::Automatic
            };
            let result = plan(&project_dir, &args.targets, mode)?;
            prepare_postgres_plan(
                &project_dir,
                &result,
                &variables,
                &template_options(&project_dir)?,
            )?;
            if args.json {
                Ok(Some(
                    serde_json::to_value(result)
                        .map_err(|error| DowaiError::Model(error.to_string()))?,
                ))
            } else {
                println!("Plan ({})", result.execution_mode);
                for node in result.nodes {
                    if node.depends_on.is_empty() {
                        println!("  {}", node.id);
                    } else {
                        println!("  {} <- {}", node.id, node.depends_on.join(", "));
                    }
                }
                for warning in result.warnings {
                    eprintln!("Warning: {warning}");
                }
                Ok(None)
            }
        }
        Command::Session { command } => {
            let path = default_session_store_path().ok_or_else(|| {
                DowaiError::State("No user state directory is available".to_owned())
            })?;
            dispatch_session(command, &SessionStore::for_user_state(path))
        }
        Command::Logs {
            run_id,
            node,
            level,
            json,
        } => {
            let path = default_run_store_path().ok_or_else(|| {
                DowaiError::State("No user state directory is available".to_owned())
            })?;
            dispatch_logs(
                &run_id,
                node.as_deref(),
                &level,
                json,
                &RunStore::for_user_state(path),
            )
        }
        Command::Runs { command } => {
            let path = default_run_store_path().ok_or_else(|| {
                DowaiError::State("No user state directory is available".to_owned())
            })?;
            dispatch_runs(command, &RunStore::for_user_state(path))
        }
        Command::Config { command } => match command {
            ConfigCommand::Show {
                effective,
                json,
                project_dir,
            } => {
                if !effective {
                    return Err(DowaiError::Config(
                        "Specify --effective; partial config inspection is not available"
                            .to_owned(),
                    ));
                }
                let config_root = match project_dir {
                    Some(path) => path,
                    None => find_config_root(&std::env::current_dir()?),
                };
                let config = resolve_effective_configuration(
                    &config_root,
                    user_config_path().as_deref(),
                    admin_config_path().as_deref(),
                )?;
                if json {
                    Ok(Some(config))
                } else {
                    let output = serde_json::to_string_pretty(&config)
                        .map_err(|error| DowaiError::Config(error.to_string()))?;
                    println!("{output}");
                    Ok(None)
                }
            }
            ConfigCommand::Migrate { path, output, json } => {
                let report = dowai_core::migrate_legacy_yaml(&path, &output)?;
                let summary = serde_json::json!({
                    "status": "MIGRATED",
                    "input": path,
                    "output": report.output,
                    "changes": report.changes,
                });
                if json {
                    Ok(Some(summary))
                } else {
                    println!("Migrated {} -> {}", path.display(), report.output.display());
                    for change in &report.changes {
                        println!("  {change}");
                    }
                    println!("The original file was not modified. Review the output, then remove the YAML.");
                    Ok(None)
                }
            }
        },
        // Reached only through `execute`, which handles these two commands with run ids.
        Command::Retry(args) => retry_command(args).map_err(|failure| failure.error),
        Command::Run(args) => run_command(args).map_err(|failure| failure.error),
    }
}

fn retry_command(args: RetryArgs) -> std::result::Result<Option<Value>, CliFailure> {
    let project_dir = find_project_root(&current_dir()?)?;
    let variables = parse_vars(args.vars.as_deref())?;
    let cancel = install_cancel_handler()?;
    let result = retry_run_detailed_with_cancel(
        &project_dir,
        &args.run_id,
        RetryRequest {
            failed_only: args.failed_only,
            variables,
            confirm_destructive: args.yes,
        },
        &cancel,
    )
    .map_err(|failure| CliFailure {
        error: failure.error,
        run_id: failure.run_id,
    })?;
    run_result_output(result, args.json).map_err(CliFailure::from)
}

fn run_command(args: RunArgs) -> std::result::Result<Option<Value>, CliFailure> {
    if args.full_refresh {
        return Err(DowaiError::NotImplemented(
            "The separate full-refresh operation is not implemented yet".to_owned(),
        )
        .into());
    }
    let project_dir = find_project_root(&current_dir()?)?;
    // SIGINT/SIGTERM (Ctrl+C on Windows) only flip the token; the runtime stops
    // the Python process tree and cancels any running SQL, then reports exit 130.
    let cancel = install_cancel_handler()?;
    let result = run_postgres_detailed_with_cancel(
        &project_dir,
        RunRequest {
            targets: args.targets,
            mode: if args.manual {
                Some(ExecutionMode::Manual)
            } else {
                None
            },
            profile: args.profile,
            session_id: args.session,
            variables: parse_vars(args.vars.as_deref())?,
            confirm_destructive: args.yes,
        },
        &cancel,
    )
    .map_err(|failure| CliFailure {
        error: failure.error,
        run_id: failure.run_id,
    })?;
    run_result_output(result, args.json).map_err(CliFailure::from)
}

/// Current directory as a CLI error when it is unavailable.
fn current_dir() -> Result<PathBuf> {
    std::env::current_dir().map_err(DowaiError::from)
}

/// Cancellation token wired to SIGINT/SIGTERM (Ctrl+C on Windows).
fn install_cancel_handler() -> Result<CancellationToken> {
    let cancel = CancellationToken::new();
    let signal_token = cancel.clone();
    ctrlc::set_handler(move || signal_token.cancel()).map_err(|_| {
        DowaiError::State("Could not install the cancellation signal handler".to_owned())
    })?;
    Ok(cancel)
}

/// Output shared by `dowai run` and `dowai retry`.
fn run_result_output(result: RunResult, json: bool) -> Result<Option<Value>> {
    if json {
        Ok(Some(
            serde_json::to_value(result).map_err(|error| DowaiError::Model(error.to_string()))?,
        ))
    } else {
        println!(
            "Run {} succeeded (session {}).",
            result.run_id, result.session_id
        );
        for node in result.nodes {
            println!(
                "  {} -> {} ({})",
                node.id, node.relation, node.materialization
            );
        }
        for warning in result.warnings {
            eprintln!("Warning: {warning}");
        }
        Ok(None)
    }
}

fn template_options(project_dir: &Path) -> Result<TemplateOptions> {
    let configuration = resolve_effective_configuration(
        project_dir,
        user_config_path().as_deref(),
        admin_config_path().as_deref(),
    )?;
    let templates = &configuration["effective"]["templates"];
    let max_nesting = templates["max_nesting_depth"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            DowaiError::Config("Effective templates.max_nesting_depth is invalid".to_owned())
        })?;
    let strict_undefined = templates["strict_undefined"].as_bool().ok_or_else(|| {
        DowaiError::Config("Effective templates.strict_undefined is invalid".to_owned())
    })?;
    let allow_dowai_if_alias = templates["allow_dowai_if_alias"].as_bool().ok_or_else(|| {
        DowaiError::Config("Effective templates.allow_dowai_if_alias is invalid".to_owned())
    })?;
    Ok(TemplateOptions {
        max_nesting,
        strict_undefined,
        allow_dowai_if_alias,
    })
}

fn wants_json(command: &Command) -> bool {
    match command {
        Command::Init(args) => args.json,
        Command::Doctor(args) => args.json,
        Command::Validate(args) => args.json,
        Command::Plan(args) => args.json,
        Command::Session { command } => match command {
            SessionCommand::Start { json, .. }
            | SessionCommand::Show { json, .. }
            | SessionCommand::End { json, .. } => *json,
        },
        Command::Config { command } => match command {
            ConfigCommand::Show { json, .. } | ConfigCommand::Migrate { json, .. } => *json,
        },
        Command::Run(args) => args.json,
        Command::Retry(args) => args.json,
        Command::Logs { json, .. } => *json,
        Command::Runs { command } => match command {
            RunsCommand::List { json, .. } | RunsCommand::Show { json, .. } => *json,
        },
    }
}

fn dispatch_runs(command: RunsCommand, store: &RunStore) -> Result<Option<Value>> {
    match command {
        RunsCommand::List { limit, json } => {
            let runs = store.list(limit)?;
            if json {
                return Ok(Some(json!({
                    "schema_version": 2,
                    "limit": limit,
                    "runs": runs
                })));
            }
            if runs.is_empty() {
                println!("No runs recorded.");
            }
            for run in &runs {
                print_run_line(run);
            }
            Ok(None)
        }
        RunsCommand::Show { run_id, json } => {
            validate_run_id(&run_id)?;
            let detail = store.get(&run_id)?;
            if json {
                return Ok(Some(json!({ "schema_version": 2, "run": detail })));
            }
            print_run_detail(&detail);
            Ok(None)
        }
    }
}

fn dispatch_logs(
    run_id: &str,
    node: Option<&str>,
    level: &str,
    json: bool,
    store: &RunStore,
) -> Result<Option<Value>> {
    let level: LogLevel = level.parse()?;
    validate_run_id(run_id)?;
    let detail = store.get(run_id)?;
    let depths = invocation_depths(&detail.invocations);
    let visible = filter_invocations(&detail.invocations, node, level);
    let entries: Vec<(usize, &InvocationEntry)> = visible
        .iter()
        .map(|entry| {
            let depth = detail
                .invocations
                .iter()
                .position(|candidate| candidate.invocation_id == entry.invocation_id)
                .map_or(0, |index| depths[index]);
            (depth, entry)
        })
        .collect();
    if json {
        let invocations: Vec<Value> = entries
            .iter()
            .map(|(depth, entry)| {
                let mut value = json!(entry);
                value["depth"] = json!(depth);
                value
            })
            .collect();
        return Ok(Some(json!({
            "schema_version": 2,
            "run_id": detail.run.run_id,
            "status": detail.run.status,
            "level": level.as_str(),
            "node": node,
            "invocations": invocations
        })));
    }
    print_run_line(&detail.run);
    for (depth, entry) in entries {
        println!(
            "{}{} {} {} {} {} ms{}",
            "  ".repeat(depth),
            entry.status,
            entry.kind,
            entry.call_type,
            entry.target,
            entry.duration_ms,
            entry
                .error_code
                .as_deref()
                .map(|code| format!("  {code}"))
                .unwrap_or_default()
        );
    }
    Ok(None)
}

fn print_run_line(run: &RunSummary) {
    println!(
        "{}  {}  {}  {} ms  {}{}",
        run.run_id,
        run.status,
        run.started_at,
        run.duration_ms,
        run.project,
        run.error_code
            .as_deref()
            .map(|code| format!("  {code}"))
            .unwrap_or_default()
    );
}

fn print_run_detail(detail: &RunDetail) {
    print_run_line(&detail.run);
    println!(
        "Mode: {}  Session: {}",
        detail.run.execution_mode.as_deref().unwrap_or("-"),
        detail.run.session_id.as_deref().unwrap_or("-")
    );
    println!("Invocations: {}", detail.invocations.len());
    for entry in &detail.invocations {
        println!(
            "  {} {} {} {} {} ms{}",
            entry.status,
            entry.kind,
            entry.call_type,
            entry.target,
            entry.duration_ms,
            entry
                .error_code
                .as_deref()
                .map(|code| format!("  {code}"))
                .unwrap_or_default()
        );
    }
}

/// Run ids are UUIDs (8-4-4-4-12 hexadecimal digits).
fn validate_run_id(run_id: &str) -> Result<()> {
    let groups: Vec<&str> = run_id.split('-').collect();
    let valid = groups.len() == 5
        && [8, 4, 4, 4, 12].iter().zip(&groups).all(|(length, group)| {
            group.len() == *length && group.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    if valid {
        Ok(())
    } else {
        Err(DowaiError::Config("Run id must be a UUID".to_owned()))
    }
}

fn dispatch_session(command: SessionCommand, store: &SessionStore) -> Result<Option<Value>> {
    match command {
        SessionCommand::Start { project_dir, json } => {
            let project_root = match project_dir {
                Some(path) => path,
                None => find_project_root(&std::env::current_dir()?)?,
            };
            let session = store.create(
                &project_root,
                Duration::from_secs(DEFAULT_SESSION_TTL_SECONDS),
            )?;
            if json {
                Ok(Some(session_json(&session, &RevisionSnapshot::default())))
            } else {
                println!("Session started: {}", session.session_id);
                println!("Project: {}", session.project_root.display());
                Ok(None)
            }
        }
        SessionCommand::Show { session_id, json } => {
            let session = store.get(&session_id)?;
            // Revisions only: values never reach the output (they may be secrets).
            let revisions = store.revision_snapshot(&session_id)?;
            if json {
                Ok(Some(session_json(&session, &revisions)))
            } else {
                println!("Session: {}", session.session_id);
                println!("Status: {}", session.status);
                println!("Project: {}", session.project_root.display());
                println!("Variables: {}", revisions.revisions.len());
                println!("Variable revision: {}", revisions.variable_revision);
                Ok(None)
            }
        }
        SessionCommand::End { session_id, json } => {
            store.end(&session_id)?;
            if json {
                Ok(Some(json!({
                    "schema_version": 2,
                    "status": "ENDED",
                    "session_id": session_id
                })))
            } else {
                println!("Session ended: {session_id}");
                Ok(None)
            }
        }
    }
}

fn session_json(session: &SessionMetadata, revisions: &RevisionSnapshot) -> Value {
    json!({
        "schema_version": 2,
        "status": session.status,
        "session_id": session.session_id,
        "project_root": session.project_root,
        "created_at_ms": session.created_at_ms,
        "expires_at_ms": session.expires_at_ms,
        "variable_names": revisions.revisions.keys().collect::<Vec<_>>(),
        "variable_revision": revisions.variable_revision,
        "variable_revisions": revisions.revisions
    })
}

fn parse_vars(raw: Option<&str>) -> Result<BTreeMap<String, Value>> {
    if let Some(raw) = raw {
        let value: Value = serde_json::from_str(raw)
            .map_err(|error| DowaiError::Config(format!("Invalid --vars JSON: {error}")))?;
        let object = value
            .as_object()
            .ok_or_else(|| DowaiError::Config("--vars must be a JSON object".to_owned()))?;
        return Ok(object
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect());
    }
    Ok(BTreeMap::new())
}

fn find_project_root(start: &Path) -> Result<PathBuf> {
    for candidate in start.ancestors() {
        if candidate.join("dowai_project.json").is_file() {
            return Ok(candidate.to_path_buf());
        }
    }
    Err(DowaiError::Config(
        "No dowai_project.json found in the current directory or its parents; use --project-dir"
            .to_owned(),
    ))
}

fn find_config_root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|candidate| {
            candidate.join("dowai_project.json").is_file()
                || candidate.join("workspace.json").is_file()
        })
        .map(Path::to_path_buf)
        .unwrap_or_else(|| start.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{
        dispatch, dispatch_session, parse_vars, Command, PlanArgs, SessionCommand, ValidateArgs,
    };
    use dowai_core::{SessionLimits, SessionStore};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestProject(PathBuf);

    impl TestProject {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock is after Unix epoch")
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("dowai-cli-test-{}-{nonce}", std::process::id()));
            fs::create_dir_all(root.join("models")).expect("create models directory");
            fs::write(
                root.join("dowai_project.json"),
                r#"{"schema_version":2,"name":"fixture","models_dir":"models"}"#,
            )
            .expect("write project configuration");
            Self(root)
        }

        fn write_model(&self, source: &str) {
            fs::write(self.0.join("models/model.sql"), source).expect("write model");
        }
    }

    impl Drop for TestProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn vars_must_be_a_json_object() {
        assert!(parse_vars(Some(r#"{"flag":true}"#)).is_ok());
        assert!(parse_vars(Some(r#"[true]"#)).is_err());
        assert!(parse_vars(Some("not-json")).is_err());
    }

    #[test]
    fn plan_uses_vars_to_validate_sql_templates() {
        let project = TestProject::new();
        project.write_model("{% if var('enabled') %}select 1{% endif %}");

        let result = dispatch(Command::Plan(PlanArgs {
            targets: vec!["model.sql".to_owned()],
            manual: false,
            vars: Some(r#"{"enabled":true}"#.to_owned()),
            project_dir: Some(project.0.clone()),
            json: true,
        }))
        .expect("provided vars should validate the model template");

        assert!(result.is_some());
    }

    #[test]
    fn plan_fails_for_template_variables_missing_from_vars_and_definitions() {
        let project = TestProject::new();
        project.write_model("{% if var('enabled') %}select 1{% endif %}");

        let error = dispatch(Command::Plan(PlanArgs {
            targets: vec!["model.sql".to_owned()],
            manual: false,
            vars: None,
            project_dir: Some(project.0.clone()),
            json: true,
        }))
        .expect_err("missing strict variables should fail planning");

        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
    }

    #[test]
    fn plan_rejects_undeclared_external_sources_before_execution() {
        let project = TestProject::new();
        project.write_model("select * from {{ source('sales', 'orders') }}");

        let error = dispatch(Command::Plan(PlanArgs {
            targets: vec!["model.sql".to_owned()],
            manual: false,
            vars: None,
            project_dir: Some(project.0.clone()),
            json: true,
        }))
        .expect_err("undeclared sources must fail planning");

        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
    }

    #[test]
    fn plan_does_not_parse_python_models_as_sql_templates() {
        let project = TestProject::new();
        fs::write(
            project.0.join("models/model.py"),
            "def model(env):\n    return \"{{ var('enabled') }}\"\n",
        )
        .expect("write Python model");

        let result = dispatch(Command::Plan(PlanArgs {
            targets: vec!["model.py".to_owned()],
            manual: false,
            vars: None,
            project_dir: Some(project.0.clone()),
            json: true,
        }))
        .expect("planning Python models must not invoke SQL template rendering");

        assert!(result.is_some());
    }

    #[test]
    fn validate_rejects_undeclared_external_sources() {
        let project = TestProject::new();
        project.write_model("select * from {{ source('sales', 'orders') }}");

        let error = dispatch(Command::Validate(ValidateArgs {
            path: Some(project.0.clone()),
            strict: false,
            json: true,
        }))
        .expect_err("validation must resolve all declared source macros");

        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
    }

    #[test]
    fn validate_allows_variables_that_are_supplied_only_at_run_time() {
        let project = TestProject::new();
        project.write_model("{% if var('enabled') %}select 1{% endif %}");

        let result = dispatch(Command::Validate(ValidateArgs {
            path: Some(project.0.clone()),
            strict: false,
            json: true,
        }))
        .expect("static validation should not require run-time values")
        .expect("JSON validation report");

        assert_eq!(result["valid"], true);
    }

    #[test]
    fn session_commands_show_metadata_without_variable_values() {
        let project = TestProject::new();
        let store = SessionStore::with_limits(
            project.0.join("state/sessions.sqlite3"),
            SessionLimits::default(),
        );
        let started = dispatch_session(
            SessionCommand::Start {
                project_dir: Some(project.0.clone()),
                json: true,
            },
            &store,
        )
        .expect("start session")
        .expect("JSON response");
        let session_id = started["session_id"]
            .as_str()
            .expect("session id")
            .to_owned();
        store
            .set_variable(
                &session_id,
                "secret_token",
                &serde_json::json!("must not appear"),
            )
            .expect("set session variable");

        let shown = dispatch_session(
            SessionCommand::Show {
                session_id: session_id.clone(),
                json: true,
            },
            &store,
        )
        .expect("show session")
        .expect("JSON response");

        assert_eq!(shown["variable_names"], serde_json::json!(["secret_token"]));
        assert_eq!(shown["variable_revision"], serde_json::json!(1));
        assert_eq!(
            shown["variable_revisions"],
            serde_json::json!({ "secret_token": 1 })
        );
        assert!(shown.to_string().find("must not appear").is_none());
        let ended = dispatch_session(
            SessionCommand::End {
                session_id,
                json: true,
            },
            &store,
        )
        .expect("end session")
        .expect("JSON response");
        assert_eq!(ended["status"], "ENDED");
    }
}
