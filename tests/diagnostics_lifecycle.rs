//! Consumer-facing startup error preservation and diagnostic ownership boundaries.
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::{Command, Output}};

const DOCKER: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys
r = pathlib.Path(os.environ['DIAGNOSTIC_FIXTURE'])
a = sys.argv[1:]
s = json.loads((r / 'state.json').read_text())
mode = (r / 'docker-mode').read_text().strip()
backend = (r / 'backend').read_text().strip()
owner = json.loads((r / '.dockstride/identity.json').read_text())['id']
def save(): (r / 'state.json').write_text(json.dumps(s))
def labels(name):
    return {'io.dockstride.owner':owner,'io.dockstride.project':'diagnostic-fixture',
            'com.docker.compose.project':'diagnostic-fixture','com.docker.compose.service':name,
            'com.docker.compose.oneoff':'False'}
def row(name, c):
    label = labels(name)
    if mode == 'foreign': label['io.dockstride.owner'] = 'another-checkout'
    if backend == 'swarm':
        label = {'com.docker.swarm.service.id':'serviceabc','com.docker.swarm.task.id':'taskabc'}
        if mode == 'foreign': label['io.dockstride.owner'] = 'another-checkout'
        if mode == 'wrong-task': label['com.docker.swarm.task.id'] = 'othertask'
    return {'Id':c['id'],'Config':{'Labels':label,'Env':['PASSWORD=must-not-leak']},
            'State':{'Status':c['status'],'Running':c['status']=='running','ExitCode':c.get('exit',0),
                     'OOMKilled':False,'Health':{'Status':'healthy','Log':[{'Output':'must-not-leak'}]}},
            'Mounts':[], 'NetworkSettings':{'Ports':{}}}
if a[:1] == ['version']: print(json.dumps({'Server':{'Version':'fixture'}}))
elif a[:1] == ['info']:
    if a[-1] == '{{.ID}}': print('replacement-daemon' if mode == 'changed-daemon' else 'diagnostic-daemon')
    elif a[-1] == '{{json .ID}}': print(json.dumps('replacement-daemon' if mode == 'changed-daemon' else 'diagnostic-daemon'))
    elif a[-1] == '{{json .SecurityOptions}}': print('[]')
    elif a[-1] == '{{.Swarm.LocalNodeState}}': print('active')
    elif a[-1] == '{{json .Swarm}}': print(json.dumps({'LocalNodeState':'active','ControlAvailable':True,'Cluster':{'ID':'diagnostic-cluster'}}))
    else: print(json.dumps({'ID':'diagnostic-daemon','Swarm':{'LocalNodeState':'active','ControlAvailable':True}}))
elif a[:1] == ['ps']: print('\n'.join(c['id'] for c in s['containers'].values()))
elif a[:2] == ['container','inspect']:
    if '--format' in a:
        cid = a[2]
        name = next(n for n,c in s['containers'].items() if c['id'] == cid)
        if a[-1] == '{{.Id}}': print(s['containers'][name]['id'])
        else: print(json.dumps(row(name,s['containers'][name])['Config']['Labels']))
    else: print(json.dumps([row(n,c) for n,c in s['containers'].items() if c['id'] in a[2:]]))
elif a[:2] == ['service','ls']:
    if backend == 'swarm': print('diagnostic-fixture_db')
elif a[:2] == ['service','inspect']:
    label = {'io.dockstride.owner':owner,'io.dockstride.project':'diagnostic-fixture'}
    if mode == 'foreign-service': label['io.dockstride.owner'] = 'another-checkout'
    if '--format' in a: print(json.dumps(label))
    else: print(json.dumps([{'ID':'serviceabc','Spec':{'Name':'diagnostic-fixture_db','Labels':label,
          'Mode':{'Replicated':{'Replicas':1}},'TaskTemplate':{'ContainerSpec':{'Image':'fixture'},'ForceUpdate':0}}}]))
elif a[:2] == ['service','ps']: print('taskabc')
elif a[:1] == ['inspect'] and 'task' in a:
    cid = s['containers']['db']['id']
    print(json.dumps([{'ID':'taskabc','ServiceID':'serviceabc','Slot':1,
          'Spec':{'ContainerSpec':{'Image':'fixture'},'ForceUpdate':0},'DesiredState':'running',
          'Status':{'State':'running','ContainerStatus':{'ContainerID':cid,'ExitCode':0}}}]))
