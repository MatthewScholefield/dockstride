use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
}
impl Fixture {
    fn new(backend: &str, policy: &str, user: &str, rootless: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = &temp.path().join("checkout");
        fs::create_dir(root).unwrap();
        fs::create_dir(root.join("libs")).unwrap();
        fs::write(
            root.join("libs/dockstride.ncl"),
            include_str!("../assets/dockstride.ncl"),
        )
        .unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::create_dir(root.join("fake-secrets")).unwrap();
        fs::write(root.join("bin/docker"), r#"#!/usr/bin/env python3
import json,os,sys,pathlib
args=sys.argv[1:]
r=pathlib.Path(os.environ['FIXTURE'])
with (r/'docker-argv').open('a') as f: f.write(json.dumps(args)+'\n')
if args[:1]==['--context']: args=args[2:]
if args[:2]==['context','show']: print('fixture')
elif args[:2]==['context','inspect']:
    if '--format' in args: print('unix:///fixture.sock')
    else: print(json.dumps([{'Endpoints':{'docker':{'Host':os.environ.get('FAKE_ENDPOINT','unix:///fixture.sock')}}}]))
elif args[:1]==['info']:
    if args[-1]=='{{json .Swarm}}': print(json.dumps({'LocalNodeState':'active','ControlAvailable':True,'Cluster':{'ID':os.environ.get('FAKE_CLUSTER','cluster-a')}}))
    elif args[-1]=='{{json .SecurityOptions}}': print(json.dumps(['name=rootless'] if os.environ.get('ROOTLESS')=='1' else []))
    elif args[-1]=='{{.ID}}': print(os.environ.get('FAKE_DAEMON','fixture-daemon'))
    elif args[-1]=='{{json .ID}}': print(json.dumps(os.environ.get('FAKE_DAEMON','fixture-daemon')))
    elif args[-1]=='{{.Swarm.LocalNodeState}}': print('active')
    else: raise SystemExit(9)
elif args[:2]==['secret','create']:
    name=args[-2]; labels={}
    if len(name)>64:
        print('secret name must not exceed 64 characters',file=sys.stderr);raise SystemExit(42)
    for i,a in enumerate(args):
        if a=='--label': k,v=args[i+1].split('=',1); labels[k]=v
    p=r/'fake-secrets'/name
    data=sys.stdin.buffer.read()
    if (r/'creation-failure').exists():
        import base64
        print('permission denied; supplied='+data.decode('utf-8')+'; encoded='+base64.b64encode(data).decode(),file=sys.stderr)
        raise SystemExit(42)
    p.write_text(json.dumps({'Spec':{'Name':name,'Labels':labels}}));p.chmod(0o600)
    (r/'last-secret-stdin').write_bytes(data);(r/'last-secret-stdin').chmod(0o600)
    print('fake-id')
elif args[:2]==['secret','ls']: print('\n'.join(p.name for p in (r/'fake-secrets').iterdir()))
elif args[:2]==['config','ls']: print('')
elif args[:2]==['secret','inspect']:
    p=r/'fake-secrets'/(args[2] if '--format' in args else args[-1])
    if not p.exists(): raise SystemExit(1)
    if '--format' in args: print(json.dumps(json.loads(p.read_text())['Spec']['Labels']))
    else: print('['+p.read_text()+']')
elif args[:2]==['secret','rm']: (r/'fake-secrets'/args[-1]).unlink()
elif args[:2]==['ps','-aq'] or args[:2] in (['volume','ls'],['network','ls']): print('')
elif args[:2] in (['service','ls'], ['ps','--all']): print('consumer' if (r/'consumer').exists() else '')
elif args[:2]==['service','inspect']:
    if '--format' in args:
        owner=str(r.resolve())
        print(json.dumps({'io.dockstride.owner':owner}))
    else:
        print(json.dumps([{'Spec':{'TaskTemplate':{'ContainerSpec':{'Secrets':[{'SecretName':(r/'consumer').read_text()}]}}}}]))
elif args[:1]==['inspect']:
    print(json.dumps([{'Mounts':[{'Source':(r/'consumer').read_text()}]}]))
else: raise SystemExit(9)
"#).unwrap();
        fs::set_permissions(root.join("bin/docker"), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = serde_json::to_string(&temp.path().join("private")).unwrap();
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let access = if rootless && user != "0" {
            format!("setup.secretAccess.authKey = {{uid={uid},gid={gid}}},")
        } else {
            String::new()
        };
        let source = format!(
            r#"let lib=import "libs/dockstride.ncl" in
let configContract={{project|String,backend|lib.Backend|default="compose",secrets.authKey|lib.SecretSource}} in
let env|configContract=import "env.yaml" in
{{dockstride|not_exported={{Config=configContract,setup.secrets.authKey={policy},setup.secretDirectory={directory},{access}}},
secrets=env.secrets,services.api={{image="alpine",user="{user}",secrets=["authKey"]}}}}
"#
        );
        fs::write(root.join("compose.ncl"), source).unwrap();
        fs::write(
            root.join("env.yaml"),
            format!("project: secret-fixture\nbackend: {backend}\n"),
        )
        .unwrap();
        fs::write(root.join("rootless"), if rootless { "1" } else { "0" }).unwrap();
        Self { root:root.clone(), temp }
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn command(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dks"));
        command.current_dir(self.root());
        command
            .args(["--json", "--non-interactive", "-C"])
            .arg(self.root())
            .args(args)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("FIXTURE", self.root())
            .env("HOME", self.temp.path().join("home"))
            .env("XDG_DATA_HOME", self.temp.path().join("data"))
            .env(
                "ROOTLESS",
                fs::read_to_string(self.root().join("rootless")).unwrap(),
            )
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_CONTEXT")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Ok(cluster) = fs::read_to_string(self.root().join("cluster")) {
            command.env("FAKE_CLUSTER", cluster);
        }
        let mut child = command.spawn().unwrap();
        if let Some(bytes) = stdin {
            child.stdin.take().unwrap().write_all(bytes).unwrap();
        }
        child.wait_with_output().unwrap()
    }
    fn success(&self, args: &[&str], stdin: Option<&[u8]>) -> Value {
        let output = self.command(args, stdin);
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(
            output
                .stdout
                .split(|b| *b == b'\n')
                .rfind(|line| !line.is_empty())
                .unwrap(),
        )
        .unwrap()
    }
    fn env(&self) -> Value {
        serde_yaml::from_slice(&fs::read(self.root().join("env.yaml")).unwrap()).unwrap()
    }
}

#[test]
fn generated_reference_and_bytes_survive_repeated_setup_and_disposable_state_removal() {
    let f = Fixture::new("compose", "lib.GenerateSecret {bytes=32,encoding=\"hex\"}", "0", false);
    f.success(&["setup"],None);
    let first = f.env()["secrets"]["authKey"].clone();
    let path = Path::new(first["file"].as_str().unwrap());
    let bytes = fs::read(path).unwrap();
    assert_eq!(bytes.len(),64);
    assert!(!path.starts_with(f.root()));
    assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,0o700);
    fs::remove_dir_all(f.root().join(".dockstride")).unwrap();
    f.success(&["setup"],None);
    assert_eq!(f.env()["secrets"]["authKey"],first);
    assert_eq!(fs::read(path).unwrap(),bytes);
    fs::remove_file(path).unwrap();
    assert!(!f.command(&["setup"],None).status.success());
    assert_eq!(f.env()["secrets"]["authKey"],first);
}

#[test]
fn setup_keeps_new_secrets_last_after_autoallocating_ports() {
    let f = Fixture::new("compose", "lib.GenerateSecret {bytes=32,encoding=\"hex\"}", "0", false);
    let socket = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let source = fs::read_to_string(f.root().join("compose.ncl")).unwrap()
        .replace("project|String,", "project|String,apiPort|Number,")
        .replace("Config=configContract,", &format!("Config=configContract,setup.ports.apiPort={{service=\"api\",target=8000,from={port},to={port}}},"));
    fs::write(f.root().join("compose.ncl"), source).unwrap();
    f.success(&["setup"], None);
    assert_eq!(f.env()["apiPort"], port);
    let text = fs::read_to_string(f.root().join("env.yaml")).unwrap();
    assert!(text.find("apiPort:").unwrap() < text.find("secrets:\n").unwrap());
    assert!(text[text.find("secrets:\n").unwrap()..].lines().skip(1).all(|line| line.starts_with(' ')));
    f.success(&["setup"], None);
    assert_eq!(fs::read_to_string(f.root().join("env.yaml")).unwrap(), text);
}

#[test]
fn secret_updates_move_existing_block_after_ordinary_fields() {
    let f = Fixture::new("compose", "lib.GenerateSecret {bytes=32,encoding=\"hex\"}", "0", false);
    f.success(&["setup"], None);
    let text = fs::read_to_string(f.root().join("env.yaml")).unwrap();
    let start = text.find("secrets:\n").unwrap();
    let secret = text[start..].replace("secrets:\n", "'secrets': # references\n  # credential\n");
    let fields = "# deployment identity\nproject: 'secret-fixture' # retained\nbackend: compose\n";
    fs::write(f.root().join("env.yaml"), format!("{secret}\n{fields}")).unwrap();
    f.success(&["secrets", "replace", "authKey", "--stdin"], Some(b"new-material"));
    let edited = fs::read_to_string(f.root().join("env.yaml")).unwrap();
    assert!(edited.starts_with(&format!("\n{fields}'secrets': # references\n  # credential\n")));
    assert_eq!(fs::read(f.env()["secrets"]["authKey"]["file"].as_str().unwrap()).unwrap(), b"new-material");
}

#[test]
fn replacement_keeps_old_material_and_never_discloses_new_bytes() {
    let f = Fixture::new("compose", "lib.GenerateSecret {bytes=32,encoding=\"hex\"}", "0", false);
    f.success(&["setup"],None);
    let old = f.env()["secrets"]["authKey"].clone();
    let bytes = b"specific-sensitive-credential";
    let output = f.command(&["secrets","replace","authKey","--stdin"],Some(bytes));
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));
    let new = f.env()["secrets"]["authKey"].clone();
    assert_ne!(old,new);
    assert_eq!(fs::read(new["file"].as_str().unwrap()).unwrap(),bytes);
    assert!(Path::new(old["file"].as_str().unwrap()).exists());
    for output in [output.stdout,output.stderr,fs::read(f.root().join("env.yaml")).unwrap(),fs::read(f.root().join("docker-argv")).unwrap()] {
        assert!(!output.windows(bytes.len()).any(|window| window == bytes));
    }
    assert!(!f.root().join(".dockstride/secrets.json").exists());
}

