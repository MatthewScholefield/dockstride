//! Consumer-visible native action regressions against a stateful Docker subprocess.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

// No Docker daemon or Python packages are needed. Unlike an argv recorder, this
// adapter executes Compose effects: an ordinary dependency traversal launches a
// completed one-shot again, whereas --no-deps leaves its container untouched.
const DOCKER: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, signal, sys, time
root = pathlib.Path(os.environ['NATIVE_FIXTURE'])
p = root / 'docker-state.json'
args = sys.argv[1:]
if args[:1] == ['--context']: args = args[2:]
with (root / 'docker-calls').open('a') as f: f.write(json.dumps(args) + '\n')
s = json.loads(p.read_text())
model = json.loads((root / 'services.json').read_text())
one_shots = {'migrate', 'prepare', 'audit'}
def save():
    temporary = p.with_suffix('.pending')
    temporary.write_text(json.dumps(s))
    temporary.replace(p)
def labels(name):
    owner = str(root.resolve())
    return {'io.dockstride.owner': owner, 'io.dockstride.project': 'native-fixture',
            'com.docker.compose.project': 'native-fixture',
            'com.docker.compose.service': name, 'com.docker.compose.oneoff': 'False'}
def row(name, c):
    state = {'Status': c['status'], 'Running': c['status'] == 'running',
             'ExitCode': c.get('exit', 0), 'OOMKilled': False}
    if c.get('health') is not None: state['Health'] = {'Status': c['health']}
    return {'Id': c['id'], 'Name': '/native-fixture-' + name + '-1',
            'Config': {'Labels': labels(name)}, 'State': state,
            'NetworkSettings': {'Ports': {}}, 'Mounts': []}
def inspect_progress():
    for name, c in s['containers'].items():
        if c.get('pending'):
            c['probes'] = c.get('probes', 0) + 1
            if c['probes'] >= 2:
                c['pending'] = False
                if name == 'db': c['health'] = 'healthy'
                else: c.update(status='exited', exit=0)
    save()
def observation(name):
    deps = model[name].get('depends_on', {})
    if isinstance(deps, list): deps = {n: {} for n in deps}
    for dependency, spec in deps.items():
        c = s['containers'].get(dependency)
        if not c and isinstance(spec, dict) and spec.get('required') is False: continue
        condition = spec.get('condition', 'service_started') if isinstance(spec, dict) else 'service_started'
        ok = c is not None and (
            c['status'] == 'running' if condition == 'service_started' else
            c.get('health') == 'healthy' if condition == 'service_healthy' else
            c['status'] == 'exited' and c.get('exit') == 0)
        if not ok: s['violations'].append({'consumer': name, 'dependency': dependency, 'condition': condition})
    m = s['containers'].get('migrate', {})
    s['observations'].append({'service': name, 'migrationId': m.get('id'),
                              'migrationStatus': m.get('status'), 'migrationExit': m.get('exit')})
def launch(name, fresh=False):
    old = s['containers'].get(name)
    if name not in one_shots and old and old['status'] == 'running' and not fresh: return
    if name not in model: raise SystemExit(29)
    if old and not fresh:
        c = old
        c.update(status='running', exit=0)
    else:
        generation = s['generations'].get(name, 0) + 1
        s['generations'][name] = generation
        c = {'id': name + '-' + str(generation), 'status': 'running', 'exit': 0}
        s['containers'][name] = c
    s['starts'][name] = s['starts'].get(name, 0) + 1
    if name == 'db': c.update(health='starting', pending=True, probes=0)
    elif name in {'prepare', 'audit'}: c.update(pending=True, probes=0)
    elif name == 'migrate':
        observation(name)
        mode = (root / 'migration-mode').read_text().strip()
        if mode == 'block':
            save()
            (root / 'migration-entered').write_text(c['id'])
            # Detached Compose returns while the container remains running.
            if '--detach' in args: return
            def interrupted(signum, frame):
                c.update(status='exited', exit=128 + signum)
                save()
                raise SystemExit(128 + signum)
            signal.signal(signal.SIGTERM, interrupted)
            signal.signal(signal.SIGINT, interrupted)
            until = time.monotonic() + 20
            while time.monotonic() < until: time.sleep(0.02)
            c.update(status='exited', exit=124)
        else: c.update(status='exited', exit=23 if mode == 'fail' else 0)
        save()
    else: observation(name)
def dependencies(name, seen):
    if name in seen: return
    seen.add(name)
    deps = model[name].get('depends_on', {})
    if isinstance(deps, list): deps = {n: {} for n in deps}
    for dependency, spec in deps.items():
        if dependency not in model:
            if isinstance(spec, dict) and spec.get('required') is False: continue
            raise SystemExit(29)
        dependencies(dependency, seen)
    launch(name)

