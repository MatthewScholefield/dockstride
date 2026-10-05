use serde_json::{Value, json};
use std::{fs, io::Write, os::unix::fs::{PermissionsExt, symlink}, path::Path, process::{Command, Output, Stdio}};
use tempfile::TempDir;

struct Fixture { temp: TempDir }
impl Fixture {
    fn new(backend: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let fixture = Self { temp };
        let root = fixture.root();
        fs::create_dir(root.join("libs")).unwrap();
        fs::write(root.join("libs/dockstride.ncl"), include_str!("../assets/dockstride.ncl")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::create_dir(root.join("objects")).unwrap();
        fs::write(root.join("bin/docker"), r#"#!/usr/bin/env python3
import json,os,sys,pathlib,base64
r=pathlib.Path(os.environ['FIXTURE']);a=sys.argv[1:]
with (r/'docker-argv').open('a') as f:f.write(json.dumps(a)+'\n')
if a[:1]==['--context']:a=a[2:]
if a[:2]==['context','show']:print('fixture')
elif a[:2]==['context','inspect']:
 if '--format' in a:print('unix:///fixture.sock')
 else:print(json.dumps([{'Endpoints':{'docker':{'Host':'unix:///fixture.sock'}}}]))
elif a[:1]==['info']:
 if a[-1]=='{{json .Swarm}}':print(json.dumps({'LocalNodeState':'active','ControlAvailable':True,'Cluster':{'ID':'sync-cluster'}}))
 elif a[-1]=='{{json .SecurityOptions}}':print('[]')
 elif a[-1]=='{{json .ID}}':print(json.dumps('sync-daemon'))
 elif a[-1]=='{{.ID}}':print('sync-daemon')
 elif a[-1]=='{{.Swarm.LocalNodeState}}':print('active')
 else:raise SystemExit(9)
elif a[:2]==['secret','create']:
 data=sys.stdin.buffer.read();name=a[-2];labels={}
 if (r/'fail-second').exists():
  n=int((r/'fail-second').read_text());(r/'fail-second').write_text(str(n+1))
  if n==1:
   print('failed supplied='+data.decode()+' encoded='+base64.b64encode(data).decode(),file=sys.stderr);raise SystemExit(42)
 for i,x in enumerate(a):
  if x=='--label':k,v=a[i+1].split('=',1);labels[k]=v
 (r/'objects'/name).write_text(json.dumps({'Spec':{'Name':name,'Labels':labels}}))
 (r/'last-input').write_bytes(data);print('id')
elif a[:2]==['secret','inspect']:
 p=r/'objects'/a[-1]
 if not p.exists():raise SystemExit(1)
 print('['+p.read_text()+']')
elif a[:2]==['secret','rm']:(r/'objects'/a[-1]).unlink()
elif a[:2] in (['service','ls'],['ps','--all'],['ps','-aq'],['volume','ls'],['network','ls'],['config','ls'],['secret','ls']):print('')
else:raise SystemExit(9)
"#).unwrap();
        fs::set_permissions(root.join("bin/docker"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("env.yaml"), format!("project: sync-fixture\nbackend: {backend}\n")).unwrap();
        fixture.private("alpha.input", b"initial-alpha-sensitive");
        fixture.private("beta.input", b"initial-beta-sensitive");
        fixture.model("file", "file", "");
        fixture
    }
    fn root(&self) -> &Path { self.temp.path() }
    fn private(&self, path: &str, bytes: &[u8]) {
        fs::write(self.root().join(path), bytes).unwrap();
        fs::set_permissions(self.root().join(path), fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn model(&self, alpha: &str, beta: &str, extra: &str) {
        let policy = |kind: &str, name: &str| if kind == "file" { format!("lib.FileSecret \"{name}.input\"") } else { "lib.PromptSecret".into() };
        fs::write(self.root().join("compose.ncl"), format!(r#"let lib=import "libs/dockstride.ncl" in
let contract={{project|String,backend|lib.Backend,secrets.alpha|lib.SecretSource,secrets.beta|lib.SecretSource}} in
let env|contract=import "env.yaml" in
{{dockstride|not_exported={{Config=contract,setup.secrets={{alpha={},beta={}}},{extra}}},
secrets=env.secrets,services.api={{image="alpine",user="0",secrets=["alpha","beta"]}}}}
"#, policy(alpha,"alpha"), policy(beta,"beta"))).unwrap();
    }
    fn command(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dks"));
        cmd.current_dir(self.root()).args(["--json","--non-interactive","-C"]).arg(self.root()).args(args)
            .env("HOME", self.root().join("home")).env("XDG_DATA_HOME", self.root().join("data"))
            .env("FIXTURE",self.root()).env("PATH",format!("{}:{}",self.root().join("bin").display(),std::env::var("PATH").unwrap()))
            .env_remove("DOCKER_CONTEXT").env_remove("DOCKER_HOST").env_remove("COMPOSE_PROFILES")
            .stdin(if stdin.is_some(){Stdio::piped()}else{Stdio::null()}).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child=cmd.spawn().unwrap();
        if let Some(bytes)=stdin { child.stdin.take().unwrap().write_all(bytes).unwrap(); }
        child.wait_with_output().unwrap()
    }
    fn terminal(output: &Output) -> Value {
        serde_json::from_slice(output.stdout.split(|b|*b==b'\n').rfind(|line|!line.is_empty()).unwrap()).unwrap()
    }
    fn success(&self,args:&[&str]) -> Value {
        let output=self.command(args,None);
        assert!(output.status.success(),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
        Self::terminal(&output)["result"].clone()
    }
    fn env(&self)->Value { serde_yaml::from_slice(&fs::read(self.root().join("env.yaml")).unwrap()).unwrap() }
    fn history(&self)->Value { serde_json::from_slice(&fs::read(self.root().join(".dockstride/secrets.json")).unwrap()).unwrap() }
    fn save_history(&self,value:&Value) { dockstride::state::save(self.root(),"secrets",value).unwrap(); }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let directory=self.root().join(format!("data/dockstride/secrets/u{}",unsafe{libc::geteuid()}));
        if directory.exists(){let _=fs::set_permissions(directory,fs::Permissions::from_mode(0o700));}
    }
}

#[test]
fn imported_origins_private_hmac_and_explicit_sync_without_startup_refresh() {
    let f=Fixture::new("compose");f.success(&["setup"]);
    let old=f.env()["secrets"]["alpha"].clone();
    let history=f.history();
    let source=&history["revisions"].as_array().unwrap().iter().find(|revision|revision["logical"]=="alpha").unwrap()["fileSource"];
    assert_eq!(source["origin"],"declared-file");
    assert_eq!(source["canonicalPath"],json!(f.root().join("alpha.input")));
    let digest=source["keyedDigest"].as_str().unwrap();assert_eq!(digest.len(),64);
    let key=f.root().join("home/.local/share/dockstride/.dockstride/secret-source-key");
    assert_eq!(fs::metadata(key).unwrap().permissions().mode()&0o777,0o600);
    let unchanged=f.success(&["secrets","sync","alpha","--yes"]);
    assert_eq!(unchanged["secrets"][0]["status"],"unchanged");assert_eq!(f.env()["secrets"]["alpha"],old);
    f.private("alpha.input",b"changed-sensitive-alpha");f.success(&["setup"]);
    assert_eq!(f.env()["secrets"]["alpha"],old);
    let plan=f.success(&["secrets","sync","alpha","--plan"]);
    assert_eq!(plan["comparisonsDeferred"],true);assert_eq!(plan["secrets"][0]["status"],"comparison-deferred");
    let output=f.command(&["secrets","sync","alpha","--yes"],None);assert!(output.status.success());
    let new=f.env()["secrets"]["alpha"].clone();assert_ne!(new,old);
    assert_eq!(fs::read(new["file"].as_str().unwrap()).unwrap(),b"changed-sensitive-alpha");
    assert!(Path::new(old["file"].as_str().unwrap()).exists());
    for bytes in [&output.stdout,&output.stderr] { let text=String::from_utf8_lossy(bytes);assert!(!text.contains(digest));assert!(!text.contains("changed-sensitive-alpha"));assert!(!text.contains("keyedDigest")); }
}

#[test]
fn plan_reads_no_credential_and_creates_no_key() {
    let f=Fixture::new("compose");f.success(&["setup"]);
    let key=f.root().join("home/.local/share/dockstride/.dockstride/secret-source-key");fs::remove_file(&key).unwrap();
    fs::write(f.root().join("alpha.input"),[]).unwrap();
    let before=f.history();let plan=f.success(&["secrets","sync","alpha","--plan"]);
    assert_eq!(plan["secrets"][0]["status"],"comparison-deferred");assert!(!key.exists());assert_eq!(f.history(),before);
    assert!(!f.command(&["secrets","sync","alpha","--yes"],None).status.success());
}

#[test]
fn cli_origin_relative_cwd_declared_precedence_and_stdin_clearing() {
    let f=Fixture::new("compose");f.success(&["setup"]);f.private("cli.input",b"explicit-file-value");
    f.success(&["secrets","replace","alpha","--file","cli.input"]);
    assert_eq!(f.history()["revisions"].as_array().unwrap().last().unwrap()["fileSource"]["origin"],"cli-file");
    let declared=f.success(&["secrets","sync","alpha","--yes"]);
    assert_eq!(declared["secrets"][0]["source"]["origin"],"declared-file");
    f.model("prompt","file","");
    // Current declared import cannot fall back to a stale older explicit origin.
    assert!(!f.command(&["secrets","sync","alpha","--yes"],None).status.success());
    f.success(&["secrets","replace","alpha","--file","cli.input"]);
    let fallback=f.success(&["secrets","sync","alpha","--yes"]);
    assert_eq!(fallback["secrets"][0]["source"]["canonicalPath"],json!(f.root().join("cli.input")));
    assert_eq!(fallback["secrets"][0]["status"],"unchanged");
    let output=f.command(&["secrets","replace","alpha","--stdin"],Some(b"manual-stdin-value"));assert!(output.status.success());
    assert!(f.history()["revisions"].as_array().unwrap().last().unwrap().get("fileSource").is_none());
    assert!(!f.command(&["secrets","sync","alpha","--yes"],None).status.success());
}

#[test]
fn all_names_sources_and_rotations_preflight_before_publication() {
    let f=Fixture::new("compose");f.success(&["setup"]);let old=f.env();
    f.private("alpha.input",b"changed-alpha");
    for args in [vec!["secrets","sync","alpha","missing","--yes"],vec!["secrets","sync","alpha","alpha","--yes"],vec!["secrets","sync","alpha","--yes","--apply"]] {
        assert!(!f.command(&args,None).status.success());assert_eq!(f.env(),old);
    }
    f.private("beta.input",b"changed-beta");
    f.model("file","file",r#"setup.rotations.alpha={workflow="rotate-alpha",services=["api"]},actions=[{name="rotate",kind="command",argv=["true"],workflows=["rotate-alpha"],services=["api"]}],"#);
    assert!(!f.command(&["secrets","sync","alpha","beta","--yes","--apply"],None).status.success());
    assert_eq!(f.env(),old);
    for bytes in [Vec::new(),vec![b'x';1_048_577]] {
        f.private("beta.input",&bytes);assert!(!f.command(&["secrets","sync","alpha","beta","--yes"],None).status.success());assert_eq!(f.env(),old);
    }
    f.private("beta.input",b"beta");fs::set_permissions(f.root().join("beta.input"),fs::Permissions::from_mode(0o640)).unwrap();
    assert!(!f.command(&["secrets","sync","alpha","beta","--yes"],None).status.success());assert_eq!(f.env(),old);
    fs::remove_file(f.root().join("beta.input")).unwrap();symlink("alpha.input",f.root().join("beta.input")).unwrap();
    assert!(!f.command(&["secrets","sync","alpha","beta","--yes"],None).status.success());assert_eq!(f.env(),old);
}

#[test]
fn legacy_compose_baseline_and_swarm_uncomparable_are_honest() {
    for backend in ["compose","swarm"] {
        let f=Fixture::new(backend);f.success(&["setup"]);let old=f.env()["secrets"]["alpha"].clone();
        let mut history=f.history();for revision in history["revisions"].as_array_mut().unwrap(){revision.as_object_mut().unwrap().remove("fileSource");}f.save_history(&history);
        let result=f.success(&["secrets","sync","alpha","--yes"]);
        if backend=="compose" {assert_eq!(result["secrets"][0]["status"],"unchanged");assert_eq!(result["secrets"][0]["baselineEstablished"],true);assert_eq!(f.env()["secrets"]["alpha"],old);}
        else {assert_eq!(result["secrets"][0]["status"],"replaced");assert_eq!(result["secrets"][0]["priorContentComparable"],false);assert_ne!(f.env()["secrets"]["alpha"],old);}
        assert_eq!(f.success(&["secrets","sync","alpha","--yes"])["secrets"][0]["status"],"unchanged");
    }
}

#[test]
fn later_backend_failure_reports_exact_commits_and_redacts_original_error() {
    let f=Fixture::new("swarm");f.success(&["setup"]);let old=f.env();
    f.private("alpha.input",b"new-alpha-sensitive");f.private("beta.input",b"new-beta-sensitive");fs::write(f.root().join("fail-second"),"0").unwrap();
    let output=f.command(&["secrets","sync","alpha","beta","--yes"],None);assert!(!output.status.success());
    let report=Fixture::terminal(&output);let sync=&report["details"]["secretSync"];
    assert_eq!(sync["committed"],json!(["alpha"]));assert_eq!(sync["uncommitted"],json!(["beta"]));
    assert_ne!(f.env()["secrets"]["alpha"],old["secrets"]["alpha"]);assert_eq!(f.env()["secrets"]["beta"],old["secrets"]["beta"]);
    for bytes in [&output.stdout,&output.stderr] {assert!(!String::from_utf8_lossy(bytes).contains("new-beta-sensitive"));}
}

#[test]
fn application_failure_keeps_committed_revision_without_rollback() {
    let f=Fixture::new("compose");f.success(&["setup"]);let old=f.env()["secrets"]["alpha"].clone();
    f.model("file","file",r#"setup.rotations.alpha={workflow="rotate-alpha",services=["api"]},actions=[{name="rotate",kind="command",argv=["false"],workflows=["rotate-alpha"],services=["api"]}],"#);
    f.private("alpha.input",b"rotated-alpha-value");let output=f.command(&["secrets","sync","alpha","--yes","--apply"],None);assert!(!output.status.success());
    let terminal=Fixture::terminal(&output);let report=&terminal["details"]["secretSync"];
    assert_eq!(report["committed"],json!(["alpha"]));assert_eq!(report["uncommitted"],json!([]));assert_eq!(report["applied"],json!([]));assert_eq!(report["secrets"][0]["applied"],false);
    assert_ne!(f.env()["secrets"]["alpha"],old);assert!(Path::new(old["file"].as_str().unwrap()).exists());
}

#[test]
fn interrupted_valid_revision_recovers_before_sync_and_missing_revision_never_regenerates() {
    let f=Fixture::new("compose");f.success(&["setup"]);let old=f.env();
    f.private("alpha.input",b"recovered-alpha");
    f.success(&["secrets","sync","alpha","--yes"]);let replacement=f.env()["secrets"]["alpha"].clone();
    let mut history=f.history();
    history["revisions"].as_array_mut().unwrap().last_mut().unwrap()["pending"]=json!(true);
    f.save_history(&history);
    fs::write(f.root().join("env.yaml"),serde_yaml::to_string(&old).unwrap()).unwrap();
    let result=f.success(&["secrets","sync","alpha","--yes"]);
    assert_eq!(result["secrets"][0]["status"],"unchanged");
    assert_eq!(f.env()["secrets"]["alpha"],replacement);
    let mut history=f.history();
    history["revisions"].as_array_mut().unwrap().last_mut().unwrap()["pending"]=json!(true);
    f.save_history(&history);
    fs::remove_file(replacement["file"].as_str().unwrap()).unwrap();
    let before=f.env();
    assert!(!f.command(&["secrets","sync","alpha","--yes"],None).status.success());
    assert_eq!(f.env(),before);
    assert!(!Path::new(replacement["file"].as_str().unwrap()).exists());
}

#[test]
fn unchanged_apply_skips_rotation_requirement_and_private_foreign_sources_fail() {
    let f=Fixture::new("compose");f.success(&["setup"]);let old=f.env();
    assert_eq!(f.success(&["secrets","sync","alpha","--yes","--apply"])["secrets"][0]["status"],"unchanged");
    // Ownership rejection is exercised when this process is allowed to create
    // a foreign-owned fixture; unprivileged hosts cannot chown arbitrary users.
    if unsafe { libc::geteuid() } == 0 {
        let path=std::ffi::CString::new(f.root().join("alpha.input").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(path.as_ptr(),65534,65534) },0);
        assert!(!f.command(&["secrets","sync","alpha","--yes"],None).status.success());
        assert_eq!(f.env(),old);
    }
}

#[test]
fn initial_cli_import_has_canonical_origin_and_legacy_group_granted_baseline_is_readable() {
    let f=Fixture::new("compose");f.private("cli.input",b"cli-import-initial");
    let input=format!("alpha={}",f.root().join("cli.input").display());
    f.success(&["setup","--secret-file",&input]);
    let history=f.history();
    let revision=history["revisions"].as_array().unwrap().iter().find(|revision|revision["logical"]=="alpha").unwrap();
    assert_eq!(revision["fileSource"]["origin"],"cli-file");
    assert_eq!(revision["fileSource"]["canonicalPath"],json!(f.root().join("cli.input")));
    assert_eq!(fs::read(f.env()["secrets"]["alpha"]["file"].as_str().unwrap()).unwrap(),b"cli-import-initial");

    let g=Fixture::new("compose");
    let uid=unsafe{libc::geteuid()};let gid=unsafe{libc::getegid()};
    g.model("file","file",&format!("setup.secretAccess.alpha={{uid={uid},gid={gid}}},"));
    g.success(&["setup"]);let old=g.env()["secrets"]["alpha"].clone();
    assert_eq!(fs::metadata(old["file"].as_str().unwrap()).unwrap().permissions().mode()&0o777,0o640);
    let mut history=g.history();
    for revision in history["revisions"].as_array_mut().unwrap(){revision.as_object_mut().unwrap().remove("fileSource");}
    g.save_history(&history);
    let result=g.success(&["secrets","sync","alpha","--yes"]);
    assert_eq!(result["secrets"][0]["status"],"unchanged");assert_eq!(result["secrets"][0]["baselineEstablished"],true);
    assert_eq!(g.env()["secrets"]["alpha"],old);
}