#[test]
fn original_file_input_is_never_copied_and_reuse_ignores_new_input() {
    let f = Fixture::new("compose","lib.PromptSecret","0",false);
    let provider = f.temp.path().join("provider");
    fs::write(&provider,b"original-provider").unwrap();
    fs::set_permissions(&provider,fs::Permissions::from_mode(0o600)).unwrap();
    let input = format!("authKey={}",provider.display());
    f.success(&["setup","--secret-file",&input],None);
    assert_eq!(f.env()["secrets"]["authKey"],json!({"file":provider}));
    f.success(&["setup","--secret-file","authKey=/missing-provider"],None);
    assert!(!f.temp.path().join("private").exists());
}

#[test]
fn swarm_generated_file_is_saved_before_failed_create_and_reused_on_retry() {
    let f = Fixture::new("swarm","lib.GenerateSecret {bytes=32,encoding=\"hex\"}","0",false);
    fs::write(f.root().join("creation-failure"),"").unwrap();
    assert!(!f.command(&["setup"],None).status.success());
    let source = f.env()["secrets"]["authKey"].clone();
    let bytes = fs::read(source["file"].as_str().unwrap()).unwrap();
    assert!(f.env()["_dockstride"]["swarmSecrets"]["authKey"].is_null());
    fs::remove_file(f.root().join("creation-failure")).unwrap();
    f.success(&["setup"],None);
    assert_eq!(f.env()["secrets"]["authKey"],source);
    assert_eq!(fs::read(f.root().join("last-secret-stdin")).unwrap(),bytes);
    let binding = f.env()["_dockstride"]["swarmSecrets"]["authKey"].as_str().unwrap().to_owned();
    assert_eq!(binding.len(),36);
    let object: Value = serde_json::from_slice(&fs::read(f.root().join("fake-secrets").join(&binding)).unwrap()).unwrap();
    assert_eq!(object["Spec"]["Labels"]["io.dockstride.owner"],json!(f.root().canonicalize().unwrap()));
    fs::remove_dir_all(f.root().join(".dockstride")).unwrap();
    f.success(&["setup"],None);
    assert_eq!(f.env()["_dockstride"]["swarmSecrets"]["authKey"],binding);
}

