//! ENT-021 / AC053: `{{ if }}`, `{{ elif }}`, `{{ else }}` and `{{ endif }}` are
//! normalized by the parser as equivalents of the `{% ... %}` conditionals.

use dowai_core::{render_template, TemplateOptions};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn vars(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

fn render_with(
    source: &str,
    variables: &BTreeMap<String, Value>,
    allow_alias: bool,
) -> dowai_core::Result<String> {
    let options = TemplateOptions {
        allow_dowai_if_alias: allow_alias,
        ..TemplateOptions::default()
    };
    render_template(source, variables, &options).map(|rendered| rendered.sql)
}

fn assert_same_output(standard: &str, alias: &str, variables: &BTreeMap<String, Value>) {
    let expected = render_with(standard, variables, true).expect("standard delimiters render");
    let actual = render_with(alias, variables, true).expect("DOWAI aliases render");
    assert_eq!(
        actual, expected,
        "alias output differs from standard output"
    );
}

#[test]
fn if_elif_else_alias_matches_standard_delimiters_for_every_branch() {
    let standard =
        "{% if var('level') == 1 %}one{% elif var('level') == 2 %}two{% else %}other{% endif %}";
    let alias =
        "{{ if var('level') == 1 }}one{{ elif var('level') == 2 }}two{{ else }}other{{ endif }}";
    for level in [1, 2, 3] {
        assert_same_output(standard, alias, &vars(&[("level", json!(level))]));
    }
}

#[test]
fn alias_accepts_compact_and_padded_spacing() {
    let variables = vars(&[("enabled", json!(true))]);
    let standard = "{% if var('enabled') %}yes{% else %}no{% endif %}";
    for alias in [
        "{{if var('enabled')}}yes{{else}}no{{endif}}",
        "{{   if   var('enabled')   }}yes{{   else   }}no{{   endif   }}",
        "{{\n  if var('enabled')\n}}yes{{\n  else\n}}no{{\n  endif\n}}",
    ] {
        assert_same_output(standard, alias, &variables);
    }
}

#[test]
fn alias_nested_with_standard_delimiters_matches_standard_output() {
    let standard =
        "{% if var('outer') %}A{{ if var('inner') }}B{{ else }}C{{ endif }}{% else %}D{% endif %}";
    let all_standard =
        "{% if var('outer') %}A{% if var('inner') %}B{% else %}C{% endif %}{% else %}D{% endif %}";
    for outer in [true, false] {
        for inner in [true, false] {
            let variables = vars(&[("outer", json!(outer)), ("inner", json!(inner))]);
            assert_same_output(all_standard, standard, &variables);
        }
    }
}

#[test]
fn alias_at_end_of_file_without_trailing_text_renders() {
    let variables = vars(&[("enabled", json!(true))]);
    let output = render_with(
        "select 1{{ if var('enabled') }} as v{{ endif }}",
        &variables,
        true,
    )
    .expect("alias at end of file should render");
    assert_eq!(output, "select 1 as v");
}

#[test]
fn alias_inside_sql_comment_is_ignored() {
    let output = render_with(
        "{# {{ if var('missing') }} #}select 1",
        &BTreeMap::new(),
        true,
    )
    .expect("commented alias should not be evaluated");
    assert_eq!(output, "select 1");
}

#[test]
fn unclosed_alias_block_is_an_error() {
    let error = render_with("{{ if var('x', true) }}select 1", &BTreeMap::new(), true)
        .expect_err("missing endif must fail");
    assert!(error.to_string().contains("endif"), "{error}");
}

#[test]
fn stray_alias_endif_without_block_is_an_error() {
    assert!(render_with("select 1{{ endif }}", &BTreeMap::new(), true).is_err());
}

#[test]
fn disabled_alias_rejects_if_elif_else_and_endif() {
    let variables = vars(&[("enabled", json!(true))]);
    for source in [
        "{{ if var('enabled') }}x{{ endif }}",
        "{% if var('enabled') %}x{{ else }}y{% endif %}",
        "{% if var('enabled') %}x{% endif %}{{ endif }}",
        "{% if var('enabled') %}x{{ elif var('enabled') }}y{% endif %}",
    ] {
        let error = render_with(source, &variables, false)
            .expect_err("disabled DOWAI alias must be rejected");
        assert!(
            error.to_string().contains("allow_dowai_if_alias"),
            "unexpected message for {source}: {error}"
        );
    }
}

#[test]
fn disabled_alias_keeps_standard_delimiters_and_macros_working() {
    let variables = vars(&[("enabled", json!(true))]);
    let output = render_with(
        "{{ def_var('extra', 1) }}{% if var('enabled') %}yes{% endif %}",
        &variables,
        false,
    )
    .expect("standard delimiters must still render");
    assert_eq!(output, "yes");
}

#[test]
fn disabled_alias_is_rejected_by_the_sql_model_path_used_before_execution() {
    use dowai_core::render_sql_model;

    let options = TemplateOptions {
        allow_dowai_if_alias: false,
        ..TemplateOptions::default()
    };
    let variables = vars(&[("enabled", json!(true))]);
    let error = render_sql_model(
        "{{ if var('enabled') }}select 1{{ endif }}",
        &variables,
        &options,
        &BTreeMap::new(),
    )
    .expect_err("disabled alias must fail the SQL model preparation");
    assert!(
        error.to_string().contains("allow_dowai_if_alias"),
        "{error}"
    );

    let model = render_sql_model(
        "{{ def_var('x', 1) }}{% if var('enabled') %}select 1{% endif %}",
        &variables,
        &options,
        &BTreeMap::new(),
    )
    .expect("standard delimiters must still prepare");
    assert_eq!(model.sql, "select 1");
}
