//! ENT-017: `init --workspace` sem root e recusa de caminhos fora da raiz autorizada.

use dowai_core::create_project;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("dowai-ent017-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("create scratch directory");
        Self(path)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn init_creates_workspace_under_user_writable_directory() {
    let scratch = ScratchDir::new();
    let workspace = scratch.0.join("ws");

    let root = create_project(&workspace, "demo").expect("init without root");

    assert!(root.join("dowai_project.json").is_file());
    assert!(workspace.join("workspace.json").is_file());
}

#[test]
fn init_refuses_parent_directory_components() {
    let scratch = ScratchDir::new();
    let workspace = scratch.0.join("a").join("..").join("ws");

    let error = create_project(&workspace, "demo").expect_err("reject ..");

    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
    assert!(!scratch.0.join("ws").exists());
}

#[test]
fn init_refuses_protected_system_locations() {
    for protected in ["/etc/dowai-ws", "/usr/dowai-ws", "/var/dowai-ws"] {
        let error = create_project(&PathBuf::from(protected), "demo")
            .expect_err("reject protected location");
        assert_eq!(error.code(), "DOWAI_POLICY_PATH", "{protected}");
    }
}

#[test]
fn init_refuses_filesystem_root() {
    let error = create_project(&PathBuf::from("/"), "demo").expect_err("reject /");

    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
}

#[test]
fn init_refuses_symlink_that_resolves_to_protected_location() {
    let scratch = ScratchDir::new();
    let link = scratch.0.join("link");
    std::os::unix::fs::symlink("/etc", &link).expect("create symlink");

    let error = create_project(&link.join("dowai-ws"), "demo").expect_err("reject symlink escape");

    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
    assert!(!PathBuf::from("/etc/dowai-ws").exists());
}
