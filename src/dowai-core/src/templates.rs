use crate::{DowaiError, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// Rendering limits and behavior for the safe SQL template subset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateOptions {
    /// Maximum number of nested conditional blocks allowed.
    pub max_nesting: usize,
    /// Whether a missing variable without a default is an error.
    pub strict_undefined: bool,
    /// Whether the DOWAI aliases `{{ if }}`, `{{ elif }}`, `{{ else }}` and `{{ endif }}`
    /// are accepted as equivalents of the `{% ... %}` conditional delimiters.
    pub allow_dowai_if_alias: bool,
}

impl Default for TemplateOptions {
    fn default() -> Self {
        Self {
            max_nesting: 32,
            strict_undefined: true,
            allow_dowai_if_alias: true,
        }
    }
}

/// Rendered SQL and the final values visible to template conditions.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateRender {
    /// SQL with conditional sections selected; variable values are not interpolated.
    pub sql: String,
    /// Initial and defined variables after rendering.
    pub variables: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Materialization {
    Table,
    View,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderedSqlModel {
    pub sql: String,
    pub variables: BTreeMap<String, Value>,
    pub materialization: Materialization,
    /// Python files named by `{{ run("arq.py") }}` at the end of the model (ENT-124), in order.
    /// They are executed only after the SQL is materialized (ENT-126); rendering runs nothing.
    pub post_run_targets: Vec<String>,
}

/// Render the supported conditional SQL template subset without evaluating SQL values.
///
/// Conditions can read JSON values with `var('name')`, optionally supplying a default,
/// and may compare the result with a JSON scalar. Bare values use JSON-style truthiness:
/// null, false, zero, and empty strings, arrays, or objects are false. `def_var` sets a
/// value only when it is absent. Other SQL text, including `{{ ref(...) }}`, is copied
/// verbatim.
pub fn render_template(
    source: &str,
    variables: &BTreeMap<String, Value>,
    options: &TemplateOptions,
) -> Result<TemplateRender> {
    let tokens = tokenize(source, false, options.allow_dowai_if_alias)?;
    let mut parser = Parser {
        tokens,
        position: 0,
        max_nesting: options.max_nesting,
    };
    let (nodes, stop) = parser.parse_sequence(0)?;
    if stop.is_some() {
        return Err(template_error("unexpected conditional closing tag"));
    }

    let mut context = variables.clone();
    let mut sql = String::new();
    render_nodes(&nodes, &mut context, &mut sql, options.strict_undefined)?;
    Ok(TemplateRender {
        sql,
        variables: context,
    })
}

#[derive(Debug)]
enum Node {
    Text(String),
    Define {
        name: String,
        value: Value,
    },
    Conditional {
        branches: Vec<(Condition, Vec<Node>)>,
        otherwise: Vec<Node>,
    },
}

#[derive(Clone, Debug)]
struct Condition {
    name: String,
    default: Option<Value>,
    comparison: Option<(Comparison, Value)>,
}

#[derive(Clone, Copy, Debug)]
enum Comparison {
    Equal,
    NotEqual,
}

#[derive(Clone, Debug)]
enum Token {
    Text(String),
    Define { name: String, value: Value },
    If(Condition),
    Elif(Condition),
    Else,
    EndIf,
}

fn template_error(message: impl Into<String>) -> DowaiError {
    DowaiError::Template(message.into())
}

/// `plan_errors` reports malformed `ref()`/`source()`/`config()` macros as plan
/// errors (`DOWAI_PLAN_INVALID`); the renderer keeps `DOWAI_TEMPLATE_INVALID`.
fn tokenize(source: &str, plan_errors: bool, allow_dowai_if_alias: bool) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut position = 0;
    while position < source.len() {
        let next = ["{%", "{{", "{#"]
            .iter()
            .filter_map(|opening| {
                source[position..]
                    .find(opening)
                    .map(|offset| position + offset)
            })
            .min();
        let next = match next {
            Some(index) => index,
            None => {
                push_text(&mut tokens, &source[position..]);
                break;
            }
        };
        push_text(&mut tokens, &source[position..next]);
        if source[next..].starts_with("{#") {
            let content_start = next + 2;
            let Some(close_offset) = source[content_start..].find("#}") else {
                return Err(template_error("template comment is missing '#}'"));
            };
            position = content_start + close_offset + 2;
            continue;
        }
        let is_alias = source[next..].starts_with("{{");
        let opening_len = 2;
        let close = if is_alias { "}}" } else { "%}" };
        let content_start = next + opening_len;
        let Some(close_offset) = source[content_start..].find(close) else {
            return Err(template_error(format!(
                "template expression is missing '{close}'"
            )));
        };
        let close_start = content_start + close_offset;
        let content = source[content_start..close_start].trim();
        let full_end = close_start + close.len();
        match parse_directive(content)? {
            Some(token) if is_alias && !allow_dowai_if_alias && is_conditional_token(&token) => {
                return Err(template_error(format!(
                    "DOWAI alias '{{{{ {content} }}}}' is disabled by templates.allow_dowai_if_alias; use the '{{% ... %}}' delimiters"
                )))
            }
            Some(token) => tokens.push(token),
            None if is_alias && is_static_macro_call(content) => {
                push_text(&mut tokens, &source[next..full_end])
            }
            None if plan_errors && is_alias && is_reference_call(content) => {
                return Err(DowaiError::Model(format!(
                    "invalid reference macro '{content}': ref(), source() and config() require literal string arguments"
                )))
            }
            None => return Err(template_error("unsupported or unsafe template expression")),
        }
        position = full_end;
    }
    Ok(tokens)
}

