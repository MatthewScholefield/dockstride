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

fn secrets_fixture() -> TempDir {
    let directory = fixture();
    let source = fs::read_to_string(directory.path().join("compose.ncl")).unwrap();
    fs::write(directory.path().join("compose.ncl"), source.replace("  project | String", "  secrets | Dyn,\n  project | String")).unwrap();
    directory
}

#[test]
fn ordinary_edits_move_secrets_last_without_reformatting_other_fields() {
    let directory = secrets_fixture();
    let root = directory.path();
    let secrets = "'secrets': # references\n  # token location\n  'key': {file: '/private/token'} # retained\n";
    let siblings = "\n# deployment identity\nproject: 'app' # identity\noauth:\n  enabled: false\n  issuer: 'example'\napiPort: 8080 # host port\n";
    fs::write(root.join("env.yaml"), format!("# environment\n{secrets}{siblings}")).unwrap();
    config::set(root, "apiPort", json!(8081)).unwrap();
    let expected = format!("# environment\n{}{secrets}", siblings.replace("8080", "8081"));
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), expected);
    config::set(root, "apiPort", json!(8081)).unwrap();
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), expected);
    config::unset(root, "apiPort").unwrap();
    assert!(fs::read_to_string(root.join("env.yaml")).unwrap().ends_with(secrets));
}

#[test]
fn edits_keep_secrets_before_the_document_end_marker() {
    let directory = secrets_fixture();
    let root = directory.path();
    let secrets = "secrets:\n  key: {file: '/private/token'}\n";
    fs::write(root.join("env.yaml"), format!("---\n{secrets}project: app\n... # end\n")).unwrap();
    config::set(root, "apiPort", json!(8081)).unwrap();
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), format!("---\nproject: app\napiPort: 8081\n{secrets}... # end\n"));
    config::set(root, "oauth.enabled", json!(true)).unwrap();
    assert!(fs::read_to_string(root.join("env.yaml")).unwrap().ends_with(&format!("{secrets}... # end\n")));
}

#[test]
fn flow_root_edits_serialize_secrets_last() {
    let directory = secrets_fixture();
    let root = directory.path();
    fs::write(root.join("env.yaml"), "# environment\n{secrets: {key: {file: '/private/token'}}, project: app, apiPort: 8080} # root\n... # end\n").unwrap();
    config::set(root, "apiPort", json!(8081)).unwrap();
    let text = fs::read_to_string(root.join("env.yaml")).unwrap();
    assert!(text.starts_with("# environment\n# root\n"));
    assert!(text.ends_with("secrets:\n  key:\n    file: /private/token\n... # end\n"));
    assert_eq!(config::read_env(root).unwrap()["apiPort"], 8081);
    config::set(root, "apiPort", json!(8081)).unwrap();
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), text);
}

