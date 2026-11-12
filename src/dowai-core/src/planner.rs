use crate::templates::{static_sql_references, StaticSqlReference};
use crate::{load_project, render_template, DowaiError, Result, TemplateOptions};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionMode {
    Automatic,
    Manual,
}

impl ExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanNode {
    pub id: String,
    pub path: String,
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionPlan {
    pub schema_version: u32,
    pub execution_mode: String,
    pub nodes: Vec<PlanNode>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
struct Model {
    id: String,
    path: PathBuf,
    dependencies: BTreeSet<String>,
}

pub fn plan(project_root: &Path, targets: &[String], mode: ExecutionMode) -> Result<ExecutionPlan> {
    let root = project_root.canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Project directory is unavailable: {} ({error})",
            project_root.display()
        ))
    })?;
    let project = load_project(&root)?;
    let models_dir = root
        .join(&project.models_dir)
        .canonicalize()
        .map_err(|error| {
            DowaiError::Config(format!(
                "Models directory is unavailable: {} ({error})",
                root.join(&project.models_dir).display()
            ))
        })?;
    ensure_within(&root, &models_dir)?;

    let models = discover_models(&models_dir)?;
    let models_by_id: BTreeMap<String, Model> = models
        .into_iter()
        .map(|model| (model.id.clone(), model))
        .collect();
    if models_by_id.is_empty() && targets.iter().any(|target| target == "all") {
        return Ok(ExecutionPlan {
            schema_version: 2,
            execution_mode: mode.as_str().to_owned(),
            nodes: Vec::new(),
            warnings: Vec::new(),
        });
    }

    let mut models_by_name: HashMap<String, Vec<String>> = HashMap::new();
    for model in models_by_id.values() {
        models_by_name
            .entry(stem(&model.id).to_owned())
            .or_default()
            .push(model.id.clone());
    }

    let mut resolved = BTreeMap::new();
    for model in models_by_id.values() {
        let body = fs::read_to_string(&model.path)?;
        let dependencies = extract_dependencies(&model.id, &body)?;
        let mut ids = BTreeSet::new();
        for dependency in dependencies {
            let id = resolve_reference(&dependency, &models_by_id, &models_by_name)?;
            ids.insert(id);
        }
        resolved.insert(
            model.id.clone(),
            Model {
                dependencies: ids,
                ..model.clone()
            },
        );
    }

    let target_ids = resolve_targets(
        targets,
        &root,
        &models_dir,
        &project.models_dir,
        &models_by_id,
        &models_by_name,
    )?;
    let selected = match mode {
        ExecutionMode::Automatic => closure(&target_ids, &resolved)?,
        ExecutionMode::Manual => target_ids.iter().cloned().collect(),
    };
    let ordered_ids = match mode {
        ExecutionMode::Automatic => topological_order(&target_ids, &selected, &resolved)?,
        ExecutionMode::Manual => target_ids.clone(),
    };
    let warnings = manual_warnings(&ordered_ids, &selected, &resolved, mode);
    let nodes = ordered_ids
        .iter()
        .map(|id| {
            let model = &resolved[id];
            PlanNode {
                id: model.id.clone(),
                path: model.id.clone(),
                depends_on: model.dependencies.iter().cloned().collect(),
            }
        })
        .collect();

    Ok(ExecutionPlan {
        schema_version: 2,
        execution_mode: mode.as_str().to_owned(),
        nodes,
        warnings,
    })
}

/// Validate SQL templates in plan order using a private, ephemeral variable copy.
///
/// The function discards rendered SQL and changes. It never writes session state.
pub fn validate_plan_templates(
    project_root: &Path,
    execution_plan: &ExecutionPlan,
    variables: &BTreeMap<String, Value>,
    options: &TemplateOptions,
) -> Result<()> {
    let root = project_root.canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Project directory is unavailable: {} ({error})",
            project_root.display()
        ))
    })?;
    let project = load_project(&root)?;
    let models_dir = root
        .join(project.models_dir)
        .canonicalize()
        .map_err(|error| DowaiError::Config(format!("Models directory is unavailable: {error}")))?;
    ensure_within(&root, &models_dir)?;

    let mut ephemeral_variables = variables.clone();
    for node in &execution_plan.nodes {
        if Path::new(&node.path)
            .extension()
            .and_then(|ext| ext.to_str())
            != Some("sql")
        {
            continue;
        }
        let source = read_planned_model(&root, &node.path)?;
        let rendered = render_template(&source, &ephemeral_variables, options)
            .map_err(|error| DowaiError::Template(format!("{}: {}", node.path, error.message())))?;
        ephemeral_variables = rendered.variables;
    }
    Ok(())
}

