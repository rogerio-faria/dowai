//! Execução de plugin em subprocesso (ENT-107, plano §9.1 a §9.3, AC006, AC017).
//!
//! A execução só acontece sobre uma [`ResolvedCall`], isto é, depois de `resolve_call` ter
//! aprovado o alvo. O entrypoint `module:function` roda em um interpretador Python isolado
//! (`-I`), em grupo de processos próprio, com o `timeout_seconds` do manifesto. Entrada e saída
//! são JSON. Nada que o plugin escreva fora do resultado (stderr, texto livre) é copiado para
//! erros, AUDIT ou logs: os erros usam códigos `DOWAI_PLUGIN_*` e mensagens fixas.
//!
//! Idempotência: quando o manifesto exige chave (ou declara efeitos externos) e há chave, a
//! execução é registrada na outbox (ENT-051). Chave já confirmada devolve `DUPLICATE` sem
//! executar. Chave em `FAILED`, `UNCERTAIN` ou `SENT` nunca é reexecutada.
//!
//! Permissões (ENT-056): o manifesto declara `network`, `filesystem` e `secrets`. Uma permissão
//! só é concedida quando o manifesto a declara **e** a política efetiva (`plugins.allow_network`,
//! `plugins.allow_filesystem`, `plugins.allow_secrets`) a permite. Declarado e não permitido recusa
//! a execução com `DOWAI_POLICY_DENIED` antes de qualquer código do plugin rodar. Não declarado é
//! negado no worker. O bloqueio no worker é cooperativo (hook de auditoria do Python): reduz o
//! acesso acidental ou não autorizado de código do plugin, mas não é sandbox contra código hostil.
//! Stderr do plugin nunca é capturado; stdout é limitado por `plugins.max_output_bytes`.

use crate::python_runtime::{
    apply_worker_environment, attach_process_tree, configure_process_group, terminate_descendants,
    terminate_process_tree,
};
use dowai_core::plugin_manifest::resolve_entrypoint_file;
use dowai_core::plugin_registry::{resolved_call_json, ResolvedCall};
use dowai_core::state::OutboxStatus;
use dowai_core::{sha256_hex, AuditEvent, AuditLog, DowaiError, Result, RunStore};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Versão do formato JSON do resultado de `plugins run`.
pub const PLUGIN_RUN_SCHEMA_VERSION: u32 = 1;

/// Tamanho máximo da entrada JSON enviada ao plugin.
const MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Tamanho máximo padrão do resultado JSON lido do stdout (`plugins.max_output_bytes`).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_KEY_CHARS: usize = 64;
const MAX_OUTBOX_KEY_CHARS: usize = 128;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const READER_GRACE: Duration = Duration::from_secs(2);

/// Códigos de saída do worker Python. Qualquer outro valor (inclusive sinal) é falha incerta.
const EXIT_PLUGIN_RAISED: i32 = 10;
const EXIT_UNSERIALIZABLE: i32 = 12;

/// Política efetiva de execução de plugins, lida da configuração pelo chamador.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPolicy {
    pub allow_network: bool,
    pub allow_filesystem: bool,
    pub allow_secrets: bool,
    pub max_output_bytes: usize,
}

