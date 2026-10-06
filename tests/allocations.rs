use dockstride::config;
use serde_json::json;
use std::{fs, net::TcpListener, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output}};

struct Fixture { directory: tempfile::TempDir, bin: PathBuf }
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("docker"), "#!/bin/sh\ncase \"$1\" in\n info) printf '[]\\n';;\n ps|network|volume|service|secret|config) exit 0;;\n *) exit 32;;\nesac\n").unwrap();
        fs::set_permissions(bin.join("docker"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { directory, bin }
    }
    fn checkout(&self, from: u16, to: u16) -> PathBuf {
        let root = self.directory.path().join("stack");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("env.yaml"), "# retained environment comment\nproject: allocation-fixture\n").unwrap();
        let model = r#"
let contract = {project | String, backend | String | default = "compose", apiPort | Number | default = 8000} in
let env | contract = import "env.yaml" in
{
  dockstride | not_exported = {
    Config = contract,
    setup.ports.apiPort = {service = "api", target = 8000, from = FROM, to = TO},
  },
  name = env.project,
  services.api.image = "alpine",
}
"#.replace("FROM", &from.to_string()).replace("TO", &to.to_string());
        fs::write(root.join("compose.ncl"), model).unwrap();
        root
    }
    fn run(&self, root: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dks")).args(["--json", "--non-interactive", "-C"]).arg(root).args(args)
            .env("HOME", self.directory.path().join("home"))
            .env("PATH", format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()))
            .env_remove("DOCKER_CONTEXT").env("DOCKER_HOST", "unix:///allocation-fixture.sock").output().unwrap()
    }
}
fn succeeded(output: &Output) {
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
fn occupied() -> (TcpListener, u16) {
    loop {
        let socket = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        if port < 65530 { return (socket, port); }
    }
}

#[test]
fn missing_raw_port_overrides_schema_default_once_and_survives_scratch_removal() {
    let fixture = Fixture::new();
    let (_busy, from) = occupied();
    let root = fixture.checkout(from, from + 5);
    succeeded(&fixture.run(&root, &["setup"]));
    let first = config::read_env(&root).unwrap()["apiPort"].as_u64().unwrap();
    assert!((u64::from(from + 1)..=u64::from(from + 5)).contains(&first));
    let before = fs::read(root.join("env.yaml")).unwrap();
    fs::remove_dir_all(root.join(".dockstride")).unwrap();
    succeeded(&fixture.run(&root, &["setup"]));
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), before);
    assert!(std::str::from_utf8(&before).unwrap().contains("# retained environment comment"));
}

#[test]
fn explicit_local_and_inherited_values_are_not_rewritten() {
    for shared in [false, true] {
        let fixture = Fixture::new();
        let (_busy, port) = occupied();
        let root = fixture.checkout(port, port);
        if shared {
            fs::write(root.join("shared.yaml"), format!("apiPort: {port}\n")).unwrap();
            fs::write(root.join("env.yaml"), "project: allocation-fixture\n_dockstride:\n  sources:\n    - path: shared.yaml\n").unwrap();
        } else { fs::write(root.join("env.yaml"), format!("project: allocation-fixture\napiPort: {port}\n")).unwrap(); }
        let before = fs::read(root.join("env.yaml")).unwrap();
        succeeded(&fixture.run(&root, &["setup"]));
        assert_eq!(fs::read(root.join("env.yaml")).unwrap(), before);
        assert_eq!(config::get(&root, "apiPort").unwrap()["value"], json!(port));
        if shared { assert!(config::read_env(&root).unwrap().get("apiPort").is_none()); }
    }
}

#[test]
fn same_batch_excludes_overlapping_explicit_and_new_endpoints() {
    let fixture = Fixture::new();
    let (_busy, port) = occupied();
    let root = fixture.checkout(port, port + 6);
    let model = fs::read_to_string(root.join("compose.ncl")).unwrap()
        .replace("apiPort | Number | default = 8000", "apiPort | Number | default = 8000, secondPort | Number | default = 8001, explicitPort | Number | default = 8002")
        .replace("setup.ports.apiPort =", &format!("setup.ports.secondPort = {{service = \"api\", target = 8001, host = \"0.0.0.0\", from = {port}, to = {}}},\n    setup.ports.explicitPort = {{service = \"api\", target = 8002, from = {port}, to = {}}},\n    setup.ports.apiPort =", port + 6, port + 6));
    fs::write(root.join("compose.ncl"), model).unwrap();
    fs::write(root.join("env.yaml"), format!("project: allocation-fixture\nexplicitPort: {}\n", port + 1)).unwrap();
    succeeded(&fixture.run(&root, &["setup"]));
    let local = config::read_env(&root).unwrap();
    assert_ne!(local["apiPort"], local["secondPort"]);
    for field in ["apiPort", "secondPort"] {
        let selected = local[field].as_u64().unwrap();
        assert!((u64::from(port + 2)..=u64::from(port + 6)).contains(&selected));
    }
}

#[test]
fn exhausted_range_and_invalid_policy_leave_yaml_byte_identical() {
    for invalid in [false, true] {
        let fixture = Fixture::new();
        let (_busy, port) = occupied();
        let root = fixture.checkout(port, port);
        if invalid {
            let model = fs::read_to_string(root.join("compose.ncl")).unwrap().replace("target = 8000", "target = 0");
            fs::write(root.join("compose.ncl"), model).unwrap();
        }
        let before = fs::read(root.join("env.yaml")).unwrap();
        assert!(!fixture.run(&root, &["setup"]).status.success());
        assert_eq!(fs::read(root.join("env.yaml")).unwrap(), before);
    }
}

#[test]
fn required_policy_port_is_materialized_on_initial_setup() {
    let fixture = Fixture::new();
    let (_busy, port) = occupied();
    let root = fixture.checkout(port, port + 5);
    let model = fs::read_to_string(root.join("compose.ncl")).unwrap().replace("apiPort | Number | default = 8000", "apiPort | Number");
    fs::write(root.join("compose.ncl"), model).unwrap();
    succeeded(&fixture.run(&root, &["setup"]));
    assert!((u64::from(port + 1)..=u64::from(port + 5)).contains(&config::read_env(&root).unwrap()["apiPort"].as_u64().unwrap()));
}

#[test]
fn explicit_null_policy_port_is_assigned_not_treated_as_a_default() {
    let fixture = Fixture::new();
    let (_busy, port) = occupied();
    let root = fixture.checkout(port, port + 5);
    fs::write(root.join("env.yaml"), "project: allocation-fixture\napiPort: null\n").unwrap();
    succeeded(&fixture.run(&root, &["setup"]));
    let selected = config::read_env(&root).unwrap()["apiPort"].as_u64().unwrap();
    assert!((u64::from(port + 1)..=u64::from(port + 5)).contains(&selected));
}
