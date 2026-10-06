use dockstride::{config, publication, state};
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

fn fixture_docker(root: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("test-bin");
    fs::create_dir_all(&bin).unwrap();
    let docker = bin.join("docker");
    fs::write(&docker, "#!/bin/sh\ncase \"$1\" in\n info) printf 'config-fixture-daemon\\n';;\n *) exit 0;;\nesac\n").unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
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
fn lifecycle_lock_serializes_configuration_publication_and_identity_checks() {
    let directory = fixture();
    let root = directory.path();
    config::setup(
        root,
        &["project=app".into(), "oauth.issuer=example".into()],
        true,
    )
    .unwrap();
    let home = root.join("isolated-home");
    fs::create_dir(&home).unwrap();
    let path = fixture_docker(root);
    let lifecycle = state::lock(root, "lifecycle").unwrap();
    let identity_root = root.to_owned();
    let identity_home = home.clone();
    let identity_path = path.clone();
    let (identity_tx, identity_rx) = std::sync::mpsc::channel();
    let identity_thread = std::thread::spawn(move || {
        identity_tx
            .send(std::process::Command::new(env!("CARGO_BIN_EXE_dks"))
                .args(["--json", "--non-interactive", "-C"]).arg(identity_root)
                .args(["config", "set", "project", "other"])
                .env("PATH", identity_path).env_remove("DOCKER_CONTEXT")
                .env("DOCKER_HOST", "unix:///config-fixture.sock")
                .env("HOME", identity_home).output().unwrap().status.success()).unwrap();
    });
    let port_root = root.to_owned();
    let port_home = home;
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let port_thread = std::thread::spawn(move || {
        port_tx
            .send(std::process::Command::new(env!("CARGO_BIN_EXE_dks"))
                .args(["--json", "--non-interactive", "-C"]).arg(port_root)
                .args(["config", "set", "apiPort", "9091"])
                .env("PATH", path).env_remove("DOCKER_CONTEXT")
                .env("DOCKER_HOST", "unix:///config-fixture.sock")
                .env("HOME", port_home).output().unwrap().status.success()).unwrap();
    });
    let port_blocked = port_rx.recv_timeout(std::time::Duration::from_millis(100)).is_err();
    state::ensure_identity(root, "app", "compose", "host;DOCKER_HOST=unix:///config-fixture.sock").unwrap();
    state::mark_resources(root, true).unwrap();
    drop(lifecycle);
    assert!(port_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap());
    assert!(!identity_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap());
    port_thread.join().unwrap();
    identity_thread.join().unwrap();
    assert!(port_blocked, "configuration changed while lifecycle work held its guard");
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
if [ "$1" = "context" ] && [ "$2" = "show" ]; then printf 'fixture\n'; exit 0; fi
if [ "$1" = "context" ] && [ "$2" = "inspect" ]; then
  [ "$3" = "fixture" ] || exit 23
  printf '%s\n' "$FIXTURE_DOCKER_ENDPOINT"
  exit 0
fi
case "$1" in info) printf 'config-fixture-daemon\n';; ps|volume|network) exit 0;; *) exit 24;; esac
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
            .env("HOME", root.join("isolated-home"))
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
fn transition_probe_pins_previous_host_and_tls_but_cannot_transfer_to_current_context() {
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
case "$1" in info) printf 'config-fixture-daemon\n';; ps|volume|network) exit 0;; *) exit 35;; esac
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
        .env("HOME", root.join("isolated-home"))
        .env("DOCKER_CONTEXT", "different-context")
        .env("DOCKER_HOST", "tcp://different-host:2376")
        .env("DOCKER_TLS_VERIFY", "1")
        .env("DOCKER_CERT_PATH", "/fixture/certs")
        .output()
        .unwrap();
    assert!(
        !changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert_eq!(config::get(root, "project").unwrap()["value"], "app");
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

#[test]
fn recursive_live_layers_replace_units_and_record_winning_and_overridden_origins() {
    let directory = fixture();
    let root = directory.path();
    fs::create_dir(root.join("shared")).unwrap();
    fs::write(root.join("base.yaml"),"oauth: {enabled: true, issuer: base}\nitems: [one, two]\nnullable: base\n").unwrap();
    fs::write(root.join("shared/layer.yaml"),"_dockstride:\n  sources: [{path: ../base.yaml}]\noauth: {issuer: inherited}\nitems: [three]\nnullable: null\n").unwrap();
    fs::write(root.join("env.yaml"),"_dockstride:\n  sources: [{path: shared/layer.yaml}]\nproject: local\noauth: {enabled: false}\n").unwrap();
    let snapshot = dockstride::sources::snapshot(root,None).unwrap();
    assert_eq!(snapshot.values,json!({"project":"local","oauth":{"enabled":false,"issuer":"inherited"},"items":["three"],"nullable":null}));
    let issuer = &snapshot.provenance["oauth.issuer"];
    assert_eq!(issuer.file,fs::canonicalize(root.join("shared/layer.yaml")).unwrap());
    assert_eq!(issuer.overridden[0].file,fs::canonicalize(root.join("base.yaml")).unwrap());
    assert_eq!(snapshot.provenance["oauth.enabled"].file,root.join("env.yaml"));
    fs::write(root.join("base.yaml"),"oauth: {enabled: true, issuer: changed}\nitems: [new]\nnullable: different\n").unwrap();
    assert!(snapshot.verify().is_err());
    fs::write(root.join("shared/layer.yaml"),"_dockstride:\n  sources: [{path: ../base.yaml}]\n").unwrap();
    assert_eq!(dockstride::sources::snapshot(root,None).unwrap().values["oauth"]["issuer"],"changed");
}

#[test]
fn missing_cycle_and_plaintext_shared_secret_sources_fail_without_publication() {
    let directory = fixture();
    let root = directory.path();
    fs::write(root.join("env.yaml"),"_dockstride:\n  sources: [{path: absent.yaml}]\n").unwrap();
    assert!(dockstride::sources::snapshot(root,None).is_err());
    fs::write(root.join("a.yaml"),"_dockstride:\n  sources: [{path: b.yaml}]\n").unwrap();
    fs::write(root.join("b.yaml"),"_dockstride:\n  sources: [{path: a.yaml}]\n").unwrap();
    fs::write(root.join("env.yaml"),"_dockstride:\n  sources: [{path: a.yaml}]\n").unwrap();
    assert!(dockstride::sources::snapshot(root,None).is_err());
    fs::write(root.join("a.yaml"),"secrets:\n  token: raw-credential-bytes\n").unwrap();
    assert!(dockstride::sources::snapshot(root,None).is_err());
    assert_eq!(config::read_env(root).unwrap(),json!({"_dockstride":{"sources":[{"path":"a.yaml"}]}}));
}

#[test]
fn shared_changes_are_live_local_unset_reveals_inheritance_and_comments_survive() {
    let first = fixture();
    let second = fixture();
    let source = first.path().join("settings.yaml");
    fs::write(&source,"# ordinary preferences\napiPort: 8181 # host port\n").unwrap();
    for root in [first.path(),second.path()] {
        fs::write(root.join("env.yaml"),"project: shared-test\noauth: {issuer: example}\n").unwrap();
        config::sources_add(root,&source,false).unwrap();
    }
    config::set_shared(first.path(),"apiPort",json!(8282),None).unwrap();
    assert_eq!(config::get(second.path(),"apiPort").unwrap()["value"],8282);
    assert_eq!(fs::read_to_string(&source).unwrap(),"# ordinary preferences\napiPort: 8282 # host port\n");
    config::set(second.path(),"apiPort",json!(8383)).unwrap();
    let local = config::get(second.path(),"apiPort").unwrap();
    assert_eq!(local["origin"],"env.yaml");
    assert_eq!(local["provenance"]["overridden"][0]["file"],json!(source));
    config::unset(second.path(),"apiPort").unwrap();
    assert_eq!(config::get(second.path(),"apiPort").unwrap()["value"],8282);
    let before = fs::read_to_string(&source).unwrap();
    assert!(config::set_shared(first.path(),"apiPort",json!("wrong"),None).is_err());
    assert_eq!(fs::read_to_string(&source).unwrap(),before);
    assert!(config::read_env(second.path()).unwrap().get("apiPort").is_none());
    assert_eq!(dockstride::nickel::evaluate(second.path(),None).unwrap().env["apiPort"],8282);
}

#[test]
fn source_edit_targets_are_direct_explicit_and_empty_selection_persists() {
    let directory = fixture();
    let root = directory.path();
    fs::write(root.join("env.yaml"),"project: source-edit\noauth: {issuer: example}\n").unwrap();
    fs::write(root.join("transitive.yaml"),"apiPort: 9191\n").unwrap();
    fs::write(root.join("first.yaml"),"_dockstride:\n  sources: [{path: transitive.yaml}]\n").unwrap();
    config::sources_add(root,std::path::Path::new("first.yaml"),false).unwrap();
    config::sources_add(root,std::path::Path::new("second.yaml"),true).unwrap();
    assert_eq!(fs::read_to_string(root.join("second.yaml")).unwrap(),"{}\n");
    assert!(config::set_shared(root,"apiPort",json!(9292),None).is_err());
    assert!(config::set_shared(root,"apiPort",json!(9292),Some(std::path::Path::new("transitive.yaml"))).is_err());
    config::set_shared(root,"apiPort",json!(9292),Some(std::path::Path::new("second.yaml"))).unwrap();
    assert_eq!(config::get(root,"apiPort").unwrap()["value"],9292);
    config::sources_remove(root,std::path::Path::new("second.yaml")).unwrap();
    assert_eq!(config::get(root,"apiPort").unwrap()["value"],9191);
    config::sources_remove(root,std::path::Path::new("first.yaml")).unwrap();
    assert_eq!(config::read_env(root).unwrap()["_dockstride"]["sources"],json!([]));
    assert_eq!(config::get(root,"apiPort").unwrap()["origin"],"default");
    assert!(root.join("first.yaml").exists() && root.join("second.yaml").exists());
}

fn defaults_fixture(script: &str) -> TempDir {
    let directory = fixture();
    let definition = fs::read_to_string(directory.path().join("compose.ncl")).unwrap();
    fs::write(directory.path().join("compose.ncl"),definition.replace(
        "dockstride | not_exported = { Config = configContract }",
        "dockstride | not_exported = { Config = configContract, commands.defaults.argv = [\"python3\", \"defaults.py\"], setup.defaults = {command = \"defaults\", fields = [\"project\"], sources = true} }",
    )).unwrap();
    fs::write(directory.path().join("defaults.py"),script).unwrap();
    directory
}

#[test]
fn setup_resolves_proposed_sources_before_gaps_and_never_flattens_them() {
    let directory = defaults_fixture(r#"import json,sys
json.load(sys.stdin)
with open('calls','a') as f: f.write('run\n')
print(json.dumps({'schemaVersion':1,'values':{'project':'generated'},'sources':[{'path':'shared.yaml'}]}))
"#);
    let root = directory.path();
    fs::write(root.join("shared.yaml"),"project: inherited\noauth: {issuer: source-issuer}\n").unwrap();
    assert_eq!(config::setup(root,&[],true).unwrap()["complete"],true);
    assert!(config::read_env(root).unwrap().get("project").is_none());
    assert_eq!(config::get(root,"project").unwrap()["value"],"inherited");
    config::setup(root,&[],true).unwrap();
    assert_eq!(fs::read_to_string(root.join("calls")).unwrap(),"run\n");
}

#[test]
fn invalid_generated_candidate_creates_no_source_or_hook_values() {
    let directory = defaults_fixture(r#"import json,sys
json.load(sys.stdin)
print(json.dumps({'schemaVersion':1,'values':{'project':27},'sources':[{'path':'new.yaml','createIfMissing':True}]}))
"#);
    let root = directory.path();
    let original = "# explicit settings\noauth: {issuer: example}\n";
    fs::write(root.join("env.yaml"),original).unwrap();
    assert!(config::setup(root,&[],true).is_err());
    assert!(!root.join("new.yaml").exists());
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(),original);
}

#[test]
fn reads_render_and_plan_do_not_run_defaults_and_explicit_empty_sources_disable_discovery() {
    let directory = defaults_fixture("raise RuntimeError('must not run')\n");
    let root = directory.path();
    fs::write(root.join("env.yaml"),"project: explicit\noauth: {issuer: example}\n").unwrap();
    config::list(root).unwrap();
    config::get(root,"project").unwrap();
    dockstride::nickel::evaluate(root,None).unwrap();
    let candidate = config::setup_plan_candidate(root,&["_dockstride.sources=[]".into()]).unwrap();
    assert_eq!(dockstride::defaults::plan(root,&candidate).unwrap()["wouldRun"],false);
    config::setup(root,&["_dockstride.sources=[]".into()],true).unwrap();
    assert_eq!(config::read_env(root).unwrap()["_dockstride"]["sources"],json!([]));
}

#[test]
fn concurrent_supplied_values_win_without_rerunning_the_hook() {
    let directory = defaults_fixture(r#"import json,sys
from pathlib import Path
json.load(sys.stdin)
with open('calls','a') as f: f.write('run\n')
# Simulate another publisher during the unlocked command execution.
Path('env.yaml').write_text('project: concurrent\noauth: {issuer: concurrent-issuer}\n_dockstride: {sources: []}\n')
print(json.dumps({'schemaVersion':1,'values':{'project':'generated'},'sources':[]}))
"#);
    let root = directory.path();
    config::setup(root,&[],true).unwrap();
    assert_eq!(config::get(root,"project").unwrap()["value"],"concurrent");
    assert_eq!(fs::read_to_string(root.join("calls")).unwrap(),"run\n");
}

#[test]
fn generated_values_rediscover_conditional_requirements_and_keep_incremental_progress() {
    let directory = defaults_fixture(r#"import json,sys
json.load(sys.stdin)
with open('calls','a') as f: f.write('run\n')
print(json.dumps({'schemaVersion':1,'values':{'project':'generated'},'sources':[]}))
"#);
    let root = directory.path();
    fs::write(root.join("compose.ncl"),r#"
let input = import "env.yaml" in
let configContract = {project | String, backend | String | default = "compose"} &
  (if std.record.has_field "project" input then {flavor | String} else {}) in
let env | configContract = input in
{
  dockstride | not_exported = {
    Config = configContract,
    commands.defaults.argv = ["python3", "defaults.py"],
    setup.defaults = {command = "defaults", fields = ["project"], sources = true},
  },
  name = env.project,
  services.api.image = env.flavor,
}
"#).unwrap();
    let error = config::setup(root,&[],true).unwrap_err();
    let missing = error.downcast_ref::<config::MissingInputs>().unwrap();
    assert!(missing.fields.iter().any(|field| field.path == "flavor"));
    assert_eq!(config::read_env(root).unwrap()["project"],"generated");
    config::setup(root,&["flavor=nginx:alpine".into()],true).unwrap();
    assert_eq!(dockstride::nickel::evaluate(root,None).unwrap().model["services"]["api"]["image"],"nginx:alpine");
    assert_eq!(fs::read_to_string(root.join("calls")).unwrap(),"run\n");
}

#[test]
fn editor_can_publish_concurrently_but_cannot_overwrite_the_new_configuration() {
    for shared in [false, true] {
        let directory = fixture();
        let root = directory.path();
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        let path = fixture_docker(root);
        fs::write(
            root.join("env.yaml"),
            if shared {
                "project: app\noauth: {issuer: example}\n_dockstride: {sources: [{path: shared.yaml}]}\n"
            } else {
                "project: app\noauth: {issuer: example}\napiPort: 8080\n"
            },
        )
        .unwrap();
        if shared {
            fs::write(root.join("shared.yaml"), "apiPort: 8080\n").unwrap();
        }
        let script = root.join("editor.py");
        fs::write(
            &script,
            r#"import os, pathlib, subprocess, sys
args = [os.environ["DKS_BINARY"], "--json", "--non-interactive", "-C", os.environ["EDIT_ROOT"], "config", "set", "apiPort", "9091"]
if os.environ["EDIT_SHARED"] == "true":
    args.append("--shared")
subprocess.run(args, check=True, capture_output=True, timeout=20)
path = pathlib.Path(sys.argv[1])
path.write_text(path.read_text().replace("8080", "8181"))
"#,
        )
        .unwrap();
        // The editor entry point requires terminal stdin and human output.
        // Give only this subprocess a PTY; do not alter the test process's env.
        let mut command = std::process::Command::new("python3");
        command
            .args(["-c", r#"import os, pty, subprocess, sys
master, slave = pty.openpty()
try:
    result = subprocess.run([sys.argv[1], "--no-color", "-C", sys.argv[2], "config", "edit"] + sys.argv[3:], stdin=slave, capture_output=True, timeout=30)
    sys.stdout.buffer.write(result.stdout)
    sys.stderr.buffer.write(result.stderr)
    sys.exit(result.returncode)
finally:
    os.close(slave)
    os.close(master)
"#])
            .arg(env!("CARGO_BIN_EXE_dks"))
            .arg(root)
            .env("HOME", &home)
            .env("PATH", &path).env_remove("DOCKER_CONTEXT")
            .env("DOCKER_HOST", "unix:///config-fixture.sock")
            .env("EDITOR", format!("python3 {}", script.display()))
            .env("DKS_BINARY", env!("CARGO_BIN_EXE_dks"))
            .env("EDIT_ROOT", root)
            .env("EDIT_SHARED", shared.to_string());
        if shared {
            command.arg("--shared");
        }
        let result = command.output().unwrap();
        assert!(!result.status.success(), "stale editor publication succeeded");
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(
            output.contains("configuration or shared sources changed")
                || String::from_utf8_lossy(&result.stderr).contains("configuration or shared sources changed"),
            "editor must run unlocked and reject its stale snapshot: {output}; {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(config::get(root, "apiPort").unwrap()["value"], 9091);
        let target = if shared { "shared.yaml" } else { "env.yaml" };
        assert!(!fs::read_to_string(root.join(target)).unwrap().contains("8181"));
    }
}

#[test]
fn interrupted_source_and_setup_publications_recover_without_overwriting_edits() {
    let directory = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "config_publication_recovery_worker", "--nocapture"])
        .env("DKS_CONFIG_PUBLICATION_HOME", directory.path())
        .env("HOME", directory.path())
        .output()
        .unwrap();
    assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
}

#[test]
fn config_publication_recovery_worker() {
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os("DKS_CONFIG_PUBLICATION_HOME").is_none() {
        return;
    }
    let directory = fixture();
    let root = directory.path();
    let source = root.join("shared.yaml");
    let recovered = "# retained by recovery\nproject: recovered-project\noauth:\n  issuer: example\n_dockstride:\n  sources:\n    - path: shared.yaml\n";
    {
        let _lifecycle = state::lock(root, "lifecycle").unwrap();
        let _global = state::global_lock().unwrap();
        let _config = state::lock(root, "config").unwrap();
        publication::stage_locked(root, "setup", vec![
            publication::Change::create(&source, b"{}\n", 0o600).unwrap(),
            publication::Change::replace(&root.join("env.yaml"), recovered.as_bytes(), 0o600).unwrap(),
        ], json!({})).unwrap();
    }
    // Neither inspection nor a plan completes a staged operation or exposes its payload.
    let summary = config::sources_list(root).unwrap();
    assert_eq!(summary["pending"]["pending"], true);
    assert!(!serde_json::to_string(&summary["pending"]).unwrap().contains("recovered-project"));
    config::setup_plan_candidate(root, &[]).unwrap();
    assert!(!source.exists());
    assert!(!root.join("env.yaml").exists());
    // Simulate interruption after the first create, before local selection publication.
    fs::write(&source, "{}\n").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    let model = fs::read_to_string(root.join("compose.ncl")).unwrap();
    fs::write(root.join("compose.ncl"), model.replace(
        "dockstride | not_exported = { Config = configContract }",
        "dockstride | not_exported = { Config = configContract, commands.defaults.argv = [\"cat\", \"invalid-response.json\"], setup.defaults = { command = \"defaults\", fields = [\"project\"], sources = true } }",
    )).unwrap();
    fs::write(root.join("invalid-response.json"), "invalid hook output").unwrap();
    // Recovery must precede defaults discovery: the recovered values/selection
    // satisfy the hook, so the intentionally invalid command is never needed.
    config::setup(root, &[], true).unwrap();
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), recovered);
    assert_eq!(config::get(root, "project").unwrap()["value"], "recovered-project");
    assert_eq!(config::sources_list(root).unwrap()["sources"][0]["resolved"], json!(source));

    let directory = fixture();
    let root = directory.path();
    let source = root.join("new-source.yaml");
    let original = "# original\nproject: app\noauth:\n  issuer: example\n";
    fs::write(root.join("env.yaml"), original).unwrap();
    let selected = format!("{original}_dockstride:\n  sources:\n    - path: new-source.yaml\n");
    {
        let _lifecycle = state::lock(root, "lifecycle").unwrap();
        let _global = state::global_lock().unwrap();
        let _config = state::lock(root, "config").unwrap();
        publication::stage_locked(root, "config-sources", vec![
            publication::Change::create(&source, b"{}\n", 0o600).unwrap(),
            publication::Change::replace(&root.join("env.yaml"), selected.as_bytes(), 0o600).unwrap(),
        ], json!({})).unwrap();
    }
    fs::write(&source, "# external winner\napiPort: 9191\n").unwrap();
    assert!(config::sources_add(root, &root.join("another.yaml"), true).is_err());
    assert_eq!(fs::read_to_string(&source).unwrap(), "# external winner\napiPort: 9191\n");
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), original);
    assert!(!root.join("another.yaml").exists());
    assert_eq!(config::sources_list(root).unwrap()["pending"]["pending"], true);
    // Resolve the recorded transition explicitly and prove the next source
    // mutation recovers before reading its local selection.
    fs::write(&source, "{}\n").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    config::sources_remove(root, &source).unwrap();
    assert_eq!(fs::read_to_string(&source).unwrap(), "{}\n");
    assert_eq!(config::sources_list(root).unwrap()["sources"], json!([]));
    assert!(fs::read_to_string(root.join("env.yaml")).unwrap().starts_with("# original\n"));

    let directory = fixture();
    let root = directory.path();
    let source = root.join("generated-source.yaml");
    let model = fs::read_to_string(root.join("compose.ncl")).unwrap();
    fs::write(root.join("compose.ncl"), model.replace(
        "dockstride | not_exported = { Config = configContract }",
        "dockstride | not_exported = { Config = configContract, commands.defaults.argv = [\"python3\", \"hook.py\"], setup.defaults = { command = \"defaults\", fields = [\"project\"], sources = true } }",
    ).replace("name = env.project", "name | String = if env.project == \"safe\" then env.project else 42")).unwrap();
    fs::write(root.join("hook.py"), "import json, pathlib, sys\njson.load(sys.stdin)\nsys.stdout.buffer.write(pathlib.Path('response.json').read_bytes())\n").unwrap();
    let response = |project: &str| {
        fs::write(root.join("response.json"), serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "values": {"project": project},
            "sources": [{"path": source, "createIfMissing": true}],
        })).unwrap()).unwrap();
    };
    response("invalid");
    // A field-valid proposal can fail the completed operational model. Neither
    // its proposed local values nor its empty source may escape that validation.
    assert!(config::setup(root, &["oauth.issuer=example".into()], true).is_err());
    assert!(!source.exists());
    assert!(!root.join("env.yaml").exists());
    response("safe");
    config::setup(root, &["oauth.issuer=example".into()], true).unwrap();
    assert_eq!(fs::read_to_string(&source).unwrap(), "{}\n");
    assert_eq!(config::get(root, "project").unwrap()["value"], "safe");
    assert_eq!(config::get(root, "oauth.issuer").unwrap()["value"], "example");
    assert_eq!(config::sources_list(root).unwrap()["sources"][0]["resolved"], json!(source));
    assert!(config::sources_list(root).unwrap()["pending"].is_null());
}