if args[:1] == ['version']: print(json.dumps({'Server': {'Version': 'fixture'}}))
elif args[:1] == ['info']:
    if args[-1] == '{{.ID}}': print('native-fixture-daemon')
    elif args[-1] == '{{json .ID}}': print(json.dumps('native-fixture-daemon'))
    elif args[-1] == '{{json .SecurityOptions}}': print('[]')
    elif args[-1] == '{{.Swarm.LocalNodeState}}': print('active')
    elif args[-1] == '{{json .Swarm}}': print(json.dumps({'LocalNodeState': 'active', 'ControlAvailable': True, 'Cluster': {'ID': 'native-cluster'}}))
    else: raise SystemExit(31)
elif args[:1] == ['ps']:
    print('\n'.join(c['id'] for c in s['containers'].values()))
elif args[:2] in (['network', 'ls'], ['volume', 'ls'], ['service', 'ls'], ['secret', 'ls'], ['config', 'ls']): pass
elif args[:1] == ['inspect'] or args[:2] == ['container', 'inspect']:
    rest = args[1:] if args[0] == 'inspect' else args[2:]
    if '--format' in rest:
        identifier = rest[0]
        name = next(n for n, c in s['containers'].items() if c['id'] == identifier or n == identifier)
        print(json.dumps(labels(name)))
    else:
        inspect_progress()
        print(json.dumps([row(n, c) for n, c in s['containers'].items() if c['id'] in rest or n in rest]))
elif args[:1] == ['compose']:
    i = 1
    while i < len(args) and args[i].startswith('-'):
        i += 2 if args[i] in {'--project-directory', '--project-name', '--file', '--profile'} else 1
    op = args[i]
    rest = args[i + 1:]
    names = []
    i = 0
    while i < len(rest):
        if rest[i] in {'--exit-code-from', '--timeout', '-t', '--tail', '--format'}: i += 2
        elif rest[i].startswith('-'): i += 1
        else: names.append(rest[i]); i += 1
    if op == 'version': print('2.30.0')
    elif op == 'stop':
        for name in names or list(s['containers']):
            c = s['containers'].get(name)
            if c and c['status'] == 'running':
                c.update(status='exited', exit=0)
                s['stops'][name] = s['stops'].get(name, 0) + 1
        save()
    elif op == 'up':
        if not names:
            names = [n for n, definition in model.items() if not definition.get('profiles')]
        seen = set()
        for name in names:
            if '--no-deps' in rest: launch(name, '--force-recreate' in rest)
            else: dependencies(name, seen)
        save()
        if '--exit-code-from' in rest:
            name = rest[rest.index('--exit-code-from') + 1]
            raise SystemExit(s['containers'][name].get('exit', 0))
    elif op == 'ps':
        if '--format' in rest:
            print(json.dumps([{'ID': c['id'], 'Service': n, 'State': c['status']} for n, c in s['containers'].items() if not names or n in names]))
        else: print('\n'.join(c['id'] for n, c in s['containers'].items() if not names or n in names))
    elif op in {'logs', 'build', 'wait'}: pass
    else: raise SystemExit(31)
else: raise SystemExit(31)
"#;

const SEED: &str = r#"import json, pathlib
r = pathlib.Path(__file__).parent
s = json.loads((r / 'docker-state.json').read_text())
assert s['containers']['backend']['status'] == 'running'
assert s['containers']['migrate']['status'] == 'exited'
assert s['containers']['migrate']['exit'] == 0
p = r / 'seed-count'
p.write_text(str(int(p.read_text()) + 1))
"#;

fn services() -> Value {
    json!({
        "db": {"image":"fixture", "healthcheck":{"test":["CMD", "true"]}},
        "prepare": {"image":"fixture"},
        "audit": {"image":"fixture", "depends_on":{"db":{"condition":"service_healthy"}}},
        "migrate": {"image":"fixture", "depends_on":{
            "db":{"condition":"service_healthy"},
            "prepare":{"condition":"service_completed_successfully"}}},
        "backend": {"image":"fixture", "depends_on":{
            "migrate":{"condition":"service_completed_successfully"},
            "audit":{"condition":"service_completed_successfully"},
            "optional-missing":{"condition":"service_started", "required":false}}},
        "indirect": {"image":"fixture", "depends_on":{"backend":{"condition":"service_started"}}},
        "worker": {"image":"fixture", "profiles":["worker"], "depends_on":{
            "migrate":{"condition":"service_completed_successfully", "restart":true}}},
        "absent": {"image":"fixture", "profiles":["unrelated"]},
        "untouched": {"image":"fixture", "profiles":["unrelated"]}
    })
}

