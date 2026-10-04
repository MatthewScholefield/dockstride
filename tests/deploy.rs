use dockstride::{deploy, model::Project, output::Output};
use serde_json::{Value, json};

fn project(service: Value, metadata: Value) -> Project {
    Project {
        root: std::path::PathBuf::from("/path-that-does-not-exist"),
        env: json!({"project":"scope-fixture","backend":"swarm"}),
        model: json!({"services":{"api":service}}),
        metadata,
        fields: Vec::new(),
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
    let error = deploy::deploy(&project, &[], false, 5, &output()).unwrap_err();
    assert!(error.to_string().contains("privileged"));
}

#[test]
fn selection_must_exist_even_for_readonly_plans() {
    let project = project(json!({"image":"registry.example/api"}), json!({}));
    let error = deploy::deploy(&project, &["database".into()], true, 5, &output()).unwrap_err();
    assert!(error.to_string().contains("unknown service database"));
}

#[test]
fn a_compose_prerequisite_is_not_silently_reinterpreted_in_production() {
    let project = project(
        json!({"image":"registry.example/api"}),
        json!({"actions":[{
            "name":"migration","workflows":["deploy"],"services":["api"],"kind":"run","service":"api"
        }]}),
    );
    let error = deploy::deploy(&project, &["api".into()], true, 5, &output()).unwrap_err();
    assert!(error.to_string().contains("Swarm migration semantics"));
}

#[test]
fn production_secrets_require_durable_external_references() {
    let mut project = project(
        json!({"image":"registry.example/api","secrets":["key"]}),
        json!({}),
    );
    project.model["secrets"] = json!({"key":{"file":"/private/key"}});
    let error = deploy::deploy(&project, &[], true, 5, &output()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("external immutable provisioned reference")
    );
}