pub fn read_planned_model(project_root: &Path, relative_path: &str) -> Result<String> {
    let root = project_root.canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Project directory is unavailable: {} ({error})",
            project_root.display()
        ))
    })?;
    let project = load_project(&root)?;
    let models_dir = root
        .join(project.models_dir)
        .canonicalize()
        .map_err(|error| DowaiError::Config(format!("Models directory is unavailable: {error}")))?;
    ensure_within(&root, &models_dir)?;
    let relative = Path::new(relative_path);
    if relative.is_absolute()
        || relative.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(DowaiError::Path(format!(
            "Plan model path is not a safe relative path: {relative_path}"
        )));
    }
    let model_path = models_dir.join(relative).canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Planned model is unavailable: {relative_path} ({error})"
        ))
    })?;
    ensure_within(&models_dir, &model_path)?;
    fs::read_to_string(&model_path).map_err(Into::into)
}

fn discover_models(models_dir: &Path) -> Result<Vec<Model>> {
    let mut paths = Vec::new();
    collect_model_paths(models_dir, models_dir, &mut paths)?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let relative = path.strip_prefix(models_dir).map_err(|_| {
                DowaiError::Path(format!(
                    "Model escaped models directory: {}",
                    path.display()
                ))
            })?;
            Ok(Model {
                id: path_string(relative),
                path,
                dependencies: BTreeSet::new(),
            })
        })
        .collect()
}

fn collect_model_paths(root: &Path, current: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_model_paths(root, &path, paths)?;
        } else if file_type.is_file() && supported_extension(&path) {
            let canonical = path.canonicalize()?;
            ensure_within(root, &canonical)?;
            paths.push(canonical);
        }
    }
    Ok(())
}

fn resolve_targets(
    targets: &[String],
    project_root: &Path,
    models_dir: &Path,
    models_dir_relative: &Path,
    models: &BTreeMap<String, Model>,
    models_by_name: &HashMap<String, Vec<String>>,
) -> Result<Vec<String>> {
    if targets.is_empty() {
        return Err(DowaiError::Model(
            "At least one model target is required".to_owned(),
        ));
    }
    if targets.len() == 1 && targets[0] == "all" {
        return Ok(models.keys().cloned().collect());
    }
    if targets.iter().any(|target| target == "all") {
        return Err(DowaiError::Model(
            "'all' cannot be combined with individual targets".to_owned(),
        ));
    }

    let mut ids = Vec::new();
    for target in targets {
        let path = Path::new(target);
        if path.is_absolute() || path.components().count() > 1 {
            let candidate = if path.is_absolute() {
                path.to_path_buf()
            } else if path.starts_with(models_dir_relative) {
                project_root.join(path)
            } else {
                models_dir.join(path)
            };
            let canonical = candidate
                .canonicalize()
                .map_err(|_| DowaiError::Model(format!("Model not found: {target}")))?;
            ensure_within(models_dir, &canonical)?;
            let id =
                path_string(canonical.strip_prefix(models_dir).map_err(|_| {
                    DowaiError::Path(format!("Model path escaped project: {target}"))
                })?);
            if !models.contains_key(&id) || !supported_extension(&canonical) {
                return Err(DowaiError::Model(format!(
                    "Unsupported or undiscovered model: {target}"
                )));
            }
            ids.push(id);
            continue;
        }

        let target_path = Path::new(target);
        if target_path.extension().is_some() {
            let id = path_string(target_path);
            if models.contains_key(&id) {
                ids.push(id);
                continue;
            }
        } else {
            let matching = ["sql", "py"]
                .iter()
                .map(|extension| format!("{target}.{extension}"))
                .filter(|id| models.contains_key(id))
                .collect::<Vec<_>>();
            if matching.len() == 1 {
                ids.push(matching[0].clone());
                continue;
            }
            if matching.len() > 1 {
                return Err(DowaiError::Model(format!(
                    "Ambiguous model name '{target}'; use a file extension"
                )));
            }
        }

        let name = Path::new(target)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(target);
        match models_by_name.get(name).map(Vec::as_slice) {
            Some([id]) => ids.push(id.clone()),
            Some(_) => {
                return Err(DowaiError::Model(format!(
                    "Ambiguous model ID '{target}'; use its relative path"
                )))
            }
            None => return Err(DowaiError::Model(format!("Model not found: {target}"))),
        }
    }
    Ok(ids)
}

