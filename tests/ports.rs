use dockstride::{publication, state};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::{Path,PathBuf}, process::{Command,Output}};

const CONNECTION: &str = "host;DOCKER_HOST=unix:///ports-fixture.sock";
struct Fixture { directory: tempfile::TempDir, home: PathBuf, bin: PathBuf }
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let bin = directory.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("docker"),r#"#!/usr/bin/env python3
import json,os,sys
args=sys.argv[1:]
if args[:1]==['--context']: args=args[2:]
if os.environ.get('PORTS_FAIL_SERVICE') and args[:2]==['service','ls']: sys.exit('service listing transport failure')
if 'unreachable' in os.environ.get('DOCKER_HOST',''): sys.exit('fixture daemon unreachable')
if os.environ.get('PORTS_QUERY_LOG'):
    with open(os.environ['PORTS_QUERY_LOG'],'a') as out: out.write(json.dumps(args)+'\n')
if args[:1]==['info']:
    if args[-1]=='{{.ID}}': print(os.environ.get('PORTS_DAEMON','daemon-a'))
    elif args[-1]=='{{json .ID}}': print(json.dumps(os.environ.get('PORTS_DAEMON','daemon-a')))
    elif args[-1]=='{{json .}}': print(json.dumps({'ID':os.environ.get('PORTS_DAEMON','daemon-a'),'Swarm':{'LocalNodeState':'active','ControlAvailable':True}}))
    elif args[-1]=='{{.Swarm.LocalNodeState}}': print('active')
    elif args[-1]=='{{json .SecurityOptions}}': print('[]')
    elif args[-1]=='{{json .Swarm}}': print(json.dumps({'LocalNodeState':'active','ControlAvailable':True,'Cluster':{'ID':'fixture'}}))
    else: sys.exit(41)
elif args[:2]==['context','show']: print('fixture')
elif args[:2]==['context','inspect']: print('unix:///ports-fixture.sock')
elif args[:1]==['ps'] or (len(args)>1 and args[1]=='ls'):
    kind='container' if args[0]=='ps' else args[0]
    owner=next((a.split('=',2)[-1] for a in args if a.startswith('label=io.dockstride.owner=')),None)
    if kind==os.environ.get('PORTS_RESOURCE') and owner==os.environ.get('PORTS_OWNER','owner-a'): print('stopped-container' if kind=='container' else 'owned-'+kind)
elif len(args)>1 and args[1]=='inspect':
    print(json.dumps({'io.dockstride.owner':os.environ.get('PORTS_OWNER','owner-a')}))
