use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

// Each CLI invocation uses a disposable checkout, external credential storage,
// and a fake Docker target that records secret bytes only in its engine store.
struct Fixture {
    temp: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new(backend: &str, policy: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("libs")).unwrap();
        fs::write(root.join("libs/dockstride.ncl"), include_str!("../assets/dockstride.ncl")).unwrap();
        fs::create_dir(temp.path().join("bin")).unwrap();
        fs::create_dir(temp.path().join("docker-secrets")).unwrap();
        fs::write(temp.path().join("bin/docker"), r#"#!/usr/bin/env python3
import json, os, pathlib, sys
r = pathlib.Path(os.environ['FIXTURE'])
a = sys.argv[1:]
with (r / 'docker-argv').open('a') as f: f.write(json.dumps(a) + '\n')
if a[:1] == ['--context']: a = a[2:]
if a[:2] == ['context', 'show']: print('reference-fixture')
elif a[:2] == ['context', 'inspect']:
    if '--format' in a: print('unix:///reference-fixture.sock')
    else: print(json.dumps([{'Endpoints':{'docker':{'Host':'unix:///reference-fixture.sock'}}}]))
elif a[:1] == ['version']: print(json.dumps({'Server':{'Version':'fixture'}}))
elif a[:1] == ['info']:
    if a[-1] == '{{json .Swarm}}': print(json.dumps({'LocalNodeState':'active','ControlAvailable':True,'Cluster':{'ID':'reference-cluster'}}))
    elif a[-1] == '{{json .SecurityOptions}}': print('[]')
    elif a[-1] == '{{.ID}}': print('reference-daemon')
    elif a[-1] == '{{json .ID}}': print(json.dumps('reference-daemon'))
    elif a[-1] == '{{.Swarm.LocalNodeState}}': print('active')
    else: print(json.dumps({'ID':'reference-daemon','Swarm':{'LocalNodeState':'active','ControlAvailable':True}}))
elif a[:2] == ['secret', 'create']:
    labels = {}
    for i, arg in enumerate(a):
        if arg == '--label':
            key, value = a[i + 1].split('=', 1); labels[key] = value
    name = a[-2]
    (r / 'docker-secrets' / name).write_text(json.dumps({'Spec':{'Name':name,'Labels':labels}}))
    (r / 'docker-secret-bytes').write_bytes(sys.stdin.buffer.read())
    with (r / 'docker-publications').open('a') as f: f.write(name + '\n')
    print('fixture-secret-id')
elif a[:2] == ['secret', 'inspect']:
    name = a[2] if '--format' in a else a[-1]
    p = r / 'docker-secrets' / name
    if not p.exists(): raise SystemExit(1)
    spec = json.loads(p.read_text())
    if '--format' in a: print(json.dumps(spec['Spec']['Labels']))
    else: print(json.dumps([spec]))
elif a[:2] == ['secret', 'ls']:
    filters = [a[i + 1] for i, arg in enumerate(a[:-1]) if arg == '--filter']
    for p in (r / 'docker-secrets').iterdir():
        labels = json.loads(p.read_text())['Spec']['Labels']
        matches = True
        for query in filters:
            kind, value = query.split('=', 1)
            if kind == 'label':
                key, _, expected = value.partition('=')
                matches &= labels.get(key) == expected
            elif kind == 'name': matches &= value in p.name
            else: raise SystemExit(9)
        if matches: print(p.name)
elif a[:2] == ['secret', 'rm']: (r / 'docker-secrets' / a[-1]).unlink()
elif a[:1] == ['compose']:
    i = 1
    while i < len(a) and a[i].startswith('-'): i += 2
    if a[i] == 'version': print('2.30.0')
    elif a[i] == 'ps' and '--format' in a: print('[]')
    elif a[i] not in ['down', 'ps']: raise SystemExit(9)
elif a[:2] == ['stack', 'rm']: pass
elif a[:1] == ['ps'] or a[:2] in (['service','ls'], ['volume','ls'], ['network','ls'], ['config','ls']): pass
else: raise SystemExit(9)
"#).unwrap();
        fs::set_permissions(temp.path().join("bin/docker"), fs::Permissions::from_mode(0o700)).unwrap();
        let private = serde_json::to_string(&temp.path().join("private")).unwrap();
        fs::write(root.join("compose.ncl"), format!(r#"let lib = import "libs/dockstride.ncl" in
let contract = {{project | String, backend | lib.Backend | default = "compose", secrets.token | lib.SecretSource}} in
let env | contract = import "env.yaml" in
{{dockstride | not_exported = {{Config = contract, setup.secrets.token = {policy}, setup.secretDirectory = {private}}},
secrets = env.secrets, services.api = {{image = "alpine", user = "0", secrets = ["token"]}}}}
"#)).unwrap();
        fs::write(root.join("env.yaml"), format!("project: reference-fixture\nbackend: {backend}\n")).unwrap();
        Self { temp, root }
    }

    fn private_file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.temp.path().join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn shared(&self, reference: Value) -> PathBuf {
        let source = self.temp.path().join("shared.yaml");
        fs::write(&source, serde_yaml::to_string(&json!({"secrets":{"token":reference}})).unwrap()).unwrap();
        let mut env = self.env();
        env["_dockstride"] = json!({"sources":[{"path":source}]});
        fs::write(self.root.join("env.yaml"), serde_yaml::to_string(&env).unwrap()).unwrap();
        source
    }

    fn command_builder(&self, args: &[&str]) -> Command {
        let editing = args.first() == Some(&"config") && args.get(1) == Some(&"edit");
        let mut command = if editing {
            let mut command = Command::new("python3");
            command.args(["-c", r#"import os, pty, subprocess, sys
master, slave = pty.openpty()
try:
    result = subprocess.run(sys.argv[1:], stdin=slave, capture_output=True, timeout=30)
    sys.stdout.buffer.write(result.stdout)
    sys.stderr.buffer.write(result.stderr)
    sys.exit(result.returncode)
finally:
    os.close(slave)
    os.close(master)
"#]).arg(env!("CARGO_BIN_EXE_dks"));
            command
        } else {
            Command::new(env!("CARGO_BIN_EXE_dks"))
        };
        if !editing {
            command.arg("--json");
        }
        command.current_dir(&self.root).arg("-C").arg(&self.root).args(args)
            .env("PATH", format!("{}:{}", self.temp.path().join("bin").display(), std::env::var("PATH").unwrap()))
            .env("FIXTURE", self.temp.path())
            .env("HOME", self.temp.path().join("home"))
            .env("XDG_DATA_HOME", self.temp.path().join("data"))
            .env_remove("DOCKER_HOST").env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_TLS_VERIFY").env_remove("DOCKER_CERT_PATH")
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if !editing {
            command.arg("--non-interactive");
        }
        command
    }

    fn command(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let mut command = self.command_builder(args);
        if input.is_some() { command.stdin(Stdio::piped()); }
        let mut child = command.spawn().unwrap();
        if let Some(bytes) = input { child.stdin.take().unwrap().write_all(bytes).unwrap(); }
        child.wait_with_output().unwrap()
    }

    fn result(output: Output) -> Value {
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let line = output.stdout.split(|byte| *byte == b'\n').rfind(|line| !line.is_empty()).unwrap();
        serde_json::from_slice::<Value>(line).unwrap()["result"].clone()
    }

    fn success(&self, args: &[&str]) -> Value { Self::result(self.command(args, None)) }

    fn env(&self) -> Value {
        serde_yaml::from_slice(&fs::read(self.root.join("env.yaml")).unwrap()).unwrap()
    }

    fn current(&self) -> Value { self.success(&["config", "get", "secrets.token"])["value"].clone() }

    fn assert_no_host_copy(&self) {
        for path in [self.temp.path().join("private"), self.temp.path().join("data/dockstride/secrets"), self.temp.path().join("home/.local/share/dockstride/secrets")] {
            assert!(!path.exists(), "external credential unexpectedly materialized into {}", path.display());
        }
    }
}


fn metadata(path: &Path) -> (u32, u32, u32, u64) {
    let value = fs::metadata(path).unwrap();
    (value.uid(), value.gid(), value.permissions().mode() & 0o777, value.ino())
}

#[test]
fn shared_compose_setup_keeps_the_original_authoritative_without_local_materialization() {
    let f = Fixture::new("compose", "lib.ReferenceSecret");
    let bytes = b"externally-owned-provider-credential";
    let file = f.private_file("provider", bytes);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).unwrap();
    let before = metadata(&file);
    let reference = json!({"file":file});
    let source = f.shared(reference.clone());
    let source_before = fs::read(&source).unwrap();
    let output = f.command(&["setup"], None);
    assert!(!output.stdout.windows(bytes.len()).any(|window| window == bytes));
    assert!(!output.stderr.windows(bytes.len()).any(|window| window == bytes));
    Fixture::result(output);
    f.success(&["setup"]);
    assert!(f.env().get("secrets").is_none());
    assert_eq!(f.current(), reference);
    let render = f.success(&["render", "--target", "compose"]);
    assert!(serde_json::to_string(&render).unwrap().contains(file.to_str().unwrap()));
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert_eq!(metadata(&file), before);
    f.assert_no_host_copy();
}

#[test]
fn cli_reference_file_stays_original_and_stdin_is_not_a_reference() {
    let f = Fixture::new("compose", "lib.ReferenceSecret");
    let file = f.private_file("provider", b"cli-original-provider-credential");
    let before = metadata(&file);
    let input = format!("token={}", file.display());
    f.success(&["setup", "--secret-file", &input]);
    assert_eq!(f.env()["secrets"]["token"], json!({"file":file}));
    assert_eq!(metadata(&file), before);
    f.assert_no_host_copy();

    let rejected = Fixture::new("compose", "lib.ReferenceSecret");
    let env_before = fs::read(rejected.root.join("env.yaml")).unwrap();
    assert!(!rejected.command(&["setup", "--secret-stdin", "token"], Some(b"must-not-be-imported")).status.success());
    assert_eq!(fs::read(rejected.root.join("env.yaml")).unwrap(), env_before);
    rejected.assert_no_host_copy();
}

#[test]
fn native_overrides_unset_and_live_shared_path_changes_do_not_copy_credentials() {
    let f = Fixture::new("compose", "lib.ReferenceSecret");
    let first = f.private_file("first-provider", b"first-provider-value");
    let second = f.private_file("second-provider", b"second-provider-value");
    let local = f.private_file("local-provider", b"local-provider-value");
    let source = f.shared(json!({"file":first}));
    f.success(&["setup"]);
    f.success(&["config", "set", "secrets.token", &json!({"file":local}).to_string()]);
    assert_eq!(f.current(), json!({"file":local}));
    f.success(&["config", "set", "secrets.token.file", &json!(second).to_string(), "--shared"]);
    assert_eq!(f.current(), json!({"file":local}));
    f.success(&["config", "unset", "secrets.token"]);
    assert_eq!(f.current(), json!({"file":second}));
    fs::write(&source, serde_yaml::to_string(&json!({"secrets":{"token":{"file":first}}})).unwrap()).unwrap();
    assert_eq!(f.current(), json!({"file":first}));
    f.success(&["setup"]);
    assert!(f.env()["secrets"].get("token").is_none());
    for (path, bytes) in [(&first, b"first-provider-value".as_slice()), (&second, b"second-provider-value".as_slice()), (&local, b"local-provider-value".as_slice())] {
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    f.assert_no_host_copy();
}

#[test]
fn shared_file_security_failures_precede_any_swarm_publication() {
    for invalid in ["missing", "empty", "oversized", "unsafe", "symlink", "symlink-parent", "directory", "relative", "checkout", "repository"] {
        let f = Fixture::new("swarm", "lib.ReferenceSecret");
        let file = f.private_file("provider", b"must-never-be-published");
        let reference = match invalid {
            "missing" => { fs::remove_file(&file).unwrap(); json!({"file":file}) }
            "empty" => { fs::write(&file, []).unwrap(); json!({"file":file}) }
            "oversized" => { fs::write(&file, vec![b'x';1_048_577]).unwrap(); json!({"file":file}) }
            "unsafe" => { fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap(); json!({"file":file}) }
            "symlink" => { let link = f.temp.path().join("provider-link"); symlink(&file, &link).unwrap(); json!({"file":link}) }
            "symlink-parent" => { let link = f.temp.path().join("provider-parent"); symlink(f.temp.path(), &link).unwrap(); json!({"file":link.join("provider")}) }
            "directory" => json!({"file":f.temp.path()}),
            "relative" => json!({"file":"../provider"}),
            "checkout" => { let local=f.root.join("provider"); fs::rename(&file,&local).unwrap(); json!({"file":local}) }
            "repository" => {
                let repo=f.temp.path().join("repository"); fs::create_dir(&repo).unwrap();
                fs::write(repo.join(".git"),"gitdir: /elsewhere").unwrap();
                let local=repo.join("provider"); fs::rename(&file,&local).unwrap(); json!({"file":local})
            }
            _ => unreachable!(),
        };
        let source = f.shared(reference);
        let env_before = fs::read(f.root.join("env.yaml")).unwrap();
        let source_before = fs::read(&source).unwrap();
        assert!(!f.command(&["setup"], None).status.success(), "accepted {invalid} source");
        assert_eq!(fs::read(f.root.join("env.yaml")).unwrap(), env_before);
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert!(!f.temp.path().join("docker-publications").exists());
        f.assert_no_host_copy();
    }
}

#[test]
fn shared_plaintext_and_generated_policy_inheritance_are_rejected() {
    for reference in [json!("raw-provider-credential"), json!({"value":"raw-provider-credential"})] {
        let f = Fixture::new("swarm", "lib.ReferenceSecret");
        f.shared(reference);
        let before = fs::read(f.root.join("env.yaml")).unwrap();
        assert!(!f.command(&["setup"], None).status.success());
        assert_eq!(fs::read(f.root.join("env.yaml")).unwrap(), before);
        assert!(!f.temp.path().join("docker-publications").exists());
    }
    let f = Fixture::new("compose", "lib.GenerateSecret {bytes=32,encoding=\"hex\"}");
    let file = f.private_file("provider", b"externally-owned-not-generated");
    f.shared(json!({"file":file}));
    let before = fs::read(f.root.join("env.yaml")).unwrap();
    assert!(!f.command(&["setup"], None).status.success());
    assert_eq!(fs::read(f.root.join("env.yaml")).unwrap(), before);
    assert_eq!(fs::read(&file).unwrap(), b"externally-owned-not-generated");
    f.assert_no_host_copy();
}

#[test]
fn shared_native_updates_validate_invalid_hidden_references_before_publication() {
    let f = Fixture::new("compose", "lib.ReferenceSecret");
    let shared = f.private_file("shared-provider", b"shared-provider-value");
    let local = f.private_file("local-provider", b"local-provider-value");
    let source = f.shared(json!({"file":shared}));
    f.success(&["setup"]);
    f.success(&["config", "set", "secrets.token", &json!({"file":local}).to_string()]);
    let before = fs::read(&source).unwrap();
    let unsafe_file = f.private_file("unsafe-provider", b"unsafe-provider-value");
    fs::set_permissions(&unsafe_file, fs::Permissions::from_mode(0o644)).unwrap();
    for reference in [json!({"file":unsafe_file}), json!({"file":f.temp.path().join("absent")}), json!("raw-secret-value")] {
        assert!(!f.command(&["config", "set", "secrets.token", &reference.to_string(), "--shared"], None).status.success());
        assert_eq!(fs::read(&source).unwrap(), before);
        assert_eq!(f.current(), json!({"file":local}));
    }
    assert!(!f.command(&["config", "set", "secrets", &json!({"token":{"file":unsafe_file}}).to_string(), "--shared"], None).status.success());
    assert_eq!(fs::read(&source).unwrap(), before);
    f.assert_no_host_copy();
}

#[test]
fn native_editor_publishes_valid_shared_references_and_rejects_hidden_invalid_candidates() {
    let f = Fixture::new("compose", "lib.ReferenceSecret");
    let first = f.private_file("first-provider", b"first-provider-value");
    let second = f.private_file("second-provider", b"second-provider-value");
    let local = f.private_file("local-provider", b"local-provider-value");
    let source = f.shared(json!({"file":first}));
    f.success(&["setup"]);
    let editor = f.temp.path().join("editor");
    fs::write(&editor, "#!/usr/bin/env python3\nimport os,pathlib,sys\npathlib.Path(sys.argv[1]).write_text(os.environ['EDIT_CANDIDATE'])\n").unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o700)).unwrap();
    let candidate = serde_yaml::to_string(&json!({"secrets":{"token":{"file":second}}})).unwrap();
    let mut command = f.command_builder(&["config", "edit", "--shared"]);
    let edited = command.env("EDITOR", &editor).env("VISUAL", &editor).env("EDIT_CANDIDATE", &candidate).output().unwrap();
    assert!(edited.status.success(), "{}\n{}", String::from_utf8_lossy(&edited.stdout), String::from_utf8_lossy(&edited.stderr));
    assert_eq!(f.current(), json!({"file":second}));
    f.success(&["config", "set", "secrets.token", &json!({"file":local}).to_string()]);
    let before = fs::read(&source).unwrap();
    let invalid = serde_yaml::to_string(&json!({"secrets":{"token":{"file":f.temp.path().join("missing")}}})).unwrap();
    let mut command = f.command_builder(&["config", "edit", "--shared"]);
    assert!(!command.env("EDITOR", &editor).env("VISUAL", &editor).env("EDIT_CANDIDATE", invalid).output().unwrap().status.success());
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_eq!(f.current(), json!({"file":local}));
    f.assert_no_host_copy();
}

#[test]
fn list_and_doctor_follow_effective_shared_references_without_claiming_ownership() {
    let f = Fixture::new("compose", "lib.ReferenceSecret");
    let file = f.private_file("provider", b"provider-value-for-diagnostics");
    f.shared(json!({"file":file}));
    f.success(&["setup"]);
    let list = f.success(&["secrets", "list"]);
    let row = list["secrets"].as_array().unwrap().iter().find(|row| row["name"] == "token").unwrap();
    assert_eq!(row["reference"], json!({"file":file}));
    assert_eq!(row["present"], true);
    assert_eq!(row["binding"], Value::Null);
    assert_eq!(row["consumers"], json!(["api"]));
    assert_eq!(f.success(&["doctor"])["secrets"]["ok"], true);
    fs::remove_file(&file).unwrap();
    let list = f.success(&["secrets", "list"]);
    let row = list["secrets"].as_array().unwrap().iter().find(|row| row["name"] == "token").unwrap();
    assert_eq!(row["present"], false);
    let report = f.success(&["doctor"]);
    assert_eq!(report["secrets"]["ok"], false);
    assert!(report["secrets"]["issues"].as_array().unwrap().iter().any(|issue| issue["secret"] == "token"));
    f.assert_no_host_copy();
}

#[test]
fn reference_replacement_sync_and_destroy_never_mutate_external_files() {
    let f = Fixture::new("compose", "lib.ReferenceSecret");
    let first = f.private_file("first-provider", b"first-provider-value");
    let second = f.private_file("second-provider", b"second-provider-value");
    let first_metadata = metadata(&first);
    let second_metadata = metadata(&second);
    f.shared(json!({"file":first}));
    f.success(&["setup"]);
    f.success(&["secrets", "replace", "token", "--file", second.to_str().unwrap()]);
    assert_eq!(f.current(), json!({"file":second}));
    f.success(&["secrets", "sync", "token", "--yes"]);
    f.success(&["destroy", "--yes"]);
    assert_eq!(fs::read(&first).unwrap(), b"first-provider-value");
    assert_eq!(fs::read(&second).unwrap(), b"second-provider-value");
    assert_eq!(metadata(&first), first_metadata);
    assert_eq!(metadata(&second), second_metadata);
    f.assert_no_host_copy();
}


#[test]
fn swarm_publishes_only_to_docker_and_explicit_sync_uses_the_current_shared_source() {
    let f = Fixture::new("swarm", "lib.ReferenceSecret");
    let first = f.private_file("first-provider", b"first-swarm-provider-value");
    let second = f.private_file("second-provider", b"second-swarm-provider-value");
    let first_metadata = metadata(&first);
    let second_metadata = metadata(&second);
    let source = f.shared(json!({"file":first}));
    f.success(&["setup"]);
    assert_eq!(fs::read(f.temp.path().join("docker-secret-bytes")).unwrap(), b"first-swarm-provider-value");
    let initial = f.env()["_dockstride"]["swarmSecrets"]["token"].clone();
    assert!(f.env().get("secrets").is_none());
    assert_eq!(f.current(), json!({"file":first}));
    let publications = fs::read(f.temp.path().join("docker-publications")).unwrap();
    fs::write(&source, serde_yaml::to_string(&json!({"secrets":{"token":{"file":second}}})).unwrap()).unwrap();
    let source_before = fs::read(&source).unwrap();
    f.success(&["setup"]);
    assert_eq!(f.env()["_dockstride"]["swarmSecrets"]["token"], initial);
    assert_eq!(fs::read(f.temp.path().join("docker-publications")).unwrap(), publications);
    assert_eq!(fs::read(f.temp.path().join("docker-secret-bytes")).unwrap(), b"first-swarm-provider-value");
    f.success(&["secrets", "sync", "token", "--yes"]);
    let replacement = f.env()["_dockstride"]["swarmSecrets"]["token"].clone();
    assert_ne!(replacement, initial);
    assert!(f.env().get("secrets").is_none());
    assert_eq!(f.current(), json!({"file":second}));
    assert_eq!(fs::read(f.temp.path().join("docker-secret-bytes")).unwrap(), b"second-swarm-provider-value");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&first).unwrap(), b"first-swarm-provider-value");
    assert_eq!(fs::read(&second).unwrap(), b"second-swarm-provider-value");
    assert_eq!(metadata(&first), first_metadata);
    assert_eq!(metadata(&second), second_metadata);
    let argv = fs::read(f.temp.path().join("docker-argv")).unwrap();
    for bytes in [b"first-swarm-provider-value".as_slice(), b"second-swarm-provider-value".as_slice()] {
        assert!(!argv.windows(bytes.len()).any(|window| window == bytes));
    }
    f.assert_no_host_copy();
}