fn is_conditional_token(token: &Token) -> bool {
    matches!(
        token,
        Token::If(_) | Token::Elif(_) | Token::Else | Token::EndIf
    )
}

fn push_text(tokens: &mut Vec<Token>, text: &str) {
    if !text.is_empty() {
        if let Some(Token::Text(previous)) = tokens.last_mut() {
            previous.push_str(text);
        } else {
            tokens.push(Token::Text(text.to_owned()));
        }
    }
}

fn is_reference_call(expression: &str) -> bool {
    ["ref(", "source(", "config(", "run("]
        .iter()
        .any(|prefix| expression.starts_with(prefix))
}

fn is_static_macro_call(expression: &str) -> bool {
    parse_static_macro(expression).is_ok()
}

enum StaticMacro {
    Ref(String),
    Source(String, String),
    Config(BTreeMap<String, Value>),
    Run(String),
}

fn parse_static_macro(expression: &str) -> Result<StaticMacro> {
    let mut cursor = Cursor::new(expression);
    let name = cursor.parse_name()?;
    if !matches!(name.as_str(), "ref" | "source" | "config" | "run") {
        return Err(template_error("unsupported static SQL macro"));
    }
    cursor.expect_char('(')?;
    let mut positional = Vec::new();
    let mut named = BTreeMap::new();
    loop {
        cursor.skip_whitespace();
        if cursor.consume_char(')') {
            break;
        }
        let key = if cursor
            .peek_char()
            .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        {
            let start = cursor.position;
            let key = cursor.parse_name()?;
            cursor.skip_whitespace();
            if cursor.consume_char('=') {
                Some(key)
            } else {
                cursor.position = start;
                None
            }
        } else {
            None
        };
        let value = cursor.parse_scalar()?;
        if let Some(key) = key {
            if named.insert(key, value).is_some() {
                return Err(template_error("duplicate named SQL macro argument"));
            }
        } else {
            positional.push(value);
        }
        cursor.skip_whitespace();
        if cursor.consume_char(')') {
            break;
        }
        cursor.expect_char(',')?;
        cursor.skip_whitespace();
        if cursor.peek_char() == Some(')') {
            return Err(template_error(
                "SQL macro arguments cannot end with a comma",
            ));
        }
    }
    cursor.expect_end()?;
    match name.as_str() {
        "ref" if positional.len() == 1 && named.is_empty() => positional[0]
            .as_str()
            .map(|reference| StaticMacro::Ref(reference.to_owned()))
            .ok_or_else(|| template_error("ref() requires one string literal")),
        "source" if positional.len() == 2 && named.is_empty() => {
            let source = positional[0]
                .as_str()
                .ok_or_else(|| template_error("source() requires two string literals"))?;
            let table = positional[1]
                .as_str()
                .ok_or_else(|| template_error("source() requires two string literals"))?;
            Ok(StaticMacro::Source(source.to_owned(), table.to_owned()))
        }
        "config" if positional.is_empty() && !named.is_empty() => Ok(StaticMacro::Config(named)),
        "run" if positional.len() == 1 && named.is_empty() => positional[0]
            .as_str()
            .map(|path| StaticMacro::Run(path.to_owned()))
            .ok_or_else(|| template_error("run() requires one string literal")),
        "run" => Err(template_error("run() requires exactly one string literal")),
        "ref" => Err(template_error("ref() requires exactly one string literal")),
        "source" => Err(template_error(
            "source() requires exactly two string literals",
        )),
        "config" => Err(template_error("config() requires named scalar arguments")),
        _ => Err(template_error("unsupported static SQL macro")),
    }
}

