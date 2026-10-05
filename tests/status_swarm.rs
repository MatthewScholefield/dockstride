use dockstride::state;
use serde_json::{Value, json};
use std::{fs, io::{Read, Write}, net::TcpListener, os::unix::fs::PermissionsExt, path::Path, process::{Command, Output}, thread, time::{Duration, Instant}};

const DOCKER: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ['SWARM_STATUS_FIXTURE'])
a = sys.argv[1:]
s = json.loads((root / 'docker.json').read_text())
with (root / 'calls').open('a') as f: f.write(json.dumps(a)+'\n')
if a[0] == 'info':
    if s.get('transportFailure'): sys.exit('fixture transport unavailable')
    if s.get('slowInfo'): time.sleep(20)
    info = {'ID':'status-daemon','SecurityOptions':[],'Swarm':{'LocalNodeState':'active','ControlAvailable':not s.get('worker',False)}}
    if '{{.ID}}' in a: print(info['ID'])
    elif '{{.Swarm.LocalNodeState}}' in a: print('active')
    else: print(json.dumps(info))
elif a[:2] == ['service','ls']:
    if '--filter' in a and any(x.startswith('name=') for x in a): print('1/1')
    else:
        for name in s['services']: print('status-fixture_'+name)
elif a[:2] == ['service','inspect']:
    identifier = a[2].removeprefix('status-fixture_')
    service = s['services'].get(identifier) or next(v for v in s['services'].values() if v['ID'] == identifier)
    print(json.dumps(service['Spec']['Labels']) if '--format' in a else json.dumps([service]))
elif a[:2] == ['service','ps']:
    name = a[-1].removeprefix('status-fixture_')
    for t in s['tasks'].get(name,[]): print(t['ID'])
elif a[:3] == ['inspect','--type','task']:
    print(json.dumps([t for ts in s['tasks'].values() for t in ts if t['ID'] in a[3:]]))
elif a[0] == 'ps':
    if s.get('container'): print(s['container']['id'])
elif a[:2] == ['container','inspect']:
    c = s['container']
    labels = {'com.docker.stack.namespace':'status-fixture','com.docker.swarm.service.id':c['service'],'com.docker.swarm.task.id':c['task']}
    if 'owner' in c: labels['io.dockstride.owner'] = c['owner']
    print(c['id'] if '{{.Id}}' in a else json.dumps(labels))
elif a[0] in ['network','volume','config','secret']: pass
else: sys.exit('unexpected Docker invocation: '+str(a))
"#;

