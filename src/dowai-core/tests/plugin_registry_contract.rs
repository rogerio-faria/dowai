//! ENT-053: descoberta de plugins por escopo, dispatcher único de resolução e listagem.
//!
//! Garante que project/global são registrados com origem, que manifesto inválido não oculta os
//! demais, que ID duplicado no mesmo nível é erro, que a descoberta não executa código do plugin
//! e que `namespace.method` resolve por um único caminho (recusando ambiguidade entre escopos).

use dowai_core::plugin_registry::{
    discover, find_plugins, plugin_list_json, plugin_roots_from, resolve_call, PluginRoots,
    PluginScope,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dowai-plugin-registry-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create scratch directory");
        Self(root)
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

fn manifest(id: &str) -> String {
    format!(
        r#"{{
  "schema_version": 2,
  "id": "{id}",
  "version": "1.0.0",
  "entrypoints": {{ "send": "sender:send", "ping": "sender:ping" }},
  "permissions": {{ "network": [], "filesystem": [], "secrets": [] }},
  "timeout_seconds": 30,
  "side_effects": false,
  "idempotency_key_required": false
}}"#
    )
}

/// Cria `<root>/<dir>/plugin.json` e um módulo que, se fosse importado, gravaria uma sentinela.
fn write_plugin(root: &Path, dir: &str, id: &str, sentinel: &Path) -> PathBuf {
    let plugin_dir = root.join(dir);
    fs::create_dir_all(&plugin_dir).expect("plugin dir");
    fs::write(plugin_dir.join("plugin.json"), manifest(id)).expect("manifest");
    let code = format!(
        "open({:?}, 'w').write('executed')\ndef send(to, subject, message):\n    return None\ndef ping():\n    return None\n",
        sentinel.display().to_string()
    );
    fs::write(plugin_dir.join("sender.py"), code).expect("module");
    plugin_dir
}

/// Workspace com `workspace.json`, projeto `fat` e plugins nos dois escopos.
fn workspace_layout(scratch: &Scratch) -> (PathBuf, PathBuf, PluginRoots) {
    let workspace = scratch.path().join("ws");
    let project = workspace.join("projects").join("fat");
    fs::create_dir_all(&project).expect("project dir");
    fs::write(workspace.join("workspace.json"), "{}").expect("workspace");
    fs::write(project.join("dowai_project.json"), "{}").expect("project");
    let roots = PluginRoots {
        project: Some(project.join("plugins")),
        global: Some(workspace.join("plugins")),
    };
    (workspace, project, roots)
}

#[test]
fn list_reports_origin_hash_and_declared_methods_for_each_scope() {
    let scratch = Scratch::new("origin");
    let sentinel = scratch.path().join("sentinel.txt");
    let (workspace, project, roots) = workspace_layout(&scratch);
    write_plugin(&workspace.join("plugins"), "email", "email", &sentinel);
    write_plugin(&project.join("plugins"), "rest", "rest", &sentinel);

    let catalog = discover(&roots, PluginScope::All).expect("discover");
    assert!(catalog.rejected.is_empty());
    assert_eq!(catalog.plugins.len(), 2);

    let email = find_plugins(&catalog, "email");
    assert_eq!(email.len(), 1);
    assert_eq!(email[0].origin.as_str(), "global");
    assert_eq!(email[0].manifest.manifest_sha256.len(), 64);
    assert!(email[0].manifest.entrypoints.contains_key("send"));

    let rest = find_plugins(&catalog, "rest");
    assert_eq!(rest[0].origin.as_str(), "project");

    let json = plugin_list_json(PluginScope::All, &catalog);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["scope"], "all");
    assert_eq!(json["plugins"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        json["plugins"][0]["methods"],
        serde_json::json!(["ping", "send"])
    );
    assert!(json["plugins"][0]["manifest_sha256"].as_str().is_some());
}

#[test]
fn scope_filter_restricts_discovery_to_the_requested_level() {
    let scratch = Scratch::new("scope");
    let sentinel = scratch.path().join("sentinel.txt");
    let (workspace, project, roots) = workspace_layout(&scratch);
    write_plugin(&workspace.join("plugins"), "email", "email", &sentinel);
    write_plugin(&project.join("plugins"), "rest", "rest", &sentinel);

    let only_project = discover(&roots, PluginScope::Project).expect("project");
    assert_eq!(only_project.plugins.len(), 1);
    assert_eq!(only_project.plugins[0].manifest.id, "rest");

    let only_global = discover(&roots, PluginScope::Global).expect("global");
    assert_eq!(only_global.plugins.len(), 1);
    assert_eq!(only_global.plugins[0].manifest.id, "email");

    assert!(PluginScope::parse("workspace").is_err());
}