#[test]
fn missing_or_foreign_binding_is_not_automatically_republished_or_adopted() {
    let f = Fixture::new("swarm","lib.GenerateSecret {bytes=32,encoding=\"hex\"}","0",false);
    f.success(&["setup"],None);
    let initial = f.env()["_dockstride"]["swarmSecrets"]["authKey"].as_str().unwrap().to_owned();
    let object = f.root().join("fake-secrets").join(&initial);
    fs::remove_file(&object).unwrap();
    assert!(!f.command(&["setup"],None).status.success());
    assert_eq!(f.env()["_dockstride"]["swarmSecrets"]["authKey"],initial);
    f.success(&["secrets","sync","authKey","--yes"],None);
    let current = f.env()["_dockstride"]["swarmSecrets"]["authKey"].as_str().unwrap().to_owned();
    assert_ne!(initial,current);
    let object = f.root().join("fake-secrets").join(&current);
    let mut spec:Value = serde_json::from_slice(&fs::read(&object).unwrap()).unwrap();
    spec["Spec"]["Labels"]["io.dockstride.owner"] = json!("foreign-checkout");
    fs::write(&object,spec.to_string()).unwrap();
    assert!(!f.command(&["setup"],None).status.success());
    assert!(!f.command(&["secrets","sync","authKey","--yes"],None).status.success());
    assert_eq!(f.env()["_dockstride"]["swarmSecrets"]["authKey"],current);
}

