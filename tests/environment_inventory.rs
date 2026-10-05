use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output}};
use tempfile::TempDir;

struct Fixture { temp: TempDir, home: PathBuf, root: PathBuf, bin: PathBuf }
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("checkout");
        let bin = temp.path().join("bin");
        for path in [&home, &root, &bin] { fs::create_dir_all(path).unwrap(); }
        let docker = bin.join("docker");
        fs::write(&docker, r#"#!/usr/bin/python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ['DOCKER_CALLS'], 'a') as out:
    out.write(json.dumps({'args':args,'host':os.environ.get('DOCKER_HOST')})+'\n')
if args[:1] == ['info']:
    if os.environ.get('EVIDENCE') == 'unreachable':
        print('target unreachable', file=sys.stderr); sys.exit(1)
    print(json.dumps({'ID':os.environ.get('DAEMON','fixture-daemon'), 'Swarm':{'LocalNodeState':os.environ.get('SWARM','inactive'),'ControlAvailable':os.environ.get('WORKER') != 'yes'}}))
elif len(args) > 1 and args[1] == 'ls':
    assert 'label=io.dockstride.owner=fixture-owner' in args
    kind = args[0]
    if kind == 'service' and '--no-trunc' in args:
        print('unknown flag: --no-trunc',file=sys.stderr); sys.exit(125)
    if kind == 'container': assert '--all' in args
    if os.environ.get('EVIDENCE') == 'partial' and kind == 'volume': sys.exit(1)
    if os.environ.get('RESOURCES') == 'all' or (os.environ.get('EVIDENCE') == 'partial' and kind == 'container'):
        print(json.dumps({'ID':kind+'-id','Name':kind+'-name','Names':kind+'-name','Status':'Exited (0)'}))
else:
    print('Unexpected mutation/query: '+repr(args),file=sys.stderr); sys.exit(1)
"#).unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
        Self { temp, home, root, bin }
    }
    fn private_json(&self, root: &Path, name: &str, value: &Value) {
        let dir = root.join(".dockstride");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join(format!("{name}.json"));
        fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn global(&self) -> PathBuf { self.home.join(".local/share/dockstride") }
    fn register(&self, roots: &[PathBuf]) {
        let mut records = serde_json::Map::new();
        for root in roots {
            records.insert(root.to_str().unwrap().into(), json!({"root":root,"ownerId":"fixture-owner","project":"fixture","backend":"compose","connection":"host;DOCKER_HOST=unix:///fixture.sock","daemonId":"fixture-daemon","sourceFiles":[],"allocatedEndpoints":{},"state":"committed"}));
            if root.is_dir() {
                fs::write(root.join("compose.ncl"), "this is deliberately not valid Nickel").unwrap();
                fs::write(root.join("env.yaml"), "project: fixture\n").unwrap();
                self.private_json(root, "identity", &json!({"id":"fixture-owner","root":root,"project":"fixture","backend":"compose","context":"host;DOCKER_HOST=unix:///fixture.sock","resources":false}));
            }
        }
        self.private_json(&self.global(), "environment-registry", &json!({"schemaVersion":1,"environments":records}));
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dks"));
        command.args(["--json", "--non-interactive", "--no-color", "--directory"]).arg(&self.root).args(args)
            .env("HOME", &self.home).env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("GIT_CEILING_DIRECTORIES", self.temp.path())
            .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_COMMON_DIR")
            .env("DOCKER_CALLS", self.temp.path().join("docker-calls"))
            .env_remove("DOCKER_CONTEXT").env_remove("DOCKER_HOST").env_remove("DOCKER_CONFIG")
            .env_remove("DOCKER_TLS_VERIFY").env_remove("DOCKER_CERT_PATH").env_remove("DOCKER_TLS");
        command
    }
    fn run(&self, args: &[&str]) -> Output { self.command(args).output().unwrap() }
    fn result(&self, args: &[&str]) -> Value { successful(&self.run(args)) }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.temp.path().join("docker-calls")).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
}
fn terminal(output: &Output) -> Value {
    serde_json::from_str(std::str::from_utf8(&output.stdout).unwrap().lines().last().unwrap()).unwrap()
}
fn successful(output: &Output) -> Value {
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    terminal(output)["result"].clone()
}

