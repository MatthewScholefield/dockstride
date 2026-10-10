use dockstride::nickel;
use serde_json::{Value, json};
use std::fs;

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    nickel::init(root.path()).unwrap();
    fs::write(root.path().join("compose.ncl"), r#"
let lib = import "libs/dockstride.ncl" in
let configContract = {
  project | String | doc "Unique lowercase Compose project or Swarm stack name.",
  backend | lib.Backend | doc "Runtime backend." | default = "compose",
  apiPort | lib.Port | doc "Host HTTP port." | default = 8080,
  imagePrefix | String | doc "Image repository prefix; use a reachable registry for Swarm." | default = "dockstride-sample",
  oauth = {
    enabled | Bool | default = false,
    issuer | String | default = "",
  },
  secrets = {
    authKey | lib.SecretSource | doc "Authentication signing key (secret reference, never plaintext).",
  },
} in
let env | configContract = import "env.yaml" in
let dc = lib.forEnvironment env in
dc.ComposeFile {
  dockstride | not_exported = {
    Config = configContract,
    setup.secrets.authKey = lib.GenerateSecret {bytes = 32, encoding = "hex"},
    endpoints.api = "http://localhost:%{env.apiPort}",
  },
  secrets = env.secrets,
  services.api = dc.Service {
    image = dc.image "api",
    build.context = "./app",
    # Explicit numeric identity makes the private 0600 development grant verifiable.
    # The richer sample demonstrates non-root consumers and mapped-group access.
    user = "0:0",
    ports = ["%{env.apiPort}:8000"],
    secrets = dc.grantSecrets ["authKey"],
    environment = dc.Env {
      AUTH_KEY_FILE = dc.secretPath "authKey",
      OAUTH_ENABLED = env.oauth.enabled,
      OAUTH_ISSUER = env.oauth.issuer,
    },
    healthcheck = {
      test = ["CMD", "python", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8000/health', timeout=2)"],
      interval = "2s",
      timeout = "3s",
      retries = 15,
      start_period = "3s",
    },
    # Rebuild avoids archive-copy remounts of read-only secret binds on rootless Docker.
    develop.watch = [{action = "rebuild", path = "./app/server.py"}],
    deploy = {
      replicas = 1,
      update_config = {order = "start-first", failure_action = "rollback"},
      restart_policy.condition = "on-failure",
    },
  },
}
"#).unwrap();
    fs::remove_file(root.path().join("env.yaml")).unwrap();
    root
}

fn complete(backend: &str) -> Value {
    let mut local = json!({"project":"boundary", "backend":backend,
        "secrets":{"authKey":{"file":"/private/key"}}});
    if backend == "swarm" {
        local["_dockstride"] = json!({"swarmSecrets":{"authKey":"dks-boundary-current"}});
    }
    local
}

#[test]
fn missing_environment_fields_focus_on_local_file_and_contract_location() {
    let root = fixture();
    let candidate = json!({"project":"boundary"});
    let error = format!("{:#}", nickel::evaluate(root.path(), Some(&candidate)).unwrap_err());
    assert!(error.starts_with("Missing field `authKey` in env.yaml"), "{error}");
    assert!(error.contains("compose.ncl"), "{error}");
    assert!(error.contains("required here"), "{error}");
    assert!(!error.contains("in this record"), "{error}");
    assert!(!error.contains("evaluating environment"), "{error}");
    fs::write(root.path().join("env.shared.yaml"), "project: inherited\n").unwrap();
    let candidate = json!({"_dockstride":{"sources":[{"path":"env.shared.yaml"}]}});
    let error = format!("{:#}", nickel::evaluate(root.path(), Some(&candidate)).unwrap_err());
    assert!(error.starts_with("Missing field `authKey` in env.yaml (even after combining with env.shared.yaml)"), "{error}");
    assert!(error.contains("required here"), "{error}");
    assert!(!error.contains("in this record"), "{error}");
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
fn named_secret_handles_render_grants_and_paths_without_forcing_setup() {
    let root = fixture();
    let path = root.path().join("compose.ncl");
    let source = fs::read_to_string(&path).unwrap()
        .replace("dc.grantSecrets [\"authKey\"]", "dc.grantSecrets [dc.secrets.authKey]")
        .replace("dc.secretPath \"authKey\"", "dc.secretPath dc.secrets.authKey");
    fs::write(&path, &source).unwrap();
    assert!(nickel::schema(root.path(), None).unwrap().iter().any(|field| field.path == "secrets.authKey"));
    assert_eq!(nickel::setup_metadata(root.path(), None).unwrap()["setup"]["secrets"]["authKey"]["kind"], "generate");
    for backend in ["compose", "swarm"] {
        let project = nickel::evaluate(root.path(), Some(&complete(backend))).unwrap();
        assert_eq!(project.model["services"]["api"]["secrets"], json!([{ "source":"authKey", "target":"authKey" }]));
        assert_eq!(project.model["services"]["api"]["environment"]["AUTH_KEY_FILE"], "/run/secrets/authKey");
    }
    fs::write(&path, source.replace("dc.grantSecrets [dc.secrets.authKey]", "dc.grantSecrets [dc.secrets.auhtKey]")).unwrap();
    let error = format!("{:#}", nickel::evaluate(root.path(), Some(&complete("compose"))).unwrap_err());
    assert!(error.contains("auhtKey"), "{error}");
    assert!(error.contains("compose.ncl"), "{error}");
    assert!(error.contains("lacks the field `auhtKey`"), "{error}");
    assert!(error.contains("Did you mean `authKey`?"), "{error}");
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
        let mut effective = complete(backend);
        effective.as_object_mut().unwrap().remove("_dockstride");
        fs::write(
            root.path().join("env.yaml"),
            serde_yaml::to_string(&effective).unwrap(),
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

#[test]
fn init_creates_minimal_scaffold_and_starter_needs_only_project() {
    let root = tempfile::tempdir().unwrap();
    let result = nickel::init(root.path()).unwrap();
    assert_eq!(
        result["created"],
        json!([
            "compose.ncl",
            "libs/dockstride.ncl",
            "env.yaml",
            ".gitignore"
        ])
    );
    assert_eq!(
        fs::read_to_string(root.path().join("env.yaml")).unwrap(),
        "{}\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join(".gitignore")).unwrap(),
        "/env.yaml\n/.dockstride/\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("libs/dockstride.ncl")).unwrap(),
        include_str!("../assets/dockstride.ncl")
    );
    assert!(!root.path().join("app").exists());
    let fields = nickel::schema(root.path(), None).unwrap();
    assert_eq!(
        fields
            .iter()
            .map(|field| field.path.as_str())
            .collect::<Vec<_>>(),
        ["apiPort", "backend", "project"]
    );
    assert_eq!(
        nickel::setup_metadata(root.path(), None).unwrap(),
        json!({"setup":{}})
    );
    assert!(nickel::evaluate(root.path(), None).is_err());
    for candidate in [
        json!({"project":"hello"}),
        json!({"project":"hello", "backend":"swarm"}),
    ] {
        fs::write(
            root.path().join("env.yaml"),
            serde_yaml::to_string(&candidate).unwrap(),
        )
        .unwrap();
        let project = nickel::evaluate(root.path(), None).unwrap();
        assert_eq!(project.env["apiPort"], 8080);
        assert_eq!(
            project.metadata["endpoints"]["hello"],
            "http://localhost:8080"
        );
        assert!(project.metadata.get("setup").is_none());
        for rendered in [project.compose().unwrap(), project.swarm().unwrap()] {
            assert!(rendered.get("secrets").is_none());
            assert!(rendered.get("dockstride").is_none());
            let service = &rendered["services"]["hello"];
            assert_eq!(service["image"], "hashicorp/http-echo:1.0.0");
            assert_eq!(
                service["command"],
                json!(["-listen=:5678", "-text=Hello world!"])
            );
            assert_eq!(service["ports"], json!(["8080:5678"]));
            for field in ["build", "develop", "secrets"] {
                assert!(service.get(field).is_none(), "{field}");
            }
        }
    }
}

#[test]
fn init_adds_only_missing_ignore_rules_and_preserves_existing_environment() {
    for (existing, expected) in [
        ("", "/env.yaml\n/.dockstride/\n"),
        (
            "# keep\n*.log\n",
            "# keep\n*.log\n/env.yaml\n/.dockstride/\n",
        ),
        ("# keep\n*.log", "# keep\n*.log\n/env.yaml\n/.dockstride/\n"),
        ("/env.yaml\n/.dockstride/\n", "/env.yaml\n/.dockstride/\n"),
        ("env.yaml\n.dockstride/", "env.yaml\n.dockstride/"),
        ("env.yaml", "env.yaml\n/.dockstride/\n"),
        ("/.dockstride/\n", "/.dockstride/\n/env.yaml\n"),
        ("/env.yaml\n.dockstride/\n", "/env.yaml\n.dockstride/\n"),
    ] {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".gitignore"), existing).unwrap();
        let env = "# preserve values\nproject: existing\napiPort: 9090\n";
        fs::write(root.path().join("env.yaml"), env).unwrap();
        let result = nickel::init(root.path()).unwrap();
        assert_eq!(
            result["created"],
            json!(["compose.ncl", "libs/dockstride.ncl"])
        );
        assert_eq!(
            fs::read_to_string(root.path().join("env.yaml")).unwrap(),
            env
        );
        assert_eq!(
            fs::read_to_string(root.path().join(".gitignore")).unwrap(),
            expected
        );
    }
}

#[test]
fn init_with_existing_environment_reports_only_new_files() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("env.yaml"), "project: preserved\n").unwrap();
    let result = nickel::init(root.path()).unwrap();
    assert_eq!(
        result["created"],
        json!(["compose.ncl", "libs/dockstride.ncl", ".gitignore"])
    );
    assert_eq!(
        fs::read_to_string(root.path().join("env.yaml")).unwrap(),
        "project: preserved\n"
    );
}

