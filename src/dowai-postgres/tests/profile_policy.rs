//! ENT-030: connection failures must not expose the password, which is read from an
//! environment variable only at connect time. Runs without PostgreSQL: the target port is closed.

use dowai_postgres::{PgProfile, PostgresAdapter};

const SENTINEL: &str = "ent030-sentinel-password-do-not-leak";

#[test]
fn connection_failure_does_not_expose_the_password() {
    std::env::set_var("DOWAI_ENT030_TEST_PASSWORD", SENTINEL);
    // Port 1 on loopback is closed: connection is refused without any network access.
    let profile = PgProfile::new(
        "127.0.0.1",
        1,
        "dowai",
        "dowai_test",
        "DOWAI_ENT030_TEST_PASSWORD",
    );
    let error = PostgresAdapter::connect(&profile)
        .err()
        .expect("connection must fail");

    assert!(!format!("{error:?}").contains(SENTINEL));
    assert!(!error.to_string().contains(SENTINEL));
    assert!(!format!("{profile:?}").contains(SENTINEL));
}

#[test]
fn remote_disabled_tls_is_refused_before_any_connection_attempt() {
    std::env::set_var("DOWAI_ENT030_TEST_PASSWORD", SENTINEL);
    let mut profile = PgProfile::new(
        "db.example.test",
        5432,
        "dowai",
        "dowai_test",
        "DOWAI_ENT030_TEST_PASSWORD",
    );
    profile.set_tls_mode(dowai_postgres::TlsMode::Disabled);
    let error = PostgresAdapter::connect(&profile)
        .err()
        .expect("policy must refuse");
    assert_eq!(format!("{error:?}"), "InsecureTlsDisabled");
}
