use dockstride::{diagnostics::{self, DiagnosticConfiguration, DiagnosticReport}, model::Project, output::Output, runtime::{Docker, PrerequisiteFailed}, status::StatusReport};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, time::{Duration, Instant}};

fn project(root: &Path) -> Project {
    Project {
        root: root.canonicalize().unwrap(),
        env: json!({"project":"diagnostic-fixture","backend":"compose"}),
        model: json!({"name":"diagnostic-fixture","services":{
            "postgres":{"image":"postgres","secrets":[{"source":"password","target":"db-password"}]},
            "migrate":{"image":"app","depends_on":{"postgres":{"condition":"service_healthy"}}},
            "unrelated":{"image":"other"}},
            "secrets":{"password":{"file":"/private/mounted-password","contents":"NEVER-FORWARD-MODEL-CONTENTS"}}}),
        metadata: json!({"commands":{"diagnose":{"argv":["python3","hook.py"],"timeoutSeconds":30}},
            "diagnostics":{"postgres":{"command":"diagnose","services":["postgres"],"on":["startup-failed"]}}}),
        fields: vec![],
        swarm_secrets: Default::default(),
    }
}

#[test]
fn declarations_fail_before_running_commands() {
    let root = tempfile::tempdir().unwrap();
    let original = project(root.path());
    for declaration in [
        json!({"command":"unknown","services":["postgres"],"on":["startup-failed"]}),
        json!({"command":"diagnose","services":["absent"],"on":["startup-failed"]}),
        json!({"command":"diagnose","services":["postgres"],"on":["repair"]}),
        json!({"command":"diagnose","services":[],"on":["startup-failed"]}),
        json!({"command":"diagnose","services":["postgres"],"on":["unhealthy","unhealthy"]}),
        json!({"command":"diagnose","services":"postgres","on":["startup-failed"]}),
        json!({"command":"diagnose","services":["postgres"],"on":["startup-failed"],"argv":["repair"]}),
    ] {
        let mut project = original.clone();
        project.metadata["diagnostics"]["postgres"] = declaration;
        assert!(diagnostics::validate(&project).unwrap_err().is::<DiagnosticConfiguration>());
    }
    for argv in [json!("python3 hook.py"), json!([]), json!([""]), json!(["python3",3]), json!(["python3","\u{0}"])] {
        let mut project = original.clone();
        project.metadata["commands"]["diagnose"]["argv"] = argv;
        assert!(diagnostics::validate(&project).unwrap_err().is::<DiagnosticConfiguration>());
    }
    assert!(!root.path().join(".dockstride").exists());
    assert!(!root.path().join("calls").exists());
}