#[test]
fn native_reference_updates_cannot_bypass_generated_secret_lifecycle() {
    let f = Fixture::new("compose", "lib.GenerateSecret {bytes=32,encoding=\"hex\"}");
    f.success(&["setup"]);
    let original = f.env()["secrets"]["token"].clone();
    let generated = PathBuf::from(original["file"].as_str().unwrap());
    let generated_bytes = fs::read(&generated).unwrap();
    let file = f.private_file("provider", b"not-a-generated-deployment-secret");
    let before = fs::read(f.root.join("env.yaml")).unwrap();
    for (path, value) in [
        ("secrets.token", json!({"file":file})),
        ("secrets.token.file", json!(file)),
        ("secrets", json!({"token":{"file":file}})),
    ] {
        assert!(!f.command(&["config", "set", path, &value.to_string()], None).status.success());
        assert_eq!(fs::read(f.root.join("env.yaml")).unwrap(), before);
        assert_eq!(fs::read(&generated).unwrap(), generated_bytes);
    }
    assert!(!f.command(&["config", "unset", "secrets.token"], None).status.success());
    assert_eq!(f.env()["secrets"]["token"], original);
    assert_eq!(fs::read(&file).unwrap(), b"not-a-generated-deployment-secret");
}

#[test]
fn shared_swarm_named_references_remain_externally_owned_and_native_updates_do_not_publish() {
    let f = Fixture::new("swarm", "lib.ReferenceSecret");
    for name in ["external-provider-first", "external-provider-second"] {
        fs::write(f.temp.path().join("docker-secrets").join(name),
            json!({"Spec":{"Name":name,"Labels":{}}}).to_string()).unwrap();
    }
    let first = json!({"name":"external-provider-first","external":true});
    let second = json!({"name":"external-provider-second","external":true});
    f.shared(first.clone());
    f.success(&["setup"]);
    let list = f.success(&["secrets", "list"]);
    let row = list["secrets"].as_array().unwrap().iter().find(|row| row["name"] == "token").unwrap();
    assert_eq!(row["reference"], first);
    assert_eq!(row["present"], true);
    assert_eq!(row["binding"], Value::Null);
    f.success(&["config", "set", "secrets.token", &second.to_string(), "--shared"]);
    f.success(&["setup"]);
    let list = f.success(&["secrets", "list"]);
    let row = list["secrets"].as_array().unwrap().iter().find(|row| row["name"] == "token").unwrap();
    assert_eq!(row["reference"], second);
    assert_eq!(row["binding"], Value::Null);
    assert!(!f.command(&["secrets", "sync", "token", "--yes"], None).status.success());
    for name in ["external-provider-first", "external-provider-second"] {
        assert!(f.temp.path().join("docker-secrets").join(name).exists());
    }
    assert!(!f.temp.path().join("docker-publications").exists());
    f.assert_no_host_copy();
}

