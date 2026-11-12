//! Descoberta, dispatcher único e listagem de plugins (ENT-053, ENT-054, plano §6.1, §9.1, §9.2,
//! AC006, AC007).
//!
//! Plugins são descobertos em `<projeto>/plugins/<id>/plugin.json` (escopo `project`) e em
//! `<workspace>/plugins/<id>/plugin.json` (escopo `global`). A descoberta só lê manifestos
//! (via [`read_manifest`]); nenhum código do plugin é importado ou executado.
//!
//! Toda chamada de plugin, tipada ou genérica, DEVE ser resolvida por [`resolve_call`]. Ainda
//! não existe execução de plugin nem API Python `env.plugins`; quando existirem, passarão por
//! este mesmo caminho. Override: o plugin local com o mesmo ID substitui o global inteiro, somente
//! com `plugins.allow_local_override = true` (ENT-054).

use crate::plugin_manifest::{read_manifest, PluginManifest};
use crate::{DowaiError, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Versão do formato JSON de `plugins list` e `plugins info`.
pub const PLUGIN_LIST_SCHEMA_VERSION: u32 = 1;

const PROJECT_FILE: &str = "dowai_project.json";
const WORKSPACE_FILE: &str = "workspace.json";
const PLUGINS_DIR: &str = "plugins";

/// Escopo pedido pelo usuário (`--scope project|global|all`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginScope {
    Project,
    Global,
    All,
}

impl PluginScope {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "project" => Ok(Self::Project),
            "global" => Ok(Self::Global),
            "all" => Ok(Self::All),
            other => Err(DowaiError::Config(format!(
                "--scope inválido: {other}; use project, global ou all"
            ))),
        }
    }

    fn includes(self, origin: PluginOrigin) -> bool {
        matches!(
            (self, origin),
            (Self::All, _)
                | (Self::Project, PluginOrigin::Project)
                | (Self::Global, PluginOrigin::Global)
        )
    }
}

/// Origem de um plugin descoberto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginOrigin {
    Project,
    Global,
}

impl PluginOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

/// Plugin com manifesto válido e origem registrada.
#[derive(Clone, Debug)]
pub struct DiscoveredPlugin {
    pub origin: PluginOrigin,
    pub manifest: PluginManifest,
}

/// Entrada de plugin recusada na descoberta (manifesto inválido, symlink, ausência de `plugin.json`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedPlugin {
    pub origin: PluginOrigin,
    pub path: PathBuf,
    pub message: String,
}

/// Resultado da descoberta: plugins válidos e entradas recusadas, em ordem determinística.
#[derive(Clone, Debug, Default)]
pub struct PluginCatalog {
    pub plugins: Vec<DiscoveredPlugin>,
    pub rejected: Vec<RejectedPlugin>,
}

/// Diretórios `plugins/` do projeto e do workspace a partir de um diretório de trabalho.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginRoots {
    pub project: Option<PathBuf>,
    pub global: Option<PathBuf>,
}

/// Localiza o projeto (ancestral com `dowai_project.json`) e o workspace (ancestral com
/// `workspace.json`). Ausentes ficam `None` e o escopo correspondente fica vazio.
pub fn plugin_roots_from(start: &Path) -> PluginRoots {
    PluginRoots {
        project: start
            .ancestors()
            .find(|candidate| candidate.join(PROJECT_FILE).is_file())
            .map(|root| root.join(PLUGINS_DIR)),
        global: start
            .ancestors()
            .find(|candidate| candidate.join(WORKSPACE_FILE).is_file())
            .map(|root| root.join(PLUGINS_DIR)),
    }
}

/// Descobre plugins nos escopos pedidos. ID duplicado no mesmo nível é erro (plano §9.2).
pub fn discover(roots: &PluginRoots, scope: PluginScope) -> Result<PluginCatalog> {
    let mut catalog = PluginCatalog::default();
    let levels = [
        (PluginOrigin::Project, roots.project.as_deref()),
        (PluginOrigin::Global, roots.global.as_deref()),
    ];
    for (origin, root) in levels {
        if !scope.includes(origin) {
            continue;
        }
        if let Some(root) = root {
            discover_level(root, origin, &mut catalog)?;
        }
    }
    Ok(catalog)
}