else: sys.exit(42)
"#).unwrap();
        fs::set_permissions(bin.join("docker"),fs::Permissions::from_mode(0o700)).unwrap();
        Self {directory,home,bin}
    }
    fn global(&self) -> PathBuf { self.home.join(".local/share/dockstride/.dockstride") }
    fn checkout(&self,name: &str,port: u64) -> PathBuf {
        let root=self.directory.path().join(name);
        fs::create_dir_all(root.join(".dockstride")).unwrap();
        fs::set_permissions(root.join(".dockstride"),fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("env.yaml"),format!("# keep this comment\nproject: {name}\nport: {port}\n")).unwrap();
        fs::write(root.join("compose.ncl"),r#"
let contract = {project | String, backend | String | default = "compose", port | Number | default = 8080} in
let env | contract = import "env.yaml" in
{dockstride | not_exported = {Config = contract, endpoints.api = "http://localhost:%{env.port}"}, name = env.project, services.api.image = "alpine", volumes.data = {}}
"#).unwrap();
        private_json(&root.join(".dockstride/identity.json"),&json!({"id":format!("owner-{name}"),"project":name,"backend":"compose","root":root,"context":CONNECTION,"resources":false}));
        let key=format!("127.0.0.1:{port}/tcp");
        let allocation=json!({"port":port,"key":key,"generated":true,"ownerId":format!("owner-{name}"),"daemonId":"daemon-a","connection":CONNECTION});
        private_json(&root.join(".dockstride/ports.json"),&json!({"schemaVersion":1,"allocations":{"port":allocation}}));
        let mut registry=read_or(&self.global().join("environment-registry.json"),json!({"schemaVersion":1,"environments":{}}));
        registry["environments"][root.to_str().unwrap()]=json!({"root":root,"ownerId":format!("owner-{name}"),"project":name,"backend":"compose","connection":CONNECTION,"daemonId":"daemon-a","sourceFiles":[],"allocatedEndpoints":{"api":format!("http://localhost:{port}")},"allocations":{"port":allocation},"state":"committed"});
        private_json(&self.global().join("environment-registry.json"),&registry);
        let mut global=read_or(&self.global().join("port-reservations.json"),json!({"schemaVersion":1,"reservations":{}}));
        global["reservations"][&key]=json!({"root":root,"ownerId":format!("owner-{name}"),"daemonId":"daemon-a","connection":CONNECTION,"port":port,"host":"127.0.0.1","protocol":"tcp","field":"port"});
        private_json(&self.global().join("port-reservations.json"),&global);
        root
    }
    fn configure(&self,command: &mut Command) {
        command.env("HOME",&self.home).env("PATH",format!("{}:{}",self.bin.display(),std::env::var("PATH").unwrap()))
            .env_remove("DOCKER_CONTEXT").env("DOCKER_HOST","unix:///ports-fixture.sock");
    }
    fn command(&self,root:&Path,args:&[&str])->Command {
        let mut command=Command::new(env!("CARGO_BIN_EXE_dks"));
        command.args(["--json","--non-interactive","-C"]).arg(root).args(args);
        self.configure(&mut command);
        command
    }
    fn run(&self,root:&Path,args:&[&str])->Output { self.command(root,args).output().unwrap() }
    fn saved(&self)->Value { read_or(&self.global().join("port-reservations.json"),Value::Null) }
    fn stage_release(&self,root:&Path) {
        let mut worker=Command::new(std::env::current_exe().unwrap());
        worker.args(["--exact","stage_release_worker","--nocapture"]).env("PORTS_STAGE_ROOT",root);
        self.configure(&mut worker);
        succeeded(&worker.output().unwrap());
    }
}
fn private_json(path:&Path,value:&Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(),fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(path,serde_json::to_vec_pretty(value).unwrap()).unwrap();
    fs::set_permissions(path,fs::Permissions::from_mode(0o600)).unwrap();
}
fn read_or(path:&Path,otherwise:Value)->Value { fs::read(path).map(|bytes|serde_json::from_slice(&bytes).unwrap()).unwrap_or(otherwise) }
fn succeeded(output:&Output) { assert!(output.status.success(),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr)); }
fn terminal(output:&Output)->Value {
    String::from_utf8_lossy(&output.stdout).lines().filter_map(|line|serde_json::from_str::<Value>(line).ok()).last().unwrap()
}
fn result(output:&Output)->Value { let envelope=terminal(output); envelope.get("result").cloned().unwrap_or(envelope) }
fn tree(root:&Path)->BTreeMap<PathBuf,Vec<u8>> {
    let mut files=BTreeMap::new();
    if root.is_dir() {
        for entry in fs::read_dir(root).unwrap() {
            let path=entry.unwrap().path();
            if path.is_dir() { files.extend(tree(&path)); } else { files.insert(path.clone(),fs::read(path).unwrap()); }
        }
    }
    files
}

#[test]
fn release_plan_is_exact_read_only_and_stopped_containers_block_commit() {
    let fixture=Fixture::new();
    let root=fixture.checkout("a",57101);
    let other=fixture.checkout("b",57102);
    let secret=root.join(".dockstride/immutable-secret");
    fs::write(&secret,b"retained private credential").unwrap();
    let before=tree(fixture.directory.path());
    let planned=fixture.run(&root,&["ports","release","--plan"]);
    succeeded(&planned);
    let report=result(&planned);
    assert_eq!(report["reservations"][0]["key"],"127.0.0.1:57101/tcp");
    assert_eq!(report["fields"][0]["field"],"port");
    assert_eq!(tree(fixture.directory.path()),before);
    let blocked=fixture.command(&root,&["ports","release","--yes"]).env("PORTS_RESOURCE","container").output().unwrap();
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stdout).contains("stopped-container"));
    assert!(fixture.saved()["reservations"].get("127.0.0.1:57101/tcp").is_some());
    let released=fixture.run(&root,&["ports","release","--yes"]);
    succeeded(&released);
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(),"# keep this comment\nproject: a\n");
    assert_eq!(read_or(&root.join(".dockstride/ports.json"),Value::Null)["allocations"],json!({}));
    assert!(fixture.saved()["reservations"].get("127.0.0.1:57102/tcp").is_some());
    assert!(fs::read_to_string(other.join("env.yaml")).unwrap().contains("57102"));
    assert_eq!(fs::read(secret).unwrap(),b"retained private credential");
    succeeded(&fixture.run(&root,&["env","forget",root.to_str().unwrap(),"--yes"]));
}

