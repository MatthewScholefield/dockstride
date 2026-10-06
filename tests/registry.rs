use dockstride::{nickel, publication, registry, state};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output}};

struct Fixture { directory: tempfile::TempDir, home: PathBuf, bin: PathBuf }
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let bin = directory.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("docker"), r#"#!/usr/bin/env python3
import os,sys,json
args=sys.argv[1:]
if args[:1]==['--context']: args=args[2:]
if args[:1]==['info']:
    if args[-1]=='{{.ID}}': print(os.environ.get('REGISTRY_DAEMON','daemon-a'))
    elif args[-1]=='{{.Swarm.LocalNodeState}}': print('active')
    elif args[-1]=='{{json .SecurityOptions}}': print('[]')
    elif args[-1]=='{{json .Swarm}}': print(json.dumps({'LocalNodeState':'active','ControlAvailable':True,'Cluster':{'ID':'fixture-cluster'}}))
elif args[:2]==['context','show']: print('fixture')
elif args[:2]==['context','inspect']: print('unix:///registry-fixture.sock')
elif args[:1]==['ps']:
    if os.environ.get('FOREIGN_KIND')=='container': print('foreign-container')
elif len(args)>1 and args[1]=='ls':
    if args[0]==os.environ.get('FOREIGN_KIND'):
        if '--format' in args: print('claim_default' if args[0]=='network' else 'claim_data')
        else: print('foreign-'+args[0])
elif len(args)>1 and args[1]=='inspect': print('{}')
else: sys.exit(32)
"#).unwrap();
        fs::set_permissions(bin.join("docker"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { directory, home, bin }
    }
    fn checkout(&self, name: &str) -> PathBuf {
        let root = self.directory.path().join(name);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("env.yaml"), "{}\n").unwrap();
        fs::write(root.join("compose.ncl"), r#"
let contract = {project | String, backend | String | default = "compose", port | Number | default = 8080} in
let env | contract = import "env.yaml" in
{dockstride | not_exported = {Config = contract, endpoints.api = "http://localhost:%{env.port}"}, name = env.project, services.api.image = "alpine", volumes.data = {}}
"#).unwrap();
        root
    }
    fn configure(&self, command: &mut Command) {
        command.env("HOME", &self.home)
            .env("PATH", format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()))
            .env_remove("DOCKER_CONTEXT").env("DOCKER_HOST", "unix:///registry-fixture.sock");
    }
    fn command(&self, root: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dks"));
        command.args(["--json", "--non-interactive", "-C"]).arg(root).args(args);
        self.configure(&mut command);
        command
    }
    fn run(&self, root: &Path, args: &[&str]) -> Output { self.command(root,args).output().unwrap() }
    fn saved(&self) -> Value {
        serde_json::from_slice(&fs::read(self.home.join(".local/share/dockstride/.dockstride/environment-registry.json")).unwrap()).unwrap()
    }
}
fn succeeded(output: &Output) {
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
fn concurrent_complete_setups_reserve_one_daemon_project_before_local_publication() {
    let fixture = Fixture::new();
    let a = fixture.checkout("a");
    let b = fixture.checkout("b");
    let first = fixture.command(&a, &["setup", "--set", "project=claim"]).spawn().unwrap();
    let second = fixture.command(&b, &["setup", "--set", "project=claim"]).env("DOCKER_HOST", "unix:///alias-for-same-daemon.sock").spawn().unwrap();
    let a_ok = first.wait_with_output().unwrap().status.success();
    let b_ok = second.wait_with_output().unwrap().status.success();
    assert_ne!(a_ok,b_ok);
    let (winner, loser) = if a_ok { (&a,&b) } else { (&b,&a) };
    assert_eq!(fs::read_to_string(loser.join("env.yaml")).unwrap(), "{}\n");
    let saved = fixture.saved();
    let entry = &saved["environments"][winner.to_str().unwrap()];
    assert_eq!(entry["project"],"claim");
    assert_eq!(entry["daemonId"],"daemon-a");
    assert_eq!(entry["state"],"committed");
    assert_eq!(saved["environments"].as_object().unwrap().keys().collect::<Vec<_>>(), vec![winner.to_str().unwrap()]);
}

#[test]
fn same_folder_proposals_require_an_explicit_name_on_the_same_daemon() {
    let fixture = Fixture::new();
    let first = fixture.checkout("one/voxellum");
    let second = fixture.checkout("two/voxellum");
    for root in [&first, &second] {
        let project = dockstride::config::project_proposal(root).unwrap();
        let result = fixture.run(root, &["setup", "--set", &format!("project={project}")]);
        if root == &first {
            succeeded(&result);
        } else {
            assert!(!result.status.success());
            assert!(String::from_utf8_lossy(&result.stdout).contains("already registered"));
            assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), "{}\n");
        }
    }
    succeeded(&fixture.run(&second, &["setup", "--set", "project=voxellum-worker"]));
    let saved = fixture.saved();
    assert_eq!(saved["environments"][first.to_str().unwrap()]["project"], "voxellum");
    assert_eq!(saved["environments"][second.to_str().unwrap()]["project"], "voxellum-worker");
}

