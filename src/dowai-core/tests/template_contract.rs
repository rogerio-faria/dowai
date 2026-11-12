use dowai_core::{
    render_sql_model, render_template, source_relation_key, Materialization, TemplateOptions,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn render(source: &str) -> dowai_core::Result<dowai_core::TemplateRender> {
    render_template(source, &BTreeMap::new(), &TemplateOptions::default())
}

#[test]
fn nested_conditions_select_the_expected_sql() {
    let variables = BTreeMap::from([
        ("outer".to_owned(), json!(true)),
        ("inner".to_owned(), json!(false)),
    ]);

    let rendered = render_template(
        "{% if var('outer') %}A{% if var('inner') %}B{% else %}C{% endif %}{% else %}D{% endif %}",
        &variables,
        &TemplateOptions::default(),
    )
    .expect("nested conditions should render");

    assert_eq!(rendered.sql, "AC");
}

#[test]
fn elif_and_else_select_one_branch() {
    let variables = BTreeMap::from([("level".to_owned(), json!(2))]);
    let rendered = render_template(
        "{% if var('level') == 1 %}one{% elif var('level') == 2 %}two{% else %}other{% endif %}",
        &variables,
        &TemplateOptions::default(),
    )
    .expect("elif should be supported");

    assert_eq!(rendered.sql, "two");
}

#[test]
fn dowai_condition_aliases_match_jinja_delimiters() {
    let variables = BTreeMap::from([("enabled".to_owned(), json!(true))]);
    let regular = render_template(
        "{% if var('enabled') %}yes{% else %}no{% endif %}",
        &variables,
        &TemplateOptions::default(),
    )
    .expect("standard delimiters should render");
    let aliases = render_template(
        "{{ if var('enabled') }}yes{{ else }}no{{ endif }}",
        &variables,
        &TemplateOptions::default(),
    )
    .expect("DOWAI aliases should render");

    assert_eq!(regular.sql, aliases.sql);
}

#[test]
fn def_var_sets_absent_values_for_later_conditions() {
    let rendered = render("{{ def_var('enabled', true) }}{% if var('enabled') %}active{% endif %}")
        .expect("definition should be usable later in render");

    assert_eq!(rendered.sql, "active");
    assert_eq!(rendered.variables.get("enabled"), Some(&json!(true)));
}

#[test]
fn def_var_does_not_overwrite_existing_values() {
    let initial = BTreeMap::from([("count".to_owned(), json!(4))]);
    let rendered = render_template(
        "{{ def_var(count, 7) }}{{ if var('count') == 4 }}kept{{ endif }}",
        &initial,
        &TemplateOptions::default(),
    )
    .expect("existing values should be preserved");

    assert_eq!(rendered.sql, "kept");
    assert_eq!(rendered.variables.get("count"), Some(&json!(4)));
}

#[test]
fn definitions_in_unselected_branches_have_no_side_effect() {
    let rendered = render(
        "{% if var('missing', false) %}{{ def_var('leaked', true) }}{% else %}safe{% endif %}",
    )
    .expect("unselected definitions should not run");

    assert_eq!(rendered.sql, "safe");
    assert!(!rendered.variables.contains_key("leaked"));
}

#[test]
fn undefined_variables_are_strict_unless_a_default_is_provided() {
    assert!(render("{% if var('missing') %}x{% endif %}").is_err());

    let rendered =
        render(r#"{% if var('missing', "fallback") == "fallback" %}defaulted{% endif %}"#)
            .expect("default should satisfy an undefined lookup");
    assert_eq!(rendered.sql, "defaulted");
}

#[test]
fn arbitrary_expressions_and_output_interpolation_are_rejected() {
    for source in [
        "{% if system('anything') %}x{% endif %}",
        "{% if var('x') and true %}x{% endif %}",
        "{{ if __import__('os') }}x{{ endif }}",
        "{{ var('secret') }}",
    ] {
        let error = render(source).expect_err("unsafe or arbitrary expressions must fail");
        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
        assert!(!error.message().is_empty());
    }
}

#[test]
fn configured_nesting_limit_is_enforced() {
    let options = TemplateOptions {
        max_nesting: 1,
        ..TemplateOptions::default()
    };
    assert!(render_template(
        "{% if var('a', true) %}{% if var('b', true) %}x{% endif %}{% endif %}",
        &BTreeMap::new(),
        &options,
    )
    .is_err());
}

#[test]
fn comments_are_ignored_and_unterminated_delimiters_fail() {
    let source = "{# {% if var('missing') %} #}select 1";
    let rendered = render(source).expect("comment contents must not be evaluated");
    assert_eq!(rendered.sql, "select 1");

    for source in ["{% if var('flag')", "{{ ref('orders')", "{# comment"] {
        assert!(
            render(source).is_err(),
            "accepted unclosed syntax: {source}"
        );
    }
}

#[test]
fn lenient_undefined_variables_default_to_null_only_when_configured() {
    let options = TemplateOptions {
        strict_undefined: false,
        ..TemplateOptions::default()
    };
    let rendered = render_template(
        "{% if var('missing') == null %}fallback{% endif %}",
        &BTreeMap::new(),
        &options,
    )
    .expect("lenient mode should treat missing values as null");

    assert_eq!(rendered.sql, "fallback");
}

#[test]
fn refs_and_other_sql_template_text_are_preserved() {
    let source = "select * from {{ ref('orders') }} where id = 1";
    let rendered = render(source).expect("refs are not output interpolation");

    assert_eq!(rendered.sql, source);
}

#[test]
fn source_and_config_macros_are_preserved_as_static_calls() {
    for source in [
        "select * from {{ source('sales', 'orders') }}",
        "{{ config(materialized='table') }} select 1",
        "{{ config(enabled=true, retries=2) }} select 1",
    ] {
        let rendered = render(source).expect("documented static macros are preserved");
        assert_eq!(rendered.sql, source);
    }
}

#[test]
fn sql_model_rendering_resolves_declared_external_sources() {
    let sources = BTreeMap::from([(
        source_relation_key("sales", "orders"),
        "\"staging\".\"orders\"".to_owned(),
    )]);

    let model = render_sql_model(
        "select * from {{ source('sales', 'orders') }}",
        &BTreeMap::new(),
        &TemplateOptions::default(),
        &sources,
    )
    .expect("declared source should resolve");

    assert_eq!(model.sql, "select * from \"staging\".\"orders\"");
}

#[test]
fn sql_model_rendering_rejects_undeclared_external_sources() {
    let error = render_sql_model(
        "select * from {{ source('sales', 'orders') }}",
        &BTreeMap::new(),
        &TemplateOptions::default(),
        &BTreeMap::new(),
    )
    .expect_err("undeclared sources must be rejected");

    assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
}

#[test]
fn sql_model_rendering_validates_sources_in_unselected_branches() {
    let options = TemplateOptions {
        max_nesting: 32,
        strict_undefined: false,
        allow_dowai_if_alias: true,
    };

    let error = render_sql_model(
        "{% if var('enabled') %}select * from {{ source('sales', 'orders') }}{% endif %}",
        &BTreeMap::new(),
        &options,
        &BTreeMap::new(),
    )
    .expect_err("all statically declared source references must be checked");

    assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
}

#[test]
fn unknown_macros_and_invalid_macro_arguments_are_rejected() {
    for source in [
        "{{ __import__('os') }}",
        "{{ var('secret') }}",
        "{{ ref(model_name) }}",
        "{{ source('sales', table_name) }}",
        "{{ config(materialized=run()) }}",
        "{{ ref() }}",
        "{{ ref('orders', 'extra') }}",
        "{{ source('sales') }}",
        "{{ source('sales', 'orders', 'extra') }}",
        "{{ config() }}",
        "{{ config(materialized='table',) }}",
        "{{ config(materialized='table', materialized='view') }}",
        "{{ ref('orders'; DROP TABLE orders) }}",
    ] {
        let error = render(source).expect_err("unknown/dynamic macros must not pass through");
        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID", "{source}");
    }
}

#[test]
fn static_macros_accept_single_quoted_strings_with_escaped_quotes() {
    let source = r"select * from {{ ref('owner\'s_orders') }}";
    let rendered = render(source).expect("escaped single quotes should be parsed safely");

    assert_eq!(rendered.sql, source);
}

#[test]
fn sql_model_rendering_resolves_refs_and_extracts_table_materialization() {
    let relations = BTreeMap::from([
        ("upstream.sql".to_owned(), "\"upstream\"".to_owned()),
        ("upstream".to_owned(), "\"upstream\"".to_owned()),
    ]);
    let rendered = render_sql_model(
        "{{ config(materialized='table') }} select * from {{ ref('upstream') }}",
        &BTreeMap::new(),
        &TemplateOptions::default(),
        &relations,
    )
    .expect("supported ref and table config should render");

    assert_eq!(rendered.sql.trim(), "select * from \"upstream\"");
    assert_eq!(rendered.materialization, Materialization::Table);
}

#[test]
fn sql_model_rendering_defaults_to_table_and_accepts_views() {
    let defaulted = render_sql_model(
        "select 1",
        &BTreeMap::new(),
        &TemplateOptions::default(),
        &BTreeMap::new(),
    )
    .expect("models default to table materialization");
    let view = render_sql_model(
        "{{ config(materialized='view') }} select 1",
        &BTreeMap::new(),
        &TemplateOptions::default(),
        &BTreeMap::new(),
    )
    .expect("view materialization should render");

    assert_eq!(defaulted.materialization, Materialization::Table);
    assert_eq!(view.materialization, Materialization::View);
    assert_eq!(view.sql.trim(), "select 1");
}

#[test]
fn sql_model_rendering_rejects_unknown_refs_and_unsupported_config() {
    for source in [
        "select * from {{ ref('unknown') }}",
        "{{ config(materialized='incremental') }} select 1",
        "{{ config(alias='other') }} select 1",
        "{{ config(materialized='table', materialized='view') }} select 1",
        "{{ config(materialized='table',) }} select 1",
    ] {
        let error = render_sql_model(
            source,
            &BTreeMap::new(),
            &TemplateOptions::default(),
            &BTreeMap::new(),
        )
        .expect_err("unsupported SQL-model macros must fail");
        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID", "{source}");
    }

    let error = render_sql_model(
        "select * from {{ source('sales', 'orders') }}",
        &BTreeMap::new(),
        &TemplateOptions::default(),
        &BTreeMap::new(),
    )
    .expect_err("undeclared sources must fail explicitly");
    assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
}

#[test]
fn supports_identifier_names_and_json_scalar_comparisons() {
    let rendered = render(
        "{{ def_var(flag, false) }}{% if var('flag') == false %}n{% elif var('flag') != null %}y{% else %}z{% endif %}",
    )
    .expect("JSON scalar values and identifiers should parse");

    assert_eq!(rendered.sql, "n");
    assert_eq!(rendered.variables.get("flag"), Some(&Value::Bool(false)));
}

#[test]
fn every_dowai_alias_form_matches_its_standard_form() {
    // D16 (ADR 0004): each alias form renders exactly like the standard form.
    let variables = BTreeMap::from([("mode".to_owned(), json!("delta"))]);
    let cases = [
        (
            "{% if var('mode') == 'full' %}A{% elif var('mode') == 'delta' %}B{% else %}C{% endif %}",
            "{{ if var('mode') == 'full' }}A{{ elif var('mode') == 'delta' }}B{{ else }}C{{ endif }}",
            "B",
        ),
        (
            "{% def_var('limit', 10) %}{% if var('limit', 0) == 10 %}ok{% endif %}",
            "{{ def_var('limit', 10) }}{{ if var('limit', 0) == 10 }}ok{{ endif }}",
            "ok",
        ),
    ];
    for (standard, alias, expected) in cases {
        let standard = render_template(standard, &variables, &TemplateOptions::default())
            .expect("standard form should render");
        let alias = render_template(alias, &variables, &TemplateOptions::default())
            .expect("alias form should render");
        assert_eq!(standard.sql, alias.sql);
        assert_eq!(standard.sql, expected);
    }
}

#[test]
fn dbt_constructs_outside_the_supported_subset_are_rejected() {
    for source in [
        "{% macro helper() %}x{% endmacro %}",
        "{% for item in var('items') %}x{% endfor %}",
        "{{ env_var('HOME') }}",
        "{{ ref('a', 'b') }}",
    ] {
        let error = render(source).expect_err("unsupported dbt construct must fail");
        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID", "source: {source}");
    }
    for source in [
        "{{ config(materialized='incremental') }}select 1",
        "{{ config(materialized='table', unique_key='id') }}select 1",
        "{{ config(materialized='ephemeral') }}select 1",
    ] {
        let error = render_sql_model(
            source,
            &BTreeMap::new(),
            &TemplateOptions::default(),
            &BTreeMap::new(),
        )
        .expect_err("unsupported config must fail");
        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID", "source: {source}");
    }
}

#[test]
fn dynamic_source_arguments_in_any_branch_fail_rendering() {
    let options = TemplateOptions::default();
    let mut sources = BTreeMap::new();
    sources.insert(
        source_relation_key("sales", "orders"),
        "\"staging\".\"orders\"".to_owned(),
    );

    let error = render_sql_model(
        "{% if var('enabled') %}select 1{% else %}select * from {{ source(src_name, 'orders') }}{% endif %}",
        &BTreeMap::new(),
        &options,
        &sources,
    )
    .expect_err("a dynamic source in an unselected branch must not be omitted");

    assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
}