fn native_actions(workflow: &str) -> Value {
    json!([
        {"name":"database", "kind":"up", "service":"db", "workflows":[workflow], "stage":"before", "services":["backend", "worker", "migrate"]},
        {"name":"stop-consumers", "kind":"stop", "targets":["backend", "worker", "absent"], "workflows":[workflow], "stage":"before", "services":["backend", "worker", "migrate"]},
        {"name":"migration", "kind":"prerequisite", "service":"migrate", "fresh":true, "workflows":[workflow], "stage":"before", "services":["backend", "worker", "migrate"]},
        {"name":"seed", "kind":"command", "argv":["python3", "seed.py"], "workflows":[workflow], "stage":"after", "services":["backend", "worker", "migrate"]}
    ])
}

struct Fixture {
    temp: tempfile::TempDir,
}
impl Fixture {
    fn new(backend: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(root.join("bin/docker"), DOCKER).unwrap();
        fs::set_permissions(root.join("bin/docker"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("compose.ncl"), r#"
let contract = {project | String, backend | String | default = "compose"} in
let env | contract = import "env.yaml" in
{
  dockstride | not_exported = {
    Config = contract,
    actions = import "actions.json",
    oneshots = ["migrate", "prepare", "audit"],
    commands.defaults = {argv = ["python3", "defaults.py"]},
    setup.defaults = {command = "defaults", fields = ["project"]},
  },
  name = env.project,
  services = import "services.json",
}
"#).unwrap();
        fs::write(root.join("env.yaml"), format!("project: native-fixture\nbackend: {backend}\n")).unwrap();
        fs::write(root.join("seed.py"), SEED).unwrap();
        fs::write(root.join("defaults.py"), "import pathlib\npathlib.Path('hook-ran').write_text('ran')\nprint('{\"schemaVersion\":1,\"values\":{\"project\":\"native-fixture\"}}')\n").unwrap();
        fs::write(root.join("seed-count"), "0").unwrap();
        fs::write(root.join("migration-mode"), "success").unwrap();
        fs::write(root.join("data-marker"), "persistent database contents\n").unwrap();
        let fixture = Self { temp };
        fixture.write_json("services.json", &services());
        fixture.write_json("actions.json", &json!([]));
        fixture.write_json("docker-state.json", &json!({"containers":{}, "generations":{}, "starts":{}, "stops":{}, "observations":[], "violations":[]}));
        fixture.success(&["setup"]);
        fixture.write_json("actions.json", &native_actions(if backend == "swarm" { "deploy" } else { "up" }));
        let mut state = fixture.state();
        for name in ["backend", "worker", "untouched"] {
            state["containers"][name] = json!({"id":format!("{name}-incumbent"), "status":"running", "exit":0});
        }
        fixture.write_json("docker-state.json", &state);
        fixture
    }
    fn root(&self) -> &Path { self.temp.path() }
    fn write_json(&self, name: &str, value: &Value) {
        fs::write(self.root().join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }
    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root().join("docker-state.json")).unwrap()).unwrap()
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dks"));
        command.current_dir(self.root()).env_clear()
            .env("PATH", format!("{}:{}", self.root().join("bin").display(), std::env::var("PATH").unwrap()))
            .env("HOME", self.root().join("home"))
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("DOCKER_HOST", "unix:///native-fixture.sock")
            .env("NATIVE_FIXTURE", self.root())
            .args(["--json", "--non-interactive", "--no-color", "--timeout", "15", "-C"])
            .arg(self.root()).args(args)
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        command
    }
    fn run(&self, args: &[&str]) -> Output { self.command(args).output().unwrap() }
    fn success(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let terminal = terminal(&output);
        assert_eq!(terminal["type"], "result");
        terminal["result"].clone()
    }
    fn assert_failure_safe(&self, state: &Value) {
        for name in ["backend", "worker"] {
            assert_eq!(state["containers"][name]["status"], "exited", "{state}");
            assert!(state["starts"].get(name).is_none(), "{state}");
            assert_eq!(state["stops"][name], 1, "{state}");
        }
        assert_eq!(state["generations"]["migrate"], 1);
        assert!(state["containers"]["absent"].is_null());
        assert_eq!(state["containers"]["untouched"]["id"], "untouched-incumbent");
        assert_eq!(fs::read_to_string(self.root().join("seed-count")).unwrap(), "0");
        assert_eq!(fs::read_to_string(self.root().join("data-marker")).unwrap(), "persistent database contents\n");
    }
}
fn terminal(output: &Output) -> Value {
    let records: Vec<Value> = output.stdout.split(|b| *b == b'\n').filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap()).collect();
    assert_eq!(records.iter().filter(|r| r["type"] == "result" || r["type"] == "error").count(), 1, "{}", String::from_utf8_lossy(&output.stdout));
    records.last().unwrap().clone()
}