fn extract_dependencies(model_id: &str, body: &str) -> Result<Vec<String>> {
    if is_python_model(model_id) {
        return extract_python_dependencies(model_id, body);
    }
    let references =
        static_sql_references(body, TemplateOptions::default().max_nesting).map_err(|error| {
            match error {
                DowaiError::Model(message) => DowaiError::Model(format!("{model_id}: {message}")),
                DowaiError::Template(message) => {
                    DowaiError::Template(format!("{model_id}: {message}"))
                }
                other => other,
            }
        })?;
    Ok(references
        .into_iter()
        .filter_map(|reference| match reference {
            StaticSqlReference::Ref(name) => Some(name),
            StaticSqlReference::Source { .. } => None,
        })
        .collect())
}

fn is_python_model(model_id: &str) -> bool {
    Path::new(model_id)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("py"))
}

fn extract_python_dependencies(model_id: &str, body: &str) -> Result<Vec<String>> {
    let tokens = tokenize(body);
    let mut dependencies = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let call_name = match &tokens[index] {
            Token::Identifier(name)
                if (name == "ref" || name == "depends_on")
                    && is_env_method_call(&tokens, index) =>
            {
                Some(name)
            }
            _ => None,
        };
        if let Some(call_name) =
            call_name.filter(|_| matches!(tokens.get(index + 1), Some(Token::Symbol('('))))
        {
            match (
                tokens.get(index + 2),
                tokens.get(index + 3),
                tokens.get(index + 4),
            ) {
                (
                    Some(Token::StringLiteral(name)),
                    Some(Token::Symbol(')')),
                    _,
                ) if !name.trim().is_empty() => {
                    dependencies.push(name.clone());
                    index += 4;
                }
                _ => {
                    return Err(DowaiError::Model(format!(
                        "Dynamic or unsupported {call_name}() in {model_id}; references must use a string literal"
                    )))
                }
            }
        }
        index += 1;
    }
    Ok(dependencies)
}

fn is_env_method_call(tokens: &[Token], method_index: usize) -> bool {
    method_index >= 2
        && matches!(
            (&tokens[method_index - 2], &tokens[method_index - 1]),
            (Token::Identifier(object), Token::Symbol('.')) if object == "env"
        )
        && (method_index < 3 || !matches!(tokens[method_index - 3], Token::Symbol('.')))
}

#[derive(Debug)]
enum Token {
    Identifier(String),
    StringLiteral(String),
    Symbol(char),
}

fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if starts_with(bytes, index, b"--") || bytes[index] == b'#' {
            index = skip_line(bytes, index);
            continue;
        }
        if starts_with(bytes, index, b"/*") {
            index = skip_block(bytes, index + 2, b"*/");
            continue;
        }
        if starts_with(bytes, index, b"{#") {
            index = skip_block(bytes, index + 2, b"#}");
            continue;
        }

        let current = bytes[index];
        if current == b'\'' || current == b'"' || current == b'`' {
            let quote = current;
            let triple = quote != b'`'
                && index + 2 < bytes.len()
                && bytes[index + 1] == quote
                && bytes[index + 2] == quote;
            let delimiter_size = if triple { 3 } else { 1 };
            index += delimiter_size;
            let mut value = String::new();
            while index < bytes.len() {
                if triple
                    && index + 2 < bytes.len()
                    && bytes[index] == quote
                    && bytes[index + 1] == quote
                    && bytes[index + 2] == quote
                {
                    index += 3;
                    break;
                }
                if !triple && bytes[index] == quote {
                    index += 1;
                    break;
                }
                if bytes[index] == b'\\' && index + 1 < bytes.len() {
                    index += 1;
                }
                let character = source[index..]
                    .chars()
                    .next()
                    .expect("index is within source");
                value.push(character);
                index += character.len_utf8();
            }
            tokens.push(Token::StringLiteral(value));
            continue;
        }
        if current.is_ascii_alphabetic() || current == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            tokens.push(Token::Identifier(source[start..index].to_owned()));
            continue;
        }

        let character = source[index..]
            .chars()
            .next()
            .expect("index is within source");
        tokens.push(Token::Symbol(character));
        index += character.len_utf8();
    }
    tokens
}

fn starts_with(source: &[u8], index: usize, token: &[u8]) -> bool {
    source
        .get(index..index + token.len())
        .is_some_and(|slice| slice == token)
}