/// Render and validate a SQL model, resolve declared refs, and extract the
/// supported `materialized` setting. Relation names must already be quoted
/// safely for the target adapter.
pub fn render_sql_model(
    source: &str,
    variables: &BTreeMap<String, Value>,
    options: &TemplateOptions,
    relations: &BTreeMap<String, String>,
) -> Result<RenderedSqlModel> {
    validate_static_sources(
        source,
        relations,
        options.max_nesting,
        options.allow_dowai_if_alias,
    )?;
    let rendered = render_template(source, variables, options)?;
    let mut sql = String::with_capacity(rendered.sql.len());
    let mut position = 0;
    let mut materialization = Materialization::Table;
    let mut configured_materialization = false;
    let mut post_run_targets: Vec<String> = Vec::new();
    while let Some(opening_offset) = rendered.sql[position..].find("{{") {
        let opening = position + opening_offset;
        let segment = &rendered.sql[position..opening];
        if !post_run_targets.is_empty() && !segment.trim().is_empty() {
            return Err(template_error(
                "run() must be placed at the end of the SQL model, after all SQL",
            ));
        }
        sql.push_str(segment);
        let content_start = opening + 2;
        let close_offset = rendered.sql[content_start..]
            .find("}}")
            .ok_or_else(|| template_error("static SQL macro is missing '}}'"))?;
        let close = content_start + close_offset;
        let macro_call = parse_static_macro(rendered.sql[content_start..close].trim())?;
        if !post_run_targets.is_empty() && !matches!(macro_call, StaticMacro::Run(_)) {
            return Err(template_error(
                "run() must be placed at the end of the SQL model, after all SQL",
            ));
        }
        match macro_call {
            StaticMacro::Run(path) => {
                validate_run_target(&path)?;
                post_run_targets.push(path);
            }
            StaticMacro::Ref(reference) => {
                let relation = relations.get(&reference).ok_or_else(|| {
                    template_error(format!("ref('{reference}') is not in the plan"))
                })?;
                sql.push_str(relation);
            }
            StaticMacro::Source(source, table) => {
                let key = source_relation_key(&source, &table);
                let relation = relations.get(&key).ok_or_else(|| {
                    template_error(format!(
                        "source('{source}', '{table}') is not declared in the project"
                    ))
                })?;
                sql.push_str(relation);
            }
            StaticMacro::Config(arguments) => {
                if configured_materialization {
                    return Err(template_error(
                        "a SQL model may contain only one config() call",
                    ));
                }
                configured_materialization = true;
                if arguments.keys().any(|key| key != "materialized") {
                    return Err(template_error(
                        "only config(materialized='table'|'view') is supported",
                    ));
                }
                let value = arguments
                    .get("materialized")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        template_error("config materialized value must be 'table' or 'view'")
                    })?;
                materialization = match value {
                    "table" => Materialization::Table,
                    "view" => Materialization::View,
                    _ => {
                        return Err(template_error(format!(
                            "unsupported SQL materialization '{value}'"
                        )))
                    }
                };
            }
        }
        position = close + 2;
    }
    let tail = &rendered.sql[position..];
    if !post_run_targets.is_empty() && !tail.trim().is_empty() {
        return Err(template_error(
            "run() must be placed at the end of the SQL model, after all SQL",
        ));
    }
    sql.push_str(tail);
    Ok(RenderedSqlModel {
        sql,
        variables: rendered.variables,
        materialization,
        post_run_targets,
    })
}

