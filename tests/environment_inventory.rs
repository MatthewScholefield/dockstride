use dockstride::environment;
use serde_json::json;
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(root)
        .env("GIT_AUTHOR_NAME", "Fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.test").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn non_git_inventory_is_empty_without_evaluating_configuration() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("compose.ncl"), "invalid Nickel must not be evaluated").unwrap();
    fs::write(root.path().join("env.yaml"), "not: [valid YAML").unwrap();
    assert_eq!(environment::list(root.path()).unwrap(), json!({
        "schemaVersion":1,"status":"not-git","scope":"invoking-repository","worktrees":[]
    }));
    assert!(!root.path().join(".dockstride").exists());
}

#[test]
fn worktree_inventory_is_repository_scoped_and_nul_safe() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("repository");
    fs::create_dir(&root).unwrap();
    git(&root, &["init"]);
    git(&root, &["commit", "--allow-empty", "-m", "fixture"]);
    let extra = directory.path().join("worktree with spaces\nand newline");
    git(&root, &["worktree", "add", "--detach", extra.to_str().unwrap()]);
    fs::write(extra.join("compose.ncl"), "not evaluated").unwrap();
    fs::write(extra.join("env.yaml"), "not parsed").unwrap();
    let other = directory.path().join("other-repository");
    fs::create_dir(&other).unwrap();
    git(&other, &["init"]);
    git(&other, &["commit", "--allow-empty", "-m", "unrelated"]);
    let result = environment::list(&root).unwrap();
    assert_eq!(result["status"], "available");
    let rows = result["worktrees"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let extra = fs::canonicalize(extra).unwrap();
    assert_eq!(rows.iter().find(|row| row["root"] == extra.to_str().unwrap()).unwrap()["configuration"]["status"], "present");
    let root = fs::canonicalize(root).unwrap();
    assert_eq!(rows.iter().find(|row| row["root"] == root.to_str().unwrap()).unwrap()["configuration"]["status"], "absent");
    assert!(!rows.iter().any(|row| row["root"] == other.to_str().unwrap()));
    assert!(!root.join(".dockstride").exists());
}

#[test]
fn unavailable_checkout_returns_no_inventory_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let result = environment::list(&missing).unwrap();
    assert_eq!(result["status"], "unavailable");
    assert_eq!(result["scope"], "invoking-repository");
    assert_eq!(result["worktrees"], json!([]));
    assert!(result["error"].is_string());
}

#[test]
fn configuration_presence_distinguishes_non_file_from_missing() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init"]);
    fs::create_dir(directory.path().join("compose.ncl")).unwrap();
    let result = environment::list(directory.path()).unwrap();
    let row = &result["worktrees"][0];
    assert_eq!(row["checkout"]["status"], "present");
    assert_eq!(row["configuration"]["status"], "unreadable");
    assert_eq!(row["configuration"]["files"][0]["status"], "unreadable");
    assert_eq!(row["configuration"]["files"][1]["status"], "missing");
}
