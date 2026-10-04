use dockstride::{config, state};
use serde_json::{Value, json};
use std::fs;
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

fn fixture() -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("compose.ncl"),
        r#"
let configContract = {
  project | String | doc "Deployment identity.",
  backend | String | default = "compose",
  apiPort | Number | doc "API host port." | default = 8080,
  oauth | {
    enabled | Bool | default = false,
    issuer | String | doc "OAuth provider.",
  },
} in
let env | configContract = import "env.yaml" in
{
  dockstride | not_exported = { Config = configContract },
  name = env.project,
  services.api.image = "nginx:alpine",
}
"#,
    )
    .unwrap();
    directory
}

fn field(list: &Value, path: &str) -> Value {
    list["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap()
        .clone()
}

#[test]
fn discovery_defaults_and_incremental_nested_configuration() {
    let directory = fixture();
    let root = directory.path();
    let initial = config::list(root).unwrap();
    assert_eq!(field(&initial, "project")["origin"], "missing");
    assert_eq!(field(&initial, "apiPort")["value"], 8080);
    assert_eq!(field(&initial, "apiPort")["origin"], "default");
    assert_eq!(field(&initial, "apiPort")["doc"], "API host port.");
    config::set(root, "oauth.enabled", json!(true)).unwrap();
    assert_eq!(config::get(root, "oauth.enabled").unwrap()["value"], true);
    assert_eq!(config::get(root, "project").unwrap()["origin"], "missing");
    config::set(root, "project", json!("app-alice")).unwrap();
    config::set(root, "oauth.issuer", json!("https://issuer.example")).unwrap();
    assert!(
        config::list(root).unwrap()["missing"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    config::unset(root, "oauth.enabled").unwrap();
    assert_eq!(config::get(root, "oauth.enabled").unwrap()["value"], false);
    assert_eq!(
        config::get(root, "oauth.enabled").unwrap()["origin"],
        "default"
    );
}

#[test]
fn scalar_edits_preserve_comments_order_quotes_and_invalid_candidates() {
    let directory = fixture();
    let root = directory.path();
    let source = "# environment\nproject: app # identity\napiPort: 8080 # host port\noauth:\n  # useful notes\n  'enabled': false # switch\n  issuer: 'https://issuer.example/#fragment'\n";
    fs::write(root.join("env.yaml"), source).unwrap();
    config::set(root, "apiPort", json!(8081)).unwrap();
    config::set(root, "oauth.enabled", json!(true)).unwrap();
    let expected = source
        .replace("apiPort: 8080", "apiPort: 8081")
        .replace("'enabled': false", "'enabled': true");
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), expected);
    assert!(config::set(root, "oauth.enabled", json!("not a boolean")).is_err());
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), expected);
    assert!(config::unset(root, "project").is_err());
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), expected);
}

#[test]
fn insertion_keeps_sibling_and_header_comments() {
    let directory = fixture();
    let root = directory.path();
    fs::write(root.join("env.yaml"),"# retained\nproject: app\noauth: # provider options\n  enabled: false\napiPort: 9000 # pinned\n").unwrap();
    config::set(root, "oauth.issuer", json!("https://issuer.example")).unwrap();
    let text = fs::read_to_string(root.join("env.yaml")).unwrap();
    assert!(
        text.starts_with("# retained\nproject: app\noauth: # provider options\n  enabled: false\n")
    );
    assert!(text.ends_with("apiPort: 9000 # pinned\n"));
    assert_eq!(
        config::read_env(root).unwrap()["oauth"]["issuer"],
        "https://issuer.example"
    );
}

#[test]
fn setup_reports_structured_missing_and_keeps_valid_incremental_inputs() {
    let directory = fixture();
    let root = directory.path();
    let error = config::setup(root, &["oauth.enabled=true".into()], true).unwrap_err();
    let missing = error.downcast_ref::<config::MissingInputs>().unwrap();
    assert!(missing.fields.iter().any(|field| field.path == "project"));
    assert!(
        missing
            .fields
            .iter()
            .any(|field| field.path == "oauth.issuer")
    );
    assert_eq!(config::read_env(root).unwrap()["oauth"]["enabled"], true);
    let result = config::setup(
        root,
        &[
            "project=app".into(),
            "oauth.issuer=https://issuer.example".into(),
        ],
        true,
    )
    .unwrap();
    assert_eq!(result["complete"], true);
}