struct Fixture { temp: tempfile::TempDir }
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let f = Self { temp };
        fs::create_dir(f.root().join("bin")).unwrap();
        fs::write(f.root().join("bin/docker"), DOCKER).unwrap();
        fs::set_permissions(f.root().join("bin/docker"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(f.root().join("compose.ncl"), r#"
let contract = {project | String, backend | String, preference | String | default = "old"} in
let env | contract = import "env.yaml" in
{dockstride | not_exported = (import "metadata.json") & {Config = contract},
 name = env.project, services = import "services.json"}
"#).unwrap();
        fs::write(f.root().join("env.yaml"), "project: status-fixture\nbackend: swarm\n").unwrap();
        f.write("services.json", &json!({"api":{"image":"example/api@sha256:abc"},"db":{"image":"example/db@sha256:abc","profiles":["inactive"]}}));
        f.write("metadata.json", &json!({}));
        state::save(f.root(), "identity", &json!({"id":"status-owner","root":f.root(),"project":"status-fixture","backend":"swarm","context":"host;DOCKER_HOST=unix:///swarm-status.sock"})).unwrap();
        f.applied(&["api","db"]);
        f.write("docker.json", &json!({"services":{"api":service("api",1),"db":service("db",1)},"tasks":{"api":[task("api",1,"running",0)],"db":[task("db",1,"running",0)]}}));
        f
    }
    fn root(&self) -> &Path { self.temp.path() }
    fn write(&self, path: &str, value: &Value) { fs::write(self.root().join(path), serde_json::to_vec(value).unwrap()).unwrap(); }
    fn read(&self, path: &str) -> Value { serde_json::from_slice(&fs::read(self.root().join(path)).unwrap()).unwrap() }
    fn applied(&self, names: &[&str]) {
        let services = names.iter().map(|name| ((*name).to_owned(), json!({"image":format!("example/{name}@sha256:abc")}))).collect::<serde_json::Map<_,_>>();
        state::save(self.root(), "deployment-applied", &json!({"active":true,"revision":"applied","owner":"status-owner","context":"host;DOCKER_HOST=unix:///swarm-status.sock","selectedServices":["api"],"rendered":{"services":services}})).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output { self.command(args).output().unwrap() }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dks"));
        c.env_clear().env("PATH",format!("{}:{}",self.root().join("bin").display(),std::env::var("PATH").unwrap()))
            .env("HOME",self.root().join("home")).env("DOCKER_HOST","unix:///swarm-status.sock")
            .env("SWARM_STATUS_FIXTURE",self.root()).args(["--json","--non-interactive","--no-color","-C"]).arg(self.root()).args(args);
        c
    }
    fn success(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
        terminal(&output)["result"].clone()
    }
    fn failure(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert_eq!(output.status.code(),Some(1),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
        let t = terminal(&output);
        assert_eq!(t["category"],"operation", "{t}");
        t["details"]["status"].clone()
    }
}
fn terminal(output: &Output) -> Value {
    let rows: Vec<Value> = String::from_utf8_lossy(&output.stdout).lines().map(|s|serde_json::from_str(s).unwrap()).collect();
    assert_eq!(rows.iter().filter(|r| r["type"] == "result" || r["type"] == "error").count(),1,"{rows:?}");
    rows.last().unwrap().clone()
}
fn row<'a>(report: &'a Value, name: &str) -> &'a Value { report["services"].as_array().unwrap().iter().find(|r|r["name"] == name).unwrap() }
fn service(name: &str, replicas: u64) -> Value {
    json!({"ID":format!("service-{name}"),"Spec":{"Labels":{"io.dockstride.owner":"status-owner","io.dockstride.project":"status-fixture"},"Mode":{"Replicated":{"Replicas":replicas}},"TaskTemplate":{"ContainerSpec":{"Image":format!("example/{name}@sha256:abc"),"Env":["TOKEN=secret-never-report"]}}}})
}
fn task(name: &str, slot: u64, status: &str, exit: i64) -> Value {
    json!({"ID":format!("task-{name}-{slot}"),"Slot":slot,"DesiredState":if status == "complete" {"shutdown"} else {"running"},"Spec":{"ContainerSpec":{"Image":format!("example/{name}@sha256:abc"),"Env":["TOKEN=secret-never-report"]}},"Status":{"State":status,"ContainerStatus":{"ExitCode":exit,"OOMKilled":false}}})
}

#[test]
fn recorded_scope_survives_selected_updates_and_explicit_selection_narrows_it() {
    let f = Fixture::new();
    let report = f.success(&["status"]);
    assert_eq!(report["requiredServices"],json!(["api","db"]));
    assert_eq!(row(&report,"db")["ready"],true); // Swarm ignores Compose profiles.
    assert!(!report.to_string().contains("secret-never-report"));
    let mut s = f.read("docker.json"); s["services"].as_object_mut().unwrap().remove("db"); f.write("docker.json",&s);
    assert_eq!(row(&f.failure(&["status"]),"db")["status"],"missing");
    let selected = f.success(&["status","api"]);
    assert_eq!(selected["requiredServices"],json!(["api"]));
    assert_eq!(row(&selected,"db")["status"],"excluded");
}

#[test]
fn missing_deployment_and_missing_services_fail_but_inspection_succeeds() {
    let f = Fixture::new();
    state::save(f.root(),"deployment-applied",&json!({})).unwrap();
    let report = f.failure(&["status"]);
    assert_eq!(report["deploymentRecorded"],false);
    assert_eq!(report["ready"],false);
    let mut s = f.read("docker.json"); s["services"] = json!({}); f.write("docker.json",&s);
    let report = f.failure(&["status"]);
    assert_eq!(row(&report,"api")["status"],"missing");
    assert_eq!(row(&report,"db")["status"],"missing");
    assert_eq!(f.success(&["status","--inspect-only"])["ready"],false);
}

#[test]
fn replica_shortfall_revision_drift_and_rollout_failures_are_not_ready() {
    let f = Fixture::new();
    let baseline = f.read("docker.json");
    let mut s = baseline.clone(); s["tasks"]["api"] = json!([]); f.write("docker.json",&s);
    assert_eq!(row(&f.failure(&["status"]),"api")["containerReady"],false);
    let mut s = baseline.clone(); s["services"]["api"]["Spec"]["Mode"]["Replicated"]["Replicas"] = json!(0); f.write("docker.json",&s);
    assert_eq!(row(&f.failure(&["status"]),"api")["containerReady"],false);
    let mut s = baseline.clone(); s["services"]["api"]["Spec"]["TaskTemplate"]["ContainerSpec"]["Image"] = json!("example/api@sha256:foreign"); s["tasks"]["api"][0]["Spec"]["ContainerSpec"]["Image"] = json!("example/api@sha256:foreign"); f.write("docker.json",&s);
    assert_eq!(row(&f.failure(&["status"]),"api")["containerReady"],false);
    for state in ["paused","rollback_started","rollback_completed"] {
        let mut s = baseline.clone(); s["services"]["api"]["UpdateStatus"] = json!({"State":state}); f.write("docker.json",&s);
        assert_eq!(row(&f.failure(&["status"]),"api")["rolloutState"],state);
    }
    for state in ["failed","rejected"] {
        let mut s = baseline.clone(); s["tasks"]["api"][0]["Status"]["State"] = json!(state); f.write("docker.json",&s);
        assert_eq!(row(&f.failure(&["status"]),"api")["containerReady"],false);
    }
}

#[test]
fn configured_replicas_require_each_distinct_task_slot() {
    let f = Fixture::new();
    let mut applied = state::read(f.root(),"deployment-applied").unwrap(); applied["rendered"]["services"]["api"]["deploy"] = json!({"replicas":2}); state::save(f.root(),"deployment-applied",&applied).unwrap();
    let mut s = f.read("docker.json"); s["services"]["api"] = service("api",2); f.write("docker.json",&s);
    assert_eq!(row(&f.failure(&["status"]),"api")["successfulTasks"],1);
    s["tasks"]["api"] = json!([task("api",1,"running",0),task("api",2,"running",0)]); f.write("docker.json",&s);
    assert_eq!(row(&f.success(&["status"]),"api")["successfulTasks"],2);
}

#[test]
fn registered_swarm_tasks_require_exact_container_and_service_ownership() {
    let f = Fixture::new();
    let mut s = f.read("docker.json");
    s["container"] = json!({"id":"container-api","service":"service-api","task":"task-api-1"});
    s["tasks"]["api"][0]["ServiceID"] = json!("service-api");
    s["tasks"]["api"][0]["Status"]["ContainerStatus"]["ContainerID"] = json!("container-api");
    f.write("docker.json",&s);
    f.success(&["config","set","preference","new"]);
    for (task_container, service_owner, container_owner) in [
        ("another-container","status-owner",None),
        ("container-api","foreign",None),
        ("container-api","status-owner",Some("foreign")),
    ] {
        s["tasks"]["api"][0]["Status"]["ContainerStatus"]["ContainerID"] = json!(task_container);
        s["services"]["api"]["Spec"]["Labels"]["io.dockstride.owner"] = json!(service_owner);
        if let Some(owner) = container_owner {
            s["container"]["owner"] = json!(owner);
        } else {
            s["container"].as_object_mut().unwrap().remove("owner");
        }
        f.write("docker.json",&s);
        assert!(!f.run(&["config","set","preference","rejected"]).status.success());
        assert_eq!(f.success(&["config","get","preference"])["value"],"new");
    }
}

#[test]
fn oneshots_need_completed_successful_expected_tasks_not_service_existence() {
    let f = Fixture::new(); f.write("metadata.json",&json!({"oneshots":["api"]}));
    let baseline = f.read("docker.json");
    for task_values in [json!([]),json!([task("api",1,"running",0)]),json!([task("api",1,"complete",9)]),json!([task("api",1,"failed",137)])] {
        let mut s = baseline.clone(); s["tasks"]["api"] = task_values; f.write("docker.json",&s);
        assert_eq!(row(&f.failure(&["status"]),"api")["containerReady"],false);
    }
    let mut s = baseline.clone(); s["tasks"]["api"] = json!([task("api",1,"complete",0)]); s["tasks"]["api"][0]["Status"]["ContainerStatus"]["OOMKilled"] = json!(true); f.write("docker.json",&s);
    assert_eq!(row(&f.failure(&["status"]),"api")["containerReady"],false);
    s["tasks"]["api"][0]["Status"]["ContainerStatus"]["OOMKilled"] = json!(false); f.write("docker.json",&s);
    assert_eq!(row(&f.success(&["status"]),"api")["status"],"completed");
}

#[test]
fn command_application_failure_is_reported_once_and_inspection_skips_it() {
    let f = Fixture::new();
    fs::write(f.root().join("probe.py"),"import pathlib,sys\np=pathlib.Path('probes')\np.write_text(p.read_text()+'x' if p.exists() else 'x')\nsys.exit(1)\n").unwrap();
    f.write("metadata.json",&json!({"readiness":{"api":{"command":["python3","probe.py"]}}}));
    let report = f.failure(&["status"]);
    assert_eq!(row(&report,"api")["applicationReady"],false);
    assert_eq!(row(&report,"api")["containerReady"],true);
    assert_eq!(fs::read_to_string(f.root().join("probes")).unwrap(),"x");
    let report = f.success(&["status","--inspect-only"]);
    assert_eq!(row(&report,"api")["applicationReady"],Value::Null);
    assert_eq!(row(&report,"api")["ready"],false);
    assert_eq!(fs::read_to_string(f.root().join("probes")).unwrap(),"x");
}

#[test]
fn http_identity_mismatch_is_an_application_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream,_) = listener.accept().unwrap(); stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buffer = [0;4096]; let _ = stream.read(&mut buffer).unwrap();
        let body = r#"{"project":"some-other-environment"}"#;
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    let f = Fixture::new(); f.write("metadata.json",&json!({"readiness":{"api":{"url":format!("http://{address}/identity"),"json":{"project":"status-fixture"}}}}));
    let report = f.failure(&["status"]);
    assert_eq!(row(&report,"api")["applicationReady"],false);
    server.join().unwrap();
}

