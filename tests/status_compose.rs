//! Strict status exercises Docker observations and actual HTTP/command probes.
use serde_json::{Value, json};
use std::{fs, io::{Read, Write}, net::TcpListener, os::unix::fs::PermissionsExt, path::Path, process::{Command, Output, Stdio}, thread, time::{Duration, Instant}};

const DOCKER: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ['STATUS_FIXTURE'])
a = sys.argv[1:]
if a[:1] == ['--context']: a = a[2:]
mode = (root / 'mode').read_text().strip()
if mode == 'slow-context' and a[:1] == ['context']: time.sleep(5)
if mode == 'context-error' and a[:1] == ['context']:
    print('context transport failed', file=sys.stderr); sys.exit(37)
if mode == 'inspect-error' and a[:2] == ['container', 'inspect'] and '--format' not in a:
    print('inspection transport failed', file=sys.stderr); sys.exit(38)
s = json.loads((root / 'containers.json').read_text())
services = json.loads((root / 'services.json').read_text())
def labels(n):
    identity = root / '.dockstride/identity.json'
    owner = json.loads(identity.read_text())['id'] if identity.exists() else ''
    return {'io.dockstride.owner': 'foreign' if mode == 'foreign' else owner,
            'io.dockstride.project':'status-fixture', 'com.docker.compose.project':'status-fixture',
            'com.docker.compose.service':n, 'com.docker.compose.oneoff':'False'}
def row(n, c):
    state = {'Status':c.get('status','running'), 'Running':c.get('status','running') == 'running',
             'ExitCode':c.get('exit',0), 'OOMKilled':c.get('oom',False)}
    if 'health' in c: state['Health'] = {'Status':c['health']}
    return {'Id':n+'-id', 'Config':{'Labels':labels(n), 'Env':['PASSWORD=never-print-this']},
            'State':state, 'NetworkSettings':{'Ports':{}}}
if a[:2] == ['context','show']: print('fixture')
elif a[:2] == ['context','inspect']: print('unix:///status-fixture.sock')
elif a[:1] == ['version']: print('{}')
elif a[:1] == ['info']:
    if '{{.ID}}' in a: print('status-fixture-daemon')
    elif '{{json .SecurityOptions}}' in a: print('[]')
    else: print(json.dumps({'ID':'status-fixture-daemon','SecurityOptions':[],'Swarm':{'LocalNodeState':'inactive'}}))
elif a[:1] == ['ps']: print('\n'.join(n+'-id' for n in s))
elif a[:2] in (['volume','ls'], ['network','ls'], ['service','ls'], ['secret','ls'], ['config','ls']): pass
elif a[:2] == ['container','inspect']:
    if '--format' in a: print(json.dumps(labels(a[2].removesuffix('-id'))))
    else: print(json.dumps([row(n,c) for n,c in s.items() if n+'-id' in a]))
elif a[:2] == ['container','rm']:
    for identifier in a[2:]: s.pop(identifier.removesuffix('-id'),None)
    (root / 'containers.json').write_text(json.dumps(s))
elif a[:1] == ['compose']:
    i = 1; profiles = set()
    while i < len(a) and a[i].startswith('-'):
        if a[i] == '--profile': profiles.add(a[i+1])
        i += 2
    op = a[i]; rest = a[i+1:]
    if op == 'version': print('2.30.0')
    elif op == 'up':
        # Assert the effective environment is pinned, not ambient/.env selection.
        assert profiles == set(filter(None,os.environ.get('COMPOSE_PROFILES','').split(',')))
        names = [n for n in rest if not n.startswith('-')]
        if not names:
            names = [n for n,d in services.items() if not d.get('profiles') or '*' in profiles or profiles.intersection(d['profiles'])]
        for n in names: s[n] = {'status':'running'}
        (root / 'containers.json').write_text(json.dumps(s))
    elif op in ('logs','watch'): pass
    else: sys.exit(39)
else: sys.exit(39)
"#;