#[test]
fn saved_inventory_handles_missing_stale_unreadable_without_docker_or_evaluation() {
    let f = Fixture::new();
    let missing = f.temp.path().join("missing");
    let stale = f.temp.path().join("stale");
    let unreadable = f.temp.path().join("unreadable");
    fs::create_dir(&stale).unwrap(); fs::create_dir(&unreadable).unwrap();
    f.register(&[f.root.clone(), missing.clone(), stale.clone(), unreadable.clone()]);
    f.private_json(&stale, "identity", &json!({"id":"another-owner"}));
    fs::remove_file(unreadable.join("env.yaml")).unwrap();
    fs::create_dir(unreadable.join("env.yaml")).unwrap();
    let result = successful(&f.command(&["env", "list"]).env("PATH", "").output().unwrap());
    let entries = result["environments"].as_array().unwrap();
    for (root, expected) in [(&f.root, "present"), (&missing, "missing"), (&stale, "stale"), (&unreadable, "unreadable")] {
        assert_eq!(entries.iter().find(|entry| entry["root"] == root.to_str().unwrap()).unwrap()["status"], expected);
    }
    assert!(f.calls().is_empty());
    assert!(!f.root.join(".dockstride/locks").exists());
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(root)
        .env("GIT_AUTHOR_NAME", "Fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.test").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn worktree_discovery_is_repository_scoped_and_git_is_optional() {
    let f = Fixture::new();
    f.register(&[f.root.clone()]);
    assert_eq!(f.result(&["env", "list", "--worktrees"])["worktreeDiscovery"]["status"], "not-git");
    git(&f.root, &["init"]);
    git(&f.root, &["commit", "--allow-empty", "-m", "fixture"]);
    let extra = f.temp.path().join("worktree with spaces\nand newline");
    git(&f.root, &["worktree", "add", "--detach", extra.to_str().unwrap()]);
    fs::write(extra.join("compose.ncl"), "not evaluated").unwrap();
    fs::write(extra.join("env.yaml"), "{}\n").unwrap();
    let result = f.result(&["env", "list", "--worktrees"]);
    let rows = result["worktreeDiscovery"]["worktrees"].as_array().unwrap();
    let extra_row = rows.iter().find(|row| row["root"] == extra.to_str().unwrap()).unwrap();
    assert_eq!(extra_row["registration"], "unregistered");
    assert_eq!(extra_row["status"], "configuration-present");
    assert_eq!(rows.iter().find(|row| row["root"] == f.root.to_str().unwrap()).unwrap()["registration"], "registered");
    assert!(f.calls().is_empty());
}

#[test]
fn forget_reports_all_owned_resources_including_stopped_and_swarm_objects() {
    let f = Fixture::new(); f.register(&[f.root.clone()]);
    let result = successful(&f.command(&["env", "forget", f.root.to_str().unwrap(), "--plan"])
        .env("RESOURCES", "all").env("SWARM", "active").output().unwrap());
    assert_eq!(result["allowed"], false);
    let resources = result["resources"].as_array().unwrap();
    for kind in ["container", "network", "volume", "service", "secret", "config"] {
        assert!(resources.iter().any(|resource| resource["kind"] == kind && resource["name"] == format!("{kind}-name")));
    }
    assert!(f.calls().iter().all(|call| call["host"] == "unix:///fixture.sock"));
    assert!(!f.root.join(".dockstride/locks").exists());
    let rejected = f.command(&["env", "forget", f.root.to_str().unwrap(), "--yes"])
        .env("RESOURCES", "all").env("SWARM", "active").output().unwrap();
    assert!(!rejected.status.success());
    assert!(fs::read_to_string(f.global().join(".dockstride/environment-registry.json")).unwrap().contains("fixture-owner"));
}

#[test]
fn forget_fails_closed_for_wrong_daemon_unreachable_worker_and_partial_evidence() {
    let f = Fixture::new(); f.register(&[f.root.clone()]);
    for (key, value) in [("DAEMON", "replacement-daemon"), ("EVIDENCE", "unreachable")] {
        let result = successful(&f.command(&["env", "forget", f.root.to_str().unwrap(), "--plan"]).env(key, value).output().unwrap());
        assert_eq!(result["allowed"], false);
        assert_eq!(result["blockers"][0]["kind"], "resource-evidence-failure");
    }
    let worker = successful(&f.command(&["env", "forget", f.root.to_str().unwrap(), "--plan"]).env("SWARM", "active").env("WORKER", "yes").output().unwrap());
    assert_eq!(worker["allowed"], false);
    let partial = successful(&f.command(&["env", "forget", f.root.to_str().unwrap(), "--plan"]).env("EVIDENCE", "partial").output().unwrap());
    assert_eq!(partial["allowed"], false);
    assert!(partial["resources"].as_array().unwrap().iter().any(|resource| resource["kind"] == "container"));
    assert!(partial["blockers"].as_array().unwrap().iter().any(|blocker| blocker["kind"] == "resource-evidence-failure" && blocker["resourceKind"] == "volume"));
}

#[test]
fn forget_blocks_legacy_versioned_and_local_reservations() {
    let f = Fixture::new(); f.register(&[f.root.clone()]);
    for reservations in [json!({"127.0.0.1:45000/tcp":f.root}), json!({"schemaVersion":1,"reservations":{"127.0.0.1:45000/tcp":{"root":f.root,"ownerId":"fixture-owner","daemonId":"fixture-daemon"}}})] {
        f.private_json(&f.global(), "port-reservations", &reservations);
        let result = f.result(&["env", "forget", f.root.to_str().unwrap(), "--plan"]);
        assert_eq!(result["allowed"], false);
        assert!(result["blockers"].as_array().unwrap().iter().any(|blocker| blocker["kind"] == "reservation" && blocker["key"] == "127.0.0.1:45000/tcp"));
    }
    f.private_json(&f.global(), "port-reservations", &json!({}));
    f.private_json(&f.root, "ports", &json!({"apiPort":45000}));
    let result = f.result(&["env", "forget", f.root.to_str().unwrap(), "--plan"]);
    assert_eq!(result["allowed"], false);
    assert!(result["blockers"].as_array().unwrap().iter().any(|blocker| blocker["kind"] == "local-allocation" && blocker["field"] == "apiPort"));
}

#[test]
fn forget_missing_original_root_does_not_adopt_moved_checkout_or_delete_files() {
    let f = Fixture::new();
    let saved = f.temp.path().join("saved"); fs::create_dir(&saved).unwrap();
    f.register(&[saved.clone()]);
    fs::write(saved.join(".dockstride/private-revision"), "retained secret fixture").unwrap();
    let moved = f.temp.path().join("moved"); fs::rename(&saved, &moved).unwrap();
    let rejected = f.run(&["env", "forget", moved.to_str().unwrap(), "--yes"]);
    assert!(!rejected.status.success());
    assert_eq!(f.result(&["env", "forget", saved.to_str().unwrap(), "--plan"])["allowed"], true);
    let result = f.result(&["env", "forget", saved.to_str().unwrap(), "--yes"]);
    assert_eq!(result["forgotten"], true);
    let registry: Value = serde_json::from_slice(&fs::read(f.global().join(".dockstride/environment-registry.json")).unwrap()).unwrap();
    assert_eq!(registry["environments"], json!({}));
    assert_eq!(fs::read_to_string(moved.join(".dockstride/private-revision")).unwrap(), "retained secret fixture");
    assert!(moved.join("env.yaml").is_file());
}

#[test]
fn inventory_pending_worker() {
    let Some(root) = std::env::var_os("INVENTORY_PENDING_ROOT") else { return };
    let root = PathBuf::from(root);
    let _lifecycle = dockstride::state::lock(&root, "lifecycle").unwrap();
    let _global = dockstride::state::global_lock().unwrap();
    let _config = dockstride::state::lock(&root, "config").unwrap();
    let change = dockstride::publication::Change::replace(&root.join("env.yaml"), b"project: pending\n", 0o600).unwrap();
    dockstride::publication::stage_locked(&root, "pending-fixture", vec![change], json!({"root":root,"ownerId":"fixture-owner","project":"fixture","daemonId":"fixture-daemon"})).unwrap();
}

#[test]
fn pending_claims_remain_visible_and_block_forget_without_recovery() {
    let f = Fixture::new(); f.register(&[f.root.clone()]);
    let output = Command::new(std::env::current_exe().unwrap()).args(["--exact", "inventory_pending_worker", "--nocapture"])
        .env("HOME", &f.home).env("INVENTORY_PENDING_ROOT", &f.root).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let list = f.result(&["env", "list"]);
    assert_eq!(list["environments"][0]["state"], "pending");
    assert_eq!(list["environments"][0]["pendingOperations"][0]["operation"], "pending-fixture");
    let result = f.result(&["env", "forget", f.root.to_str().unwrap(), "--plan"]);
    assert_eq!(result["allowed"], false);
    assert!(result["blockers"].as_array().unwrap().iter().any(|blocker| blocker["kind"] == "pending-operation"));
    assert_eq!(fs::read_to_string(f.root.join("env.yaml")).unwrap(), "project: fixture\n");
}

#[test]
fn resource_free_swarm_target_can_be_forgotten() {
    let f = Fixture::new(); f.register(&[f.root.clone()]);
    let plan = successful(&f.command(&["env","forget",f.root.to_str().unwrap(),"--plan"])
        .env("SWARM","active").output().unwrap());
    assert_eq!(plan["allowed"],true);
    let result = successful(&f.command(&["env","forget",f.root.to_str().unwrap(),"--yes"])
        .env("SWARM","active").output().unwrap());
    assert_eq!(result["forgotten"],true);
    assert_eq!(f.result(&["env","list"])["environments"],json!([]));
}