impl Default for PluginPolicy {
    /// Padrão da configuração: tudo negado e limite de saída de 1 MiB.
    fn default() -> Self {
        Self {
            allow_network: false,
            allow_filesystem: false,
            allow_secrets: false,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// Permissões concedidas a uma execução: declaradas no manifesto e permitidas pela política.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Grants {
    network: bool,
    filesystem: bool,
}

impl Grants {
    fn as_worker_arg(self) -> String {
        format!(
            "network={},filesystem={}",
            u8::from(self.network),
            u8::from(self.filesystem)
        )
    }
}

/// Decide as permissões. Declarado sem política recusa antes de qualquer processo existir.
/// `banco` não é categoria do schema de manifesto e por isso é tratado como `network`.
fn resolve_grants(
    manifest: &dowai_core::plugin_manifest::PluginManifest,
    policy: &PluginPolicy,
) -> Result<Grants> {
    let declares = &manifest.permissions;
    let denied = |category: &str| {
        Err(DowaiError::Policy(format!(
            "Plugin {} declares {category} permission, which the effective policy does not allow",
            manifest.id
        )))
    };
    if !declares.network.is_empty() && !policy.allow_network {
        return denied("network (plugins.allow_network = false)");
    }
    if !declares.filesystem.is_empty() && !policy.allow_filesystem {
        return denied("filesystem (plugins.allow_filesystem = false)");
    }
    if !declares.secrets.is_empty() && !policy.allow_secrets {
        return denied("secrets (plugins.allow_secrets = false)");
    }
    Ok(Grants {
        network: !declares.network.is_empty() && policy.allow_network,
        filesystem: !declares.filesystem.is_empty() && policy.allow_filesystem,
    })
}

/// Segredos declarados pelo manifesto e liberados pela política (`plugins.allow_secrets`), lidos
/// de `DOWAI_SECRET_<NOME>`. Ausência não é erro aqui: o plugin decide se precisa do valor.
/// Declarado sem política já foi recusado em `resolve_grants`.
fn resolve_secrets(
    manifest: &dowai_core::plugin_manifest::PluginManifest,
    policy: &PluginPolicy,
) -> serde_json::Map<String, Value> {
    let mut resolved = serde_json::Map::new();
    if !policy.allow_secrets {
        return resolved;
    }
    for name in &manifest.permissions.secrets {
        let variable = format!("DOWAI_SECRET_{}", name.to_ascii_uppercase());
        if let Ok(value) = std::env::var(variable) {
            resolved.insert(name.clone(), json!(value));
        }
    }
    resolved
}

/// Acrescenta os segredos ao payload sob a chave reservada `__secrets__`, que sempre é
/// sobrescrita: uma entrada do chamador com essa chave não vira segredo.
fn with_secrets(input: &Value, secrets: serde_json::Map<String, Value>) -> Value {
    let mut payload = input.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert("__secrets__".to_owned(), Value::Object(secrets));
    }
    payload
}

/// Worker: lê a entrada de stdin, instala o hook de permissões antes de qualquer código do
/// plugin, carrega o módulo validado pelo caminho canônico e chama a função com a entrada como
/// kwargs. O resultado é o único conteúdo escrito no stdout real.
const WORKER: &str = r#"
import sys, os, json, importlib.util, inspect
path, module_name, function, plugin_dir, grants = sys.argv[1:6]
allow_network = "network=1" in grants.split(",")
allow_filesystem = "filesystem=1" in grants.split(",")
real_stdout = sys.stdout
sys.stdout = open(os.devnull, "w")
sys.dont_write_bytecode = True
NETWORK_EVENTS = {
    "socket.connect", "socket.sendto", "socket.sendmsg", "socket.bind",
    "socket.getaddrinfo", "socket.gethostbyname", "socket.gethostbyaddr",
}
FILE_MUTATION_EVENTS = {
    "os.remove", "os.rename", "os.replace", "os.mkdir", "os.rmdir",
    "os.link", "os.symlink", "os.truncate", "shutil.rmtree",
}
WRITE_FLAGS = os.O_WRONLY | os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_TRUNC
PROCESS_EVENTS = {
    "subprocess.Popen", "os.system", "os.posix_spawn", "os.spawn", "os.exec",
}
def _guard(event, args):
    if event in PROCESS_EVENTS or event.startswith("os.exec") or event.startswith("os.spawn"):
        raise PermissionError("process creation is not permitted for plugins")
    if event in NETWORK_EVENTS and not allow_network:
        raise PermissionError("network access is not permitted for this plugin")
    if allow_filesystem:
        return
    if event in FILE_MUTATION_EVENTS:
        raise PermissionError("filesystem changes are not permitted for this plugin")
    if event == "open":
        mode = args[1]
        flags = args[2]
        writes = isinstance(mode, str) and any(c in mode for c in "wxa+")
        writes = writes or (isinstance(flags, int) and bool(flags & WRITE_FLAGS))
        if writes:
            raise PermissionError("filesystem writes are not permitted for this plugin")
sys.addaudithook(_guard)
try:
    payload = json.loads(sys.stdin.read())
    secrets = payload.pop("__secrets__", {})
    sys.path.append(plugin_dir)
    spec = importlib.util.spec_from_file_location(module_name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    target = getattr(module, function)
    kwargs = payload
    if "secrets" in inspect.signature(target).parameters:
        kwargs = dict(payload, secrets=secrets)
    result = target(**kwargs)
    if inspect.iscoroutine(result):
        import asyncio
        result = asyncio.run(result)
except BaseException as error:
    # 13 = efeito possivelmente aplicado (ver EXIT_UNCERTAIN no plugin email); demais = 10.
    os._exit(13 if getattr(error, "dowai_exit_code", None) == 13 else 10)
try:
    data = json.dumps(result, allow_nan=False)
except BaseException:
    os._exit(12)
real_stdout.write(data)
real_stdout.flush()
"#;

/// Uma execução pedida: a chamada já resolvida, o interpretador do projeto, a entrada e,
/// opcionalmente, a chave de idempotência. `store` e `audit` são injetados pelo chamador.
pub struct PluginExecution<'a> {
    pub call: &'a ResolvedCall<'a>,
    pub interpreter: &'a Path,
    /// Deve ser um objeto JSON; vira os argumentos nomeados do entrypoint.
    pub input: Value,
    pub idempotency_key: Option<&'a str>,
    /// Outbox (ENT-051). Necessário somente quando a chave é rastreada.
    pub store: Option<&'a RunStore>,
    /// AUDIT. Falha de escrita nunca altera o resultado; vira `audit_recorded: false`.
    pub audit: Option<&'a AuditLog>,
    /// Política efetiva de permissões e limites (ENT-056).
    pub policy: &'a PluginPolicy,
}

/// Falha após o início do subprocesso. `uncertain` indica que o efeito externo pode ter sido
/// aplicado (timeout, queda, saída inválida); sem isso a falha é definitiva.
struct RunFailure {
    error: DowaiError,
    uncertain: bool,
}

fn plugin_error(code: &'static str, message: &str) -> DowaiError {
    DowaiError::Plugin {
        code,
        message: message.to_owned(),
    }
}

/// Executa o plugin e registra a execução no AUDIT. Retorna o envelope JSON do resultado.
pub fn execute_plugin(execution: &PluginExecution<'_>) -> Result<Value> {
    let outcome = run(execution);
    let (result, error_code) = match &outcome {
        Ok(value) => (
            value["status"].as_str().unwrap_or("SUCCEEDED").to_owned(),
            None,
        ),
        Err(error) => (
            if is_refusal(error.code()) {
                "REFUSED"
            } else {
                "FAILED"
            }
            .to_owned(),
            Some(error.code().to_owned()),
        ),
    };
    let recorded = record_audit(execution, &result, error_code);
    outcome.map(|mut value| {
        value["audit_recorded"] = json!(recorded);
        value
    })
}

/// Recusas antes de qualquer código do plugin rodar.
fn is_refusal(code: &str) -> bool {
    matches!(
        code,
        "DOWAI_PLUGIN_INPUT_INVALID"
            | "DOWAI_PLUGIN_IDEMPOTENCY_KEY_REQUIRED"
            | "DOWAI_PLUGIN_IDEMPOTENCY_IN_PROGRESS"
            | "DOWAI_PLUGIN_IDEMPOTENCY_UNCERTAIN"
            | "DOWAI_PLUGIN_IDEMPOTENCY_FAILED"
            | "DOWAI_POLICY_PATH"
            | "DOWAI_POLICY_DENIED"
            | "DOWAI_CONFIG_INVALID"
    )
}

fn record_audit(execution: &PluginExecution<'_>, result: &str, error_code: Option<String>) -> bool {
    let Some(log) = execution.audit else {
        return false;
    };
    let call = execution.call;
    let manifest = &call.plugin.manifest;
    let execution_id = Uuid::new_v4().to_string();
    let mut event = AuditEvent::new("plugin.executed", &execution_id);
    event.invocation_id = Some(execution_id.clone());
    event.attempt_id = Some(execution_id);
    event.plugin_id = Some(manifest.id.clone());
    event.plugin_method = Some(call.method.clone());
    event.plugin_version = Some(manifest.version.clone());
    event.manifest_sha256 = Some(manifest.manifest_sha256.clone());
    if let Some(shadowed) = call.shadowed {
        event.replaced_origin = Some(shadowed.origin.as_str().to_owned());
        event.replaced_plugin_version = Some(shadowed.manifest.version.clone());
        event.replaced_manifest_sha256 = Some(shadowed.manifest.manifest_sha256.clone());
    }
    event.result = Some(result.to_owned());
    event.error_code = error_code;
    log.append(&event).is_ok()
}

fn run(execution: &PluginExecution<'_>) -> Result<Value> {
    let call = execution.call;
    let manifest = &call.plugin.manifest;
    let target = format!("{}.{}", manifest.id, call.method);

    validate_input(&execution.input)?;
    let key = validate_key(execution.idempotency_key)?;
    if manifest.idempotency_key_required && key.is_none() {
        return Err(plugin_error(
            "DOWAI_PLUGIN_IDEMPOTENCY_KEY_REQUIRED",
            &format!("Plugin {target} requires an idempotency key"),
        ));
    }

    // O módulo é resolvido dentro do diretório do plugin antes de qualquer processo existir.
    let plugin_dir = manifest
        .manifest_path
        .parent()
        .ok_or_else(|| DowaiError::Path("Plugin manifest has no directory".to_owned()))?;
    let grants = resolve_grants(manifest, execution.policy)?;
    // O payload inclui os segredos já resolvidos. Ele segue só por stdin: nunca por argumentos,
    // ambiente, erro, AUDIT ou resultado.
    let input = validate_input(&with_secrets(
        &execution.input,
        resolve_secrets(manifest, execution.policy),
    ))?;
    let (source, module, function) =
        resolve_entrypoint_file(plugin_dir, &call.method, &call.entrypoint)?;
    let working_dir = std::fs::canonicalize(plugin_dir)?;

    let tracked = key.filter(|_| manifest.idempotency_key_required || manifest.side_effects);
    // A tracked call may leave its outbox entry `SENT`; the shared hold keeps startup recovery
    // from marking it `UNCERTAIN` while the call is still in progress.
    let _activity = match (tracked, execution.store) {
        (Some(_), Some(store)) => Some(store.activity_guard()?),
        _ => None,
    };
    let outbox = match tracked {
        Some(key) => {
            let store = execution.store.ok_or_else(|| {
                DowaiError::State("The state directory is unavailable for the outbox".to_owned())
            })?;
            let outbox_key = outbox_key(&manifest.id, &call.method, key);
            if gate(store, &outbox_key, &target)? == Gate::Duplicate {
                return Ok(envelope(call, "DUPLICATE", key, Value::Null));
            }
            Some((store, outbox_key))
        }
        None => None,
    };

    let launched = launch(
        execution.interpreter,
        &working_dir,
        &source,
        &module,
        &function,
        &input,
        &target,
        manifest.timeout_seconds,
        grants,
        execution.policy.max_output_bytes,
    );
    match launched {
        Ok(result) => {
            if let Some((store, outbox_key)) = &outbox {
                store.outbox_transition(outbox_key, OutboxStatus::Confirmed, None)?;
            }
            Ok(envelope(call, "SUCCEEDED", key.unwrap_or_default(), result))
        }
        Err(failure) => {
            if let Some((store, outbox_key)) = &outbox {
                let status = if failure.uncertain {
                    OutboxStatus::Uncertain
                } else {
                    OutboxStatus::Failed
                };
                store.outbox_transition(outbox_key, status, Some(failure.error.code()))?;
            }
            Err(failure.error)
        }
    }
}

fn envelope(call: &ResolvedCall<'_>, status: &str, key: &str, result: Value) -> Value {
    let mut value = resolved_call_json(call);
    value["schema_version"] = json!(PLUGIN_RUN_SCHEMA_VERSION);
    value["status"] = json!(status);
    value["idempotency_key"] = if key.is_empty() {
        Value::Null
    } else {
        json!(key)
    };
    value["result"] = result;
    value
}

fn validate_input(input: &Value) -> Result<Vec<u8>> {
    if !input.is_object() {
        return Err(plugin_error(
            "DOWAI_PLUGIN_INPUT_INVALID",
            "Plugin input must be a JSON object",
        ));
    }
    let bytes = serde_json::to_vec(input).map_err(|_| {
        plugin_error(
            "DOWAI_PLUGIN_INPUT_INVALID",
            "Plugin input is not serializable",
        )
    })?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(plugin_error(
            "DOWAI_PLUGIN_INPUT_INVALID",
            "Plugin input exceeds the size limit",
        ));
    }
    Ok(bytes)
}