struct Fixture { temp: tempfile::TempDir }
impl Fixture {
    fn new(services: Value) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let f = Self { temp };
        fs::create_dir(f.root().join("bin")).unwrap();
        fs::write(f.root().join("bin/docker"), DOCKER).unwrap();
        fs::set_permissions(f.root().join("bin/docker"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(f.root().join("compose.ncl"), r#"
let contract = {project | String, backend | String | default = "compose"} in
let env | contract = import "env.yaml" in
{ dockstride | not_exported = {Config = contract, readiness = import "readiness.json", oneshots = import "oneshots.json"},
  name = env.project, services = import "services.json" }
"#).unwrap();
        fs::write(f.root().join("env.yaml"), "project: status-fixture\nbackend: compose\n").unwrap();
        f.put("services.json", services);
        f.put("containers.json", json!({}));
        f.put("readiness.json", json!({}));
        f.put("oneshots.json", json!([]));
        f.mode("normal");
        f.success(&["setup"]);
        f
    }
    fn root(&self) -> &Path { self.temp.path() }
    fn put(&self, name: &str, value: Value) { fs::write(self.root().join(name), serde_json::to_vec(&value).unwrap()).unwrap(); }
    fn mode(&self, value: &str) { fs::write(self.root().join("mode"), value).unwrap(); }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dks"));
        c.current_dir(self.root()).env_clear()
            .env("PATH", format!("{}:{}", self.root().join("bin").display(), std::env::var("PATH").unwrap()))
            .env("HOME", self.root().join("home")).env("XDG_DATA_HOME", self.root().join("data"))
            .env("DOCKER_HOST", "unix:///status-fixture.sock").env("STATUS_FIXTURE", self.root())
            .args(["--json", "--non-interactive", "--no-color", "-C"]).arg(self.root()).args(args)
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        c
    }
    fn run(&self, args: &[&str]) -> Output { self.command(args).output().unwrap() }
    fn success(&self, args: &[&str]) -> Value {
        let o = self.run(args);
        assert!(o.status.success(), "{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        terminal(&o)["result"].clone()
    }
    fn failure(&self, args: &[&str], code: i32) -> Value {
        let o = self.run(args);
        assert_eq!(o.status.code(), Some(code), "{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        terminal(&o)
    }
}
fn terminal(o: &Output) -> Value {
    let records: Vec<Value> = o.stdout.split(|b| *b == b'\n').filter(|line| !line.is_empty()).map(|line| serde_json::from_slice(line).unwrap()).collect();
    assert_eq!(records.iter().filter(|r| r["type"] == "result" || r["type"] == "error").count(), 1, "{}", String::from_utf8_lossy(&o.stdout));
    assert!(!String::from_utf8_lossy(&o.stdout).contains("never-print-this"));
    records.last().unwrap().clone()
}
fn report(error: &Value) -> &Value { &error["details"]["status"] }
fn service<'a>(report: &'a Value, name: &str) -> &'a Value { report["services"].as_array().unwrap().iter().find(|r| r["name"] == name).unwrap() }

#[test]
fn absent_failed_unhealthy_and_oom_services_fail_with_complete_report() {
    let f = Fixture::new(json!({"missing":{"image":"fixture"},"api":{"image":"fixture"},"worker":{"image":"fixture"},"db":{"image":"fixture"}}));
    f.put("containers.json", json!({"api":{"status":"exited","exit":7},"worker":{"oom":true},"db":{"health":"unhealthy"}}));
    let e = f.failure(&["status"], 1);
    let r = report(&e);
    assert_eq!(r["requiredServices"], json!(["api","db","missing","worker"]));
    assert_eq!(service(r,"missing")["status"], "not created");
    for name in ["api","worker","db"] { assert_eq!(service(r,name)["ready"], false); assert!(service(r,name)["error"].is_string()); }
    let inspected = f.success(&["status","--inspect-only"]);
    assert_eq!(inspected["ready"], false);
    assert_eq!(inspected["inspectOnly"], true);
}

#[test]
fn exited_application_is_not_a_successful_one_shot_and_failed_prerequisites_fail() {
    let f = Fixture::new(json!({"api":{"image":"fixture","depends_on":{"migrate":{"condition":"service_completed_successfully"}}},"migrate":{"image":"fixture"},"job":{"image":"fixture"}}));
    f.put("containers.json", json!({"api":{},"migrate":{"status":"exited","exit":0},"job":{"status":"exited","exit":0}}));
    let e = f.failure(&["status"],1);
    assert_eq!(service(report(&e),"migrate")["ready"],true);
    assert_eq!(service(report(&e),"job")["ready"],false);
    f.put("oneshots.json",json!(["job"]));
    assert_eq!(f.success(&["status"])["ready"],true);
    f.put("containers.json", json!({"api":{},"migrate":{"status":"exited","exit":4},"job":{"status":"exited","exit":0}}));
    assert_eq!(service(report(&f.failure(&["status","api"],1)),"migrate")["ready"],false);
}

#[test]
fn profile_scope_flags_environment_explicit_roots_and_optional_dependencies_agree() {
    let f = Fixture::new(json!({"api":{"image":"fixture","depends_on":{"db":{"condition":"service_started"},"optional":{"required":false}}},"db":{"image":"fixture","profiles":["storage"]},"debug":{"image":"fixture","profiles":["debug"]},"tools":{"image":"fixture","profiles":["tools"]},"optional":{"image":"fixture","profiles":["extra"]}}));
    f.put("containers.json",json!({"api":{},"db":{},"debug":{},"tools":{}}));
    let r = f.success(&["status"]);
    assert_eq!(r["requiredServices"],json!(["api","db"]));
    assert_eq!(service(&r,"debug")["status"],"excluded");
    assert_eq!(service(&r,"optional")["required"],false);
    assert_eq!(f.success(&["status","debug"])["requiredServices"],json!(["debug"]));
    assert_eq!(f.success(&["status","--profile","debug"])["requiredServices"],json!(["api","db","debug"]));
    let o = f.command(&["status","--profile","debug"]).env("COMPOSE_PROFILES","tools").output().unwrap();
    assert!(o.status.success());
    assert_eq!(terminal(&o)["result"]["requiredServices"],json!(["api","db","debug","tools"]));
    let wildcard = f.failure(&["status","--profile","*"],1);
    assert_eq!(service(report(&wildcard),"optional")["required"],true);
    let o = f.command(&["up","--profile","debug"]).env("COMPOSE_PROFILES","tools").output().unwrap();
    assert!(o.status.success(), "{}",String::from_utf8_lossy(&o.stdout));
    assert_eq!(f.success(&["status"])["activeProfiles"],json!(["debug","tools"]));
}

#[test]
fn applied_profiles_survive_teardown_and_unrelated_failed_startup_but_not_explicit_empty_environment() {
    let f = Fixture::new(json!({"api":{"image":"fixture"},"debug":{"image":"fixture","profiles":["debug"]}}));
    f.success(&["up","--profile","debug"]);
    assert_eq!(f.success(&["status"])["requiredServices"],json!(["api","debug"]));
    f.success(&["down"]);
    let e = f.failure(&["status"],1);
    assert_eq!(report(&e)["activeProfiles"],json!(["debug"]));
    let o = f.run(&["up","no-such-service"]);
    assert!(!o.status.success());
    f.put("containers.json",json!({"api":{}}));
    assert_eq!(report(&f.failure(&["status"],1))["activeProfiles"],json!(["debug"]));
    let o = f.command(&["status"]).env("COMPOSE_PROFILES","").output().unwrap();
    assert!(o.status.success());
    assert_eq!(terminal(&o)["result"]["requiredServices"],json!(["api"]));
    f.success(&["up"]);
    assert_eq!(f.success(&["status"])["activeProfiles"],json!([]));
}

#[test]
fn applied_profile_scope_is_recorded_when_application_readiness_fails() {
    let f = Fixture::new(json!({"api":{"image":"fixture"},"debug":{"image":"fixture","profiles":["debug"]}}));
    f.put("readiness.json",json!({"debug":{"command":["false"]}}));
    let o = f.command(&["up","--profile","debug","--timeout","4"]).output().unwrap();
    assert!(!o.status.success());
    let containers: Value = serde_json::from_slice(&fs::read(f.root().join("containers.json")).unwrap()).unwrap();
    assert_eq!(containers["debug"]["status"],"running");
    let e = f.failure(&["status"],1);
    assert_eq!(report(&e)["activeProfiles"],json!(["debug"]));
    assert_eq!(service(report(&e),"debug")["applicationReady"],false);
}

#[test]
fn invalid_scope_cycles_profiles_and_empty_required_scope_are_configuration_errors() {
    let f = Fixture::new(json!({"only":{"image":"fixture","profiles":["extra"]}}));
    for args in [&["status"][..], &["status","no-such-service"][..], &["status","--inspect-only"][..]] {
        let e = f.failure(args,2);
        assert_eq!(report(&e)["backend"],"compose");
    }
    f.put("services.json",json!({"api":{"image":"fixture","profiles":"broken"}}));
    f.failure(&["status"],2);
    f.put("services.json",json!({"api":{"image":"fixture","depends_on":["db"]},"db":{"image":"fixture","depends_on":["api"]}}));
    f.failure(&["status"],2);
    f.put("services.json",json!({"api":{"image":"fixture"}}));
    f.put("readiness.json",json!({"api":{"command":[]}}));
    f.failure(&["status","--inspect-only"],2);
}

fn http_identity(body: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream,_)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "HTTP readiness was not invoked");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buffer = [0;4096]; stream.read(&mut buffer).unwrap();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    (address, server)
}