#[test]
fn invalid_manifest_is_reported_without_hiding_valid_plugins() {
    let scratch = Scratch::new("invalid");
    let sentinel = scratch.path().join("sentinel.txt");
    let (workspace, _project, roots) = workspace_layout(&scratch);
    write_plugin(&workspace.join("plugins"), "email", "email", &sentinel);
    let broken = workspace.join("plugins").join("broken");
    fs::create_dir_all(&broken).expect("broken dir");
    fs::write(broken.join("plugin.json"), r#"{"id": "broken"}"#).expect("broken manifest");

    let catalog = discover(&roots, PluginScope::All).expect("discover");
    assert_eq!(catalog.plugins.len(), 1);
    assert_eq!(catalog.plugins[0].manifest.id, "email");
    assert_eq!(catalog.rejected.len(), 1);
    assert!(catalog.rejected[0].path.ends_with("broken"));
    assert_eq!(catalog.rejected[0].origin.as_str(), "global");
}

#[test]
fn duplicate_id_in_the_same_level_is_an_error() {
    let scratch = Scratch::new("duplicate");
    let sentinel = scratch.path().join("sentinel.txt");
    let (workspace, _project, roots) = workspace_layout(&scratch);
    write_plugin(&workspace.join("plugins"), "email-a", "email", &sentinel);
    write_plugin(&workspace.join("plugins"), "email-b", "email", &sentinel);

    let error = discover(&roots, PluginScope::All).expect_err("duplicate must fail");
    assert!(error.to_string().contains("duplicado"), "{error}");
}

#[test]
fn discovery_never_imports_or_executes_plugin_code() {
    let scratch = Scratch::new("no-exec");
    let sentinel = scratch.path().join("sentinel.txt");
    let (workspace, _project, roots) = workspace_layout(&scratch);
    write_plugin(&workspace.join("plugins"), "email", "email", &sentinel);

    discover(&roots, PluginScope::All).expect("discover");
    resolve_call(
        &discover(&roots, PluginScope::All).expect("discover"),
        "email.send",
    )
    .expect("resolve");
    assert!(
        !sentinel.exists(),
        "plugin module must not run during discovery"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_plugin_directory_is_rejected() {
    let scratch = Scratch::new("symlink");
    let sentinel = scratch.path().join("sentinel.txt");
    let outside = scratch.path().join("outside");
    write_plugin(&outside, "email", "email", &sentinel);
    let (workspace, _project, roots) = workspace_layout(&scratch);
    let plugins = workspace.join("plugins");
    fs::create_dir_all(&plugins).expect("plugins dir");
    std::os::unix::fs::symlink(outside.join("email"), plugins.join("email")).expect("symlink");

    let catalog = discover(&roots, PluginScope::All).expect("discover");
    assert!(catalog.plugins.is_empty());
    assert_eq!(catalog.rejected.len(), 1);
    assert!(catalog.rejected[0].message.contains("symlink"));
}

#[test]
fn dispatcher_resolves_namespace_method_to_the_declared_entrypoint() {
    let scratch = Scratch::new("resolve");
    let sentinel = scratch.path().join("sentinel.txt");
    let (workspace, _project, roots) = workspace_layout(&scratch);
    write_plugin(&workspace.join("plugins"), "email", "email", &sentinel);
    let catalog = discover(&roots, PluginScope::All).expect("discover");

    let call = resolve_call(&catalog, "email.send").expect("resolve");
    assert_eq!(call.plugin.manifest.id, "email");
    assert_eq!(call.method, "send");
    assert_eq!(call.entrypoint, "sender:send");
}

#[test]
fn dispatcher_refuses_unknown_undeclared_malformed_and_ambiguous_targets() {
    let scratch = Scratch::new("refuse");
    let sentinel = scratch.path().join("sentinel.txt");
    let (workspace, project, roots) = workspace_layout(&scratch);
    write_plugin(&workspace.join("plugins"), "email", "email", &sentinel);
    let catalog = discover(&roots, PluginScope::All).expect("discover");

    let unknown = resolve_call(&catalog, "missing.send").expect_err("unknown plugin");
    assert!(unknown.to_string().contains("não encontrado"));

    let undeclared = resolve_call(&catalog, "email.delete").expect_err("undeclared method");
    assert!(undeclared.to_string().contains("não declarado"));

    for malformed in ["email", ".send", "email."] {
        assert!(
            resolve_call(&catalog, malformed).is_err(),
            "{malformed} must be refused"
        );
    }

    // Mesmo ID nos dois escopos: sem ENT-054, a chamada é recusada em vez de escolher um.
    write_plugin(&project.join("plugins"), "email-local", "email", &sentinel);
    let both = discover(&roots, PluginScope::All).expect("discover both scopes");
    let ambiguous = resolve_call(&both, "email.send").expect_err("ambiguous");
    assert!(ambiguous.to_string().contains("ENT-054"));
}

#[test]
fn plugin_roots_are_found_from_a_nested_project_directory() {
    let scratch = Scratch::new("roots");
    let (workspace, project, _roots) = workspace_layout(&scratch);
    let nested = project.join("models").join("sub");
    fs::create_dir_all(&nested).expect("nested");

    let roots = plugin_roots_from(&nested);
    assert_eq!(roots.project, Some(project.join("plugins")));
    assert_eq!(roots.global, Some(workspace.join("plugins")));
}

#[test]
fn missing_plugin_roots_yield_an_empty_catalog() {
    let scratch = Scratch::new("empty");
    let roots = plugin_roots_from(scratch.path());
    let catalog = discover(&roots, PluginScope::All).expect("discover");
    assert!(catalog.plugins.is_empty());
    assert!(catalog.rejected.is_empty());
    let json: Value = plugin_list_json(PluginScope::All, &catalog);
    assert_eq!(json["plugins"], serde_json::json!([]));
}
