//! B-06: JSON Schema contracts under `specs/` checked against fixtures. Every valid fixture
//! must pass and every invalid fixture must be rejected, so a schema cannot drift into
//! accepting or refusing the wrong shapes without a test failing.

use dowai_core::{sha256_hex, AuditEvent, AuditLog};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const CONTRACTS: [&str; 5] = ["commands", "events", "ipc", "plugin", "data_dictionary"];

fn specs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../specs")
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{} is JSON: {error}", path.display()))
}

fn validator(contract: &str) -> jsonschema::Validator {
    let path = specs_dir().join(format!("{contract}.schema.json"));
    let schema = read_json(&path);
    jsonschema::validator_for(&schema)
        .unwrap_or_else(|error| panic!("{} is not a valid JSON Schema: {error}", path.display()))
}

fn fixtures(contract: &str, group: &str) -> Vec<PathBuf> {
    let directory = specs_dir().join("fixtures").join(contract).join(group);
    let mut files: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("fixtures for {contract}/{group}: {error}"))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    files
}

#[test]
fn every_contract_compiles_and_has_both_fixture_groups() {
    for contract in CONTRACTS {
        validator(contract);
        for group in ["valid", "invalid"] {
            assert!(
                !fixtures(contract, group).is_empty(),
                "{contract} needs at least one {group} fixture"
            );
        }
    }
}

#[test]
fn valid_fixtures_pass_their_contract() {
    for contract in CONTRACTS {
        let schema = validator(contract);
        for file in fixtures(contract, "valid") {
            let instance = read_json(&file);
            let errors: Vec<String> = schema
                .iter_errors(&instance)
                .map(|error| error.to_string())
                .collect();
            assert!(
                errors.is_empty(),
                "{} must be accepted by {contract}: {errors:?}",
                file.display()
            );
        }
    }
}

#[test]
fn invalid_fixtures_are_rejected_by_their_contract() {
    for contract in CONTRACTS {
        let schema = validator(contract);
        for file in fixtures(contract, "invalid") {
            let instance = read_json(&file);
            assert!(
                !schema.is_valid(&instance),
                "{} must be rejected by {contract}",
                file.display()
            );
        }
    }
}

/// The engine's own AUDIT writer, fed with every event name the runtime emits, must produce
/// lines the events contract accepts. Hand-written fixtures alone could drift from it.
#[test]
fn audit_lines_written_by_the_engine_match_the_events_contract() {
    let schema = validator("events");
    let directory =
        std::env::temp_dir().join(format!("dowai-events-contract-{}", std::process::id()));
    fs::create_dir_all(&directory).expect("create temporary directory");
    let path = directory.join("audit.jsonl");
    let log = AuditLog::new(&path);
    let run_id = "44444444-4444-4444-8444-444444444444";
    let hash = sha256_hex(b"plan");
    for name in [
        "run.started",
        "node.started",
        "node.finished",
        "variables.published",
        "external.write",
        "python.invocation",
        "policy.denied",
        "run.finished",
    ] {
        let mut event = AuditEvent::new(name, run_id).at_ms(1_700_000_000_123);
        event.session_id = Some("55555555-5555-4555-8555-555555555555".to_owned());
        event.invocation_id = Some("66666666-6666-4666-8666-666666666666".to_owned());
        event.node_id = Some("consumer.py".to_owned());
        event.call_type = Some("static".to_owned());
        event.project = Some("fixture".to_owned());
        event.plan_hash = Some(hash.clone());
        event.config_hash = Some(hash.clone());
        event.mode = Some("automatic".to_owned());
        event.variables = Some(vec!["stage".to_owned()]);
        event.rows = Some(3);
        event.error_code = Some("DOWAI_PYTHON_FAILED".to_owned());
        log.append(&event).expect("append audit event");
    }
    let text = fs::read_to_string(&path).expect("read audit log");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 8);
    for line in lines {
        let instance: Value = serde_json::from_str(line).expect("audit line is JSON");
        let errors: Vec<String> = schema
            .iter_errors(&instance)
            .map(|error| error.to_string())
            .collect();
        assert!(errors.is_empty(), "engine line rejected: {line} {errors:?}");
    }
    fs::remove_dir_all(directory).expect("remove temporary directory");
}