#[test]
fn indirect_and_scoped_startup_runs_one_fresh_migration_and_restarts_only_existing_consumers() {
    let fixture = Fixture::new("compose");
    for (invocation, selection) in [(1, "indirect"), (2, "backend")] {
        fixture.success(&["up", selection]);
        let state = fixture.state();
        assert_eq!(state["generations"]["migrate"], invocation, "{state}");
        assert_eq!(state["starts"]["migrate"], invocation, "{state}");
        assert_eq!(state["containers"]["migrate"]["id"], format!("migrate-{invocation}"));
        assert_eq!(state["containers"]["migrate"]["status"], "exited");
        assert_eq!(state["containers"]["migrate"]["exit"], 0);
        assert_eq!(state["violations"], json!([]), "{state}");
        for name in ["backend", "worker"] {
            assert_eq!(state["containers"][name]["status"], "running", "{state}");
            assert_eq!(state["stops"][name], invocation);
            assert_eq!(state["starts"][name], invocation);
            let observations: Vec<_> = state["observations"].as_array().unwrap().iter()
                .filter(|o| o["service"] == name).collect();
            assert_eq!(observations.len(), invocation as usize);
            assert_eq!(observations.last().unwrap()["migrationId"], format!("migrate-{invocation}"));
            assert_eq!(observations.last().unwrap()["migrationStatus"], "exited");
            assert_eq!(observations.last().unwrap()["migrationExit"], 0);
        }
        assert_eq!(state["containers"]["db"]["health"], "healthy");
        assert_eq!(state["containers"]["prepare"]["exit"], 0);
        assert_eq!(state["containers"]["audit"]["status"], "exited");
        assert!(state["containers"]["optional-missing"].is_null());
        assert!(state["containers"]["absent"].is_null());
        assert_eq!(state["containers"]["untouched"]["id"], "untouched-incumbent");
        assert!(state["starts"]["untouched"].is_null());
        assert_eq!(fs::read_to_string(fixture.root().join("seed-count")).unwrap(), invocation.to_string());
    }
}

#[test]
fn failed_fresh_prerequisite_retains_its_container_and_never_resumes_or_seeds_consumers() {
    let fixture = Fixture::new("compose");
    fs::write(fixture.root().join("migration-mode"), "fail").unwrap();
    let output = fixture.run(&["up", "indirect"]);
    assert!(!output.status.success());
    assert_eq!(terminal(&output)["type"], "error");
    let state = fixture.state();
    fixture.assert_failure_safe(&state);
    assert_eq!(state["containers"]["migrate"]["id"], "migrate-1");
    assert_eq!(state["containers"]["migrate"]["status"], "exited");
    assert_eq!(state["containers"]["migrate"]["exit"], 23);
}

struct RunningChild(Option<Child>);
impl Drop for RunningChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
#[test]
fn cancelling_a_fresh_dev_prerequisite_preserves_stopped_consumers_and_persistent_data() {
    let fixture = Fixture::new("compose");
    fixture.write_json("actions.json", &native_actions("dev"));
    fs::write(fixture.root().join("migration-mode"), "block").unwrap();
    let mut child = RunningChild(Some(fixture.command(&["dev", "indirect"]).spawn().unwrap()));
    // Model discovery precedes the lifecycle budget and competes with other
    // suite subprocesses; cancellation latency is bounded separately below.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !fixture.root().join("migration-entered").exists() {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            let output = child.0.take().unwrap().wait_with_output().unwrap();
            panic!("CLI exited before migration: {}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        }
        assert!(Instant::now() < deadline, "migration never entered");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(unsafe { libc::kill(child.0.as_ref().unwrap().id() as i32, libc::SIGINT) }, 0);
    let deadline = Instant::now() + Duration::from_secs(8);
    while child.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "CLI did not terminate after cancellation");
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.0.take().unwrap().wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(terminal(&output)["category"], "cancelled");
    let state = fixture.state();
    fixture.assert_failure_safe(&state);
    assert_eq!(state["containers"]["migrate"]["id"], "migrate-1");
    assert!(!(state["containers"]["migrate"]["status"] == "exited"
        && state["containers"]["migrate"]["exit"] == 0));
}

