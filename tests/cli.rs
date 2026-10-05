use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn cli(root: &Path, args: &[&str]) -> Output {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("test-bin");
    fs::create_dir_all(&bin).unwrap();
    let docker = bin.join("docker");
    if !docker.exists() {
        fs::write(&docker, "#!/bin/sh\ncase \"$1\" in\n info) printf 'cli-fixture-daemon\\n';;\n *) exit 0;;\nesac\n").unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    }
    Command::new(env!("CARGO_BIN_EXE_dks"))
        .arg("--directory")
        .arg(root)
        .args(["--json", "--non-interactive", "--no-color"])
        .args(args)
        .env("NO_COLOR", "1")
        .env("HOME", root.join("test-home"))
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
        .env_remove("DOCKER_CONTEXT")
        .env("DOCKER_HOST", "unix:///cli-fixture.sock")
        .output()
        .expect("start dks")
}

fn terminal(output: &Output) -> Value {
    let text = std::str::from_utf8(&output.stdout).expect("UTF-8 output");
    let records: Vec<Value> = text
        .lines()
        .map(|line| {
            let record: Value = serde_json::from_str(line).expect("every stdout line is JSON");
            assert_eq!(record["schemaVersion"], 1);
            record
        })
        .collect();
    records.last().expect("terminal record").clone()
}

fn success(root: &Path, args: &[&str]) -> Value {
    let output = cli(root, args);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let record = terminal(&output);
    assert_eq!(record["type"], "result");
    assert_eq!(record["ok"], true);
    record["result"].clone()
}

fn initialized() -> TempDir {
    let dir = TempDir::new().unwrap();
    let result = success(dir.path(), &["init"]);
    assert_eq!(
        result["created"],
        serde_json::json!([
            "compose.ncl",
            "libs/dockstride.ncl",
            "env.yaml",
            ".gitignore"
        ])
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("env.yaml")).unwrap(),
        "{}\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
        "/env.yaml\n/.dockstride/\n"
    );
    assert!(dir.path().join("compose.ncl").is_file());
    assert!(dir.path().join("libs/dockstride.ncl").is_file());
    assert!(!dir.path().join("app").exists());
    dir
}

#[test]
fn schema_and_plan_work_with_empty_environment_without_publishing_configuration() {
    let dir = initialized();
    let schema = success(dir.path(), &["config", "schema"]);
    let fields = schema["fields"].as_array().unwrap();
    assert_eq!(
        fields
            .iter()
            .map(|field| field["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["apiPort", "backend", "project"]
    );
    assert!(
        fields
            .iter()
            .any(|field| field["path"] == "apiPort" && field["default"] == 8080)
    );
    assert!(
        fields
            .iter()
            .any(|field| field["path"] == "project" && field["required"] == true)
    );
    assert!(
        fields
            .iter()
            .any(|field| field["path"] == "backend" && field["default"] == "compose")
    );
    let plan = success(dir.path(), &["up", "--plan"]);
    assert_eq!(plan["sideEffects"], false);
    assert!(plan.get("unresolved").is_some());
    assert_eq!(
        fs::read_to_string(dir.path().join("env.yaml")).unwrap(),
        "{}\n"
    );
}

#[test]
fn config_candidates_are_atomic_and_edits_preserve_comments() {
    let dir = initialized();
    fs::write(dir.path().join("env.yaml"), "# checkout settings\nproject: cli-contract # keep identity comment\n# port selection\napiPort: 8123\n").unwrap();
    success(dir.path(), &["config", "set", "apiPort", "8124"]);
    let changed = fs::read_to_string(dir.path().join("env.yaml")).unwrap();
    assert!(changed.contains("# checkout settings"));
    assert!(changed.contains("# keep identity comment"));
    assert!(changed.contains("# port selection"));
    let result = success(dir.path(), &["config", "get", "apiPort"]);
    assert_eq!(result["value"], 8124);
    let output = cli(dir.path(), &["config", "set", "apiPort", "70000"]);
    assert!(!output.status.success());
    let error = terminal(&output);
    assert_eq!(error["type"], "error");
    assert_eq!(error["ok"], false);
    assert_eq!(
        error["exitCode"].as_i64().unwrap(),
        output.status.code().unwrap() as i64
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("env.yaml")).unwrap(),
        changed
    );
    success(dir.path(), &["config", "unset", "apiPort"]);
    assert_eq!(
        success(dir.path(), &["config", "get", "apiPort"])["value"],
        8080
    );
}

#[test]
fn an_incomplete_checkout_can_be_filled_incrementally() {
    let dir = initialized();
    success(dir.path(), &["config", "set", "apiPort", "8125"]);
    assert_eq!(
        success(dir.path(), &["config", "get", "apiPort"])["value"],
        8125
    );
    success(dir.path(), &["config", "set", "project", "incremental"]);
    let missing = cli(dir.path(), &["config", "unset", "project"]);
    assert!(!missing.status.success());
    assert_eq!(
        success(dir.path(), &["config", "get", "project"])["value"],
        "incremental"
    );
}

#[test]
fn init_refuses_to_overwrite_checked_in_application_code() {
    let dir = initialized();
    let path = dir.path().join("compose.ncl");
    fs::write(&path, "# application-owned definition\n").unwrap();
    let output = cli(dir.path(), &["init"]);
    assert!(!output.status.success());
    assert_eq!(terminal(&output)["type"], "error");
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "# application-owned definition\n"
    );
}