#[test]
fn actual_http_identity_and_command_probes_execute_once_and_inspect_only_skips_them() {
    let f = Fixture::new(json!({"api":{"image":"fixture"},"cmd":{"image":"fixture"}}));
    f.put("containers.json",json!({"api":{},"cmd":{}}));
    let (address, server) = http_identity(r#"{"project":"wrong-checkout"}"#);
    fs::write(f.root().join("probe.py"),"import pathlib\np=pathlib.Path('probe-count')\np.write_text(str(int(p.read_text())+1) if p.exists() else '1')\n").unwrap();
    f.put("readiness.json",json!({"api":{"url":format!("http://{address}/identity"),"json":{"project":"status-fixture"}},"cmd":{"command":["python3","probe.py"]}}));
    let e = f.failure(&["status"],1);
    server.join().unwrap();
    assert_eq!(service(report(&e),"api")["applicationReady"],false);
    assert_eq!(service(report(&e),"cmd")["applicationReady"],true);
    assert_eq!(fs::read_to_string(f.root().join("probe-count")).unwrap(),"1");
    let r = f.success(&["status","--inspect-only"]);
    assert!(service(&r,"api")["applicationReady"].is_null());
    assert_eq!(r["ready"],false);
    assert_eq!(fs::read_to_string(f.root().join("probe-count")).unwrap(),"1");
    let (address, server) = http_identity(r#"{"project":"status-fixture","extra":"accepted"}"#);
    f.put("readiness.json",json!({"api":{"url":format!("http://{address}/identity"),"json":{"project":"status-fixture"}}}));
    assert_eq!(service(&f.success(&["status","api"]),"api")["applicationReady"],true);
    server.join().unwrap();
}

#[test]
fn shared_deadline_bounds_probes_and_first_context_lookup_and_preserves_partial_transport_errors() {
    let f = Fixture::new(json!({"a":{"image":"fixture"},"b":{"image":"fixture"}}));
    f.put("containers.json",json!({"a":{},"b":{}}));
    f.put("readiness.json",json!({"a":{"command":["sleep","5"]},"b":{"command":["touch","should-not-run"]}}));
    let start = Instant::now();
    let o = f.command(&["status"]).args(["--timeout","1"]).output().unwrap();
    assert_eq!(o.status.code(),Some(1));
    assert!(start.elapsed() < Duration::from_secs(3));
    let e = terminal(&o); let r = report(&e);
    assert_eq!(r["deadlineExceeded"],true);
    assert_eq!(service(r,"b")["observed"],true);
    assert!(service(r,"b")["applicationReady"].is_null());
    assert!(!f.root().join("should-not-run").exists());
    f.mode("slow-context");
    let start = Instant::now();
    let o = f.command(&["status"]).env_remove("DOCKER_HOST").args(["--timeout","1"]).output().unwrap();
    assert_eq!(o.status.code(),Some(1));
    assert!(start.elapsed() < Duration::from_secs(3));
    let e = terminal(&o); let r = report(&e);
    assert_eq!(r["deadlineExceeded"],true);
    assert_eq!(service(r,"a")["observed"],false);
    assert!(r["context"].is_null());
    f.mode("context-error");
    let o = f.command(&["status","--inspect-only"]).env_remove("DOCKER_HOST").output().unwrap();
    let e = terminal(&o);
    assert_eq!(e["details"]["underlyingDockerStatus"],37);
    assert_eq!(e["details"]["status"]["context"],Value::Null);
    assert_eq!(e["details"]["status"]["services"].as_array().unwrap().len(),2);
    assert_eq!(e["details"]["status"]["ready"],false);
    assert!(!o.status.success());
    f.mode("inspect-error");
    let e = f.failure(&["status","--inspect-only"],4);
    assert_eq!(report(&e)["ownershipVerified"],true);
    assert_eq!(service(report(&e),"a")["observed"],false);
    f.mode("foreign");
    assert!(!f.run(&["status","--inspect-only"]).status.success());
}