#[test]
fn simultaneous_edits_do_not_lose_each_others_fields() {
    let directory = fixture();
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for (path, value) in [
        ("project", json!("app")),
        ("oauth.issuer", json!("https://issuer.example")),
    ] {
        let root = directory.path().to_owned();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            config::set(&root, path, value).unwrap();
        }));
    }
    barrier.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    let env = config::read_env(directory.path()).unwrap();
    assert_eq!(env["project"], "app");
    assert_eq!(env["oauth"]["issuer"], "https://issuer.example");
}

#[test]
fn owned_resources_prevent_identity_and_backend_transitions() {
    let directory = fixture();
    let root = directory.path();
    config::setup(
        root,
        &[
            "project=app".into(),
            "oauth.issuer=https://issuer.example".into(),
        ],
        true,
    )
    .unwrap();
    state::ensure_identity(root, "app", "compose", "default").unwrap();
    state::mark_resources(root, true).unwrap();
    let original = fs::read(root.join("env.yaml")).unwrap();
    assert!(config::set(root, "project", json!("other")).is_err());
    assert!(config::set(root, "backend", json!("swarm")).is_err());
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), original);
    config::set(root, "apiPort", json!(9000)).unwrap();
    assert_eq!(config::get(root, "apiPort").unwrap()["value"], 9000);
}

#[test]
fn plaintext_secret_configuration_is_rejected_without_publication() {
    let directory = fixture();
    let root = directory.path();
    assert!(config::set(root, "secrets.key", json!("password")).is_err());
    assert!(!root.join("env.yaml").exists());
    fs::write(root.join("env.yaml"), "secrets:\n  key: password\n").unwrap();
    assert!(config::read_env(root).is_err());
}

#[test]
fn project_proposals_are_stable_and_worktree_isolated() {
    let first = fixture();
    let second = fixture();
    let first_name = config::project_proposal(first.path()).unwrap();
    assert_eq!(config::project_proposal(first.path()).unwrap(), first_name);
    assert_ne!(config::project_proposal(second.path()).unwrap(), first_name);
    assert!(
        first_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    );
}

#[test]
fn unset_last_nested_override_preserves_empty_record_and_effective_default() {
    let directory = fixture();
    let root = directory.path();
    fs::write(
        root.join("env.yaml"),
        "oauth: # settings\n  enabled: true # override\n",
    )
    .unwrap();
    config::unset(root, "oauth.enabled").unwrap();
    assert_eq!(config::read_env(root).unwrap(), json!({"oauth":{}}));
    assert_eq!(config::get(root, "oauth.enabled").unwrap()["value"], false);
    assert!(
        fs::read_to_string(root.join("env.yaml"))
            .unwrap()
            .contains("# settings")
    );
    config::unset(root, "apiPort").unwrap();
    assert_eq!(config::read_env(root).unwrap(), json!({"oauth":{}}));
}

#[test]
fn nested_flow_record_edits_and_unsets_preserve_other_top_level_fields() {
    let directory = fixture();
    let root = directory.path();
    fs::write(root.join("env.yaml"),"project: app # identity\noauth: {enabled: true, issuer: example}\napiPort: 9090 # endpoint\n").unwrap();
    config::unset(root, "oauth.enabled").unwrap();
    assert_eq!(config::get(root, "oauth.enabled").unwrap()["value"], false);
    config::set(root, "oauth.enabled", json!(true)).unwrap();
    assert_eq!(
        config::read_env(root).unwrap()["oauth"]["issuer"],
        "example"
    );
    let text = fs::read_to_string(root.join("env.yaml")).unwrap();
    assert!(text.starts_with("project: app # identity\n"));
    assert!(text.ends_with("apiPort: 9090 # endpoint\n"));
}

#[test]
fn complete_candidate_model_failure_does_not_overwrite_valid_environment() {
    let directory = fixture();
    let root = directory.path();
    let source = fs::read_to_string(root.join("compose.ncl")).unwrap();
    fs::write(root.join("compose.ncl"),source.replace("services.api.image = \"nginx:alpine\"","services.api.image | String = if env.apiPort == 8080 then \"nginx:alpine\" else env.apiPort")).unwrap();
    config::setup(
        root,
        &["project=app".into(), "oauth.issuer=example".into()],
        true,
    )
    .unwrap();
    let original = fs::read(root.join("env.yaml")).unwrap();
    assert!(config::set(root, "apiPort", json!(9000)).is_err());
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), original);
    assert_eq!(config::get(root, "apiPort").unwrap()["value"], 8080);
}