#[test]
fn shared_reference_can_enable_a_previously_absent_optional_integration() {
    let f = Fixture::new("compose", "lib.ReferenceSecret");
    fs::write(f.root.join("compose.ncl"), r#"let lib = import "libs/dockstride.ncl" in
let selected = import "env.yaml" in
let enabled = std.record.has_field "secrets" selected && std.record.has_field "token" selected.secrets in
let secretContract = {..} & (if enabled then {token | lib.SecretSource} else {}) in
let contract = {project | String, backend | lib.Backend | default = "compose", secrets | secretContract | default = {}} in
let env | contract = import "env.yaml" in
{dockstride | not_exported = {Config = contract, setup.secrets = if enabled then {token = lib.ReferenceSecret} else {}},
secrets = env.secrets, services.api = {image = "alpine", user = "0", secrets = if enabled then ["token"] else []}}
"#).unwrap();
    let source = f.temp.path().join("shared.yaml");
    fs::write(&source, "{}\n").unwrap();
    let mut env = f.env();
    env["_dockstride"] = json!({"sources":[{"path":source}]});
    fs::write(f.root.join("env.yaml"), serde_yaml::to_string(&env).unwrap()).unwrap();
    let file = f.private_file("optional-provider", b"optional-provider-value");
    f.success(&["config", "set", "secrets.token", &json!({"file":file}).to_string(), "--shared"]);
    f.success(&["setup"]);
    assert_eq!(f.current(), json!({"file":file}));
    assert!(f.env().get("secrets").is_none());
    let list = f.success(&["secrets", "list"]);
    let row = list["secrets"].as_array().unwrap().iter().find(|row| row["name"] == "token").unwrap();
    assert_eq!(row["consumers"], json!(["api"]));
    f.success(&["config", "unset", "secrets.token", "--shared"]);
    let render = f.success(&["render"]);
    assert_eq!(render["services"]["api"]["secrets"], json!([]));
    assert_eq!(fs::read(&file).unwrap(), b"optional-provider-value");
    f.assert_no_host_copy();
}

#[test]
fn declared_swarm_defaults_publish_and_retire_references_without_yaml_backend_copies() {
    let f = Fixture::new("swarm", "lib.ReferenceSecret");
    let source = fs::read_to_string(f.root.join("compose.ncl")).unwrap()
        .replace("default = \"compose\"", "default = \"swarm\"");
    fs::write(f.root.join("compose.ncl"), source).unwrap();
    fs::write(f.root.join("env.yaml"), "project: reference-fixture\n").unwrap();
    let original = f.private_file("provider", b"declared-default-provider");
    let original_metadata = metadata(&original);
    f.shared(json!({"file":original}));
    f.success(&["setup"]);
    let published = f.current();
    assert_eq!(published, json!({"file":original}));
    let binding = f.env()["_dockstride"]["swarmSecrets"]["token"].clone();
    assert_eq!(fs::read(f.temp.path().join("docker-secret-bytes")).unwrap(), b"declared-default-provider");
    assert!(f.env().get("backend").is_none());
    f.success(&["down"]);
    f.success(&["destroy", "--yes"]);
    assert_eq!(f.current(), published);
    assert_eq!(f.env()["_dockstride"]["swarmSecrets"]["token"], binding);
    assert!(f.env().get("backend").is_none());
    assert_eq!(metadata(&original), original_metadata);
    assert_eq!(fs::read(&original).unwrap(), b"declared-default-provider");
    f.assert_no_host_copy();
}
