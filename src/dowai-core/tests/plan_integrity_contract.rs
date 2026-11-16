//! ENT-024: symlinks que escapam do diretório de modelos são recusados e modelo alterado
//! após o plano gera erro com código estável (plano §6.1 e §12.3, AC030).

use dowai_core::{plan, verify_planned_model, ExecutionMode};
use std::fs;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "dowai-integrity-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("project").join("models")).expect("create models directory");
        fs::write(
            path.join("project").join("dowai_project.json"),
            r#"{"schema_version":2,"name":"fixture","models_dir":"models"}"#,
        )
        .expect("write project configuration");
        Self(path)
    }

    fn project(&self) -> PathBuf {
        self.0.join("project")
    }

    fn models(&self) -> PathBuf {
        self.project().join("models")
    }

    /// Diretório irmão do projeto: fora da raiz e do diretório de modelos.
    /// Usado só por testes de symlink, restritos a Unix (ENT-122).
    #[cfg(unix)]
    fn outside(&self) -> PathBuf {
        let dir = self.0.join("outside");
        fs::create_dir_all(&dir).expect("create external directory");
        dir
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create symlink");
}

#[cfg(unix)]
#[test]
fn external_file_symlink_inside_models_is_refused() {
    let sandbox = Sandbox::new("file-link");
    let external = sandbox.outside().join("secret.sql");
    fs::write(&external, "select 1").expect("write external model");
    symlink(&external, &sandbox.models().join("linked.sql"));

    let error = plan(
        &sandbox.project(),
        &["all".into()],
        ExecutionMode::Automatic,
    )
    .expect_err("external symlink must be refused");

    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
}

#[cfg(unix)]
#[test]
fn external_directory_symlink_inside_models_is_refused() {
    let sandbox = Sandbox::new("dir-link");
    let external = sandbox.outside();
    fs::write(external.join("other.sql"), "select 2").expect("write external model");
    symlink(&external, &sandbox.models().join("linked_dir"));

    let error = plan(
        &sandbox.project(),
        &["all".into()],
        ExecutionMode::Automatic,
    )
    .expect_err("external directory symlink must be refused");

    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
}

#[cfg(unix)]
#[test]
fn explicit_target_through_external_symlink_is_refused() {
    let sandbox = Sandbox::new("target-link");
    let external = sandbox.outside().join("secret.sql");
    fs::write(&external, "select 1").expect("write external model");
    symlink(&external, &sandbox.models().join("linked.sql"));

    let error = plan(
        &sandbox.project(),
        &["linked.sql".into()],
        ExecutionMode::Automatic,
    )
    .expect_err("explicit external symlink target must be refused");

    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
}

#[cfg(unix)]
#[test]
fn internal_symlink_is_tolerated_and_target_is_planned_once() {
    let sandbox = Sandbox::new("internal-link");
    fs::write(sandbox.models().join("real.sql"), "select 1").expect("write model");
    symlink(
        &sandbox.models().join("real.sql"),
        &sandbox.models().join("alias.sql"),
    );

    let plan = plan(
        &sandbox.project(),
        &["all".into()],
        ExecutionMode::Automatic,
    )
    .expect("internal symlink must not break planning");

    let ids: Vec<&str> = plan.nodes.iter().map(|node| node.id.as_str()).collect();
    assert_eq!(ids, vec!["real.sql"]);
}

#[test]
fn unchanged_model_verifies_after_planning() {
    let sandbox = Sandbox::new("unchanged");
    fs::write(sandbox.models().join("model.sql"), "select 1").expect("write model");

    let plan = plan(
        &sandbox.project(),
        &["all".into()],
        ExecutionMode::Automatic,
    )
    .expect("plan should succeed");
    let node = &plan.nodes[0];

    let content = verify_planned_model(&sandbox.project(), node).expect("unchanged model");
    assert_eq!(content, "select 1");
}

#[test]
fn model_changed_after_planning_is_refused_with_stable_code() {
    let sandbox = Sandbox::new("changed");
    fs::write(sandbox.models().join("model.sql"), "select 1").expect("write model");

    let plan = plan(
        &sandbox.project(),
        &["all".into()],
        ExecutionMode::Automatic,
    )
    .expect("plan should succeed");
    fs::write(sandbox.models().join("model.sql"), "select 999").expect("alter model");

    let error = verify_planned_model(&sandbox.project(), &plan.nodes[0])
        .expect_err("altered model must be refused");

    assert_eq!(error.code(), "DOWAI_PLAN_MODEL_CHANGED");
}

#[cfg(unix)]
#[test]
fn model_replaced_by_external_symlink_after_planning_is_refused() {
    let sandbox = Sandbox::new("swap-link");
    let model = sandbox.models().join("model.sql");
    fs::write(&model, "select 1").expect("write model");

    let plan = plan(
        &sandbox.project(),
        &["all".into()],
        ExecutionMode::Automatic,
    )
    .expect("plan should succeed");
    let external = sandbox.outside().join("replacement.sql");
    fs::write(&external, "select 999").expect("write replacement");
    fs::remove_file(&model).expect("remove original model");
    symlink(&external, &model);

    let error = verify_planned_model(&sandbox.project(), &plan.nodes[0])
        .expect_err("model swapped for external symlink must be refused");

    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
}