#[test]
fn storage_symlinks_are_rejected_but_private_existing_directory_needs_no_marker() {
    let f = Fixture::new("compose","lib.GenerateSecret {bytes=32,encoding=\"hex\"}","0",false);
    fs::create_dir(f.temp.path().join("unrelated")).unwrap();
    symlink(f.temp.path().join("unrelated"),f.temp.path().join("private")).unwrap();
    assert!(!f.command(&["setup"],None).status.success());
    fs::remove_file(f.temp.path().join("private")).unwrap();
    fs::create_dir(f.temp.path().join("private")).unwrap();
    fs::set_permissions(f.temp.path().join("private"),fs::Permissions::from_mode(0o700)).unwrap();
    let directory = f.temp.path().join("private").join(format!("u{}",unsafe{libc::geteuid()}));
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory,fs::Permissions::from_mode(0o300)).unwrap();
    f.success(&["setup"],None);
    assert_eq!(fs::metadata(&directory).unwrap().permissions().mode() & 0o777,0o300);
    assert!(!f.temp.path().join("private/.dockstride-owner").exists());
    fs::set_permissions(directory,fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn nonroot_rootless_consumers_require_declared_mapped_group() {
    let f = Fixture::new("compose","lib.GenerateSecret {bytes=32,encoding=\"hex\"}","1000:0",true);
    f.success(&["setup"],None);
    assert_eq!(fs::metadata(f.env()["secrets"]["authKey"]["file"].as_str().unwrap()).unwrap().permissions().mode() & 0o777,0o640);
    let wrong = Fixture::new("compose","lib.GenerateSecret {bytes=32,encoding=\"hex\"}","1000:1000",true);
    assert!(!wrong.command(&["setup"],None).status.success());
}

#[test]
fn unknown_inputs_and_missing_prompt_are_rejected_before_storage_publication() {
    let f = Fixture::new("compose","lib.PromptSecret","0",false);
    assert!(!f.command(&["setup","--secret-stdin","unknown"],Some(b"sensitive")).status.success());
    let output = f.command(&["setup"],None);
    assert!(!output.status.success());
    let report:Value = serde_json::from_slice(output.stdout.split(|b| *b == b'\n').rfind(|line| !line.is_empty()).unwrap()).unwrap();
    assert!(report["details"]["missingInputs"].is_array());
    assert!(f.env().get("secrets").is_none());
}

#[test]
fn external_boundary_rejects_checkout_repository_linked_worktree_and_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    assert!(dockstride::secrets::ensure_external_path(&root,&root.join("secret")).is_err());
    let repository = temp.path().join("other");
    fs::create_dir(&repository).unwrap();
    for marker in [false,true] {
        if marker { fs::write(repository.join(".git"),"gitdir: /elsewhere").unwrap(); }
        else { fs::create_dir(repository.join(".git")).unwrap(); }
        assert!(dockstride::secrets::ensure_external_path(&root,&repository.join("future/secret")).is_err());
        if marker { fs::remove_file(repository.join(".git")).unwrap(); }
        else { fs::remove_dir(repository.join(".git")).unwrap(); }
    }
    let safe = temp.path().join("safe");
    fs::create_dir(&safe).unwrap();
    assert!(dockstride::secrets::ensure_external_path(&root,&safe.join("future/secret")).is_ok());
    let link = temp.path().join("link");
    symlink(&safe,&link).unwrap();
    assert!(dockstride::secrets::ensure_external_path(&root,&link.join("secret")).is_err());
}

