use super::{
    ensure_destructive_approval, ensure_single_statement, quote_identifier, AdapterError,
    PgProfile, TlsMode,
};

fn profile(host: &str) -> PgProfile {
    PgProfile::new(host, 5432, "dowai", "dowai_test", "DOWAI_TEST_PASSWORD")
}

#[test]
fn profile_defaults_to_verified_tls() {
    let profile = profile("db.example.test");
    assert_eq!(profile.tls_mode(), TlsMode::Required);
    profile.validate().unwrap();
}

#[test]
fn profile_debug_contains_the_environment_reference_not_a_password() {
    let profile = profile("localhost");
    let debug = format!("{profile:?}");
    assert!(debug.contains("DOWAI_TEST_PASSWORD"));
    assert!(!debug.contains("literal-password"));
}

#[test]
fn profile_rejects_empty_or_whitespace_required_fields() {
    let cases = [
        PgProfile::new("", 5432, "user", "db", "PASSWORD"),
        PgProfile::new("host", 5432, " ", "db", "PASSWORD"),
        PgProfile::new("host", 5432, "user", "\t", "PASSWORD"),
        PgProfile::new("host", 5432, "user", "db", ""),
        PgProfile::new("host", 0, "user", "db", "PASSWORD"),
        PgProfile::new("host name", 5432, "user", "db", "PASSWORD"),
    ];

    for profile in cases {
        assert!(matches!(
            profile.validate(),
            Err(AdapterError::InvalidProfile(_))
        ));
    }
}

#[test]
fn profile_rejects_invalid_password_environment_variable_names() {
    let profile = PgProfile::new("localhost", 5432, "user", "db", "not a variable");
    assert!(matches!(
        profile.validate(),
        Err(AdapterError::InvalidProfile(_))
    ));
}

#[test]
fn disabling_tls_is_only_allowed_for_local_hosts() {
    let mut remote = profile("db.example.test");
    remote.set_tls_mode(TlsMode::Disabled);
    assert!(matches!(
        remote.validate(),
        Err(AdapterError::InsecureTlsDisabled)
    ));

    for host in ["localhost", "127.0.0.1", "::1"] {
        let mut local = profile(host);
        local.set_tls_mode(TlsMode::Disabled);
        local.validate().unwrap();
    }
}

#[test]
fn missing_password_environment_variable_has_a_sanitized_error() {
    let variable = "DOWAI_POSTGRES_TEST_MISSING_PASSWORD_8371";
    std::env::remove_var(variable);
    let profile = PgProfile::new("localhost", 5432, "user", "db", variable);
    let error = profile.resolve_password().unwrap_err();
    assert!(matches!(error, AdapterError::MissingPassword));
    assert_eq!(
        error.to_string(),
        "required password environment variable is missing"
    );
    assert!(!format!("{error:?}").contains("8371"));
}

#[test]
fn adapter_errors_have_stable_sanitized_messages() {
    assert_eq!(
        AdapterError::ConnectionFailed.to_string(),
        "PostgreSQL connection failed"
    );
    assert_eq!(
        AdapterError::QueryFailed.to_string(),
        "PostgreSQL query failed"
    );
    assert_eq!(
        AdapterError::DestructiveApprovalRequired.to_string(),
        "replacing an existing PostgreSQL relation requires explicit approval"
    );
}

#[test]
fn destructive_relation_changes_require_explicit_approval_when_relation_exists() {
    assert!(ensure_destructive_approval(false, false).is_ok());
    assert!(ensure_destructive_approval(true, true).is_ok());
    assert_eq!(
        ensure_destructive_approval(true, false),
        Err(AdapterError::DestructiveApprovalRequired)
    );
}

#[test]
fn relation_identifiers_are_quoted_and_escaped() {
    assert_eq!(
        quote_identifier("sales\"daily").unwrap(),
        "\"sales\"\"daily\""
    );
    assert!(matches!(
        quote_identifier(""),
        Err(AdapterError::InvalidIdentifier)
    ));
    assert!(matches!(
        quote_identifier(&"x".repeat(64)),
        Err(AdapterError::InvalidIdentifier)
    ));
    assert!(matches!(
        quote_identifier("bad\nname"),
        Err(AdapterError::InvalidIdentifier)
    ));
}

#[test]
fn statement_guard_ignores_quoted_and_commented_semicolons() {
    for sql in [
        "SELECT ';' AS value",
        "SELECT \"a;b\" FROM sample",
        "SELECT $$first;second$$",
        "SELECT $body$first;second$body$",
        "SELECT 1 /* outer ; /* nested ; */ still comment */",
        "SELECT E'escaped\\';still string'",
        "SELECT 1; -- trailing comment ;\n",
    ] {
        ensure_single_statement(sql).expect("one SQL statement should be accepted");
    }
    assert!(matches!(
        ensure_single_statement("SELECT foo$body$; DROP TABLE users"),
        Err(AdapterError::MultipleStatements)
    ));
}

#[test]
fn statement_guard_rejects_multiple_statements_and_nontrivial_tail() {
    for sql in [
        "SELECT 1; DROP TABLE users",
        "SELECT 1;;",
        "SELECT 1; /* comment */ SELECT 2",
    ] {
        assert!(matches!(
            ensure_single_statement(sql),
            Err(AdapterError::MultipleStatements)
        ));
    }
}
