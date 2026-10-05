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
}
impl Fixture {
    fn new(backend: &str, policy: &str, user: &str, rootless: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
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
        owner=json.loads((r/'.dockstride/identity.json').read_text())['id']
        print(json.dumps({'io.dockstride.owner':owner}))
    else:
        print(json.dumps([{'Spec':{'TaskTemplate':{'ContainerSpec':{'Secrets':[{'SecretName':(r/'consumer').read_text()}]}}}}]))
elif args[:1]==['inspect']:
    print(json.dumps([{'Mounts':[{'Source':(r/'consumer').read_text()}]}]))
else: raise SystemExit(9)
"#).unwrap();
        fs::set_permissions(root.join("bin/docker"), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = serde_json::to_string(&root.join("private")).unwrap();
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
        Self { temp }
    }
    fn root(&self) -> &Path {
        self.temp.path()
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
            .env("HOME", self.root().join("home"))
            .env("XDG_DATA_HOME", self.root().join("data"))
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
    fn history(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root().join(".dockstride/secrets.json")).unwrap())
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let user = format!("u{}", unsafe { libc::geteuid() });
        for base in [
            "private",
            "data/dockstride/secrets",
            "home/.local/share/dockstride/secrets",
        ] {
            let directory = self.root().join(base).join(&user);
            if directory.exists() {
                let _ = fs::set_permissions(directory, fs::Permissions::from_mode(0o700));
            }
        }
    }
}

