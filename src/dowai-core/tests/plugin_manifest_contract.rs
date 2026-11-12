//! ENT-052: validação de manifesto de plugin e assinaturas por análise estática.
//!
//! Garante que o manifesto é validado contra `specs/plugin.schema.json`, que a descoberta não
//! importa nem executa código do plugin (sentinela em disco) e que resolução de módulo não
//! escapa o diretório do plugin.

use dowai_core::plugin_manifest::{
    extract_signatures, read_manifest, validate_manifest_value, ParameterKind,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn specs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../specs")
}

fn fixtures(group: &str) -> Vec<PathBuf> {
    let directory = specs_dir().join("fixtures/plugin").join(group);
    let mut files: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("fixtures {group}: {error}"))
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    files
}

/// Diretório temporário exclusivo para um teste; removido ao final pelo `Drop`.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dowai-plugin-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("scratch dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dir");
    }
    fs::write(path, content).expect("write fixture file");
}

fn manifest_json(entrypoints: &str) -> String {
    format!(
        r#"{{
  "schema_version": 2,
  "id": "email",
  "version": "1.0.0",
  "entrypoints": {entrypoints},
  "permissions": {{ "network": ["mail_gateway"], "filesystem": [], "secrets": ["mail_credentials"] }},
  "timeout_seconds": 30,
  "side_effects": true,
  "idempotency_key_required": true
}}"#
    )
}

#[test]
fn every_valid_fixture_is_accepted() {
    let files = fixtures("valid");
    assert!(!files.is_empty());
    for file in files {
        let value: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        validate_manifest_value(&value)
            .unwrap_or_else(|error| panic!("{} deveria ser aceito: {error}", file.display()));
    }
}

#[test]
fn every_invalid_fixture_is_refused_with_config_code() {
    let files = fixtures("invalid");
    assert!(!files.is_empty());
    for file in files {
        let value: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        let error = validate_manifest_value(&value)
            .err()
            .unwrap_or_else(|| panic!("{} deveria ser recusado", file.display()));
        assert_eq!(error.code(), "DOWAI_CONFIG_INVALID", "{}", file.display());
    }
}

#[test]
fn invalid_manifest_on_disk_is_refused_before_any_entrypoint_is_read() {
    let scratch = Scratch::new("invalid-manifest");
    write(
        &scratch.path().join("plugin.json"),
        r#"{"schema_version": 1}"#,
    );
    let error = read_manifest(scratch.path()).expect_err("manifesto inválido");
    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
}