#[test]
fn secret_reordering_that_would_break_aliases_preserves_valid_edits() {
    let directory = secrets_fixture();
    let root = directory.path();
    let source = "secrets:\n  key:\n    file: &issuer '/private/token'\nproject: app\noauth:\n  enabled: false\n  issuer: *issuer\napiPort: 8080\n";
    fs::write(root.join("env.yaml"), source).unwrap();
    config::set(root, "apiPort", json!(8081)).unwrap();
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), source.replace("8080", "8081"));
    assert_eq!(config::read_env(root).unwrap()["oauth"]["issuer"], "/private/token");
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
fn project_and_backend_changes_are_ordinary_validated_edits() {
    let directory = fixture();
    let root = directory.path();
    config::setup(root, &["project=app".into(), "oauth.issuer=example".into()], true).unwrap();
    config::set(root, "project", json!("other")).unwrap();
    config::set(root, "backend", json!("swarm")).unwrap();
    assert_eq!(config::get(root, "project").unwrap()["value"], "other");
    assert_eq!(config::get(root, "backend").unwrap()["value"], "swarm");
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
fn project_proposals_use_folder_names_without_path_suffixes() {
    let directory = tempfile::tempdir().unwrap();
    for (folder, expected) in [
        ("voxellum", "voxellum"),
        (".Voxellum_worktree-1", "voxellum-worktree-1"),
        ("---", "project"),
    ] {
        let root = directory.path().join(folder);
        fs::create_dir_all(&root).unwrap();
        assert_eq!(config::project_proposal(&root).unwrap(), expected);
    }
    let other = directory.path().join("another-parent/voxellum");
    fs::create_dir_all(&other).unwrap();
    assert_eq!(config::project_proposal(&other).unwrap(), "voxellum");
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
fn lifecycle_lock_serializes_configuration_edits() {
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
        identity_tx.send(config::set(&identity_root, "project", json!("other")).is_ok()).unwrap();
    });
    let port_root = root.to_owned();
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let port_thread = std::thread::spawn(move || {
        port_tx.send(config::set(&port_root, "apiPort", json!(9091)).is_ok()).unwrap();
    });
    let port_blocked = port_rx.recv_timeout(std::time::Duration::from_millis(100)).is_err();
    drop(lifecycle);
    assert!(port_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap());
    assert!(identity_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap());
    port_thread.join().unwrap();
    identity_thread.join().unwrap();
    assert!(port_blocked, "configuration changed while lifecycle work held its guard");
    assert_eq!(config::get(root, "project").unwrap()["value"], "other");
    assert_eq!(config::get(root, "apiPort").unwrap()["value"], 9091);
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
            .env_remove("DOCKER_CONTEXT")
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
fn swarm_bindings_are_local_metadata_and_cannot_be_set_as_ordinary_fields() {
    let directory = fixture();
    let root = directory.path();
    fs::write(root.join("env.yaml"), "project: app\noauth: {issuer: example}\n_dockstride:\n  swarmSecrets:\n    token: dks-0123456789abcdef0123456789abcdef\n").unwrap();
    let snapshot = dockstride::sources::snapshot(root, None).unwrap();
    assert!(snapshot.values.get("_dockstride").is_none());
    assert_eq!(dockstride::sources::swarm_bindings(&snapshot.local).unwrap()["token"], "dks-0123456789abcdef0123456789abcdef");
    let before = fs::read(root.join("env.yaml")).unwrap();
    assert!(config::set(root, "_dockstride.swarmSecrets.token", json!("other")).is_err());
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), before);
    fs::write(root.join("shared.yaml"), "_dockstride:\n  swarmSecrets: {token: foreign}\n").unwrap();
    assert!(config::sources_add(root, &root.join("shared.yaml"), false).is_err());
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), before);
}

#[test]
fn malformed_swarm_binding_metadata_fails_before_yaml_changes() {
    let directory = fixture();
    let root = directory.path();
    for map in ["{token: 42}", "{'invalid/name': valid}", "{token: 'invalid/name'}", "[]"] {
        let text = format!("_dockstride:\n  swarmSecrets: {map}\n");
        fs::write(root.join("env.yaml"), &text).unwrap();
        assert!(config::set(root, "project", json!("app")).is_err());
        assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), text);
    }
}

#[test]
fn required_nullable_values_remain_valid_ordinary_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("compose.ncl"), r#"
let contract = { project | String, setting | Dyn } in
let env | contract = import "env.yaml" in {
  dockstride | not_exported = { Config = contract },
  name = env.project,
  services.api.image = "nginx:alpine",
}"#).unwrap();
    fs::write(root.join("env.yaml"), "project: nullable\nsetting: null\n").unwrap();
    assert_eq!(config::list(root).unwrap()["missing"], json!([]));
    assert_eq!(config::setup(root, &[], true).unwrap()["complete"], true);
    config::set(root, "setting", json!("value")).unwrap();
    config::set(root, "setting", Value::Null).unwrap();
    assert!(config::read_env(root).unwrap()["setting"].is_null());
    let before = fs::read(root.join("env.yaml")).unwrap();
    assert!(config::unset(root, "setting").is_err());
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), before);
}
