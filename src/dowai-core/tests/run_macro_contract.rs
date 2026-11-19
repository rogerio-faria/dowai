//! ENT-124: `{{ run("x.py") }}` is a deferred, static macro. It is accepted only at the end of a
//! SQL model, as literal project-relative `.py` or `.sql` paths, and rendering never executes it.

use dowai_core::{render_sql_model, TemplateOptions};
use std::collections::BTreeMap;

fn render(source: &str) -> dowai_core::Result<dowai_core::RenderedSqlModel> {
    render_sql_model(
        source,
        &BTreeMap::new(),
        &TemplateOptions::default(),
        &BTreeMap::new(),
    )
}

#[test]
fn run_at_the_end_is_collected_in_order_and_removed_from_sql() {
    let rendered =
        render("select 1 as id\n{{ run(\"transf.py\") }}\n{{ run(\"carga/outro.sql\") }}\n")
            .expect("run at the end should be accepted");

    assert_eq!(
        rendered.post_run_targets,
        vec!["transf.py", "carga/outro.sql"]
    );
    assert_eq!(rendered.sql.trim(), "select 1 as id");
}

#[test]
fn sql_without_run_has_no_post_targets() {
    let rendered = render("select 1 as id").expect("plain SQL should render");
    assert!(rendered.post_run_targets.is_empty());
}

#[test]
fn run_before_sql_is_refused() {
    let error = render("{{ run(\"transf.py\") }}\nselect 1").expect_err("run before SQL");
    assert!(error.to_string().contains("end of the SQL model"));
}

#[test]
fn run_followed_by_sql_text_is_refused() {
    let error = render("select 1\n{{ run(\"transf.py\") }}\nselect 2")
        .expect_err("SQL after run must be refused");
    assert!(error.to_string().contains("end of the SQL model"));
}

#[test]
fn ref_after_run_is_refused() {
    let error = render("select 1\n{{ run(\"transf.py\") }}\n{{ ref('x') }}")
        .expect_err("macro after run must be refused");
    assert!(error.to_string().contains("end of the SQL model"));
}

#[test]
fn invalid_run_paths_are_refused() {
    for path in [
        "../fora.py",
        "pasta/../fora.py",
        "/abs/fora.py",
        "C:/fora.py",
        "pasta\\fora.py",
        "transf.txt",
        "",
        "pasta//x.py",
    ] {
        let source = format!("select 1\n{{{{ run(\"{path}\") }}}}");
        assert!(render(&source).is_err(), "path {path:?} should be refused");
    }
}

#[test]
fn run_with_extra_or_named_arguments_is_refused() {
    for source in [
        "select 1\n{{ run(\"a.py\", \"b.py\") }}",
        "select 1\n{{ run(path=\"a.py\") }}",
        "select 1\n{{ run() }}",
        "select 1\n{{ run(1) }}",
    ] {
        assert!(render(source).is_err(), "{source:?} should be refused");
    }
}

#[test]
fn run_is_not_a_macro_inside_plain_sql_text_outside_braces() {
    // The word `run` in SQL text must not be treated as a macro.
    let rendered = render("select run_id from runs").expect("plain identifiers are SQL");
    assert!(rendered.post_run_targets.is_empty());
    assert_eq!(rendered.sql, "select run_id from runs");
}