elif a[:2] in (['network','ls'],['volume','ls'],['secret','ls'],['config','ls']): pass
elif a[:1] == ['compose']:
    i = 1
    while i < len(a) and a[i].startswith('-'): i += 2
    op = a[i]
    rest = a[i+1:]
    if op == 'version': print('2.30.0')
    elif op == 'stop':
        for name in rest:
            if name in s['containers']: s['containers'][name]['status'] = 'exited'
        save()
    elif op == 'up':
        (r / 'resource-started').write_text('yes')
        if mode == 'docker-failure': raise SystemExit(29)
        if 'migrate' in rest:
            s['containers']['migrate'] = {'id':'b'*64,'status':'exited','exit':17}
            save()
            if '--exit-code-from' in rest: raise SystemExit(17)
        else:
            for name in s['containers']:
                if name in rest or not rest: s['containers'][name]['status'] = 'running'
            save()
    elif op in ('logs','ps','build'): pass
    else: raise SystemExit(31)
else: raise SystemExit(31)
"#;

const HOOK: &str = r#"import json, os, pathlib, sys, time
r = pathlib.Path(__file__).parent
(r / 'hook-ran').write_text('yes')
i = json.load(sys.stdin)
assert os.environ['DOCKER_HOST'] == 'unix:///diagnostic-fixture.sock'
assert 'must-not-leak' not in json.dumps(i)
mode = (r / 'hook-mode').read_text().strip()
if mode == 'malformed': print('not JSON'); raise SystemExit(0)
if mode == 'nonzero': raise SystemExit(19)
if mode == 'timeout': time.sleep(20)
obs = i['diagnostics']['observations']
rows = obs.get('services', [])
ids = [cid for row in rows for cid in row.get('verifiedContainerIds', [])]
expected = (r / 'expected-proof').read_text().strip()
if expected == 'owned':
    assert 'a'*64 in ids, obs
    code = 'verified-owned-container'
elif expected == 'blocked':
    assert not ids, obs
    assert all('verifiedContainerId' not in t for row in rows for t in row.get('tasks', [])), obs
    code = 'unverified-container-blocked'
else:
    code = 'migration-failure'
print(json.dumps({'schemaVersion':1,'findings':[{'code':code,'severity':'error',
    'summary':'Read-only diagnosis','evidence':{'trigger':i['diagnostics']['trigger']},
    'suggestedCommand':['python3','-c',"open('repair-ran','w').write('unsafe')"]}]}))
"#;