#[test]
fn operational_identity_persists_and_journal_records_completed_phases() {
    let directory = fixture();
    let root = directory.path();
    let first = state::ensure_identity(root, "app", "compose", "default").unwrap();
    state::journal(
        root,
        "deploy",
        &json!({"phase":"build","status":"complete"}),
    )
    .unwrap();
    state::journal(root, "deploy", &json!({"phase":"apply","status":"started"})).unwrap();
    assert_eq!(
        state::ensure_identity(root, "app", "compose", "default").unwrap()["id"],
        first["id"]
    );
    state::mark_resources(root, true).unwrap();
    assert!(state::ensure_identity(root, "other", "compose", "default").is_err());
    assert!(state::ensure_identity(root, "app", "compose", "other-context").is_err());
    let events = state::journal_events(root, "deploy").unwrap();
    assert_eq!(
        events[0]["event"],
        json!({"phase":"build","status":"complete"})
    );
    assert_eq!(
        events[1]["event"],
        json!({"phase":"apply","status":"started"})
    );
    state::mark_resources(root, false).unwrap();
    assert_eq!(
        state::ensure_identity(root, "app", "swarm", "default").unwrap()["id"],
        first["id"]
    );
}

#[test]
fn record_replacement_cannot_remove_required_inputs_from_complete_environment() {
    let directory = fixture();
    let root = directory.path();
    config::setup(
        root,
        &["project=app".into(), "oauth.issuer=example".into()],
        true,
    )
    .unwrap();
    let original = fs::read(root.join("env.yaml")).unwrap();
    assert!(config::set(root, "oauth", json!({})).is_err());
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), original);
    assert!(config::setup(root, &["oauth={}".into()], true).is_err());
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), original);
    let incomplete = fixture();
    config::set(incomplete.path(), "oauth.enabled", json!(true)).unwrap();
    config::set(incomplete.path(), "oauth", json!({})).unwrap();
    assert_eq!(
        config::get(incomplete.path(), "oauth.enabled").unwrap()["value"],
        false
    );
}

#[test]
fn lifecycle_lock_serializes_identity_edits_but_not_port_updates() {
    let directory = fixture();
    let root = directory.path();
    config::setup(
        root,
        &["project=app".into(), "oauth.issuer=example".into()],
        true,
    )
    .unwrap();
    let lifecycle = state::lock(root, "lifecycle").unwrap();
    let identity_root = root.to_owned();
    let (identity_tx, identity_rx) = std::sync::mpsc::channel();
    let identity_thread = std::thread::spawn(move || {
        identity_tx
            .send(config::set(&identity_root, "project", json!("other")))
            .unwrap();
    });
    let port_root = root.to_owned();
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let port_thread = std::thread::spawn(move || {
        port_tx
            .send(config::set(&port_root, "apiPort", json!(9091)))
            .unwrap();
    });
    let port_result = port_rx.recv_timeout(std::time::Duration::from_secs(5));
    state::ensure_identity(root, "app", "compose", "default").unwrap();
    state::mark_resources(root, true).unwrap();
    drop(lifecycle);
    port_result.unwrap().unwrap();
    assert!(
        identity_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .is_err()
    );
    port_thread.join().unwrap();
    identity_thread.join().unwrap();
    assert_eq!(config::get(root, "project").unwrap()["value"], "app");
    assert_eq!(config::get(root, "apiPort").unwrap()["value"], 9091);
}

#[test]
fn secret_cluster_pinning_preserves_identity_and_rejects_cluster_changes() {
    let directory = fixture();
    let root = directory.path();
    let original = state::ensure_identity(root, "app", "swarm", "default").unwrap();
    state::pin_secret_cluster(root, "cluster-a").unwrap();
    state::pin_secret_cluster(root, "cluster-a").unwrap();
    assert!(state::pin_secret_cluster(root, "cluster-b").is_err());
    let identity = state::read(root, "identity").unwrap();
    assert_eq!(identity["id"], original["id"]);
    assert_eq!(identity["secretCluster"], "cluster-a");
}