/// Whether a SQL model source contains a `{{ run(...) }}` macro anywhere, including inside
/// conditional branches. Used to route a run to the executor path (ENT-126) before rendering.
pub fn declares_post_run(source: &str) -> bool {
    let mut position = 0;
    while let Some(opening_offset) = source[position..].find("{{") {
        let content_start = position + opening_offset + 2;
        let Some(close_offset) = source[content_start..].find("}}") else {
            return false;
        };
        let close = content_start + close_offset;
        if matches!(
            parse_static_macro(source[content_start..close].trim()),
            Ok(StaticMacro::Run(_))
        ) {
            return true;
        }
        position = close + 2;
    }
    false
}

/// `run("x.py")` names a Python file inside the project: relative, without `..`, absolute
/// prefixes, backslashes or empty segments, and with a `.py` extension. Symbolic links and
/// existence are checked when the file is executed (ENT-126).
fn validate_run_target(path: &str) -> Result<()> {
    let invalid = path.is_empty()
        || path.starts_with('/')
        || path.chars().any(|character| character.is_control())
        || path.contains('\\')
        || path.contains(':')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "..")
        || !path.ends_with(".py");
    if invalid {
        return Err(template_error(format!(
            "run('{path}') must be a relative project path to a .py file without '..'"
        )));
    }
    Ok(())
}

fn validate_static_sources(
    source: &str,
    relations: &BTreeMap<String, String>,
    max_nesting: usize,
    allow_dowai_if_alias: bool,
) -> Result<()> {
    let tokens = tokenize(source, false, allow_dowai_if_alias)?;
    let mut parser = Parser {
        tokens,
        position: 0,
        max_nesting,
    };
    let (nodes, stop) = parser.parse_sequence(0)?;
    if stop.is_some() {
        return Err(template_error("unexpected conditional closing tag"));
    }

    fn validate_nodes(nodes: &[Node], relations: &BTreeMap<String, String>) -> Result<()> {
        for node in nodes {
            match node {
                Node::Text(text) => {
                    let mut position = 0;
                    while let Some(opening_offset) = text[position..].find("{{") {
                        let opening = position + opening_offset;
                        let content_start = opening + 2;
                        let close_offset = text[content_start..]
                            .find("}}")
                            .ok_or_else(|| template_error("static SQL macro is missing '}}'"))?;
                        let close = content_start + close_offset;
                        if let StaticMacro::Source(source, table) =
                            parse_static_macro(text[content_start..close].trim())?
                        {
                            let key = source_relation_key(&source, &table);
                            if !relations.contains_key(&key) {
                                return Err(template_error(format!(
                                    "source('{source}', '{table}') is not declared in the project"
                                )));
                            }
                        }
                        position = close + 2;
                    }
                }
                Node::Conditional {
                    branches,
                    otherwise,
                } => {
                    for (_, branch) in branches {
                        validate_nodes(branch, relations)?;
                    }
                    validate_nodes(otherwise, relations)?;
                }
                Node::Define { .. } => {}
            }
        }
        Ok(())
    }

    validate_nodes(&nodes, relations)
}