#[test]
fn invalid_native_declarations_and_dependency_cycles_are_rejected_before_consumer_stops() {
    for invalid in ["missing-fresh", "invalid-fresh", "after", "non-one-shot", "cycle"] {
        let fixture = Fixture::new("compose");
        let mut actions = native_actions("up");
        let mut model = services();
        match invalid {
            "missing-fresh" => { actions[2].as_object_mut().unwrap().remove("fresh"); }
            "invalid-fresh" => actions[2]["fresh"] = json!("yes"),
            "after" => actions[2]["stage"] = json!("after"),
            "non-one-shot" => actions[2]["service"] = json!("db"),
            "cycle" => model["db"]["depends_on"] = json!({"indirect":{"condition":"service_started"}}),
            _ => unreachable!(),
        }
        fixture.write_json("actions.json", &actions);
        fixture.write_json("services.json", &model);
        let before = fixture.state();
        let output = fixture.run(&["up", "indirect"]);
        assert!(!output.status.success(), "accepted {invalid}");
        assert_eq!(terminal(&output)["type"], "error");
        assert_eq!(fixture.state(), before, "side effects before rejecting {invalid}");
        assert_eq!(fs::read_to_string(fixture.root().join("seed-count")).unwrap(), "0");
    }
}

#[test]
fn applicable_native_compose_kinds_are_rejected_for_swarm_before_any_action_runs() {
    for kind in ["stop", "prerequisite"] {
        let fixture = Fixture::new("swarm");
        // The preceding trusted command would leave a real side effect if all
        // declarations were not preflighted before executing the first action.
        let native = if kind == "stop" {
            json!({"name":"stop", "kind":"stop", "targets":["backend"], "stage":"before", "workflows":["deploy"], "services":["backend"]})
        } else {
            json!({"name":"migration", "kind":"prerequisite", "service":"migrate", "fresh":true, "stage":"before", "workflows":["deploy"], "services":["backend"]})
        };
        fixture.write_json("actions.json", &json!([
            {"name":"must-not-run", "kind":"command", "argv":["python3", "defaults.py"], "workflows":["deploy"], "stage":"before"}, native
        ]));
        let before = fixture.state();
        let output = fixture.run(&["deploy"]);
        assert!(!output.status.success(), "accepted {kind} on Swarm");
        assert_eq!(terminal(&output)["type"], "error");
        assert_eq!(fixture.state(), before);
        assert!(!fixture.root().join("hook-ran").exists());
    }
}

#[test]
fn ordinary_compose_services_and_one_shots_without_native_actions_still_start_successfully() {
    let fixture = Fixture::new("compose");
    fixture.write_json("actions.json", &json!([]));
    fixture.write_json("services.json", &json!({"migrate":{"image":"fixture"}, "plain":{"image":"fixture"}}));
    fixture.write_json("docker-state.json", &json!({"containers":{}, "generations":{}, "starts":{}, "stops":{}, "observations":[], "violations":[]}));
    fixture.success(&["up"]);
    let state = fixture.state();
    assert_eq!(state["containers"]["migrate"]["status"], "exited");
    assert_eq!(state["containers"]["migrate"]["exit"], 0);
    assert_eq!(state["starts"]["migrate"], 1);
    assert_eq!(state["containers"]["plain"]["status"], "running");
    assert_eq!(state["starts"]["plain"], 1);
    assert_eq!(fs::read_to_string(fixture.root().join("seed-count")).unwrap(), "0");
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Option<Vec<u8>>, SystemTime)> {
    fn visit(path: &Path, map: &mut BTreeMap<PathBuf, (Option<Vec<u8>>, SystemTime)>) {
        let metadata = fs::metadata(path).unwrap();
        map.insert(path.to_owned(), (if metadata.is_file() { Some(fs::read(path).unwrap()) } else { None }, metadata.modified().unwrap()));
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() { visit(&entry.unwrap().path(), map); }
        }
    }
    let mut map = BTreeMap::new();
    visit(root, &mut map);
    map
}
#[test]
fn native_plans_and_missing_input_plans_do_not_execute_hooks_docker_or_publish_files() {
    let fixture = Fixture::new("compose");
    for incomplete in [false, true] {
        if incomplete { fs::write(fixture.root().join("env.yaml"), "{}\n").unwrap(); }
        let before = snapshot(fixture.root());
        fixture.success(&["up", "indirect", "--plan"]);
        assert_eq!(snapshot(fixture.root()), before);
        assert!(!fixture.root().join("hook-ran").exists());
        assert_eq!(fs::read_to_string(fixture.root().join("seed-count")).unwrap(), "0");
    }
}
