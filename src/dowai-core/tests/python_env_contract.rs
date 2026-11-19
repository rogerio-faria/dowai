//! Contrato de validação de divergência entre `dowai_project.json` e `pyproject.toml` (ENT-046).

use dowai_core::validate_project_with_configuration;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

struct Sandbox(PathBuf);

impl Sandbox {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dowai-python-env-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(path.join("models")).unwrap();
        let interpreter = if cfg!(windows) {
            path.join(".venv").join("Scripts").join("python.exe")
        } else {
            path.join(".venv").join("bin").join("python")
        };
        fs::write(
            path.join("dowai_project.json"),
            json!({
                "schema_version": 2,
                "name": "demo",
                "python": {
                    "interpreter": interpreter,
                    "dependencies": ["Pandas>=2", "polars"]
                }
            })
            .to_string(),
        )
        .unwrap();
        Self(path)
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn codes(report: &dowai_core::ValidationReport) -> Vec<String> {
    report
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.clone())
        .collect()
}

#[test]
fn divergence_is_reported_without_touching_either_file() {
    let sandbox = Sandbox::new("diverge");
    let pyproject = "[project]\nname = \"demo\"\ndependencies = [\"pandas\", \"pyarrow\"]\n";
    fs::write(sandbox.path().join("pyproject.toml"), pyproject).unwrap();
    let project_before = fs::read(sandbox.path().join("dowai_project.json")).unwrap();

    let report = validate_project_with_configuration(sandbox.path(), None, None);
    assert!(
        report.project.is_some(),
        "project must load: {:?}",
        report.diagnostics
    );

    let diagnostic = report
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "DOWAI_PYPROJECT_DIVERGENCE")
        .unwrap_or_else(|| panic!("expected divergence, got {:?}", report.diagnostics));
    let message = &diagnostic.message;
    assert!(
        message.contains("somente em dowai_project.json: polars"),
        "{message}"
    );
    assert!(
        message.contains("somente em pyproject.toml: pyarrow"),
        "{message}"
    );
    assert!(message.contains("nothing was merged"), "{message}");
    assert_eq!(
        fs::read_to_string(sandbox.path().join("pyproject.toml")).unwrap(),
        pyproject
    );
    assert_eq!(
        fs::read(sandbox.path().join("dowai_project.json")).unwrap(),
        project_before
    );
}

#[test]
fn equivalent_declarations_produce_no_divergence() {
    let sandbox = Sandbox::new("equal");
    fs::write(
        sandbox.path().join("pyproject.toml"),
        "[project]\nname = \"demo\"\ndependencies = [\"pandas==2.2\", \"POLARS\"]\n",
    )
    .unwrap();

    let report = validate_project_with_configuration(sandbox.path(), None, None);
    assert!(
        report.project.is_some(),
        "project must load: {:?}",
        report.diagnostics
    );

    assert!(
        !codes(&report)
            .iter()
            .any(|code| code.starts_with("DOWAI_PYPROJECT")),
        "{:?}",
        codes(&report)
    );
}

#[test]
fn missing_pyproject_is_not_a_divergence() {
    let sandbox = Sandbox::new("absent");

    let report = validate_project_with_configuration(sandbox.path(), None, None);
    assert!(
        report.project.is_some(),
        "project must load: {:?}",
        report.diagnostics
    );

    assert!(
        !codes(&report)
            .iter()
            .any(|code| code.starts_with("DOWAI_PYPROJECT")),
        "{:?}",
        codes(&report)
    );
}
