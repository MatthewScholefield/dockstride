use serde_json::{Value, json};
use std::{fs, io::Write, os::unix::fs::{PermissionsExt, symlink}, path::Path, process::{Command, Output, Stdio}};
use tempfile::TempDir;

struct Fixture { temp: TempDir, root: std::path::PathBuf }
impl Fixture {
    fn new(backend: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let fixture = Self { temp, root };
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
 p=r/'objects'/(a[2] if '--format' in a else a[-1])
 if not p.exists():raise SystemExit(1)
 if '--format' in a:print(json.dumps(json.loads(p.read_text())['Spec']['Labels']))
 else:print('['+p.read_text()+']')
elif a[:2]==['secret','rm']:(r/'objects'/a[-1]).unlink()
elif a[:2]==['secret','ls']:
 print('' if '--filter' in a else '\n'.join(p.name for p in (r/'objects').iterdir()))
elif a[:2] in (['service','ls'],['ps','--all'],['ps','-aq'],['volume','ls'],['network','ls'],['config','ls']):print('')
else:raise SystemExit(9)
"#).unwrap();
        fs::set_permissions(root.join("bin/docker"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("env.yaml"), format!("project: sync-fixture\nbackend: {backend}\n")).unwrap();
        fixture.private("alpha.input", b"initial-alpha-sensitive");
        fixture.private("beta.input", b"initial-beta-sensitive");
        fixture.model("reference", "reference", "");
        let mut env=fixture.env();
        env["secrets"]=json!({"alpha":{"file":fixture.temp.path().join("alpha.input")},"beta":{"file":fixture.temp.path().join("beta.input")}});
        fs::write(fixture.root().join("env.yaml"),serde_yaml::to_string(&env).unwrap()).unwrap();
        fixture
    }
    fn root(&self) -> &Path { &self.root }
    fn private(&self, path: &str, bytes: &[u8]) {
        fs::write(self.temp.path().join(path), bytes).unwrap();
        fs::set_permissions(self.temp.path().join(path), fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn model(&self, alpha: &str, beta: &str, extra: &str) {
        let policy = |_kind: &str, _name: &str| "lib.ReferenceSecret";
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
            .env("HOME", self.temp.path().join("home")).env("XDG_DATA_HOME", self.temp.path().join("data"))
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
}

#[test]
fn compose_sync_validates_original_without_copy_or_fictitious_commit() {
    let f=Fixture::new("compose");f.success(&["setup"]);
    let before=fs::read(f.root().join("env.yaml")).unwrap();
    f.private("alpha.input",b"changed-sensitive-alpha");
    let result=f.success(&["secrets","sync","alpha","--yes"]);
    assert_eq!(result["secrets"][0]["status"],"validated");
    assert_eq!(result["committed"],json!([]));assert_eq!(result["uncommitted"],json!([]));
    assert_eq!(result["secrets"][0]["reference"],json!({"file":f.temp.path().join("alpha.input")}));
    assert_eq!(fs::read(f.root().join("env.yaml")).unwrap(),before);
    assert!(!f.temp.path().join("data/dockstride/secrets").exists());
}

#[test]
fn swarm_every_sync_publishes_even_identical_source_and_setup_reuses() {
    let f=Fixture::new("swarm");f.success(&["setup"]);
    let initial=f.env()["_dockstride"]["swarmSecrets"]["alpha"].clone();
    let source=f.env()["secrets"]["alpha"].clone();
    f.private("alpha.input",b"changed-alpha");
    f.success(&["setup"]);
    assert_eq!(f.env()["_dockstride"]["swarmSecrets"]["alpha"],initial);
    let one=f.success(&["secrets","sync","alpha","--yes"]);
    assert_eq!(one["secrets"][0]["status"],"published");
    assert_eq!(one["committed"],json!(["alpha"]));
    let first=f.env()["_dockstride"]["swarmSecrets"]["alpha"].clone();
    let two=f.success(&["secrets","sync","alpha","--yes"]);
    let second=f.env()["_dockstride"]["swarmSecrets"]["alpha"].clone();
    assert_ne!(initial,first);assert_ne!(first,second);
    assert_eq!(two["secrets"][0]["binding"],second);
    assert_eq!(f.env()["secrets"]["alpha"],source);
    for binding in [initial,first,second] { assert!(f.root().join("objects").join(binding.as_str().unwrap()).exists()); }
}

#[test]
fn plan_validates_metadata_without_reading_source_bytes_or_mutating_yaml() {
    let f=Fixture::new("swarm");f.success(&["setup"]);
    let source=f.temp.path().join("alpha.input");
    let before=fs::read(f.root().join("env.yaml")).unwrap();
    let metadata=fs::metadata(&source).unwrap();
    let result=f.success(&["secrets","sync","alpha","--plan"]);
    assert_eq!(result["sideEffects"],false);
    assert_eq!(result["secrets"][0]["status"],"planned");
    assert_eq!(fs::read(f.root().join("env.yaml")).unwrap(),before);
    use std::os::unix::fs::MetadataExt;
    let after=fs::metadata(&source).unwrap();
    assert_eq!((metadata.atime(),metadata.atime_nsec()),(after.atime(),after.atime_nsec()));
}

#[test]
fn all_names_permissions_and_declared_rotations_preflight_before_publication() {
    let f=Fixture::new("swarm");f.success(&["setup"]);
    let before=fs::read(f.root().join("env.yaml")).unwrap();
    for args in [vec!["secrets","sync","alpha","missing","--yes"],vec!["secrets","sync","alpha","alpha","--yes"],
        vec!["secrets","sync","alpha","--yes","--apply"],vec!["secrets","sync","alpha","--plan","--apply"]] {
        assert!(!f.command(&args,None).status.success());
        assert_eq!(fs::read(f.root().join("env.yaml")).unwrap(),before);
    }
    for bytes in [Vec::new(),vec![b'x';1_048_577]] {
        f.private("beta.input",&bytes);
        assert!(!f.command(&["secrets","sync","alpha","beta","--yes"],None).status.success());
        assert_eq!(fs::read(f.root().join("env.yaml")).unwrap(),before);
    }
    f.private("beta.input",b"beta");
    fs::set_permissions(f.temp.path().join("beta.input"),fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!f.command(&["secrets","sync","alpha","beta","--yes"],None).status.success());
    assert_eq!(fs::read(f.root().join("env.yaml")).unwrap(),before);
    fs::remove_file(f.temp.path().join("beta.input")).unwrap();
    symlink(f.temp.path().join("alpha.input"),f.temp.path().join("beta.input")).unwrap();
    assert!(!f.command(&["secrets","sync","alpha","beta","--yes"],None).status.success());
    assert_eq!(fs::read(f.root().join("env.yaml")).unwrap(),before);
}

#[test]
fn later_backend_failure_reports_exact_commits_and_redacts_credentials() {
    let f=Fixture::new("swarm");f.success(&["setup"]);let old=f.env();
    f.private("alpha.input",b"new-alpha-sensitive");f.private("beta.input",b"new-beta-sensitive");
    fs::write(f.root().join("fail-second"),"0").unwrap();
    let output=f.command(&["secrets","sync","alpha","beta","--yes"],None);assert!(!output.status.success());
    let terminal=Fixture::terminal(&output);let sync=&terminal["details"]["secretSync"];
    assert_eq!(sync["committed"],json!(["alpha"]));assert_eq!(sync["uncommitted"],json!(["beta"]));
    assert_eq!(sync["secrets"][0]["status"],"published");assert_eq!(sync["secrets"][1]["status"],"uncommitted");
    assert_ne!(f.env()["_dockstride"]["swarmSecrets"]["alpha"],old["_dockstride"]["swarmSecrets"]["alpha"]);
    assert_eq!(f.env()["_dockstride"]["swarmSecrets"]["beta"],old["_dockstride"]["swarmSecrets"]["beta"]);
    assert_eq!(f.env()["secrets"],old["secrets"]);
    for bytes in [&output.stdout,&output.stderr] {assert!(!String::from_utf8_lossy(bytes).contains("new-beta-sensitive"));}
}

#[test]
fn failed_application_keeps_committed_binding_and_reports_not_applied() {
    let f=Fixture::new("swarm");f.success(&["setup"]);let old=f.env();
    f.model("reference","reference",r#"setup.rotations.alpha={workflow="rotate-alpha",services=["api"]},actions=[{name="rotate",kind="command",argv=["false"],workflows=["rotate-alpha"],services=["api"]}],"#);
    let output=f.command(&["secrets","sync","alpha","--yes","--apply"],None);
    assert!(!output.status.success());
    let terminal=Fixture::terminal(&output);let report=&terminal["details"]["secretSync"];
    assert_eq!(report["committed"],json!(["alpha"]));assert_eq!(report["uncommitted"],json!([]));
    assert_eq!(report["applied"],json!([]));assert_eq!(report["secrets"][0]["applied"],false);
    assert_eq!(report["secrets"][0]["consumerRestartNeeded"],true);
    assert_ne!(f.env()["_dockstride"]["swarmSecrets"]["alpha"],old["_dockstride"]["swarmSecrets"]["alpha"]);
    assert!(f.root().join("objects").join(old["_dockstride"]["swarmSecrets"]["alpha"].as_str().unwrap()).exists());
}