/// Static dependency declared by a SQL model through the template grammar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaticSqlReference {
    Ref(String),
    Source { source: String, table: String },
}

/// Extract `ref()` and `source()` calls from a SQL model using the same template
/// tree that the renderer uses. SQL comments are masked first, so references in
/// comments are not dependencies; expressions outside the allowlist (including
/// dynamic arguments, operators, and loops) are rejected by the tokenizer.
/// Both branches of a conditional contribute references (conservative union).
pub fn static_sql_references(source: &str, max_nesting: usize) -> Result<Vec<StaticSqlReference>> {
    let masked = mask_sql_comments(source)?;
    let tokens = tokenize(&masked, true, true)?;
    let mut parser = Parser {
        tokens,
        position: 0,
        max_nesting,
    };
    let (nodes, stop) = parser.parse_sequence(0)?;
    if stop.is_some() {
        return Err(template_error("unexpected conditional closing tag"));
    }
    let mut references = Vec::new();
    collect_static_references(&nodes, &mut references)?;
    Ok(references)
}

fn collect_static_references(
    nodes: &[Node],
    references: &mut Vec<StaticSqlReference>,
) -> Result<()> {
    for node in nodes {
        match node {
            Node::Text(text) => {
                let mut position = 0;
                while let Some(opening_offset) = text[position..].find("{{") {
                    let opening = position + opening_offset;
                    let content_start = opening + 2;
                    let close_offset = text[content_start..]
                        .find("}}")
                        .ok_or_else(|| template_error("static SQL macro is missing '}}'"))?;
                    let close = content_start + close_offset;
                    match parse_static_macro(text[content_start..close].trim())? {
                        StaticMacro::Ref(name) => references.push(StaticSqlReference::Ref(name)),
                        StaticMacro::Source(source, table) => {
                            references.push(StaticSqlReference::Source { source, table })
                        }
                        StaticMacro::Config(_) | StaticMacro::Run(_) => {}
                    }
                    position = close + 2;
                }
            }
            Node::Conditional {
                branches,
                otherwise,
            } => {
                for (_, branch) in branches {
                    collect_static_references(branch, references)?;
                }
                collect_static_references(otherwise, references)?;
            }
            Node::Define { .. } => {}
        }
    }
    Ok(())
}

/// Replace SQL comments (`--`, `#`, `/* */`) with a single space. Template
/// delimiters (`{{ }}`, `{% %}`, `{# #}`) and SQL string literals are copied
/// without comment detection, so `'--'` does not hide the following text.
fn mask_sql_comments(source: &str) -> Result<String> {
    let bytes = source.as_bytes();
    let mut output = String::with_capacity(source.len());
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if starts_with(bytes, index, b"{{") || starts_with(bytes, index, b"{%") {
            let close: &[u8] = if bytes[index + 1] == b'{' {
                b"}}"
            } else {
                b"%}"
            };
            index = find_after(bytes, index + 2, close).unwrap_or(bytes.len());
            continue;
        }
        if starts_with(bytes, index, b"{#") {
            index = find_after(bytes, index + 2, b"#}")
                .ok_or_else(|| template_error("template comment is missing '#}'"))?;
            continue;
        }
        let current = bytes[index];
        if current == b'\'' || current == b'"' || current == b'`' {
            index += 1;
            while index < bytes.len() && bytes[index] != current {
                if bytes[index] == b'\\' && current != b'`' {
                    index += 1;
                }
                index += 1;
            }
            index += 1;
            continue;
        }
        let comment_end = if starts_with(bytes, index, b"--") || current == b'#' {
            Some(
                bytes[index..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |offset| index + offset),
            )
        } else if starts_with(bytes, index, b"/*") {
            Some(find_after(bytes, index + 2, b"*/").unwrap_or(bytes.len()))
        } else {
            None
        };
        if let Some(end) = comment_end {
            output.push_str(&source[copied..index]);
            output.push(' ');
            copied = end;
            index = end;
            continue;
        }
        index += 1;
    }
    output.push_str(&source[copied..]);
    Ok(output)
}

