use dockstride::nickel;
use serde_json::{Value, json};
use std::fs;

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    nickel::init(root.path()).unwrap();
    root
}

fn complete(backend: &str) -> Value {
    json!({"project":"boundary", "backend":backend,
        "secrets":{"authKey":{"file":"/private/key"}}})
}

#[test]
fn discovery_and_policies_do_not_force_absent_environment_or_services() {
    let root = fixture();
    let fields = nickel::schema(root.path(), None).unwrap();
    let project = fields.iter().find(|field| field.path == "project").unwrap();
    assert!(project.required);
    assert!(project.doc.as_ref().unwrap().contains("Unique"));
    let port = fields.iter().find(|field| field.path == "apiPort").unwrap();
    assert_eq!(port.default, Some(json!(8080)));
    assert_eq!(port.kind, "port");
    let oauth = fields
        .iter()
        .find(|field| field.path == "oauth.enabled")
        .unwrap();
    assert_eq!(oauth.default, Some(json!(false)));
    assert_eq!(oauth.kind, "boolean");
    let secret = fields
        .iter()
        .find(|field| field.path == "secrets.authKey")
        .unwrap();
    assert_eq!(secret.kind, "secret");
    assert!(secret.required);
    let metadata = nickel::setup_metadata(root.path(), None).unwrap();
    assert_eq!(metadata["setup"]["secrets"]["authKey"]["kind"], "generate");
    assert_eq!(metadata["setup"]["secrets"]["authKey"]["bytes"], 32);
    assert!(!root.path().join("env.yaml").exists());
}