#[test]
fn init_refuses_existing_definition_or_library_without_collateral_mutations() {
    for existing in ["compose.ncl", "libs/dockstride.ncl"] {
        for local_files in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join(existing);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "preserved\n").unwrap();
            if local_files {
                fs::write(root.path().join("env.yaml"), "project: preserved\n").unwrap();
                fs::write(root.path().join(".gitignore"), "# preserved").unwrap();
            }
            assert!(nickel::init(root.path()).is_err());
            assert_eq!(fs::read_to_string(path).unwrap(), "preserved\n");
            let other = if existing == "compose.ncl" {
                "libs/dockstride.ncl"
            } else {
                "compose.ncl"
            };
            assert!(!root.path().join(other).exists());
            assert!(!root.path().join("app").exists());
            if local_files {
                assert_eq!(
                    fs::read_to_string(root.path().join("env.yaml")).unwrap(),
                    "project: preserved\n"
                );
                assert_eq!(
                    fs::read_to_string(root.path().join(".gitignore")).unwrap(),
                    "# preserved"
                );
            } else {
                assert!(!root.path().join("env.yaml").exists());
                assert!(!root.path().join(".gitignore").exists());
            }
        }
    }
}

#[test]
fn candidate_sources_are_live_stripped_and_not_reinterpreted_after_contract_expansion() {
    let root = fixture();
    fs::write(root.path().join("shared.yaml"),"project: inherited\napiPort: 8181\n").unwrap();
    let original = "project: on-disk\napiPort: 7000\n_dockstride: {sources: []}\n";
    fs::write(root.path().join("env.yaml"),original).unwrap();
    let candidate = json!({"_dockstride":{"sources":[{"path":"shared.yaml"}]},"secrets":{"authKey":{"file":"/private/key"}}});
    let project = nickel::evaluate(root.path(),Some(&candidate)).unwrap();
    assert_eq!(project.env["project"],"inherited");
    assert_eq!(project.env["apiPort"],8181);
    assert!(project.env.get("_dockstride").is_none());
    assert_eq!(project.model["services"]["api"]["ports"],json!(["8181:8000"]));
    fs::write(root.path().join("shared.yaml"),"project: changed\napiPort: 8282\n").unwrap();
    let project = nickel::evaluate(root.path(),Some(&candidate)).unwrap();
    assert_eq!(project.env["project"],"changed");
    assert_eq!(project.metadata["endpoints"]["api"],"http://localhost:8282");
    assert_eq!(fs::read_to_string(root.path().join("env.yaml")).unwrap(),original);
}