fn starts_with(source: &[u8], index: usize, token: &[u8]) -> bool {
    source
        .get(index..index + token.len())
        .is_some_and(|slice| slice == token)
}

/// Index just past the first occurrence of `end` at or after `from`.
fn find_after(bytes: &[u8], from: usize, end: &[u8]) -> Option<usize> {
    (from..bytes.len())
        .find(|&index| starts_with(bytes, index, end))
        .map(|index| index + end.len())
}

pub fn source_relation_key(source: &str, table: &str) -> String {
    format!("source:{}:{source}:{table}", source.len())
}

fn parse_directive(content: &str) -> Result<Option<Token>> {
    let trimmed = content.trim();
    let (keyword, expression) = split_keyword(trimmed);
    if matches!(keyword, "if" | "elif") {
        let condition = parse_condition(expression)?;
        return Ok(Some(if keyword == "if" {
            Token::If(condition)
        } else {
            Token::Elif(condition)
        }));
    }
    if trimmed.starts_with("def_var") {
        let (name, value) = parse_definition(trimmed)?;
        return Ok(Some(Token::Define { name, value }));
    }
    match (keyword, expression.trim()) {
        ("else", "") => Ok(Some(Token::Else)),
        ("endif", "") => Ok(Some(Token::EndIf)),
        _ => Ok(None),
    }
}

fn split_keyword(content: &str) -> (&str, &str) {
    let end = content.find(char::is_whitespace).unwrap_or(content.len());
    (&content[..end], content[end..].trim())
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    max_nesting: usize,
}

impl Parser {
    fn parse_sequence(&mut self, nesting: usize) -> Result<(Vec<Node>, Option<Token>)> {
        let mut nodes = Vec::new();
        while let Some(token) = self.tokens.get(self.position) {
            match token {
                Token::Text(_) => {
                    if let Token::Text(text) = self.take() {
                        nodes.push(Node::Text(text));
                    }
                }
                Token::Define { .. } => {
                    if let Token::Define { name, value } = self.take() {
                        nodes.push(Node::Define { name, value });
                    }
                }
                Token::If(_) => {
                    let Token::If(condition) = self.take() else {
                        unreachable!()
                    };
                    nodes.push(self.parse_conditional(condition, nesting + 1)?);
                }
                Token::Elif(_) | Token::Else | Token::EndIf => {
                    return Ok((nodes, Some(self.take())));
                }
            }
        }
        Ok((nodes, None))
    }

    fn parse_conditional(&mut self, first: Condition, nesting: usize) -> Result<Node> {
        if nesting > self.max_nesting {
            return Err(template_error(format!(
                "template nesting exceeds configured maximum of {}",
                self.max_nesting
            )));
        }

        let mut branches = Vec::new();
        let mut condition = first;
        let mut otherwise = Vec::new();
        loop {
            let (body, stop) = self.parse_sequence(nesting)?;
            branches.push((condition, body));
            match stop {
                Some(Token::Elif(next_condition)) => condition = next_condition,
                Some(Token::Else) => {
                    let (body, ending) = self.parse_sequence(nesting)?;
                    if !matches!(ending, Some(Token::EndIf)) {
                        return Err(template_error(
                            "conditional else branch is not closed by endif",
                        ));
                    }
                    otherwise = body;
                    break;
                }
                Some(Token::EndIf) => break,
                Some(Token::If(_) | Token::Define { .. } | Token::Text(_)) => {
                    return Err(template_error("unexpected token in conditional"));
                }
                None => return Err(template_error("conditional block is missing endif")),
            }
        }
        Ok(Node::Conditional {
            branches,
            otherwise,
        })
    }