#[test]
fn generate_once_missing_reference_fails_without_changing_credential() {
    let fixture = Fixture::new(
        "compose",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\"}",
        "0",
        false,
    );
    fixture.success(&["setup"], None);
    let first = fixture.env()["secrets"]["authKey"].clone();
    let path = first["file"].as_str().unwrap();
    let bytes = fs::read(path).unwrap();
    fixture.success(&["setup"], None);
    assert_eq!(fixture.env()["secrets"]["authKey"], first);
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(
        fs::metadata(Path::new(path).parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o300
    );
    fs::remove_file(path).unwrap();
    let failed = fixture.command(&["setup"], None);
    assert!(!failed.status.success());
    assert_eq!(fixture.env()["secrets"]["authKey"], first);
    assert_eq!(fixture.history()["revisions"].as_array().unwrap().len(), 1);
}

#[test]
fn replacement_is_atomic_reference_only_and_gc_checks_live_consumers() {
    let fixture = Fixture::new(
        "compose",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\"}",
        "0",
        false,
    );
    fixture.success(&["setup"], None);
    let old = fixture.env()["secrets"]["authKey"].clone();
    let secret = b"specific-sensitive-credential";
    let output = fixture.command(&["secrets", "replace", "authKey", "--stdin"], Some(secret));
    assert!(output.status.success());
    for bytes in [
        output.stdout,
        output.stderr,
        fs::read(fixture.root().join("env.yaml")).unwrap(),
        fs::read(fixture.root().join(".dockstride/secrets.json")).unwrap(),
        fs::read(fixture.root().join("docker-argv")).unwrap(),
    ] {
        assert!(!bytes.windows(secret.len()).any(|w| w == secret));
    }
    let new = fixture.env()["secrets"]["authKey"].clone();
    assert_ne!(new, old);
    assert_eq!(fs::read(new["file"].as_str().unwrap()).unwrap(), secret);
    assert!(Path::new(old["file"].as_str().unwrap()).exists());
    let revision = fixture.history()["revisions"][0]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(
        fixture.root().join("consumer"),
        old["file"].as_str().unwrap(),
    )
    .unwrap();
    let failed = fixture.command(&["secrets", "gc", &revision, "--yes"], None);
    assert!(!failed.status.success());
    assert!(Path::new(old["file"].as_str().unwrap()).exists());
    fs::remove_file(fixture.root().join("consumer")).unwrap();
    fixture.success(&["secrets", "gc", &revision, "--yes"], None);
    assert!(!Path::new(old["file"].as_str().unwrap()).exists());
    assert!(Path::new(new["file"].as_str().unwrap()).exists());
}

#[test]
fn symlink_and_unmarked_parent_are_not_adopted() {
    let fixture = Fixture::new(
        "compose",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\"}",
        "0",
        false,
    );
    fs::create_dir(fixture.root().join("unrelated")).unwrap();
    symlink(
        fixture.root().join("unrelated"),
        fixture.root().join("private"),
    )
    .unwrap();
    assert!(!fixture.command(&["setup"], None).status.success());
    assert!(fixture.env().get("secrets").is_none());
    fs::remove_file(fixture.root().join("private")).unwrap();
    fs::create_dir(fixture.root().join("private")).unwrap();
    assert!(!fixture.command(&["setup"], None).status.success());
    assert!(!fixture.root().join("private/.dockstride-owner").exists());
}

#[test]
fn nonroot_rootless_consumers_require_explicit_mapped_group_access() {
    let fixture = Fixture::new(
        "compose",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\"}",
        "1000:0",
        true,
    );
    fixture.success(&["setup"], None);
    let path = fixture.env()["secrets"]["authKey"]["file"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let wrong = Fixture::new(
        "compose",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\"}",
        "1000:1000",
        true,
    );
    assert!(!wrong.command(&["setup"], None).status.success());
}

#[test]
fn swarm_generation_requires_recovery_and_never_places_bytes_in_argv() {
    let rejected = Fixture::new(
        "swarm",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\"}",
        "0",
        false,
    );
    assert!(!rejected.command(&["setup"], None).status.success());
    assert!(!rejected.root().join("last-secret-stdin").exists());
    let fixture = Fixture::new(
        "swarm",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\",durable=false}",
        "0",
        false,
    );
    fixture.success(&["setup"], None);
    let bytes = fs::read(fixture.root().join("last-secret-stdin")).unwrap();
    let reference = fixture.env()["secrets"]["authKey"].clone();
    fixture.success(&["setup"], None);
    assert_eq!(fixture.env()["secrets"]["authKey"], reference);
    for path in ["docker-argv", "env.yaml", ".dockstride/secrets.json"] {
        assert!(
            !fs::read(fixture.root().join(path))
                .unwrap()
                .windows(bytes.len())
                .any(|w| w == bytes)
        );
    }
    fs::remove_file(
        fixture
            .root()
            .join("fake-secrets")
            .join(reference["name"].as_str().unwrap()),
    )
    .unwrap();
    assert!(!fixture.command(&["setup"], None).status.success());
    assert_eq!(fixture.env()["secrets"]["authKey"], reference);
}

#[test]
fn pending_owned_revision_recovers_without_generation_and_rotation_requires_procedure() {
    let fixture = Fixture::new(
        "compose",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\"}",
        "0",
        false,
    );
    fixture.success(&["setup"], None);
    let reference = fixture.env()["secrets"]["authKey"].clone();
    let mut history = fixture.history();
    history["revisions"][0]["pending"] = json!(true);
    fs::write(
        fixture.root().join(".dockstride/secrets.json"),
        serde_json::to_vec(&history).unwrap(),
    )
    .unwrap();
    fs::write(
        fixture.root().join("env.yaml"),
        "project: secret-fixture\nbackend: compose\n",
    )
    .unwrap();
    fixture.success(&["setup"], None);
    assert_eq!(fixture.env()["secrets"]["authKey"], reference);
    assert_eq!(fixture.history()["revisions"][0]["pending"], false);
    let output = fixture.command(
        &["secrets", "replace", "authKey", "--stdin", "--apply"],
        Some(b"must-not-be-committed"),
    );
    assert!(!output.status.success());
    assert_eq!(fixture.env()["secrets"]["authKey"], reference);
}

#[test]
fn swarm_recovery_backup_is_private_and_scope_change_does_not_adopt_same_name() {
    let fixture = Fixture::new(
        "swarm",
        "lib.GenerateSecret {bytes=32,encoding=\"base64\",durable=false}",
        "0",
        false,
    );
    let recovery = fixture.root().join("recovery");
    fs::create_dir(&recovery).unwrap();
    fs::set_permissions(&recovery, fs::Permissions::from_mode(0o700)).unwrap();
    let source = fs::read_to_string(fixture.root().join("compose.ncl"))
        .unwrap()
        .replace(
            "durable=false",
            &format!(
                "recoveryFile={}",
                serde_json::to_string(&recovery.join("credential-{revision}")).unwrap()
            ),
        );
    fs::write(fixture.root().join("compose.ncl"), source).unwrap();
    fixture.success(&["setup"], None);
    let history = fixture.history();
    let revision = history["revisions"][0]["revision"].as_str().unwrap();
    let backup = recovery.join(format!("credential-{revision}"));
    assert_eq!(
        fs::read(&backup).unwrap(),
        fs::read(fixture.root().join("last-secret-stdin")).unwrap()
    );
    assert_eq!(
        fs::metadata(backup).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let old = fixture.env()["secrets"]["authKey"].clone();
    fs::write(fixture.root().join("cluster"), "cluster-b").unwrap();
    assert!(!fixture.command(&["setup"], None).status.success());
    assert_eq!(fixture.env()["secrets"]["authKey"], old);
}

#[test]
fn secret_gc_plan_has_no_provisioning_or_environment_mutation() {
    let fixture = Fixture::new(
        "compose",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\"}",
        "0",
        false,
    );
    let before = fs::read(fixture.root().join("env.yaml")).unwrap();
    fixture.success(&["secrets", "gc", "--plan"], None);
    assert_eq!(fs::read(fixture.root().join("env.yaml")).unwrap(), before);
    assert!(!fixture.root().join("private").exists());
    assert!(!fixture.root().join(".dockstride").exists());
}

#[test]
fn swarm_gc_refuses_foreign_labels_even_for_journaled_old_revision() {
    let fixture = Fixture::new(
        "swarm",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\",durable=false}",
        "0",
        false,
    );
    fixture.success(&["setup"], None);
    let old = fixture.env()["secrets"]["authKey"].clone();
    fixture.success(
        &["secrets", "replace", "authKey", "--stdin"],
        Some(b"replacement-known-secret"),
    );
    let revision = fixture.history()["revisions"][0]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let old_object = fixture
        .root()
        .join("fake-secrets")
        .join(old["name"].as_str().unwrap());
    let mut spec: Value = serde_json::from_slice(&fs::read(&old_object).unwrap()).unwrap();
    spec["Spec"]["Labels"]["io.dockstride.owner"] = json!("foreign-owner");
    fs::write(&old_object, serde_json::to_vec(&spec).unwrap()).unwrap();
    assert!(
        !fixture
            .command(&["secrets", "gc", &revision, "--yes"], None)
            .status
            .success()
    );
    assert!(old_object.exists());
    assert_ne!(fixture.env()["secrets"]["authKey"], old);
}

#[test]
fn supplied_file_policy_rejects_traversal_and_never_copies_source_into_environment() {
    let fixture = Fixture::new("compose", "lib.FileSecret \"input-key\"", "0", false);
    let secret = b"from-private-file";
    fs::write(fixture.root().join("input-key"), secret).unwrap();
    fs::set_permissions(
        fixture.root().join("input-key"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fixture.success(&["setup"], None);
    let path = fixture.env()["secrets"]["authKey"]["file"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(fs::read(path).unwrap(), secret);
    assert!(
        !fs::read(fixture.root().join("env.yaml"))
            .unwrap()
            .windows(secret.len())
            .any(|w| w == secret)
    );
    let rejected = Fixture::new("compose", "lib.FileSecret \"../input-key\"", "0", false);
    assert!(!rejected.command(&["setup"], None).status.success());
    assert!(rejected.env().get("secrets").is_none());
}

#[test]
fn prompted_initial_secret_accepts_private_file_flag_and_reuses_reference_without_reading_new_input()
 {
    let fixture = Fixture::new("compose", "lib.PromptSecret", "0", false);
    let bytes = b"supplied-with-file-flag";
    fs::write(fixture.root().join("input-key"), bytes).unwrap();
    fs::set_permissions(
        fixture.root().join("input-key"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fixture.success(&["setup", "--secret-file", "authKey=input-key"], None);
    let reference = fixture.env()["secrets"]["authKey"].clone();
    assert_eq!(
        fs::read(reference["file"].as_str().unwrap()).unwrap(),
        bytes
    );
    fs::remove_file(fixture.root().join("input-key")).unwrap();
    fixture.success(&["setup", "--secret-file", "authKey=input-key"], None);
    assert_eq!(fixture.env()["secrets"]["authKey"], reference);
    assert_eq!(fixture.history()["revisions"].as_array().unwrap().len(), 1);
}

#[test]
fn prompted_initial_secret_accepts_stdin_flag_but_reuse_does_not_consume_empty_stdin() {
    let fixture = Fixture::new("compose", "lib.PromptSecret", "0", false);
    let bytes = b"supplied-with-stdin-flag";
    let output = fixture.command(&["setup", "--secret-stdin", "authKey"], Some(bytes));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let reference = fixture.env()["secrets"]["authKey"].clone();
    assert_eq!(
        fs::read(reference["file"].as_str().unwrap()).unwrap(),
        bytes
    );
    assert!(!output.stdout.windows(bytes.len()).any(|w| w == bytes));
    assert!(!output.stderr.windows(bytes.len()).any(|w| w == bytes));
    fixture.success(&["setup", "--secret-stdin", "authKey"], Some(b""));
    assert_eq!(fixture.env()["secrets"]["authKey"], reference);
    assert_eq!(fixture.history()["revisions"].as_array().unwrap().len(), 1);
    let replacement = fixture.command(&["secrets", "replace", "authKey"], None);
    assert!(!replacement.status.success());
    assert_eq!(fixture.env()["secrets"]["authKey"], reference);
}

#[test]
fn unknown_initial_secret_flags_fail_and_unsupplied_prompt_is_structured_noninteractive_missing_input()
 {
    let fixture = Fixture::new("compose", "lib.PromptSecret", "0", false);
    let before = fs::read(fixture.root().join("env.yaml")).unwrap();
    let unknown = fixture.command(
        &["setup", "--secret-stdin", "notDeclared"],
        Some(b"not-to-be-published"),
    );
    assert!(!unknown.status.success());
    assert_eq!(fs::read(fixture.root().join("env.yaml")).unwrap(), before);
    assert!(!fixture.root().join("private").exists());
    let missing = fixture.command(&["setup"], None);
    assert!(!missing.status.success());
    let result: Value = serde_json::from_slice(
        missing
            .stdout
            .split(|b| *b == b'\n')
            .rfind(|line| !line.is_empty())
            .unwrap(),
    )
    .unwrap();
    assert!(
        result["details"]["missingInputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field["path"] == "secrets.authKey")
    );
    assert_eq!(fs::read(fixture.root().join("env.yaml")).unwrap(), before);
}

#[test]
fn swarm_revision_names_fit_daemon_limit_without_losing_logical_identity_or_random_revision() {
    let fixture = Fixture::new(
        "swarm",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\",durable=false}",
        "0",
        false,
    );
    let project = format!("long-project-{}", "p".repeat(40));
    let logical = format!("authKey{}", "s".repeat(100));
    let source = fs::read_to_string(fixture.root().join("compose.ncl"))
        .unwrap()
        .replace("authKey", &logical);
    fs::write(fixture.root().join("compose.ncl"), source).unwrap();
    fs::write(
        fixture.root().join("env.yaml"),
        format!("project: {project}\nbackend: swarm\n"),
    )
    .unwrap();
    fixture.success(&["setup"], None);
    let env = fixture.env();
    let name = env["secrets"][&logical]["name"].as_str().unwrap();
    assert!(name.len() <= 64);
    let history = fixture.history();
    assert!(name.ends_with(history["revisions"][0]["revision"].as_str().unwrap()));
    let spec: Value =
        serde_json::from_slice(&fs::read(fixture.root().join("fake-secrets").join(name)).unwrap())
            .unwrap();
    assert_eq!(spec["Spec"]["Labels"]["io.dockstride.project"], project);
    assert_eq!(spec["Spec"]["Labels"]["io.dockstride.secret"], logical);
}

#[test]
fn secret_create_failure_retains_docker_status_and_useful_diagnostic_but_redacts_credentials() {
    let fixture = Fixture::new("swarm", "lib.PromptSecret", "0", false);
    fs::write(fixture.root().join("creation-failure"), "").unwrap();
    let bytes = b"sensitive-request-body";
    let encoded = b"c2Vuc2l0aXZlLXJlcXVlc3QtYm9keQ==";
    let output = fixture.command(&["setup", "--secret-stdin", "authKey"], Some(bytes));
    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(
        output
            .stdout
            .split(|b| *b == b'\n')
            .rfind(|line| !line.is_empty())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["details"]["underlyingDockerStatus"], 42);
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("permission denied")
    );
    for stream in [&output.stdout, &output.stderr] {
        assert!(!stream.windows(bytes.len()).any(|window| window == bytes));
        assert!(
            !stream
                .windows(encoded.len())
                .any(|window| window == encoded)
        );
    }
    assert!(fixture.env().get("secrets").is_none());
    assert_eq!(fixture.history()["revisions"][0]["pending"], true);
}

#[test]
fn declared_identity_defaults_select_the_secret_backend_without_yaml_copies() {
    let fixture = Fixture::new(
        "swarm",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\",durable=false}",
        "0",
        false,
    );
    let source = fs::read_to_string(fixture.root().join("compose.ncl"))
        .unwrap()
        .replace(
            "project|String",
            "project|String|default=\"default-production\"",
        )
        .replace(
            "backend|lib.Backend|default=\"compose\"",
            "backend|lib.Backend|default=\"swarm\"",
        );
    fs::write(fixture.root().join("compose.ncl"), source).unwrap();
    fs::write(fixture.root().join("env.yaml"), "{}\n").unwrap();
    fixture.success(&["setup"], None);
    let reference = fixture.env()["secrets"]["authKey"].clone();
    assert_eq!(reference["external"], true);
    assert!(reference.get("file").is_none());
    assert!(fixture.env().get("project").is_none());
    assert!(fixture.env().get("backend").is_none());
    let identity: Value = serde_json::from_slice(
        &fs::read(fixture.root().join(".dockstride/identity.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(identity["project"], "default-production");
    assert_eq!(identity["backend"], "swarm");
    fixture.success(&["setup"], None);
    assert_eq!(fixture.env()["secrets"]["authKey"], reference);
}

#[test]
fn default_store_obeys_xdg_data_home_without_creating_a_home_store() {
    let fixture = Fixture::new(
        "compose",
        "lib.GenerateSecret {bytes=32,encoding=\"hex\"}",
        "0",
        false,
    );
    let source = fs::read_to_string(fixture.root().join("compose.ncl")).unwrap();
    let explicit = format!(
        "setup.secretDirectory={},",
        serde_json::to_string(&fixture.root().join("private")).unwrap()
    );
    assert!(source.contains(&explicit));
    fs::write(
        fixture.root().join("compose.ncl"),
        source.replace(&explicit, ""),
    )
    .unwrap();
    fixture.success(&["setup"], None);
    let file = PathBuf::from(
        fixture.env()["secrets"]["authKey"]["file"]
            .as_str()
            .unwrap(),
    );
    let expected = fixture
        .root()
        .join("data/dockstride/secrets")
        .join(format!("u{}", unsafe { libc::geteuid() }));
    assert_eq!(file.parent(), Some(expected.as_path()));
    assert!(
        !fixture
            .root()
            .join("home/.local/share/dockstride/secrets")
            .exists()
    );
    assert_eq!(
        fs::metadata(&expected).unwrap().permissions().mode() & 0o777,
        0o300
    );
}

#[test]
fn rotation_preflight_rejects_invalid_or_inapplicable_actions_before_storage_publication() {
    for invalid_prerequisite in [true, false] {
        let fixture = Fixture::new("compose", "lib.GenerateSecret {bytes=32,encoding=\"hex\"}", "0", false);
        let action = if invalid_prerequisite {
            r#"{name="migration",kind="prerequisite",service="migrate",services=["migrate"],workflows=["rotate-auth"]}"#
        } else {
            r#"{name="unrelated",kind="command",argv=["true"],services=["other"],workflows=["rotate-auth"]}"#
        };
        let model = fs::read_to_string(fixture.root().join("compose.ncl")).unwrap();
        fs::write(fixture.root().join("compose.ncl"), format!(r#"{model}
& {{
  dockstride = {{
    setup.rotations.authKey = {{workflow="rotate-auth",services=["api"]}},
    oneshots = ["migrate"],
    actions = [{action}],
  }},
  services.api.depends_on.migrate.condition = "service_completed_successfully",
  services.migrate = {{image="alpine",restart="no"}},
  services.other.image = "alpine",
}}
"#)).unwrap();
        fixture.success(&["setup"], None);
        let reference = fixture.env()["secrets"]["authKey"].clone();
        let history = fixture.history();
        let bytes = fs::read(reference["file"].as_str().unwrap()).unwrap();
        let result = fixture.command(&["secrets", "replace", "authKey", "--stdin", "--apply"], Some(b"must-not-be-published"));
        assert!(!result.status.success());
        assert_eq!(fixture.env()["secrets"]["authKey"], reference);
        assert_eq!(fixture.history(), history);
        assert_eq!(fs::read(reference["file"].as_str().unwrap()).unwrap(), bytes);
    }
}