#[test]
fn application_contract_cannot_claim_reserved_metadata_even_as_an_empty_record() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("compose.ncl"),"let configContract = {_dockstride = {}} in {dockstride | not_exported = {Config = configContract}, services = {}}\n").unwrap();
    let error = format!("{:#}",nickel::schema(root.path(),None).unwrap_err());
    assert!(error.contains("reserved _dockstride"),"{error}");
}

#[test]
fn snapshot_evaluation_preserves_local_bindings_and_canonical_symlink_owner() {
    let root = fixture();
    let mut local = complete("swarm");
    local["_dockstride"] = json!({"swarmSecrets":{"authKey":"dks-current-binding"}});
    let snapshot = dockstride::sources::snapshot(root.path(), Some(&local)).unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("alias ü = checkout");
    std::os::unix::fs::symlink(root.path(), &alias).unwrap();
    let project = nickel::evaluate_snapshot(&alias, &snapshot).unwrap();
    assert_eq!(project.owner().unwrap(), fs::canonicalize(root.path()).unwrap().to_str().unwrap());
    assert_eq!(project.swarm_secrets["authKey"], "dks-current-binding");
    assert_eq!(project.swarm().unwrap()["secrets"]["authKey"], json!({"external":true,"name":"dks-current-binding"}));
    assert!(project.env.get("_dockstride").is_none());
}

#[test]
fn non_utf8_checkout_root_is_rejected_before_project_evaluation() {
    use std::os::unix::ffi::OsStringExt;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join(std::ffi::OsString::from_vec(b"checkout-\xff".to_vec()));
    fs::create_dir(&root).unwrap();
    fs::write(root.join("compose.ncl"), "invalid and must not be evaluated").unwrap();
    let snapshot = dockstride::sources::snapshot(&root, None).unwrap();
    let error = format!("{:#}", nickel::evaluate_snapshot(&root, &snapshot).unwrap_err());
    assert!(error.contains("UTF-8"));
    assert!(error.contains("move the checkout"));
}