fn discover_level(root: &Path, origin: PluginOrigin, catalog: &mut PluginCatalog) -> Result<()> {
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if root_metadata.file_type().is_symlink() {
        return Err(DowaiError::Path(format!(
            "diretório de plugins {} é symlink e foi recusado",
            root.display()
        )));
    }
    if !root_metadata.is_dir() {
        return Ok(());
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(root)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();

    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    for path in entries {
        let hidden = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'));
        if hidden {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            catalog.rejected.push(RejectedPlugin {
                origin,
                path,
                message: "symlink recusado como diretório de plugin".to_owned(),
            });
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        match read_manifest(&path) {
            Ok(manifest) => {
                if let Some(first) = seen.get(&manifest.id) {
                    return Err(DowaiError::Config(format!(
                        "plugin {} duplicado no escopo {}: {} e {}",
                        manifest.id,
                        origin.as_str(),
                        first.display(),
                        path.display()
                    )));
                }
                seen.insert(manifest.id.clone(), path);
                catalog.plugins.push(DiscoveredPlugin { origin, manifest });
            }
            Err(error) => catalog.rejected.push(RejectedPlugin {
                origin,
                path,
                message: error.to_string(),
            }),
        }
    }
    Ok(())
}

/// Chamada resolvida: plugin efetivo, método, alvo `module:function` declarado no manifesto e,
/// quando houve override, o plugin global substituído integralmente (plano §9.2).
#[derive(Debug)]
pub struct ResolvedCall<'a> {
    pub plugin: &'a DiscoveredPlugin,
    pub method: String,
    pub entrypoint: String,
    /// Plugin global com o mesmo ID, substituído pelo plugin local. `None` sem override.
    pub shadowed: Option<&'a DiscoveredPlugin>,
}

/// Dispatcher único de resolução. Aceita `namespace.method` (ex.: `email.send`).
///
/// Recusa alvo malformado, plugin inexistente e método não declarado. Quando o mesmo ID existe
/// no escopo `project` e no `global`, o local substitui o global **inteiro** (sem mesclar
/// métodos) somente se `allow_local_override` for `true` (`plugins.allow_local_override`, ENT-054);
/// caso contrário a chamada é recusada com `DOWAI_POLICY_DENIED`.
pub fn resolve_call<'a>(
    catalog: &'a PluginCatalog,
    target: &str,
    allow_local_override: bool,
) -> Result<ResolvedCall<'a>> {
    let (namespace, method) = target
        .split_once('.')
        .filter(|(namespace, method)| !namespace.is_empty() && !method.is_empty())
        .ok_or_else(|| {
            DowaiError::Config(format!(
                "alvo de plugin inválido: {target}; use namespace.method"
            ))
        })?;

    let candidates = find_plugins(catalog, namespace);
    let (plugin, shadowed) = match candidates.as_slice() {
        [] => {
            return Err(DowaiError::Config(format!(
                "plugin não encontrado: {namespace}"
            )))
        }
        [only] => (*only, None),
        [_, _] => {
            let local = candidates
                .iter()
                .copied()
                .find(|plugin| plugin.origin == PluginOrigin::Project);
            let global = candidates
                .iter()
                .copied()
                .find(|plugin| plugin.origin == PluginOrigin::Global);
            match (local, global) {
                (Some(local), Some(global)) if allow_local_override => (local, Some(global)),
                (Some(_), Some(_)) => {
                    return Err(DowaiError::Policy(format!(
                        "override local do plugin {namespace} recusado: plugins.allow_local_override = false"
                    )))
                }
                _ => {
                    return Err(DowaiError::Config(format!(
                        "plugin {namespace} duplicado no mesmo escopo"
                    )))
                }
            }
        }
        _ => {
            return Err(DowaiError::Config(format!(
                "plugin {namespace} duplicado no mesmo escopo"
            )))
        }
    };

    let entrypoint = plugin
        .manifest
        .entrypoints
        .get(method)
        .ok_or_else(|| DowaiError::Config(format!("método {method} não declarado em {namespace}")))?
        .clone();

    Ok(ResolvedCall {
        plugin,
        method: method.to_owned(),
        entrypoint,
        shadowed,
    })
}

/// Registro JSON de uma resolução: plugin efetivo e, se houver, o global substituído. Inclui
/// origem, caminho, versão e hash, para que o override fique rastreável.
pub fn resolved_call_json(call: &ResolvedCall<'_>) -> Value {
    json!({
        "target": format!("{}.{}", call.plugin.manifest.id, call.method),
        "entrypoint": call.entrypoint,
        "plugin": plugin_json(call.plugin),
        "override": call.shadowed.map(|shadowed| json!({
            "replaced_origin": shadowed.origin.as_str(),
            "replaced_manifest_path": shadowed.manifest.manifest_path.display().to_string(),
            "replaced_version": shadowed.manifest.version,
            "replaced_manifest_sha256": shadowed.manifest.manifest_sha256,
        })),
    })
}

/// Todos os plugins válidos com o ID informado, em qualquer escopo.
pub fn find_plugins<'a>(catalog: &'a PluginCatalog, id: &str) -> Vec<&'a DiscoveredPlugin> {
    catalog
        .plugins
        .iter()
        .filter(|plugin| plugin.manifest.id == id)
        .collect()
}

/// Representação JSON de um plugin: origem, caminho, versão, hash, métodos e permissões declaradas.
pub fn plugin_json(plugin: &DiscoveredPlugin) -> Value {
    let manifest = &plugin.manifest;
    json!({
        "id": manifest.id,
        "version": manifest.version,
        "origin": plugin.origin.as_str(),
        "manifest_path": manifest.manifest_path.display().to_string(),
        "manifest_sha256": manifest.manifest_sha256,
        "methods": manifest.entrypoints.keys().collect::<Vec<_>>(),
        "entrypoints": manifest.entrypoints,
        "permissions": {
            "network": manifest.permissions.network,
            "filesystem": manifest.permissions.filesystem,
            "secrets": manifest.permissions.secrets,
        },
        "timeout_seconds": manifest.timeout_seconds,
        "side_effects": manifest.side_effects,
        "idempotency_key_required": manifest.idempotency_key_required,
    })
}

/// Saída JSON de `plugins list --scope ...`.
pub fn plugin_list_json(scope: PluginScope, catalog: &PluginCatalog) -> Value {
    json!({
        "schema_version": PLUGIN_LIST_SCHEMA_VERSION,
        "scope": match scope {
            PluginScope::Project => "project",
            PluginScope::Global => "global",
            PluginScope::All => "all",
        },
        "plugins": catalog.plugins.iter().map(plugin_json).collect::<Vec<_>>(),
        "rejected": catalog.rejected.iter().map(rejected_json).collect::<Vec<_>>(),
    })
}

fn rejected_json(rejected: &RejectedPlugin) -> Value {
    json!({
        "origin": rejected.origin.as_str(),
        "path": rejected.path.display().to_string(),
        "message": rejected.message,
    })
}