    fn take(&mut self) -> Token {
        let token = self.tokens[self.position].clone();
        self.position += 1;
        token
    }
}

fn parse_definition(expression: &str) -> Result<(String, Value)> {
    let mut cursor = Cursor::new(expression);
    cursor.expect_identifier("def_var")?;
    cursor.expect_char('(')?;
    let name = cursor.parse_name()?;
    cursor.expect_char(',')?;
    let value = cursor.parse_scalar()?;
    cursor.expect_char(')')?;
    cursor.expect_end()?;
    Ok((name, value))
}

fn parse_condition(expression: &str) -> Result<Condition> {
    let mut cursor = Cursor::new(expression);
    cursor.expect_identifier("var")?;
    cursor.expect_char('(')?;
    let name = cursor.parse_name()?;
    let default = if cursor.consume_char(',') {
        Some(cursor.parse_scalar()?)
    } else {
        None
    };
    cursor.expect_char(')')?;
    cursor.skip_whitespace();
    let comparison = if cursor.consume_operator("==") {
        Some((Comparison::Equal, cursor.parse_scalar()?))
    } else if cursor.consume_operator("!=") {
        Some((Comparison::NotEqual, cursor.parse_scalar()?))
    } else {
        None
    };
    cursor.expect_end()?;
    Ok(Condition {
        name,
        default,
        comparison,
    })
}

fn render_nodes(
    nodes: &[Node],
    variables: &mut BTreeMap<String, Value>,
    output: &mut String,
    strict_undefined: bool,
) -> Result<()> {
    for node in nodes {
        match node {
            Node::Text(text) => output.push_str(text),
            Node::Define { name, value } => {
                variables
                    .entry(name.clone())
                    .or_insert_with(|| value.clone());
            }
            Node::Conditional {
                branches,
                otherwise,
            } => {
                let mut selected = None;
                for (condition, body) in branches {
                    if evaluate(condition, variables, strict_undefined)? {
                        selected = Some(body.as_slice());
                        break;
                    }
                }
                render_nodes(
                    selected.unwrap_or(otherwise),
                    variables,
                    output,
                    strict_undefined,
                )?;
            }
        }
    }
    Ok(())
}

