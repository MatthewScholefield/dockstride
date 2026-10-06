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
    }
}

#[test]
fn incompatible_configuration_fails_before_touching_docker_or_state() {
    let project = project(
        json!({"image":"registry.example/api","privileged":true}),
        json!({}),
    );
    let error = deploy::deploy(&project, false, 5, &output()).unwrap_err();
    assert!(error.to_string().contains("privileged"));
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