fn validate_key(key: Option<&str>) -> Result<Option<&str>> {
    let Some(key) = key else {
        return Ok(None);
    };
    let valid = !key.is_empty()
        && key.chars().count() <= MAX_KEY_CHARS
        && key.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-' | b'/')
        });
    if valid {
        Ok(Some(key))
    } else {
        Err(plugin_error(
            "DOWAI_PLUGIN_INPUT_INVALID",
            "The idempotency key must have 1 to 64 characters from [A-Za-z0-9._:/-]",
        ))
    }
}

/// Chave da outbox: escopo por plugin e método, para que a mesma chave de usuário em plugins
/// diferentes não colida. Cai para hash quando excede o limite da outbox.
fn outbox_key(plugin_id: &str, method: &str, key: &str) -> String {
    let composed = format!("plugin:{plugin_id}.{method}:{key}");
    if composed.chars().count() <= MAX_OUTBOX_KEY_CHARS {
        composed
    } else {
        format!("plugin:sha256:{}", sha256_hex(composed.as_bytes()))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Run,
    Duplicate,
}

/// Registra a chave na outbox e decide se o efeito pode rodar. Só `PENDING` é reivindicado.
fn gate(store: &RunStore, outbox_key: &str, target: &str) -> Result<Gate> {
    let entry = store.outbox_enqueue(outbox_key, "plugin", target)?;
    match entry.status {
        OutboxStatus::Confirmed => Ok(Gate::Duplicate),
        OutboxStatus::Pending => {
            if store.outbox_claim(outbox_key)? {
                Ok(Gate::Run)
            } else {
                Err(plugin_error(
                    "DOWAI_PLUGIN_IDEMPOTENCY_IN_PROGRESS",
                    "Another execution holds this idempotency key",
                ))
            }
        }
        OutboxStatus::Sent => Err(plugin_error(
            "DOWAI_PLUGIN_IDEMPOTENCY_IN_PROGRESS",
            "Another execution holds this idempotency key",
        )),
        OutboxStatus::Uncertain => Err(plugin_error(
            "DOWAI_PLUGIN_IDEMPOTENCY_UNCERTAIN",
            "A previous execution with this key has an uncertain outcome; it will not be repeated",
        )),
        OutboxStatus::Failed => Err(plugin_error(
            "DOWAI_PLUGIN_IDEMPOTENCY_FAILED",
            "A previous execution with this key failed; use a new key to try again",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn launch(
    interpreter: &Path,
    working_dir: &Path,
    source: &Path,
    module: &str,
    function: &str,
    input: &[u8],
    target: &str,
    timeout_seconds: u64,
    grants: Grants,
    max_output_bytes: usize,
) -> std::result::Result<Value, RunFailure> {
    let failed = |message: String| RunFailure {
        error: plugin_error("DOWAI_PLUGIN_FAILED", &message),
        uncertain: false,
    };
    let mut command = Command::new(interpreter);
    command
        .arg("-I")
        .arg("-c")
        .arg(WORKER)
        .arg(source)
        .arg(module)
        .arg(function)
        .arg(working_dir)
        .arg(grants.as_worker_arg())
        .current_dir(working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    apply_worker_environment(&mut command);
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| failed(format!("Could not start the interpreter for {target}")))?;
    let tree = match attach_process_tree(&mut child) {
        Ok(tree) => tree,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(failed(format!(
                "Could not supervise the process of {target}"
            )));
        }
    };
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = terminate_process_tree(&tree, &mut child);
        let _ = child.wait();
        return Err(failed(format!(
            "Could not connect to the process of {target}"
        )));
    };

    let payload = input.to_vec();
    let writer = thread::spawn(move || {
        // O processo pode encerrar sem ler a entrada; isso é tratado pelo código de saída.
        let _ = stdin.write_all(&payload);
    });
    let oversized = Arc::new(AtomicBool::new(false));
    let oversized_flag = Arc::clone(&oversized);
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(1);
    thread::spawn(move || {
        let mut collected = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    collected.extend_from_slice(&chunk[..read]);
                    if collected.len() > max_output_bytes {
                        oversized_flag.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        let _ = sender.send(collected);
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let status = loop {
        if oversized.load(Ordering::SeqCst) {
            let _ = terminate_process_tree(&tree, &mut child);
            let _ = child.wait();
            let _ = writer.join();
            return Err(output_invalid(target));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = terminate_process_tree(&tree, &mut child);
                let _ = child.wait();
                let _ = writer.join();
                return Err(RunFailure {
                    error: plugin_error(
                        "DOWAI_PLUGIN_TIMEOUT",
                        &format!("Plugin {target} exceeded its timeout of {timeout_seconds}s"),
                    ),
                    uncertain: true,
                });
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => {
                let _ = terminate_process_tree(&tree, &mut child);
                let _ = child.wait();
                let _ = writer.join();
                return Err(RunFailure {
                    error: plugin_error(
                        "DOWAI_PLUGIN_FAILED",
                        &format!("Could not monitor the process of {target}"),
                    ),
                    uncertain: true,
                });
            }
        }
    };
    // Nenhum descendente do plugin sobrevive à execução (sem órfãos).
    let _ = terminate_descendants(&tree);
    let _ = writer.join();

    match status.code() {
        Some(0) => {}
        Some(EXIT_PLUGIN_RAISED) => {
            return Err(failed(format!("Plugin {target} raised an error")));
        }
        Some(EXIT_UNSERIALIZABLE) => return Err(output_invalid(target)),
        _ => {
            return Err(RunFailure {
                error: plugin_error(
                    "DOWAI_PLUGIN_FAILED",
                    &format!("Plugin {target} terminated abnormally"),
                ),
                uncertain: true,
            })
        }
    }
    let bytes = receiver
        .recv_timeout(READER_GRACE)
        .map_err(|_| output_invalid(target))?;
    if bytes.len() > max_output_bytes {
        return Err(output_invalid(target));
    }
    serde_json::from_slice::<Value>(&bytes).map_err(|_| output_invalid(target))
}

fn output_invalid(target: &str) -> RunFailure {
    RunFailure {
        error: plugin_error(
            "DOWAI_PLUGIN_OUTPUT_INVALID",
            &format!("Plugin {target} returned an invalid or oversized result"),
        ),
        uncertain: true,
    }
}