#[test]
fn diagnostics_execute_in_isolated_pinned_environment() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("bin")).unwrap();
    fs::write(root.path().join("bin/docker"), r#"#!/usr/bin/env python3
import json,pathlib,sys
args=sys.argv[1:]
if args[:2]==['ps','-aq']:
    print('PostgresVerifiedId\nMigrationVerifiedId')
elif args[:2]==['container','inspect']:
    print((pathlib.Path('.')/'containers.json').read_text())
else:
    raise SystemExit(42)
"#).unwrap();
    fs::set_permissions(root.path().join("bin/docker"), fs::Permissions::from_mode(0o755)).unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact","diagnostics_subprocess_worker","--nocapture"])
        .env("DKS_DIAGNOSTICS_FIXTURE", root.path())
        .env("DOCKER_HOST", "unix:///diagnostic-fixture-unused.sock")
        .env("HOME", root.path().join("home"))
        .env("XDG_DATA_HOME", root.path().join("private"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("PATH", format!("{}:{}",root.path().join("bin").display(),std::env::var("PATH").unwrap()))
        .env_remove("DOCKER_CONTEXT").env_remove("COMPOSE_PROFILES")
        .output().unwrap();
    assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
}

fn write_hook(root: &Path, response: &str, sleep: f64, exit: u8) {
    fs::write(root.join("hook.py"), format!(r#"import json,pathlib,sys,time
root=pathlib.Path('.')
context=json.load(sys.stdin)
(root/'context.json').write_text(json.dumps(context))
with (root/'calls').open('a') as calls: calls.write(context['diagnostics']['hook']+'\n')
time.sleep({sleep})
sys.stdout.write({response:?})
sys.exit({exit})
"#)).unwrap();
}

fn hooks(report: &Value) -> Vec<&Value> {
    report["failures"].as_array().unwrap().iter().filter(|failure| failure["stage"] == "hook").collect()
}

#[test]
fn diagnostics_subprocess_worker() {
    let Some(root) = std::env::var_os("DKS_DIAGNOSTICS_FIXTURE") else { return };
    let root = Path::new(&root);
    fs::write(root.join("env.yaml"), "project: diagnostic-fixture\nbackend: compose\nsecrets:\n  password:\n    file: /private/mounted-password\n").unwrap();
    let mut project = project(root);
    let output = Output { quiet:true, ..Output::default() };
    let docker = Docker::new(root, output.clone());
    let selected = vec!["postgres".into(),"migrate".into()];
    let primary: anyhow::Error = PrerequisiteFailed(json!({"service":"migrate","exitCode":12})).into();
    let finding = json!({"code":"postgres.auth.failed","severity":"error","summary":"Mounted credential cannot authenticate",
        "evidence":{"database":"fixture"},"suggestedCommand":["touch",root.join("REPAIR-MUST-NOT-RUN")],
        "suggestedAction":{"action":"choose explicit credential recovery"}});
    let response = json!({"schemaVersion":1,"findings":[finding.clone()]}).to_string();
    write_hook(root, &response, 0.0, 0);
    let report = diagnostics::run(&project, &selected, "startup-failed", 10, &docker, &output, Some(&primary));
    assert_eq!(report["findings"], json!([finding.clone()]));
    assert!(hooks(&report).is_empty());
    assert!(!root.join("REPAIR-MUST-NOT-RUN").exists());
    let wrapped = primary.context(DiagnosticReport(report));
    assert_eq!(wrapped.downcast_ref::<PrerequisiteFailed>().unwrap().0["exitCode"], 12);
    let text = fs::read_to_string(root.join("context.json")).unwrap();
    let context: Value = serde_json::from_str(&text).unwrap();
    assert!(!text.contains("NEVER-FORWARD"));
    assert!(context["settings"].get("secrets").is_none());
    assert!(context["provenance"].get("secrets.password.file").is_none());
    assert_eq!(context["diagnostics"]["secretReferences"]["postgres"],json!([{"source":"password","target":"db-password","file":"/private/mounted-password"}]));
    assert_eq!(context["diagnostics"]["failedServices"][0]["service"], "migrate");
    assert_eq!(context["diagnostics"]["triggers"], json!(["startup-failed"]));
    // An unreachable Docker observation never authorizes resource IDs.
    assert_eq!(context["diagnostics"]["observations"]["services"],json!([]));

    for (response, exit, expected) in [
        ("not JSON".to_owned(),0,"invalid-output"),
        (r#"{"schemaVersion":1,"findings":[],"findings":[]}"#.to_owned(),0,"invalid-output"),
        (r#"{"schemaVersion":1,"findings":[]} {"schemaVersion":1,"findings":[]}"#.to_owned(),0,"invalid-output"),
        (json!({"schemaVersion":2,"findings":[]}).to_string(),0,"invalid-output"),
        (json!({"schemaVersion":1,"findings":[],"padding":"x".repeat(1_048_576)}).to_string(),0,"invalid-output"),
        (json!({"schemaVersion":1,"findings":[{"code":"","severity":"error","summary":"invalid","evidence":[]}]}).to_string(),0,"invalid-output"),
        (json!({"schemaVersion":1,"findings":[{"code":"bad","severity":"critical","summary":"invalid","evidence":[]}]}).to_string(),0,"invalid-output"),
        (json!({"schemaVersion":1,"findings":[{"code":"bad","severity":"error","summary":"invalid"}]}).to_string(),0,"invalid-output"),
        (json!({"schemaVersion":1,"findings":[finding]}).to_string(),7,"command-failed"),
    ] {
        write_hook(root, &response, 0.0, exit);
        let report = diagnostics::run(&project, &selected, "startup-failed", 10, &docker, &output, Some(&wrapped));
        assert_eq!(report["findings"],json!([]));
        assert_eq!(hooks(&report)[0]["kind"],expected,"{report}");
        assert!(wrapped.is::<PrerequisiteFailed>());
    }

    // A healthy owned database is still relevant to a scoped migration failure.
    fs::write(root.join("containers.json"),json!([
        {"Id":"PostgresVerifiedId","Config":{"Labels":{"io.dockstride.owner":project.owner().unwrap(),"io.dockstride.project":"diagnostic-fixture",
            "com.docker.compose.project":"diagnostic-fixture","com.docker.compose.service":"postgres"},"Env":["PASSWORD=NEVER-FORWARD-CONTAINER"]},
            "State":{"Status":"running","Running":true,"ExitCode":0,"OOMKilled":false,"Health":{"Status":"healthy","Log":[{"Output":"NEVER-FORWARD-HEALTH-LOG"}]}}},
        {"Id":"MigrationVerifiedId","Config":{"Labels":{"io.dockstride.owner":project.owner().unwrap(),"io.dockstride.project":"diagnostic-fixture",
            "com.docker.compose.project":"diagnostic-fixture","com.docker.compose.service":"migrate"}},
            "State":{"Status":"exited","Running":false,"ExitCode":12,"OOMKilled":false}}
    ]).to_string()).unwrap();
    write_hook(root, &response, 0.0, 0);
    let report = diagnostics::run(&project,&selected,"startup-failed",10,&docker,&output,Some(&wrapped));
    assert_eq!(report["findings"][0]["code"],"postgres.auth.failed");
    let text = fs::read_to_string(root.join("context.json")).unwrap();
    assert!(!text.contains("NEVER-FORWARD"));
    let context: Value = serde_json::from_str(&text).unwrap();
    let postgres = context["diagnostics"]["observations"]["services"].as_array().unwrap().iter()
        .find(|row| row["service"]=="postgres").unwrap();
    assert_eq!(postgres["containerReady"],true);
    assert_eq!(postgres["verifiedContainerIds"],json!(["PostgresVerifiedId"]));

    // Explicit doctor ignores automatic on filters, and service selection does not.
    project.metadata["diagnostics"]["postgres"]["on"] = json!(["unhealthy"]);
    write_hook(root, &response, 0.0, 0);
    let report = diagnostics::run(&project, &selected, "doctor", 10, &docker, &output, None);
    assert_eq!(report["findings"][0]["code"],"postgres.auth.failed");
    fs::remove_file(root.join("calls")).unwrap();
    let report = diagnostics::run(&project, &["unrelated".into()], "doctor", 10, &docker, &output, None);
    assert_eq!(report["findings"],json!([]));
    assert!(!root.join("calls").exists());
    let report = diagnostics::run(&project, &selected, "startup-failed", 10, &docker, &output, None);
    assert_eq!(report["findings"],json!([]));
    assert!(!root.join("calls").exists());
    let primary: anyhow::Error = anyhow::anyhow!("Original startup readiness failure").context(StatusReport(json!({"services":[
        {"name":"postgres","observed":true,"unhealthy":true,"applicationReady":false,"Config":{"Env":["PASSWORD=NEVER-FORWARD-PRIMARY"]}}
    ]})));
    project.metadata["diagnostics"]["postgres"]["on"] = json!(["unhealthy","readiness-failed"]);
    let report = diagnostics::run(&project, &selected, "startup-failed", 10, &docker, &output, Some(&primary));
    assert_eq!(report["findings"][0]["code"],"postgres.auth.failed");
    let text = fs::read_to_string(root.join("context.json")).unwrap();
    assert!(!text.contains("NEVER-FORWARD-PRIMARY"));
    let context: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(context["diagnostics"]["triggers"],json!(["unhealthy","readiness-failed"]));

    // Two slow hooks share one phase deadline; the third must not even spawn.
    fs::remove_file(root.join("calls")).unwrap();
    project.metadata["diagnostics"] = json!({
        "a":{"command":"diagnose","services":["postgres"],"on":["startup-failed"]},
        "b":{"command":"diagnose","services":["postgres"],"on":["startup-failed"]},
        "c":{"command":"diagnose","services":["postgres"],"on":["startup-failed"]}});
    write_hook(root, &response, 2.0, 0);
    let started = Instant::now();
    let report = diagnostics::run(&project, &selected, "startup-failed", 3, &docker, &output, None);
    assert!(started.elapsed() < Duration::from_millis(5500));
    assert_eq!(report["findings"][0]["code"],"postgres.auth.failed");
    assert_eq!(fs::read_to_string(root.join("calls")).unwrap(), "a\nb\n");
    let failures = hooks(&report);
    assert_eq!(failures.iter().map(|failure| failure["kind"].as_str().unwrap()).collect::<Vec<_>>(),vec!["deadline-exceeded","deadline-exceeded"]);

    project.metadata = json!({});
    project.root = root.join("does-not-exist");
    assert_eq!(diagnostics::run(&project, &[], "doctor", 1, &docker, &output, None),json!({"schemaVersion":1,"findings":[],"failures":[]}));
}