fn skip_line(source: &[u8], mut index: usize) -> usize {
    while index < source.len() && source[index] != b'\n' {
        index += 1;
    }
    index
}

fn skip_block(source: &[u8], mut index: usize, end: &[u8]) -> usize {
    while index < source.len() {
        if starts_with(source, index, end) {
            return index + end.len();
        }
        index += 1;
    }
    source.len()
}

fn resolve_reference(
    reference: &str,
    models: &BTreeMap<String, Model>,
    models_by_name: &HashMap<String, Vec<String>>,
) -> Result<String> {
    let normalized = reference.replace('\\', "/");
    let with_extension = if Path::new(&normalized).extension().is_some() {
        normalized.clone()
    } else {
        format!("{normalized}.sql")
    };
    if models.contains_key(&with_extension) {
        return Ok(with_extension);
    }
    let name = Path::new(&normalized)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(&normalized);
    match models_by_name.get(name).map(Vec::as_slice) {
        Some([id]) => Ok(id.clone()),
        Some(_) => Err(DowaiError::Model(format!(
            "Ambiguous ref('{reference}'); use a unique model name"
        ))),
        None => Err(DowaiError::Model(format!(
            "Model reference not found: ref('{reference}')"
        ))),
    }
}

fn closure(targets: &[String], models: &BTreeMap<String, Model>) -> Result<BTreeSet<String>> {
    fn visit(
        id: &str,
        models: &BTreeMap<String, Model>,
        selected: &mut BTreeSet<String>,
    ) -> Result<()> {
        let model = models
            .get(id)
            .ok_or_else(|| DowaiError::Model(format!("Model not found: {id}")))?;
        if selected.insert(id.to_owned()) {
            for dependency in &model.dependencies {
                visit(dependency, models, selected)?;
            }
        }
        Ok(())
    }

    let mut selected = BTreeSet::new();
    for target in targets {
        visit(target, models, &mut selected)?;
    }
    Ok(selected)
}

fn topological_order(
    targets: &[String],
    selected: &BTreeSet<String>,
    models: &BTreeMap<String, Model>,
) -> Result<Vec<String>> {
    fn visit(
        id: &str,
        selected: &BTreeSet<String>,
        models: &BTreeMap<String, Model>,
        visiting: &mut Vec<String>,
        visited: &mut HashSet<String>,
        ordered: &mut Vec<String>,
    ) -> Result<()> {
        if visited.contains(id) {
            return Ok(());
        }
        if let Some(position) = visiting.iter().position(|item| item == id) {
            let mut cycle = visiting[position..].to_vec();
            cycle.push(id.to_owned());
            return Err(DowaiError::Cycle(cycle.join(" -> ")));
        }
        visiting.push(id.to_owned());
        if let Some(model) = models.get(id) {
            for dependency in &model.dependencies {
                if selected.contains(dependency) {
                    visit(dependency, selected, models, visiting, visited, ordered)?;
                }
            }
        }
        visiting.pop();
        visited.insert(id.to_owned());
        ordered.push(id.to_owned());
        Ok(())
    }

    let mut visiting = Vec::new();
    let mut visited = HashSet::new();
    let mut ordered = Vec::new();
    for target in targets {
        visit(
            target,
            selected,
            models,
            &mut visiting,
            &mut visited,
            &mut ordered,
        )?;
    }
    Ok(ordered)
}

fn manual_warnings(
    ordered_ids: &[String],
    selected: &BTreeSet<String>,
    models: &BTreeMap<String, Model>,
    mode: ExecutionMode,
) -> Vec<String> {
    if mode != ExecutionMode::Manual {
        return Vec::new();
    }
    let positions = ordered_ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut warnings = BTreeSet::new();
    for id in ordered_ids {
        if let Some(model) = models.get(id) {
            for dependency in &model.dependencies {
                if !selected.contains(dependency)
                    || positions.get(dependency.as_str()) > positions.get(id.as_str())
                {
                    warnings.insert(format!(
                        "{id} depends on {dependency}; manual mode does not reorder or run predecessors"
                    ));
                }
            }
        }
    }
    warnings.into_iter().collect()
}

fn ensure_within(root: &Path, path: &Path) -> Result<()> {
    if !path.starts_with(root) {
        return Err(DowaiError::Path(format!(
            "Path escapes the authorized project directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn supported_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("sql" | "py")
    )
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn stem(model_id: &str) -> &str {
    Path::new(model_id)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(model_id)
}
