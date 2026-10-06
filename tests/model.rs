use dockstride::model::Project;
use serde_json::{Value, json};

fn project(model: Value) -> Project {
    Project {
        root: std::fs::canonicalize(".").unwrap(),
        env: json!({"project":"adapter-check","backend":"swarm"}),
        model,
        metadata: json!({}),
        fields: vec![],
        swarm_secrets: Default::default(),
    }
}

#[test]
fn render_golden_contract_preserves_native_fields_and_adapts_only_development_fields() {
    let project = project(json!({
        "name":"adapter-check",
        "dockstride":{"endpoints":{"api":"internal-metadata-not-docker"}},
        "services":{
            "api":{
                "image":"registry.example/api:revision",
                "build":{"context":"./api"},
                "develop":{"watch":[]},
                "depends_on":{"db":{"condition":"service_healthy"}},
                "deploy":{"replicas":2,"update_config":{"order":"start-first"}},
                "healthcheck":{"test":["CMD","true"]},
                "secrets":["authKey"]
            },
            "db":{
                "image":"registry.example/db:revision",
                "healthcheck":{"test":["CMD","true"]},
                "volumes":["database:/var/lib/db"]
            }
        },
        "volumes":["database"],
        "secrets":{"authKey":{"external":true,"name":"revision-1"}}
    }));
    let compose: Value =
        serde_yaml::from_str(include_str!("fixtures/render-compose.yaml")).unwrap();
    let swarm: Value = serde_yaml::from_str(include_str!("fixtures/render-swarm.yaml")).unwrap();
    assert_eq!(project.compose().unwrap(), compose);
    assert_eq!(project.swarm().unwrap(), swarm);
}

#[test]
fn unsupported_swarm_security_and_include_fields_are_diagnosed_not_lost() {
    for field in ["privileged", "devices", "network_mode"] {
        let mut project = project(json!({"services":{"api":{"image":"api:rev"}}}));
        project.model["services"]["api"][field] = json!(true);
        let error = project.swarm().unwrap_err().to_string();
        assert!(error.contains(&format!("services.api.{field}")), "{error}");
    }
    let project =
        project(json!({"include":["another.yaml"],"services":{"api":{"image":"api:rev"}}}));
    assert!(project.swarm().unwrap_err().to_string().contains("include"));
}

#[test]
fn ambiguous_or_invalid_project_identity_cannot_name_resources() {
    for name in ["", "Uppercase", "../other", "-leading", "contains space"] {
        let mut project = project(json!({"services":{}}));
        project.env["project"] = json!(name);
        assert!(project.name().is_err(), "{name}");
    }
}

#[test]
fn swarm_file_sources_require_current_bindings_but_native_external_refs_do_not() {
    let mut project = project(json!({
        "services":{"api":{"image":"alpine","secrets":["token","native"]}},
        "secrets":{"token":{"file":"/private/provider"},"native":{"external":true,"name":"provider-native"}}
    }));
    assert!(project.swarm().is_err());
    project.swarm_secrets.insert("token".into(), "dks-current-object".into());
    project.swarm_secrets.insert("unused".into(), "dks-unused-object".into());
    let rendered = project.swarm().unwrap();
    assert_eq!(rendered["secrets"]["token"], json!({"external":true,"name":"dks-current-object"}));
    assert_eq!(rendered["secrets"]["native"], json!({"external":true,"name":"provider-native"}));
    assert!(rendered["secrets"].get("unused").is_none());
    assert_eq!(project.compose().unwrap()["secrets"]["token"], json!({"file":"/private/provider"}));
}