#[test]
fn managed_execution_reaches_setup_and_unknown_commands_are_not_passthrough() {
    let dir = initialized();
    for command in ["up", "dev", "deploy"] {
        let output = cli(dir.path(), &[command]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(terminal(&output)["category"], "configuration");
        assert_eq!(
            fs::read_to_string(dir.path().join("env.yaml")).unwrap(),
            "{}\n"
        );
    }
    let output = cli(dir.path(), &["definitely-not-a-docker-command"]);
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("env.yaml")).unwrap(),
        "{}\n"
    );
}

#[test]
fn structured_failure_retains_the_engine_diagnostic_without_unbounded_history() {
    use std::os::unix::fs::PermissionsExt;
    let dir = initialized();
    fs::write(
        dir.path().join("env.yaml"),
        "project: stderr-contract\nbackend: swarm\n",
    )
    .unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let docker = bin.join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
if [ "$1" = "--context" ]; then shift 2; fi
case "$1 $2" in
  "context show") printf 'fixture\n' ;;
  "context inspect") printf 'unix:///fixture.sock\n' ;;
  *)
    printf 'OBSOLETE_DIAGNOSTIC\n' >&2
    printf '%140000s' '' >&2
    printf '\nservice migrate failed: exit 23\n' >&2
    exit 23 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dks"))
        .arg("-C")
        .arg(dir.path())
        .args(["--json", "--non-interactive", "stack", "services"])
        .env("HOME", dir.path().join("test-home"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_HOST")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let error = terminal(&output);
    assert_eq!(error["details"]["underlyingDockerStatus"], 23);
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("service migrate failed: exit 23"),
        "{message}"
    );
    assert!(!message.contains("OBSOLETE_DIAGNOSTIC"));
    assert!(message.len() < 132_000);
}

#[test]
fn starter_renders_with_only_project_configured() {
    let dir = initialized();
    success(dir.path(), &["config", "set", "project", "hello-cli"]);
    for target in ["compose", "swarm"] {
        let rendered = success(dir.path(), &["render", "--target", target]);
        assert_eq!(
            rendered["services"]["hello"]["image"],
            "hashicorp/http-echo:1.0.0"
        );
        assert_eq!(
            rendered["services"]["hello"]["ports"],
            serde_json::json!(["8080:5678"])
        );
        assert!(rendered.get("secrets").is_none());
        assert!(rendered["services"]["hello"].get("build").is_none());
        assert!(rendered["services"]["hello"].get("develop").is_none());
    }
}

#[test]
fn named_command_runs_with_incomplete_configuration_without_applying_proposals() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("env.yaml"), "# untouched\n{}\n").unwrap();
    fs::write(dir.path().join("compose.ncl"), r#"
let env = import "env.yaml" in {
  dockstride = {
    Config = { project | String },
    commands.probe.argv = ["python3", "probe.py"],
    actions = [{argv = [env.project]}],
  },
  services.api.image = env.project,
}
"#).unwrap();
    fs::write(dir.path().join("probe.py"), r#"import json, os, sys
context = json.load(sys.stdin)
assert context["purpose"] == "manual"
assert "secrets" not in context["settings"]
print(json.dumps({"schemaVersion":1,"values":{"project":context["projectProposal"]},"connection":os.environ["DOCKER_HOST"]}))
"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dks"))
        .args(["--json", "--non-interactive", "-C"]).arg(dir.path())
        .args(["run", "probe"])
        .env_remove("DOCKER_CONTEXT").env("DOCKER_HOST", "unix:///not-a-daemon.sock")
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    let result = terminal(&output);
    assert_eq!(result["result"]["connection"], "unix:///not-a-daemon.sock");
    assert_eq!(fs::read_to_string(dir.path().join("env.yaml")).unwrap(), "# untouched\n{}\n");
}

#[test]
fn named_command_rejects_oversized_or_multiple_json_documents() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("compose.ncl"), r#"{
  dockstride = {Config = {}, commands.probe.argv = ["python3", "probe.py"]},
  services = {},
}"#).unwrap();
    for source in [
        "print('{\"schemaVersion\":1} {\"schemaVersion\":1}')",
        "print('{\"schemaVersion\":1,\"data\":\"' + 'a'*1048576 + '\"}')",
    ] {
        fs::write(dir.path().join("probe.py"), source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_dks"))
            .args(["--json", "--non-interactive", "-C"]).arg(dir.path())
            .args(["run", "probe"])
            .env_remove("DOCKER_CONTEXT").env("DOCKER_HOST", "unix:///not-a-daemon.sock")
            .output().unwrap();
        assert!(!output.status.success());
        assert_eq!(terminal(&output)["type"], "error");
        assert!(!dir.path().join("env.yaml").exists());
    }
}