struct Fixture { temp: tempfile::TempDir }
impl Fixture {
    fn new(backend: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let r = temp.path().to_path_buf();
        fs::create_dir(r.join("bin")).unwrap();
        fs::write(r.join("bin/docker"), DOCKER).unwrap();
        fs::set_permissions(r.join("bin/docker"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(r.join("compose.ncl"), r#"
let contract = {project | String, backend | String | default = "compose"} in
let env | contract = import "env.yaml" in
{
  dockstride | not_exported = {
    Config = contract,
    oneshots = ["migrate"],
    actions = import "actions.json",
    diagnostics = import "diagnostics.json",
    readiness = import "readiness.json",
    commands.diagnose = {argv = ["python3", "diagnose.py"], timeoutSeconds = 1},
  },
  name = env.project,
  services = import "services.json",
}
"#).unwrap();
        fs::write(r.join("env.yaml"), format!("project: diagnostic-fixture\nbackend: {backend}\n")).unwrap();
        fs::write(r.join("diagnose.py"), HOOK).unwrap();
        fs::write(r.join("docker-mode"), "normal").unwrap();
        fs::write(r.join("hook-mode"), "normal").unwrap();
        fs::write(r.join("expected-proof"), "owned").unwrap();
        fs::write(r.join("backend"), backend).unwrap();
        let f = Self { temp };
        dockstride::state::save(&r, "identity", &json!({"id":"fixture-owner","root":r,
            "project":"diagnostic-fixture","backend":backend,
            "context":"host;DOCKER_HOST=unix:///diagnostic-fixture.sock","resources":true})).unwrap();
        f.write_json("services.json", &json!({"db":{"image":"fixture"}}));
        f.write_json("actions.json", &json!([]));
        f.write_json("readiness.json", &json!({}));
        f.write_json("diagnostics.json", &json!({"database":{"command":"diagnose","services":["db"],"on":["startup-failed","unhealthy"]}}));
        f.write_json("state.json", &json!({"containers":{"db":{"id":"a".repeat(64),"status":"running"}}}));
        f
    }
    fn root(&self) -> &Path { self.temp.path() }
    fn write_json(&self, name: &str, value: &Value) {
        fs::write(self.root().join(name), serde_json::to_vec(value).unwrap()).unwrap();
    }
    fn write(&self, name: &str, value: &str) { fs::write(self.root().join(name), value).unwrap(); }
    fn run(&self, args: &[&str]) -> Output { self.run_timed(args, "10") }
    fn run_timed(&self, args: &[&str], timeout: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dks")).current_dir(self.root()).env_clear()
            .env("PATH", format!("{}:{}", self.root().join("bin").display(), std::env::var("PATH").unwrap()))
            .env("HOME", self.root().join("home")).env("XDG_DATA_HOME", self.root().join("data"))
            .env("DOCKER_HOST", "unix:///diagnostic-fixture.sock").env("DIAGNOSTIC_FIXTURE", self.root())
            .args(["--json", "--non-interactive", "--no-color", "--timeout", timeout, "-C"])
            .arg(self.root()).args(args).output().unwrap()
    }
    fn native(&self) {
        self.write("expected-proof", "startup");
        self.write_json("services.json", &json!({"db":{"image":"fixture"},"migrate":{"image":"fixture"},
            "backend":{"image":"fixture","depends_on":{"migrate":{"condition":"service_completed_successfully"}}}}));
        self.write_json("actions.json", &json!([
            {"name":"stop","kind":"stop","targets":["backend"],"services":["backend"],"workflows":["up"],"stage":"before"},
            {"name":"migration","kind":"prerequisite","service":"migrate","fresh":true,"services":["backend"],"workflows":["up"],"stage":"before"},
            {"name":"seed","kind":"command","argv":["python3","-c","open('seed-ran','w').write('unsafe')"],"workflows":["up"],"stage":"after"}
        ]));
        self.write_json("diagnostics.json", &json!({"migration":{"command":"diagnose","services":["migrate"],"on":["startup-failed"]}}));
        self.write_json("state.json", &json!({"containers":{
            "db":{"id":"a".repeat(64),"status":"running"},"backend":{"id":"c".repeat(64),"status":"running"}}}));
    }
}
fn terminal(output: &Output) -> Value {
    output.stdout.split(|b| *b == b'\n').filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .find(|record| record["type"] == "result" || record["type"] == "error")
        .unwrap_or_else(|| panic!("No terminal result: {}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
}

#[test]
fn native_failure_retains_exit_and_stop_safety_with_successful_or_failed_hooks() {
    for mode in ["normal", "malformed", "nonzero", "timeout"] {
        let f = Fixture::new("compose");
        f.native();
        f.write("hook-mode", mode);
        let output = f.run(&["up", "backend"]);
        let record = terminal(&output);
        assert_eq!(output.status.code(), Some(1), "{record}");
        assert_eq!(record["category"], "operation", "{record}");
        assert_eq!(record["details"]["prerequisite"]["exitCode"], 17, "{record}");
        let report = &record["details"]["diagnostics"];
        if mode == "normal" {
            assert_eq!(report["findings"][0]["code"], "migration-failure", "{record}");
        } else {
            assert!(report["failures"].as_array().unwrap().iter().any(|row| row["hook"] == "migration"), "{record}");
        }
        let state: Value = serde_json::from_slice(&fs::read(f.root().join("state.json")).unwrap()).unwrap();
        assert_eq!(state["containers"]["backend"]["status"], "exited", "{state}");
        assert_eq!(state["containers"]["migrate"]["exit"], 17, "{state}");
        assert!(!f.root().join("seed-ran").exists());
        assert!(!f.root().join("repair-ran").exists());
    }
}

#[test]
fn genuine_docker_start_failure_retains_docker_exit_status() {
    let f = Fixture::new("compose");
    f.write("docker-mode", "docker-failure");
    let output = f.run(&["up"]);
    let record = terminal(&output);
    assert_eq!(output.status.code(), Some(4), "{record}");
    assert_eq!(record["category"], "docker", "{record}");
    assert_eq!(record["details"]["underlyingDockerStatus"], 29, "{record}");
    assert_eq!(record["details"]["diagnostics"]["findings"][0]["code"], "verified-owned-container", "{record}");
}

#[test]
fn doctor_exposes_only_owned_immutable_container_linkage() {
    for (backend, mode, expected) in [
        ("compose", "normal", "owned"), ("compose", "foreign", "blocked"),
        ("swarm", "normal", "owned"), ("swarm", "foreign", "blocked"),
        ("swarm", "wrong-task", "blocked"), ("swarm", "foreign-service", "blocked"),
    ] {
        let f = Fixture::new(backend);
        f.write("docker-mode", mode);
        f.write("expected-proof", expected);
        let output = f.run(&["doctor"]);
        let record = terminal(&output);
        assert!(output.status.success(), "{record}");
        let expected_code = if expected == "owned" { "verified-owned-container" } else { "unverified-container-blocked" };
        assert_eq!(record["result"]["diagnostics"]["findings"][0]["code"], expected_code, "{record}");
        assert!(!f.root().join("repair-ran").exists());
    }
}

#[test]
fn replaced_daemon_cannot_supply_verified_ids_from_a_saved_registration() {
    let f = Fixture::new("compose");
    let setup = f.run(&["setup"]);
    assert!(setup.status.success(), "{}", terminal(&setup));
    f.write("docker-mode", "changed-daemon");
    f.write("expected-proof", "blocked");
    let output = f.run(&["doctor"]);
    let record = terminal(&output);
    assert!(output.status.success(), "{record}");
    let report = &record["result"]["diagnostics"];
    assert_eq!(report["findings"][0]["code"], "unverified-container-blocked", "{record}");
    assert!(report["failures"].as_array().unwrap().iter().any(|row| row["stage"] == "observations"), "{record}");
    assert!(!f.root().join("repair-ran").exists());
}

#[test]
fn invalid_declarations_fail_before_starts_and_read_only_surfaces_do_not_dispatch() {
    let f = Fixture::new("compose");
    for args in [&["up", "--plan"][..], &["status", "--inspect-only"][..], &["render"][..], &["doctor", "--plan"][..]] {
        let output = f.run(args);
        assert!(output.status.success(), "{}", terminal(&output));
        assert!(!f.root().join("hook-ran").exists());
    }
    for backend in ["compose", "swarm"] {
        let f = Fixture::new(backend);
        f.write_json("diagnostics.json", &json!({"invalid":{"command":"diagnose","services":["absent"],"on":["startup-failed"]}}));
        let output = f.run(&[if backend == "compose" { "up" } else { "deploy" }]);
        let record = terminal(&output);
        assert_eq!(output.status.code(), Some(2), "{record}");
        assert_eq!(record["category"], "configuration", "{record}");
        assert!(!f.root().join("hook-ran").exists());
        assert!(!f.root().join("resource-started").exists());
    }
}

#[test]
fn exhausted_startup_deadline_keeps_observed_application_failure_and_allows_diagnosis() {
    let f = Fixture::new("compose");
    f.write("readiness.py", "import pathlib,time\np=pathlib.Path('application-probed')\nif p.exists(): time.sleep(30)\np.write_text('failed')\nraise SystemExit(1)\n");
    f.write_json("readiness.json", &json!({"db":{"command":["python3","readiness.py"]}}));
    f.write_json("diagnostics.json", &json!({"application":{"command":"diagnose","services":["db"],"on":["readiness-failed"]}}));
    let output = f.run_timed(&["up"], "10");
    let record = terminal(&output);
    assert_eq!(output.status.code(), Some(1), "{record}");
    assert!(f.root().join("application-probed").exists(), "{record}");
    assert_eq!(record["details"]["status"]["services"][0]["applicationReady"], false, "{record}");
    assert_eq!(record["details"]["diagnostics"]["findings"][0]["code"], "verified-owned-container", "{record}");
    assert!(!f.root().join("repair-ran").exists());
}