#[test]
fn default_generated_storage_uses_external_xdg_without_markers_or_home_fallback() {
    let f=Fixture::new("compose","lib.GenerateSecret {bytes=32,encoding=\"hex\"}","0",false);
    let source=fs::read_to_string(f.root().join("compose.ncl")).unwrap();
    let declaration=format!("setup.secretDirectory={},",serde_json::to_string(&f.temp.path().join("private")).unwrap());
    fs::write(f.root().join("compose.ncl"),source.replace(&declaration,"")).unwrap();
    f.success(&["setup"],None);
    let reference=f.env()["secrets"]["authKey"].clone();
    let parent=f.temp.path().join("data/dockstride/secrets");
    assert!(Path::new(reference["file"].as_str().unwrap()).starts_with(&parent));
    assert!(!parent.join(".dockstride-owner").exists());
    assert!(!f.temp.path().join("home/.local/share/dockstride").exists());
}

#[test]
fn existing_legacy_marker_and_credentials_are_neither_required_nor_deleted() {
    let f=Fixture::new("compose","lib.GenerateSecret {bytes=32,encoding=\"hex\"}","0",false);
    let parent=f.temp.path().join("private");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent,fs::Permissions::from_mode(0o700)).unwrap();
    let marker=parent.join(".dockstride-owner");
    fs::write(&marker,b"old-marker-not-authority").unwrap();
    fs::set_permissions(&marker,fs::Permissions::from_mode(0o600)).unwrap();
    let old=parent.join("old-credential");
    fs::write(&old,b"retained-private-material").unwrap();
    fs::set_permissions(&old,fs::Permissions::from_mode(0o600)).unwrap();
    f.success(&["setup"],None);
    assert_eq!(fs::read(marker).unwrap(),b"old-marker-not-authority");
    assert_eq!(fs::read(old).unwrap(),b"retained-private-material");
}