#[test]
fn a_different_verified_daemon_can_reuse_a_project_name() {
    let fixture = Fixture::new();
    let a = fixture.checkout("a");
    let b = fixture.checkout("b");
    succeeded(&fixture.run(&a, &["setup", "--set", "project=claim"]));
    succeeded(&fixture.command(&b, &["setup", "--set", "project=claim"]).env("REGISTRY_DAEMON", "daemon-b").output().unwrap());
    assert_eq!(fixture.saved()["environments"][b.to_str().unwrap()]["daemonId"], "daemon-b");
}

#[test]
fn incomplete_inputs_are_retained_without_becoming_configured_entries() {
    let fixture = Fixture::new();
    let root = fixture.checkout("a");
    succeeded(&fixture.run(&root, &["config", "set", "port", "9090"]));
    let output = fixture.run(&root, &["env", "list"]);
    succeeded(&output);
    assert!(!fixture.home.join(".local/share/dockstride/.dockstride/environment-registry.json").exists());
    succeeded(&fixture.run(&root, &["config", "set", "project", "claim"]));
    assert_eq!(fixture.saved()["environments"][root.to_str().unwrap()]["allocatedEndpoints"]["api"], "http://localhost:9090");
}

#[test]
fn registration_preserves_owner_and_live_source_layers_but_never_adopts_moved_identity() {
    let fixture = Fixture::new();
    let a = fixture.checkout("a");
    fs::write(a.join("shared.yaml"), "project: claim\nport: 9091\n").unwrap();
    fs::write(a.join("env.yaml"), "_dockstride: {sources: [{path: shared.yaml}]}\n").unwrap();
    succeeded(&fixture.run(&a, &["setup"]));
    let owner = fixture.saved()["environments"][a.to_str().unwrap()]["ownerId"].clone();
    assert!(!fs::read_to_string(a.join("env.yaml")).unwrap().contains("project:"));
    let before = fs::read(a.join("env.yaml")).unwrap();
    let changed_target = fixture.command(&a, &["config", "set", "port", "9999"]).env("REGISTRY_DAEMON", "daemon-b").output().unwrap();
    assert!(!changed_target.status.success());
    assert_eq!(fs::read(a.join("env.yaml")).unwrap(), before);
    succeeded(&fixture.run(&a, &["config", "set", "port", "9191"]));
    assert_eq!(fixture.saved()["environments"][a.to_str().unwrap()]["ownerId"],owner);
    assert_eq!(fixture.saved()["environments"][a.to_str().unwrap()]["sourceFiles"],json!([a.join("shared.yaml")]));
    let b = fixture.checkout("b");
    fs::create_dir_all(b.join(".dockstride")).unwrap();
    fs::set_permissions(b.join(".dockstride"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy(a.join(".dockstride/identity.json"),b.join(".dockstride/identity.json")).unwrap();
    let rejected = fixture.run(&b, &["setup", "--set", "project=other"]);
    assert!(!rejected.status.success());
    assert_eq!(fs::read_to_string(b.join("env.yaml")).unwrap(),"{}\n");
}

#[test]
fn shared_identity_edits_reserve_before_modifying_the_shared_document() {
    let fixture = Fixture::new();
    let a = fixture.checkout("a");
    let b = fixture.checkout("b");
    fs::write(a.join("shared.yaml"), "project: first\n").unwrap();
    fs::write(a.join("env.yaml"), "_dockstride: {sources: [{path: shared.yaml}]}\n").unwrap();
    succeeded(&fixture.run(&a, &["setup"]));
    succeeded(&fixture.run(&b, &["setup", "--set", "project=second"]));
    let rejected = fixture.run(&a, &["config", "set", "project", "second", "--shared"]);
    assert!(!rejected.status.success());
    assert_eq!(fs::read_to_string(a.join("shared.yaml")).unwrap(), "project: first\n");
    assert_eq!(fixture.saved()["environments"][a.to_str().unwrap()]["project"], "first");
}

#[test]
fn missing_secret_provisioning_rejects_an_incumbent_claim_before_creating_revisions() {
    let fixture = Fixture::new();
    let a = fixture.checkout("a");
    let b = fixture.checkout("b");
    succeeded(&fixture.run(&a, &["setup", "--set", "project=claim"]));
    fs::create_dir_all(b.join("libs")).unwrap();
    fs::write(b.join("libs/dockstride.ncl"), include_str!("../assets/dockstride.ncl")).unwrap();
    fs::write(b.join("compose.ncl"), r#"
let lib = import "libs/dockstride.ncl" in
let contract = {project | String, backend | lib.Backend | default = "swarm", secrets.authKey | lib.SecretSource} in
let env | contract = import "env.yaml" in
{dockstride | not_exported = {Config = contract, setup.secrets.authKey = lib.GenerateSecret {bytes = 32, encoding = "hex"}}, secrets = env.secrets, services.api = {image = "alpine", user = "0", secrets = ["authKey"]}}
"#).unwrap();
    let rejected = fixture.run(&b, &["setup", "--set", "project=claim"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stdout).contains("already registered"), "{}", String::from_utf8_lossy(&rejected.stdout));
    assert!(!b.join(".dockstride/secrets.json").exists(), "collision created a secret revision journal");
    assert_eq!(fixture.saved()["environments"][a.to_str().unwrap()]["project"], "claim");
    assert!(fixture.saved()["environments"].get(b.to_str().unwrap()).is_none());
}

#[test]
fn foreign_project_resources_and_unidentified_daemons_cannot_claim() {
    let fixture = Fixture::new();
    for kind in ["container","network","volume","service","secret","config"] {
        let root = fixture.checkout(kind);
        let backend = if matches!(kind,"service"|"secret"|"config") { "backend=swarm" } else { "backend=compose" };
        let output = fixture.command(&root,&["setup","--set","project=claim","--set",backend]).env("FOREIGN_KIND",kind).output().unwrap();
        assert!(!output.status.success(), "unexpected claim for {kind}");
        assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(),"{}\n");
    }
    let root = fixture.checkout("unidentified");
    let output = fixture.command(&root,&["setup","--set","project=claim"]).env("REGISTRY_DAEMON", "").output().unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(),"{}\n");
}

#[test]
fn pending_identity_claims_block_other_checkouts_without_recovery_or_local_writes() {
    let fixture = Fixture::new();
    let a = fixture.checkout("a");
    let b = fixture.checkout("b");
    let mut worker = Command::new(std::env::current_exe().unwrap());
    worker.args(["--exact","pending_registry_worker","--nocapture"]).env("REGISTRY_PENDING_ROOT",&a);
    fixture.configure(&mut worker);
    succeeded(&worker.output().unwrap());
    let before = fixture.saved_if_any();
    let rejected = fixture.run(&b,&["setup","--set","project=claim"]);
    assert!(!rejected.status.success());
    assert_eq!(fs::read_to_string(a.join("env.yaml")).unwrap(),"{}\n");
    assert_eq!(fs::read_to_string(b.join("env.yaml")).unwrap(),"{}\n");
    assert_eq!(fixture.saved_if_any(),before);
    succeeded(&fixture.run(&a,&["setup"]));
    assert_eq!(fixture.saved()["environments"][a.to_str().unwrap()]["project"],"claim");
}
impl Fixture {
    fn saved_if_any(&self) -> Option<Vec<u8>> { fs::read(self.home.join(".local/share/dockstride/.dockstride/environment-registry.json")).ok() }
}

#[test]
fn pending_registry_worker() {
    let Some(root) = std::env::var_os("REGISTRY_PENDING_ROOT") else { return; };
    let root = Path::new(&root);
    let _lifecycle = state::lock(root,"lifecycle").unwrap();
    let _global = state::global_lock().unwrap();
    let _config = state::lock(root,"config").unwrap();
    let snapshot = dockstride::sources::snapshot(root,None).unwrap();
    snapshot.verify().unwrap();
    let project = nickel::evaluate(root,Some(&json!({"project":"claim"}))).unwrap();
    let registration = registry::prepare(&project,&[],None).unwrap();
    let mut changes = vec![publication::Change::replace(&root.join("env.yaml"),b"project: claim\n",0o600).unwrap()];
    changes.extend(registration.changes);
    publication::stage_locked(root,"setup",changes,registration.claims).unwrap();
}
