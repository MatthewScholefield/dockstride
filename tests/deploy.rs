use dockstride::{deploy, model::Project, output::Output};
use serde_json::{Value, json};

fn project(service: Value, metadata: Value) -> Project {
    Project {
        root: std::env::current_dir().unwrap().canonicalize().unwrap(),
        env: json!({"project":"scope-fixture","backend":"swarm"}),
        model: json!({"services":{"api":service}}),
        metadata,
        fields: Vec::new(),
        swarm_secrets: Default::default(),
    }
}
fn output() -> Output {
    Output {
        json: true,
        quiet: true,
        ..Output::default()
    }
}

#[test]
fn incompatible_configuration_fails_before_touching_docker_or_state() {
    for field in ["privileged", "network_mode", "external_links", "volumes_from", "unknown_field"] {
        for plan_only in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let mut project = project(json!({"image":"registry.example/api"}), json!({}));
            project.root = directory.path().canonicalize().unwrap();
            project.model["services"]["api"][field] = json!(true);
            let error = deploy::deploy(&project, plan_only, 5, &output()).unwrap_err();
            assert!(error.to_string().contains(field), "{error}");
            assert!(!directory.path().join(".dockstride").exists());
        }
    }
}

#[test]
fn digest_bearing_build_target_is_rejected_before_build_or_publication() {
    let project = project(json!({"image":"registry.example/api@sha256:abc","build":"."}), json!({}));
    let error = deploy::deploy(&project, true, 5, &output()).unwrap_err();
    assert!(error.to_string().contains("build target cannot contain a digest"));
}

#[test]
fn compose_actions_are_rejected_before_swarm_publication() {
    for action in [
        json!({"name":"migration","workflows":["deploy"],"services":["api"],"kind":"run","service":"api"}),
        json!({"name":"stop","workflows":["deploy"],"kind":"stop","targets":["api"]}),
        json!({"name":"migration","workflows":["deploy"],"kind":"prerequisite","service":"api","fresh":true}),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut project = project(json!({"image":"registry.example/api"}), json!({"actions":[action],"oneshots":["api"]}));
        project.root = directory.path().canonicalize().unwrap();
        assert!(deploy::deploy(&project, true, 5, &output()).is_err());
        assert!(!directory.path().join(".dockstride").exists());
    }
}

#[test]
fn render_and_deploy_plan_adapt_compose_fields_without_mutation() {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    dockstride::nickel::init(root).unwrap();
    fs::write(root.join("compose.ncl"), r#"
let lib = import "libs/dockstride.ncl" in
let config = { project | String, backend | lib.Backend } in
let env | config = import "env.yaml" in
let dc = lib.forEnvironment env in
dc.ComposeFile {
  dockstride | not_exported = { Config = config },
  services.api = dc.Service {
    image = "registry.example/api:latest",
    build = { context = ".", dockerfile = "Dockerfile" },
    develop.watch = [],
    depends_on = [],
    profiles = ["dev"],
    container_name = "local-api",
    restart = "unless-stopped",
    init = true,
    expose = ["8000"],
    deploy.restart_policy = { condition = "on-failure", max_attempts = 3 },
  },
}
"#).unwrap();
    let env = "project: adapter-check\nbackend: swarm\n";
    fs::write(root.join("env.yaml"), env).unwrap();
    let executable = root.join("bin/docker");
    fs::create_dir(root.join("bin")).unwrap();
    fs::write(&executable, r#"#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(os.environ['DEPLOY_ADAPTER_FIXTURE'])
a = sys.argv[1:]
with (root / 'calls').open('a') as f: f.write(json.dumps(a)+'\n')
if a[0] == 'info':
    if '{{.Swarm.LocalNodeState}}' in a: print('active')
    else: print(json.dumps({'Swarm': {'LocalNodeState': 'active', 'ControlAvailable': True, 'Nodes': 1}}))
elif a[:2] == ['ps', '-aq']: pass
elif a[0] in ['service', 'network', 'volume', 'config', 'secret'] and a[1] == 'ls': pass
else: sys.exit('unexpected/mutating Docker invocation: '+str(a))
"#).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let run = |args: &[&str]| {
        let result = Command::new(env!("CARGO_BIN_EXE_dks"))
            .env_clear()
            .env("PATH", format!("{}:{}", root.join("bin").display(), std::env::var("PATH").unwrap()))
            .env("HOME", root.join("home"))
            .env("DOCKER_HOST", "unix:///deploy-adapter-fixture.sock")
            .env("DEPLOY_ADAPTER_FIXTURE", root)
            .args(["--json", "--non-interactive", "-C"]).arg(root).args(args)
            .output().unwrap();
        assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        let row: Value = serde_json::from_str(String::from_utf8_lossy(&result.stdout).lines().last().unwrap()).unwrap();
        row["result"].clone()
    };
    let compose = run(&["render", "--target", "compose"]);
    let swarm = run(&["render", "--target", "swarm"]);
    for field in ["build", "develop", "depends_on", "profiles", "container_name", "restart"] {
        assert!(compose["services"]["api"].get(field).is_some(), "{field}");
        assert!(swarm["services"]["api"].get(field).is_none(), "{field}");
    }
    for field in ["init", "expose"] {
        assert_eq!(swarm["services"]["api"][field], compose["services"]["api"][field]);
    }
    assert_eq!(swarm["services"]["api"]["deploy"]["restart_policy"]["condition"], "on-failure");
    assert!(!root.join("calls").exists());
    let plan = run(&["--plan", "deploy"]);
    assert_eq!(plan["planOnly"], true);
    assert_eq!(plan["services"], json!(["api"]));
    assert_eq!(plan["images"]["api"]["build"], compose["services"]["api"]["build"]);
    assert!(plan["dockerOperations"].as_array().unwrap().iter().any(|op| op["phase"] == "build"));
    assert!(!root.join(".dockstride").exists());
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), env);
    let calls = fs::read_to_string(root.join("calls")).unwrap();
    assert!(calls.contains("info"));
    for line in calls.lines() {
        let call: Vec<String> = serde_json::from_str(line).unwrap();
        assert!(call[0] == "info" || call[..2] == ["ps", "-aq"] || call[1] == "ls", "{call:?}");
    }
}

#[test]
fn unsupported_canonical_build_fields_are_still_rejected_before_docker() {
    let directory = tempfile::tempdir().unwrap();
    let mut project = project(json!({"image":"registry.example/api","restart":"unless-stopped",
        "build":{"context":".","unsupported_build_option":true}}), json!({}));
    project.root = directory.path().canonicalize().unwrap();
    let error = deploy::deploy(&project, true, 5, &output()).unwrap_err();
    assert!(error.to_string().contains("unsupported build field unsupported_build_option"), "{error}");
    assert!(!directory.path().join(".dockstride").exists());
}

#[test]
fn file_backed_swarm_secrets_require_a_current_binding() {
    let mut project = project(
        json!({"image":"registry.example/api","secrets":["key"]}),
        json!({}),
    );
    project.model["secrets"] = json!({"key":{"file":"/private/key"}});
    let error = deploy::deploy(&project, true, 5, &output()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("binding")
    );
}