#[test]
fn shared_deadline_leaves_later_services_explicitly_unobserved() {
    let f = Fixture::new(); f.write("metadata.json",&json!({"readiness":{"api":{"command":["python3","-c","import time; time.sleep(20)"]}}}));
    let start = Instant::now(); let report = f.failure(&["--timeout","1","status"]);
    assert!(start.elapsed() < Duration::from_secs(6));
    assert_eq!(report["deadlineExceeded"],true);
    assert_eq!(row(&report,"db")["observed"],false);
    assert_eq!(row(&report,"db")["ready"],false);
    assert_eq!(row(&report,"db")["status"],"unobserved");
}

#[test]
fn inspection_still_fails_transport_ownership_and_configuration_errors_with_partial_rows() {
    let f = Fixture::new();
    let mut s = f.read("docker.json"); s["transportFailure"] = json!(true); f.write("docker.json",&s);
    let output = f.run(&["status","--inspect-only"]); assert_eq!(output.status.code(),Some(4));
    let t = terminal(&output); assert_eq!(t["category"],"docker"); assert_eq!(row(&t["details"]["status"],"db")["observed"],false);
    s["transportFailure"] = json!(false); s["services"]["api"]["Spec"]["Labels"]["io.dockstride.owner"] = json!("foreign"); f.write("docker.json",&s);
    assert!(!f.run(&["status","--inspect-only"]).status.success());
    let output = f.run(&["status","docker-unknown"]); assert_eq!(output.status.code(),Some(2));
    let t = terminal(&output); assert_eq!(t["category"],"configuration"); assert!(t["details"]["status"].is_object());
}

#[test]
fn deadline_also_bounds_initial_docker_observation_without_misclassifying_it() {
    let f = Fixture::new(); let mut s = f.read("docker.json"); s["slowInfo"] = json!(true); f.write("docker.json",&s);
    let start = Instant::now(); let report = f.failure(&["--timeout","1","status"]);
    assert!(start.elapsed() < Duration::from_secs(6));
    assert_eq!(report["deadlineExceeded"],true);
    assert_eq!(row(&report,"api")["observed"],false);
    assert_eq!(row(&report,"db")["observed"],false);
    let report = f.success(&["--timeout","1","status","--inspect-only"]);
    assert_eq!(report["deadlineExceeded"],true);
    assert_eq!(report["ready"],false);
}