#[test]
fn retained_storage_is_not_an_endpoint_and_swarm_publications_block() {
    let fixture=Fixture::new();
    let root=fixture.checkout("a",57111);
    let blocked=fixture.command(&root,&["ports","release","--yes"]).env("PORTS_RESOURCE","service").output().unwrap();
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stdout).contains("owned-service"));
    for kind in ["network","volume","secret","config"] {
        let report=fixture.command(&root,&["ports","release","--plan"]).env("PORTS_RESOURCE",kind).output().unwrap();
        succeeded(&report);
        assert_eq!(result(&report)["blockers"],json!([]));
    }
    succeeded(&fixture.command(&root,&["ports","release","--yes"]).env("PORTS_RESOURCE","volume").output().unwrap());
}

#[test]
fn explicit_overrides_including_same_value_survive_and_shared_settings_are_untouched() {
    for explicit in [57121,57999] {
        let fixture=Fixture::new();
        let root=fixture.checkout("a",57121);
        fs::write(root.join("shared.yaml"),"port: 58001\n").unwrap();
        fs::write(root.join("env.yaml"),"# keep this comment\n_dockstride: {sources: [{path: shared.yaml}]}\nproject: a\nport: 57121\n").unwrap();
        succeeded(&fixture.run(&root,&["config","set","port",&explicit.to_string()]));
        let local_before=fs::read(root.join("env.yaml")).unwrap();
        let shared_before=fs::read(root.join("shared.yaml")).unwrap();
        let report=fixture.run(&root,&["ports","release","--yes"]);
        succeeded(&report);
        assert_eq!(result(&report)["preservedFields"][0]["value"],explicit);
        assert_eq!(fs::read(root.join("env.yaml")).unwrap(),local_before);
        assert_eq!(fs::read(root.join("shared.yaml")).unwrap(),shared_before);
        assert_eq!(fixture.saved()["reservations"],json!({}));
    }
}

#[test]
fn legacy_release_requires_matching_local_value_and_leaves_unlinked_leaks() {
    let fixture=Fixture::new();
    let root=fixture.checkout("a",57131);
    private_json(&root.join(".dockstride/ports.json"),&json!({"port":57131}));
    private_json(&fixture.global().join("port-reservations.json"),&json!({"127.0.0.1:57131/tcp":root,"127.0.0.1:57132/tcp":root}));
    fs::write(root.join("env.yaml"),"project: a\nport: 59999\n").unwrap();
    let before=fixture.saved();
    let blocked=fixture.run(&root,&["ports","release","--yes"]);
    assert!(!blocked.status.success());
    assert_eq!(fixture.saved(),before);
    assert!(fs::read_to_string(root.join("env.yaml")).unwrap().contains("59999"));
    fs::write(root.join("env.yaml"),"project: a\nport: 57131\n").unwrap();
    succeeded(&fixture.run(&root,&["ports","release","--yes"]));
    assert_eq!(fixture.saved()["reservations"]["127.0.0.1:57132/tcp"]["root"],json!(root));
    assert!(fixture.saved()["reservations"].get("127.0.0.1:57131/tcp").is_none());
}

#[test]
fn changed_daemon_and_partial_transport_failures_never_free_reservations() {
    let fixture=Fixture::new();
    let root=fixture.checkout("a",57141);
    let before=fixture.saved();
    let blocked=fixture.command(&root,&["ports","release","--yes"]).env("PORTS_DAEMON","replacement-daemon").output().unwrap();
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stdout).contains("replacement-daemon"));
    assert_eq!(fixture.saved(),before);
    let partial=fixture.command(&root,&["ports","release","--yes"]).env("PORTS_RESOURCE","container").env("PORTS_FAIL_SERVICE","1").output().unwrap();
    assert!(!partial.status.success());
    assert!(String::from_utf8_lossy(&partial.stdout).contains("stopped-container"));
    assert!(String::from_utf8_lossy(&partial.stdout).contains("service listing transport failure"));
    assert_eq!(fixture.saved(),before);
    let mut registry=read_or(&fixture.global().join("environment-registry.json"),Value::Null);
    registry["environments"][root.to_str().unwrap()]["connection"]=json!("host;DOCKER_HOST=unix:///unreachable.sock");
    private_json(&fixture.global().join("environment-registry.json"),&registry);
    let blocked=fixture.run(&root,&["ports","release","--yes"]);
    assert!(!blocked.status.success());
    assert_eq!(fixture.saved(),before);
}