#[test]
fn transition_probes_pinned_context_name_and_rejects_endpoint_retargeting() {
    use std::os::unix::fs::PermissionsExt;
    let directory = fixture();
    let root = directory.path();
    config::setup(
        root,
        &["project=app".into(), "oauth.issuer=example".into()],
        true,
    )
    .unwrap();
    state::ensure_identity(
        root,
        "app",
        "compose",
        "fixture;unix:///fixture-docker.sock",
    )
    .unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let docker = bin.join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
if [ "$1" = "--context" ]; then
  [ "$2" = "fixture" ] || exit 21
  shift 2
elif [ "$1" != "context" ]; then
  exit 22
fi
if [ "$1" = "context" ] && [ "$2" = "inspect" ]; then
  [ "$3" = "fixture" ] || exit 23
  printf '%s\n' "$FIXTURE_DOCKER_ENDPOINT"
  exit 0
fi
case "$1" in ps|volume|network) exit 0;; *) exit 24;; esac
"#,
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let launch = |name: &str, endpoint: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_dks"))
            .args([
                "--json",
                "--directory",
                root.to_str().unwrap(),
                "config",
                "set",
                "project",
                name,
            ])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST")
            .env("FIXTURE_DOCKER_ENDPOINT", endpoint)
            .output()
            .unwrap()
    };
    let changed = launch("new-app", "unix:///fixture-docker.sock");
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert_eq!(config::get(root, "project").unwrap()["value"], "new-app");
    let original = fs::read(root.join("env.yaml")).unwrap();
    assert!(
        !launch("another-app", "unix:///different-docker.sock")
            .status
            .success()
    );
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), original);
}

#[test]
fn state_inspection_rejects_symlinks_and_does_not_create_absent_state() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = fixture();
    let root = directory.path();
    assert!(state::read(root, "identity").unwrap().is_null());
    assert!(!root.join(".dockstride").exists());
    state::save(root, "identity", &json!({"id":"trusted-owner"})).unwrap();
    let foreign = root.join("foreign.json");
    fs::write(&foreign, "{\"id\":\"foreign-owner\"}").unwrap();
    fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(root.join(".dockstride/identity.json")).unwrap();
    symlink(&foreign, root.join(".dockstride/identity.json")).unwrap();
    assert!(state::read(root, "identity").is_err());
    fs::rename(root.join(".dockstride"), root.join("original-state")).unwrap();
    symlink(root.join("original-state"), root.join(".dockstride")).unwrap();
    assert!(state::read(root, "identity").is_err());
}

#[test]
fn transition_probe_pins_recorded_host_and_tls_instead_of_current_context() {
    use std::os::unix::fs::PermissionsExt;
    let directory = fixture();
    let root = directory.path();
    config::setup(
        root,
        &["project=app".into(), "oauth.issuer=example".into()],
        true,
    )
    .unwrap();
    state::ensure_identity(
        root,
        "app",
        "compose",
        "host;DOCKER_HOST=tcp://original-host:2376",
    )
    .unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let docker = bin.join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
[ -z "$DOCKER_CONTEXT" ] || exit 31
[ "$DOCKER_HOST" = "tcp://original-host:2376" ] || exit 32
[ "$DOCKER_TLS_VERIFY" = "1" ] || exit 33
[ "$DOCKER_CERT_PATH" = "/fixture/certs" ] || exit 34
case "$1" in ps|volume|network) exit 0;; *) exit 35;; esac
"#,
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let changed = std::process::Command::new(env!("CARGO_BIN_EXE_dks"))
        .args([
            "--json",
            "--directory",
            root.to_str().unwrap(),
            "config",
            "set",
            "project",
            "new-app",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("DOCKER_CONTEXT", "different-context")
        .env("DOCKER_HOST", "tcp://different-host:2376")
        .env("DOCKER_TLS_VERIFY", "1")
        .env("DOCKER_CERT_PATH", "/fixture/certs")
        .output()
        .unwrap();
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert_eq!(config::get(root, "project").unwrap()["value"], "new-app");
}

#[test]
fn flow_root_mapping_edits_preserve_data_and_comments_instead_of_appending_a_second_document() {
    for mapping in ["{}", "{project: flow-config, apiPort: 8181}"] {
        let directory = fixture();
        fs::write(
            directory.path().join("env.yaml"),
            format!("# environment choices\n---\n{mapping} # initial values\n# retained footer\n"),
        )
        .unwrap();
        config::set(directory.path(), "apiPort", json!(8182)).unwrap();
        config::set(directory.path(), "oauth.enabled", json!(true)).unwrap();
        let value = config::read_env(directory.path()).unwrap();
        assert_eq!(value["apiPort"], 8182);
        assert_eq!(value["oauth"]["enabled"], true);
        if mapping != "{}" {
            assert_eq!(value["project"], "flow-config");
        }
        let text = fs::read_to_string(directory.path().join("env.yaml")).unwrap();
        for comment in [
            "# environment choices",
            "# initial values",
            "# retained footer",
        ] {
            assert!(text.contains(comment), "{text}");
        }
    }
}