#[test]
fn discovery_never_imports_or_runs_plugin_code() {
    let scratch = Scratch::new("sentinel");
    let sentinel = scratch.path().join("IMPORTED.flag");
    // Se o módulo fosse importado ou executado, este código criaria o sentinela e falharia.
    let module = format!(
        "open({:?}, 'w').write('executed')\nraise RuntimeError('plugin executado na descoberta')\n\ndef send(to, subject):\n    return None\n",
        sentinel.display().to_string()
    );
    write(&scratch.path().join("sender.py"), &module);
    write(
        &scratch.path().join("plugin.json"),
        &manifest_json(r#"{ "send": "sender:send" }"#),
    );

    let manifest = read_manifest(scratch.path()).expect("manifesto válido");
    let signatures = extract_signatures(scratch.path(), &manifest).expect("assinaturas");

    assert!(!sentinel.exists(), "descoberta não pode executar o plugin");
    assert_eq!(signatures.len(), 1);
    assert_eq!(signatures[0].method, "send");
}

#[test]
fn last_module_level_definition_wins_and_package_is_resolved() {
    let scratch = Scratch::new("signature");
    write(
        &scratch.path().join("pkg/mail.py"),
        "async def send(to, subject):\n    pass\n\ndef send(to, subject):\n    pass\n",
    );
    write(&scratch.path().join("pkg/__init__.py"), "");
    write(
        &scratch.path().join("plugin.json"),
        &manifest_json(r#"{ "send": "pkg.mail:send" }"#),
    );

    let manifest = read_manifest(scratch.path()).expect("manifesto válido");
    let signatures = extract_signatures(scratch.path(), &manifest).expect("assinaturas");
    let signature = &signatures[0];

    assert!(!signature.is_async, "a última definição prevalece");
    assert_eq!(signature.source_file, PathBuf::from("pkg/mail.py"));
    let kinds: Vec<(&str, ParameterKind)> = signature
        .parameters
        .iter()
        .map(|parameter| (parameter.name.as_str(), parameter.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("to", ParameterKind::PositionalOrKeyword),
            ("subject", ParameterKind::PositionalOrKeyword),
        ]
    );
}

#[test]
fn async_entrypoint_with_full_parameter_list() {
    let scratch = Scratch::new("async");
    write(
        &scratch.path().join("sender.py"),
        "async def send(to: list, /, subject, *args, body: str = '', **extra) -> None:\n    pass\n",
    );
    write(
        &scratch.path().join("plugin.json"),
        &manifest_json(r#"{ "send": "sender:send" }"#),
    );
    let manifest = read_manifest(scratch.path()).expect("manifesto válido");
    let signature = &extract_signatures(scratch.path(), &manifest).unwrap()[0];

    assert!(signature.is_async);
    assert!(signature.returns_annotated);
    let parameters = &signature.parameters;
    assert_eq!(parameters.len(), 5);
    assert_eq!(parameters[0].kind, ParameterKind::PositionalOnly);
    assert!(parameters[0].annotated && !parameters[0].has_default);
    assert_eq!(parameters[1].kind, ParameterKind::PositionalOrKeyword);
    assert!(!parameters[1].annotated);
    assert_eq!(parameters[2].kind, ParameterKind::VarPositional);
    assert_eq!(parameters[3].kind, ParameterKind::KeywordOnly);
    assert!(parameters[3].annotated && parameters[3].has_default);
    assert_eq!(parameters[4].kind, ParameterKind::VarKeyword);
}

#[test]
fn missing_function_is_refused() {
    let scratch = Scratch::new("missing-function");
    write(
        &scratch.path().join("sender.py"),
        "def other():\n    pass\n",
    );
    write(
        &scratch.path().join("plugin.json"),
        &manifest_json(r#"{ "send": "sender:send" }"#),
    );
    let manifest = read_manifest(scratch.path()).expect("manifesto válido");
    let error = extract_signatures(scratch.path(), &manifest).expect_err("função ausente");
    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
}

#[cfg(unix)]
#[test]
fn module_symlink_escaping_plugin_directory_is_refused() {
    let scratch = Scratch::new("symlink");
    let outside = scratch.path().join("outside.py");
    write(&outside, "def send():\n    pass\n");
    let plugin = scratch.path().join("plugin");
    fs::create_dir_all(&plugin).unwrap();
    std::os::unix::fs::symlink(&outside, plugin.join("sender.py")).unwrap();
    write(
        &plugin.join("plugin.json"),
        &manifest_json(r#"{ "send": "sender:send" }"#),
    );
    let manifest = read_manifest(&plugin).expect("manifesto válido");
    let error = extract_signatures(&plugin, &manifest).expect_err("symlink externo");
    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
}

#[cfg(unix)]
#[test]
fn manifest_symlink_is_refused() {
    let scratch = Scratch::new("manifest-symlink");
    let real = scratch.path().join("real.json");
    write(&real, &manifest_json(r#"{ "send": "sender:send" }"#));
    let plugin = scratch.path().join("plugin");
    fs::create_dir_all(&plugin).unwrap();
    std::os::unix::fs::symlink(&real, plugin.join("plugin.json")).unwrap();
    let error = read_manifest(&plugin).expect_err("manifesto como symlink");
    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
}