#[test]
fn gc_collects_only_verified_absent_or_retired_owners_and_keeps_other_files() {
    let fixture=Fixture::new();
    let invoking=fixture.checkout("a",57151);
    let absent=fixture.checkout("gone",57152);
    let retained=fixture.checkout("retired",57153);
    let blocked=fixture.checkout("blocked",57154);
    fs::remove_dir_all(&absent).unwrap();
    fs::remove_dir_all(&blocked).unwrap();
    let mut registry=read_or(&fixture.global().join("environment-registry.json"),Value::Null);
    registry["environments"].as_object_mut().unwrap().remove(retained.to_str().unwrap());
    private_json(&fixture.global().join("environment-registry.json"),&registry);
    let mut reservations=fixture.saved();
    reservations["reservations"]["127.0.0.1:57154/tcp"]["connection"]=json!("host;DOCKER_HOST=unix:///unreachable.sock");
    reservations["reservations"]["127.0.0.1:57155/tcp"]=json!({"root":fixture.directory.path().join("legacy-missing"),"legacy":true});
    private_json(&fixture.global().join("port-reservations.json"),&reservations);
    let retained_before=tree(&retained);
    let before=tree(fixture.directory.path());
    let plan=fixture.run(&invoking,&["ports","gc","--plan"]);
    succeeded(&plan);
    let candidates=result(&plan)["candidates"].as_array().unwrap().iter().map(|row|row["key"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!(candidates,vec!["127.0.0.1:57152/tcp","127.0.0.1:57153/tcp"]);
    assert_eq!(tree(fixture.directory.path()),before);
    succeeded(&fixture.run(&invoking,&["ports","gc","--yes"]));
    assert_eq!(tree(&retained),retained_before);
    let remaining=fixture.saved()["reservations"].as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(remaining,vec!["127.0.0.1:57151/tcp","127.0.0.1:57154/tcp","127.0.0.1:57155/tcp"]);
}

#[test]
fn gc_protects_absent_checkout_with_restartable_owned_container() {
    let fixture=Fixture::new();
    let invoking=fixture.checkout("a",57156);
    let absent=fixture.checkout("gone",57157);
    fs::remove_dir_all(&absent).unwrap();
    let before=fixture.saved();
    let collected=fixture.command(&invoking,&["ports","gc","--yes"])
        .env("PORTS_RESOURCE","container").env("PORTS_OWNER","owner-gone").output().unwrap();
    succeeded(&collected);
    assert_eq!(result(&collected)["collected"],0);
    assert!(String::from_utf8_lossy(&collected.stdout).contains("stopped-container"));
    assert_eq!(fixture.saved(),before);
}

#[test]
fn foreign_reservation_owner_is_not_released_by_matching_checkout_path() {
    let fixture=Fixture::new();
    let root=fixture.checkout("a",57158);
    let mut reservations=fixture.saved();
    reservations["reservations"]["127.0.0.1:57158/tcp"]["ownerId"]=json!("another-owner");
    private_json(&fixture.global().join("port-reservations.json"),&reservations);
    let before=fs::read(root.join("env.yaml")).unwrap();
    let rejected=fixture.run(&root,&["ports","release","--yes"]);
    assert!(!rejected.status.success());
    assert_eq!(fixture.saved(),reservations);
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(),before);
}

#[test]
fn release_can_remove_required_generated_field_without_provisioning_replacement() {
    let fixture=Fixture::new();
    let root=fixture.checkout("a",57159);
    fs::write(root.join("compose.ncl"),r#"
let contract = {project | String, port | Number} in
let env | contract = import "env.yaml" in
{dockstride | not_exported = {Config = contract, endpoints.api = "http://localhost:%{env.port}"}, name = env.project, services.api.image = "alpine"}
"#).unwrap();
    succeeded(&fixture.run(&root,&["ports","release","--yes"]));
    let local:Value=serde_yaml::from_str(&fs::read_to_string(root.join("env.yaml")).unwrap()).unwrap();
    assert_eq!(local["project"],"a");
    assert!(local.get("port").is_none());
    assert_eq!(fixture.saved()["reservations"],json!({}));
}

#[test]
fn interrupted_release_keeps_claims_and_external_edits_until_safe_recovery() {
    let fixture=Fixture::new();
    let root=fixture.checkout("a",57161);
    let other=fixture.checkout("b",57162);
    fixture.stage_release(&root);
    let global_path=fixture.global().join("port-reservations.json");
    let mut partial=fixture.saved();
    partial["reservations"].as_object_mut().unwrap().remove("127.0.0.1:57161/tcp");
    private_json(&global_path,&partial);
    fs::write(root.join("env.yaml"),"project: a\nport: 59999\n").unwrap();
    let before=tree(fixture.directory.path());
    let plan=fixture.run(&root,&["ports","release","--plan"]);
    succeeded(&plan);
    assert!(String::from_utf8_lossy(&plan.stdout).contains("pending publication"));
    assert_eq!(tree(fixture.directory.path()),before);
    let blocked=fixture.run(&root,&["ports","release","--yes"]);
    assert!(!blocked.status.success());
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(),"project: a\nport: 59999\n");
    assert!(read_or(&fixture.global().join("publication-pending.json"),Value::Null)["operations"].as_array().unwrap().iter().any(|op|op["claims"]["reservations"][0]["key"]=="127.0.0.1:57161/tcp"));
    fs::write(root.join("env.yaml"),"# keep this comment\nproject: a\nport: 57161\n").unwrap();
    succeeded(&fixture.run(&root,&["ports","release","--yes"]));
    assert!(fixture.saved()["reservations"].get("127.0.0.1:57161/tcp").is_none());
    assert!(fixture.saved()["reservations"].get("127.0.0.1:57162/tcp").is_some());
    assert!(fs::read_to_string(other.join("env.yaml")).unwrap().contains("57162"));
    assert_eq!(read_or(&fixture.global().join("publication-pending.json"),Value::Null)["operations"],json!([]));
}

#[test]
fn stage_release_worker() {
    let Some(root)=std::env::var_os("PORTS_STAGE_ROOT") else { return; };
    let root=Path::new(&root);
    let _lifecycle=state::lock(root,"lifecycle").unwrap();
    let _global=state::global_lock().unwrap();
    let _local=state::lock(root,"port-allocation").unwrap();
    let _config=state::lock(root,"config").unwrap();
    let global=state::global_root().unwrap();
    let reservation_path=global.join(".dockstride/port-reservations.json");
    let mut reservations=read_or(&reservation_path,Value::Null);
    let reservation=reservations["reservations"]["127.0.0.1:57161/tcp"].clone();
    reservations["reservations"].as_object_mut().unwrap().remove("127.0.0.1:57161/tcp");
    let mut registry=read_or(&global.join(".dockstride/environment-registry.json"),Value::Null);
    registry["environments"][root.to_str().unwrap()]["allocations"]=json!({});
    registry["environments"][root.to_str().unwrap()]["allocatedEndpoints"]=json!({});
    let changes=vec![
        publication::Change::replace(&reservation_path,&serde_json::to_vec_pretty(&reservations).unwrap(),0o600).unwrap(),
        publication::Change::replace(&root.join(".dockstride/ports.json"),br#"{"schemaVersion":1,"allocations":{}}"#,0o600).unwrap(),
        publication::Change::replace(&root.join("env.yaml"),b"# keep this comment\nproject: a\n",0o600).unwrap(),
        publication::Change::replace(&global.join(".dockstride/environment-registry.json"),&serde_json::to_vec_pretty(&registry).unwrap(),0o600).unwrap(),
    ];
    publication::stage_locked(root,"release-ports",changes,json!({"root":root,"ownerId":"owner-a","daemonId":"daemon-a","connection":CONNECTION,"reservations":[{"key":"127.0.0.1:57161/tcp","record":reservation}]})).unwrap();
}
