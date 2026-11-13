//! Contrato da detecção estática de escrita direta por bibliotecas (ENT-049, AC058).
//!
//! A análise é por AST, sem execução do modelo. Os sinais indicam uso que pode gravar fora do
//! `env.write`; não provam escrita.

use dowai_core::static_direct_write_signals;

fn signals(source: &str) -> Vec<String> {
    static_direct_write_signals("model.py", source).expect("source must parse")
}

#[test]
fn driver_and_orm_imports_are_reported_by_top_level_module() {
    assert_eq!(signals("import sqlalchemy\n"), vec!["import:sqlalchemy"]);
    assert_eq!(
        signals("from sqlalchemy import create_engine\n"),
        vec!["import:sqlalchemy"]
    );
    assert_eq!(signals("import mysql.connector\n"), vec!["import:mysql"]);
    assert_eq!(signals("import psycopg2 as pg\n"), vec!["import:psycopg2"]);
}

#[test]
fn dataframe_write_methods_are_reported() {
    assert_eq!(
        signals("import pandas as pd\ndf.to_sql('t', con)\n"),
        vec!["call:to_sql"]
    );
    assert_eq!(
        signals("df.write_database('t', uri)\n"),
        vec!["call:write_database"]
    );
}

#[test]
fn reads_and_unrelated_code_are_not_reported() {
    assert!(signals("import pandas as pd\ndf = pd.read_csv('a.csv')\n").is_empty());
    assert!(signals("import json\nprint(json.dumps({}))\n").is_empty());
    // `env.write` é o caminho permitido e não é sinal de escrita direta.
    assert!(signals("env.write('t', data)\n").is_empty());
}

#[test]
fn nested_imports_and_calls_are_found_and_deduplicated() {
    let source = "def load():\n    import asyncpg\n    import asyncpg\n    frame.to_sql('a', c)\n    frame.to_sql('b', c)\n";
    assert_eq!(signals(source), vec!["call:to_sql", "import:asyncpg"]);
}

#[test]
fn output_is_sorted_and_deterministic() {
    let source = "import sqlalchemy\nimport pymysql\nframe.to_sql('x', c)\n";
    assert_eq!(
        signals(source),
        vec!["call:to_sql", "import:pymysql", "import:sqlalchemy"]
    );
}

#[test]
fn unparsable_source_is_an_error_not_an_empty_result() {
    let error = static_direct_write_signals("broken.py", "def (:\n")
        .expect_err("invalid syntax must not be reported as clean");
    assert_eq!(error.code(), "DOWAI_PLAN_INVALID");
}