#[test]
fn candidates_shadow_disk_without_mutation_and_incremental_validation_is_nickel() {
    let root = fixture();
    let original = "# preserved\nproject: disk\napiPort: 8181\n";
    fs::write(root.path().join("env.yaml"), original).unwrap();
    let candidate = json!({"apiPort":9000,"oauth":{"enabled":true}});
    nickel::validate_field(root.path(), "apiPort", &json!(9000), &candidate).unwrap();
    nickel::validate_field(root.path(), "oauth.enabled", &json!(true), &candidate).unwrap();
    assert!(nickel::evaluate(root.path(), Some(&candidate)).is_err());
    assert!(
        nickel::validate_field(root.path(), "apiPort", &json!(0), &json!({"apiPort":0})).is_err()
    );
    assert!(
        nickel::validate_field(root.path(), "apiPort", &json!(1.5), &json!({"apiPort":1.5}))
            .is_err()
    );
    assert!(
        nickel::validate_field(
            root.path(),
            "oauth.enabled",
            &json!("yes"),
            &json!({"oauth":{"enabled":"yes"}})
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(root.path().join("env.yaml")).unwrap(),
        original
    );
}

#[test]
fn canonical_model_retains_build_and_metadata_is_not_docker_output() {
    let root = fixture();
    let project = nickel::evaluate(root.path(), Some(&complete("swarm"))).unwrap();
    assert_eq!(project.env["apiPort"], 8080);
    assert_eq!(project.env["oauth"]["enabled"], false);
    assert_eq!(
        project.model["services"]["api"]["build"]["context"],
        "./app"
    );
    assert!(
        project.compose().unwrap()["services"]["api"]
            .get("build")
            .is_some()
    );
    let swarm = project.swarm().unwrap();
    assert!(swarm["services"]["api"].get("build").is_none());
    assert!(swarm["services"]["api"].get("develop").is_none());
    assert!(project.model.get("dockstride").is_none());
    assert_eq!(
        project.metadata["endpoints"]["api"],
        "http://localhost:8080"
    );
    assert_eq!(
        project.model["services"]["api"]["environment"]["OAUTH_ENABLED"],
        "false"
    );
}

#[test]
fn normal_nickel_export_obeys_backend_without_exporting_operational_metadata() {
    use nickel_lang_core::{
        eval::cache::CacheImpl,
        program::{Program, ProgramBuilder},
    };
    let root = fixture();
    for backend in ["compose", "swarm"] {
        fs::write(
            root.path().join("env.yaml"),
            serde_yaml::to_string(&complete(backend)).unwrap(),
        )
        .unwrap();
        let mut program: Program<CacheImpl> = ProgramBuilder::new()
            .add_path(root.path().join("compose.ncl"))
            .build()
            .unwrap();
        let rendered = program.eval_full_for_export().unwrap();
        let rendered = serde_json::to_value(rendered).unwrap();
        assert!(rendered.get("dockstride").is_none());
        assert_eq!(
            rendered["services"]["api"].get("build").is_some(),
            backend == "compose"
        );
        assert_eq!(
            rendered["services"]["api"].get("develop").is_some(),
            backend == "compose"
        );
        assert_eq!(rendered["secrets"]["authKey"]["file"], "/private/key");
        if backend == "swarm" {
            assert_eq!(rendered["version"], "3.8");
        }
    }
}

#[test]
fn native_swarm_export_rejects_unsupported_fields_without_forcing_schema() {
    use nickel_lang_core::{
        eval::cache::CacheImpl,
        program::{Program, ProgramBuilder},
    };
    let root = fixture();
    for (top, service) in [
        ("include = [\"other.yaml\"],", ""),
        ("profiles = [\"debug\"],", ""),
        ("", "pull_policy = \"always\","),
        ("", "privileged = true,"),
        ("", "network_mode = \"host\","),
        ("", "build = \"./app\","),
    ] {
        let image = if service.starts_with("build") {
            ""
        } else {
            "image = \"alpine:latest\","
        };
        let source = format!(
            r#"
let lib = import "libs/dockstride.ncl" in
let configContract = {{ backend | lib.Backend | default = "swarm", project | String }} in
let env | configContract = import "env.yaml" in
let dc = lib.forEnvironment env in
dc.ComposeFile {{
  dockstride | not_exported = {{Config = configContract}},
  name = "discarded",
  {top}
  services.api = {{{image} {service}}},
}}
"#
        );
        fs::write(root.path().join("compose.ncl"), source).unwrap();
        nickel::schema(root.path(), None).unwrap();
        nickel::metadata(root.path(), None).unwrap();
        fs::write(root.path().join("env.yaml"), "project: boundary\n").unwrap();
        let mut program: Program<CacheImpl> = ProgramBuilder::new()
            .add_path(root.path().join("compose.ncl"))
            .build()
            .unwrap();
        assert!(program.eval_full_for_export().is_err(), "{top} {service}");
        fs::remove_file(root.path().join("env.yaml")).unwrap();
    }
}

#[test]
fn source_diagnostics_include_contract_location_and_invalid_secret_reference() {
    let root = fixture();
    let mut candidate = complete("compose");
    candidate["apiPort"] = json!(70000);
    let error = format!(
        "{:#}",
        nickel::evaluate(root.path(), Some(&candidate)).unwrap_err()
    );
    assert!(error.contains("compose.ncl"));
    assert!(error.contains("contract"));
    let bad = json!({"secrets":{"authKey":{"file":"/private/key","plaintext":"unsafe"}}});
    assert!(
        nickel::validate_field(
            root.path(),
            "secrets.authKey",
            &bad["secrets"]["authKey"],
            &bad
        )
        .is_err()
    );
}

#[test]
fn arbitrary_project_logic_remains_lazy_and_custom_contracts_validate_selected_fields() {
    let root = fixture();
    fs::write(root.path().join("compose.ncl"), r#"
let lib = import "libs/dockstride.ncl" in
let Positive = std.contract.from_predicate (fun x => std.is_number x && x > 0) in
let configContract = { project | String, count | Positive, nested = { flag | Bool | default = true } } in
let env | configContract = import "env.yaml" in
{
  dockstride | not_exported = {Config = configContract, setup.secrets = {}},
  services.api.image = env.project ++ std.string.from env.count,
}
"#).unwrap();
    let schema = nickel::schema(root.path(), None).unwrap();
    assert!(
        schema
            .iter()
            .any(|field| field.path == "nested.flag" && field.default == Some(json!(true)))
    );
    nickel::metadata(root.path(), None).unwrap();
    nickel::validate_field(root.path(), "count", &json!(2), &json!({"count":2})).unwrap();
    assert!(
        nickel::validate_field(root.path(), "count", &json!(-1), &json!({"count":-1})).is_err()
    );
}

#[test]
fn partial_record_values_preserve_contract_fields_docs_and_defaults() {
    let root = fixture();
    fs::write(
        root.path().join("compose.ncl"),
        r#"
let configContract = {
  project | String,
  oauth | {
    enabled | Bool | doc "Enable OAuth" | default = false,
    issuer | String | doc "Identity issuer",
    timeout | Number | doc "Timeout seconds" | default = 30,
  } = {enabled = true},
} in
let env | configContract = import "env.yaml" in
{
  dockstride | not_exported = {Config = configContract},
  services.api.image = env.project ++ env.oauth.issuer,
}
"#,
    )
    .unwrap();
    let fields = nickel::schema(root.path(), None).unwrap();
    let issuer = fields
        .iter()
        .find(|field| field.path == "oauth.issuer")
        .unwrap();
    assert!(issuer.required);
    assert_eq!(issuer.kind, "string");
    assert_eq!(issuer.doc.as_deref(), Some("Identity issuer"));
    let enabled = fields
        .iter()
        .find(|field| field.path == "oauth.enabled")
        .unwrap();
    assert_eq!(enabled.kind, "boolean");
    assert_eq!(enabled.doc.as_deref(), Some("Enable OAuth"));
    assert_eq!(enabled.default, Some(json!(true)));
    let timeout = fields
        .iter()
        .find(|field| field.path == "oauth.timeout")
        .unwrap();
    assert_eq!(timeout.default, Some(json!(30)));
    assert_eq!(timeout.doc.as_deref(), Some("Timeout seconds"));
    nickel::validate_field(
        root.path(),
        "oauth.issuer",
        &json!("https://issuer.example"),
        &json!({"oauth":{"issuer":"https://issuer.example"}}),
    )
    .unwrap();
}

#[test]
fn setup_policies_do_not_force_actions_with_missing_project_or_secret_inputs() {
    let root = fixture();
    fs::write(
        root.path().join("compose.ncl"),
        r#"
let lib = import "libs/dockstride.ncl" in
let configContract = {project | String, secrets = {authKey | lib.SecretSource}} in
let env | configContract = import "env.yaml" in
{
  dockstride | not_exported = {
    Config = configContract,
    setup.secrets.authKey = lib.GenerateSecret {bytes = 32, encoding = "hex"},
    setup.secrets.fromFile = lib.FileSecret "/private/source",
    actions = [{name = "announce", workflows = ["up"], kind = "command",
      argv = ["echo", env.project, env.secrets.authKey.file]}],
  },
  services.api.image = env.project,
}
"#,
    )
    .unwrap();
    nickel::schema(root.path(), None).unwrap();
    let setup = nickel::setup_metadata(root.path(), None).unwrap();
    assert_eq!(setup["setup"]["secrets"]["authKey"]["bytes"], 32);
    assert_eq!(setup["setup"]["secrets"]["authKey"]["kind"], "generate");
    assert_eq!(
        setup["setup"]["secrets"]["fromFile"],
        json!({"kind":"file","path":"/private/source"})
    );
    assert!(nickel::metadata(root.path(), None).is_err());
    assert!(!root.path().join("env.yaml").exists());
}

#[test]
fn init_refuses_existing_project_files() {
    let root = fixture();
    let before = fs::read(root.path().join("compose.ncl")).unwrap();
    assert!(nickel::init(root.path()).is_err());
    assert_eq!(fs::read(root.path().join("compose.ncl")).unwrap(), before);
}

#[test]
fn deserialized_nested_containers_preserve_merge_invariants() {
    use nickel_lang_core::{
        eval::cache::CacheImpl,
        program::{Program, ProgramBuilder},
    };
    let source = r#"(std.deserialize 'Yaml "a:\n  b: 1\n  items: [2, 3]\n") & {x = 2}"#;
    let mut program: Program<CacheImpl> = ProgramBuilder::new()
        .add_source_string(source, "nested-yaml-merge.ncl")
        .build()
        .unwrap();
    let result = program.eval_full_for_export().unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({"a":{"b":1,"items":[2,3]},"x":2})
    );
}