fn evaluate(
    condition: &Condition,
    variables: &BTreeMap<String, Value>,
    strict_undefined: bool,
) -> Result<bool> {
    let null = Value::Null;
    let value = match variables
        .get(&condition.name)
        .or(condition.default.as_ref())
    {
        Some(value) => value,
        None if !strict_undefined => &null,
        None => {
            return Err(template_error(format!(
                "undefined template variable '{}'",
                condition.name
            )))
        }
    };
    Ok(match &condition.comparison {
        Some((Comparison::Equal, expected)) => value == expected,
        Some((Comparison::NotEqual, expected)) => value != expected,
        None => truthy(value),
    })
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

struct Cursor<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn skip_whitespace(&mut self) {
        while let Some(character) = self.peek_char() {
            if !character.is_whitespace() {
                break;
            }
            self.position += character.len_utf8();
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn consume_char(&mut self, expected: char) -> bool {
        self.skip_whitespace();
        if self.peek_char() == Some(expected) {
            self.position += expected.len_utf8();
            true
        } else {
            false
        }
    }

    fn expect_char(&mut self, expected: char) -> Result<()> {
        if self.consume_char(expected) {
            Ok(())
        } else {
            Err(template_error(format!(
                "expected '{expected}' in template expression"
            )))
        }
    }

    fn expect_identifier(&mut self, expected: &str) -> Result<()> {
        self.skip_whitespace();
        if self.input[self.position..].starts_with(expected) {
            self.position += expected.len();
            if self
                .peek_char()
                .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
            {
                return Ok(());
            }
        }
        Err(template_error(format!(
            "expected '{expected}' in template expression"
        )))
    }

    fn parse_name(&mut self) -> Result<String> {
        self.skip_whitespace();
        if self.peek_char() == Some('"') || self.peek_char() == Some('\'') {
            let quote = self.peek_char().expect("checked above");
            let start = self.position;
            self.position += quote.len_utf8();
            let mut escaped = false;
            while let Some(character) = self.peek_char() {
                self.position += character.len_utf8();
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == quote {
                    let raw = &self.input[start..self.position];
                    let json = if quote == '"' {
                        raw.to_owned()
                    } else {
                        single_quoted_to_json(raw)?
                    };
                    let parsed: Value = serde_json::from_str(&json)
                        .map_err(|_| template_error("invalid variable name string"))?;
                    return parsed
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| template_error("variable name must be a string"));
                }
            }
            return Err(template_error("unterminated variable name"));
        }
        let start = self.position;
        while let Some(character) = self.peek_char() {
            if !character.is_ascii_alphanumeric() && character != '_' {
                break;
            }
            self.position += character.len_utf8();
        }
        let name = &self.input[start..self.position];
        if name.is_empty()
            || !name
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        {
            return Err(template_error(
                "variable name must be a string or identifier",
            ));
        }
        Ok(name.to_owned())
    }

    fn parse_scalar(&mut self) -> Result<Value> {
        self.skip_whitespace();
        let start = self.position;
        if matches!(self.peek_char(), Some('"' | '\'')) {
            let quote = self.peek_char().expect("checked above");
            self.position += quote.len_utf8();
            let mut escaped = false;
            let mut terminated = false;
            while let Some(character) = self.peek_char() {
                self.position += character.len_utf8();
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == quote {
                    terminated = true;
                    break;
                }
            }
            if !terminated {
                return Err(template_error("unterminated string literal"));
            }
        } else {
            while let Some(character) = self.peek_char() {
                if character.is_whitespace() || character == ')' || character == ',' {
                    break;
                }
                self.position += character.len_utf8();
            }
        }
        if start == self.position {
            return Err(template_error("expected JSON scalar"));
        }
        let literal = &self.input[start..self.position];
        let json = if literal.starts_with('\'') {
            single_quoted_to_json(literal)?
        } else {
            literal.to_owned()
        };
        let value: Value =
            serde_json::from_str(&json).map_err(|_| template_error("expected JSON scalar"))?;
        if value.is_array() || value.is_object() {
            return Err(template_error("template values must be JSON scalars"));
        }
        Ok(value)
    }

    fn consume_operator(&mut self, operator: &str) -> bool {
        self.skip_whitespace();
        if self.input[self.position..].starts_with(operator) {
            self.position += operator.len();
            true
        } else {
            false
        }
    }

    fn expect_end(&mut self) -> Result<()> {
        self.skip_whitespace();
        if self.position == self.input.len() {
            Ok(())
        } else {
            Err(template_error("unsupported template expression"))
        }
    }
}

fn single_quoted_to_json(raw: &str) -> Result<String> {
    let inner = raw
        .strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
        .ok_or_else(|| template_error("invalid quoted variable name"))?;
    let mut json = String::from("\"");
    let mut characters = inner.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '"' => json.push_str("\\\""),
            '\\' => {
                let escaped = characters
                    .next()
                    .ok_or_else(|| template_error("invalid escape in variable name"))?;
                match escaped {
                    '\'' => json.push('\''),
                    '"' => json.push_str("\\\""),
                    '\\' => json.push_str("\\\\"),
                    'n' => json.push_str("\\n"),
                    'r' => json.push_str("\\r"),
                    't' => json.push_str("\\t"),
                    other => {
                        json.push('\\');
                        json.push(other);
                    }
                }
            }
            other => json.push(other),
        }
    }
    json.push('"');
    Ok(json)
}
