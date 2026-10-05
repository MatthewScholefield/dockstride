use dockstride::{allocations, config, state};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output, Stdio}};

struct Fixture { directory: tempfile::TempDir, home: PathBuf, bin: PathBuf }
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let bin = directory.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("docker"), r#"#!/bin/sh
case "$1" in
 info)
  case "$3" in
   '{{.ID}}') printf 'allocation-daemon\n';;
   '{{json .SecurityOptions}}') printf '[]\n';;
   '{{.Swarm.LocalNodeState}}') printf 'inactive\n';;
   *) exit 32;;
  esac;;
 ps|network|volume|service|secret|config) exit 0;;
 *) exit 32;;
esac
"#).unwrap();
        fs::set_permissions(bin.join("docker"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { directory, home, bin }
    }
    fn checkout(&self, name: &str) -> PathBuf {
        let root = self.directory.path().join(name);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("env.yaml"), "# retained environment comment\n{}\n").unwrap();
        fs::write(root.join("compose.ncl"), r#"
let contract = {project | String, backend | String | default = "compose", apiPort | Dyn | default = null} in
let env | contract = import "env.yaml" in
{
  dockstride | not_exported = {
    Config = contract,
    setup.ports.apiPort = {service = "api", target = 8000, from = 54000, to = 54100},
    endpoints.api = if env.apiPort == null then "unallocated" else "http://localhost:%{std.to_string env.apiPort}",
  },
  name = env.project,
  services.api.image = "alpine",
}
"#).unwrap();
        root
    }
    fn command(&self, root: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dks"));
        command.args(["--json", "--non-interactive", "-C"]).arg(root).args(args)
            .env("HOME", &self.home)
            .env("PATH", format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()))
            .env_remove("DOCKER_CONTEXT").env("DOCKER_HOST", "unix:///allocation-fixture.sock");
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command
    }
    fn run(&self, root: &Path, args: &[&str]) -> Output { self.command(root, args).output().unwrap() }
    fn global(&self) -> PathBuf { self.home.join(".local/share/dockstride") }
    fn reservations(&self) -> Value { state::read(&self.global(), "port-reservations").unwrap() }
}
fn succeeded(output: &Output) {
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
fn allocation(root: &Path) -> Value { allocations::read_local(root).unwrap().0["allocations"]["apiPort"].clone() }

#[test]
fn competing_setups_allocate_distinct_stable_endpoints_without_flattening_sources() {
    let fixture = Fixture::new();
    let a = fixture.checkout("a");
    let b = fixture.checkout("b");
    let first = fixture.command(&a, &["setup", "--set", "project=allocation-a"]).spawn().unwrap();
    let second = fixture.command(&b, &["setup", "--set", "project=allocation-b"]).spawn().unwrap();
    succeeded(&first.wait_with_output().unwrap());
    succeeded(&second.wait_with_output().unwrap());
    let a_port = allocation(&a)["port"].clone();
    let b_port = allocation(&b)["port"].clone();
    assert_ne!(a_port, b_port);
    let before = fixture.reservations();
    succeeded(&fixture.run(&a, &["setup"]));
    assert_eq!(allocation(&a)["port"], a_port);
    assert_eq!(fixture.reservations(), before);
    assert!(fs::read_to_string(a.join("env.yaml")).unwrap().contains("# retained environment comment"));
}

#[test]
fn matching_legacy_allocation_upgrades_only_its_own_reservation() {
    let fixture = Fixture::new();
    let root = fixture.checkout("legacy");
    fs::write(root.join("env.yaml"), "project: allocation-legacy\napiPort: 54050 # stable generated port\n").unwrap();
    state::save(&root, "ports", &json!({"apiPort":54050})).unwrap();
    state::save(&fixture.global(), "port-reservations", &json!({"127.0.0.1:54050/tcp":root,"127.0.0.1:54051/tcp":"/unrelated/removed-checkout"})).unwrap();
    let original = fs::read(root.join(".dockstride/ports.json")).unwrap();
    let _ = allocations::read_local(&root).unwrap();
    assert_eq!(fs::read(root.join(".dockstride/ports.json")).unwrap(), original);
    succeeded(&fixture.run(&root, &["setup"]));
    let own = allocation(&root);
    assert_eq!(own["generated"], true);
    assert_eq!(own["daemonId"], "allocation-daemon");
    assert_eq!(own["port"], 54050);
    let reservations = fixture.reservations();
    assert_eq!(reservations["reservations"]["127.0.0.1:54051/tcp"]["root"], "/unrelated/removed-checkout");
    assert_eq!(reservations["reservations"]["127.0.0.1:54051/tcp"]["legacy"], true);
    assert!(reservations["reservations"]["127.0.0.1:54051/tcp"].get("daemonId").is_none());
}

#[test]
fn conflicting_legacy_values_fail_before_config_or_reservation_changes() {
    let fixture = Fixture::new();
    let root = fixture.checkout("conflict");
    fs::write(root.join("env.yaml"), "project: allocation-conflict\napiPort: 54052\n").unwrap();
    state::save(&root, "ports", &json!({"apiPort":54050})).unwrap();
    let env_before = fs::read(root.join("env.yaml")).unwrap();
    let ports_before = fs::read(root.join(".dockstride/ports.json")).unwrap();
    let error = config::set(&root, "apiPort", json!(54053)).unwrap_err().to_string();
    assert!(error.contains("conflicts"), "{error}");
    assert_eq!(fs::read(root.join("env.yaml")).unwrap(), env_before);
    assert_eq!(fs::read(root.join(".dockstride/ports.json")).unwrap(), ports_before);
}

#[test]
fn explicit_same_value_set_and_setup_preserve_original_claims_but_change_provenance() {
    for setup in [false, true] {
        let fixture = Fixture::new();
        let root = fixture.checkout("explicit");
        succeeded(&fixture.run(&root, &["setup", "--set", "project=allocation-explicit"]));
        let generated = allocation(&root);
        let before = fixture.reservations();
        let port = generated["port"].to_string();
        let input = format!("apiPort={port}");
        let args = if setup { vec!["setup", "--set", &input] } else { vec!["config", "set", "apiPort", &port] };
        succeeded(&fixture.run(&root, &args));
        let explicit = allocation(&root);
        assert_eq!(explicit["generated"], false);
        assert_eq!(explicit["explicitValue"], generated["port"]);
        assert_eq!(explicit["key"], generated["key"]);
        assert_eq!(fixture.reservations(), before);
        let registry = state::read(&fixture.global(), "environment-registry").unwrap();
        assert_eq!(registry["environments"][root.to_str().unwrap()]["allocations"]["apiPort"]["generated"], false);
    }
}

#[test]
fn local_unset_reveals_inherited_endpoint_without_releasing_original_claim() {
    let fixture = Fixture::new();
    let root = fixture.checkout("inherited");
    succeeded(&fixture.run(&root, &["setup", "--set", "project=allocation-inherited"]));
    let generated = allocation(&root);
    let reservations = fixture.reservations();
    fs::write(root.join("shared.yaml"), "apiPort: 53999\n").unwrap();
    succeeded(&fixture.run(&root, &["config", "sources", "add", "shared.yaml"]));
    succeeded(&fixture.run(&root, &["config", "set", "apiPort", "53997", "--shared"]));
    assert_eq!(allocation(&root), generated);
    assert_eq!(config::get(&root, "apiPort").unwrap()["value"], generated["port"]);
    succeeded(&fixture.run(&root, &["config", "unset", "apiPort"]));
    assert!(config::read_env(&root).unwrap().get("apiPort").is_none());
    assert_eq!(config::get(&root, "apiPort").unwrap()["value"], 53997);
    assert_eq!(allocation(&root)["generated"], false);
    assert_eq!(allocation(&root)["port"], generated["port"]);
    assert_eq!(fixture.reservations(), reservations);
    succeeded(&fixture.run(&root, &["setup"]));
    assert!(config::read_env(&root).unwrap().get("apiPort").is_none());
    assert_eq!(fixture.reservations(), reservations);
}

#[test]
fn inherited_explicit_ports_never_become_local_generated_fields() {
    let fixture = Fixture::new();
    let root = fixture.checkout("shared-explicit");
    fs::write(root.join("shared.yaml"), "apiPort: 53998\n").unwrap();
    fs::write(root.join("env.yaml"), "_dockstride:\n  sources:\n    - path: shared.yaml\n").unwrap();
    succeeded(&fixture.run(&root, &["setup", "--set", "project=allocation-shared"]));
    assert!(config::read_env(&root).unwrap().get("apiPort").is_none());
    assert_eq!(config::get(&root, "apiPort").unwrap()["value"], 53998);
    assert!(allocations::read_local(&root).unwrap().0["allocations"].as_object().unwrap().is_empty());
    assert!(state::read(&fixture.global(), "port-reservations").unwrap().is_null());
}

#[test]
fn changed_local_set_and_editor_override_retain_the_original_reservation() {
    for editor in [false, true] {
        let fixture = Fixture::new();
        let root = fixture.checkout("changed");
        succeeded(&fixture.run(&root, &["setup", "--set", "project=allocation-changed"]));
        let generated = allocation(&root);
        let reservations = fixture.reservations();
        if editor {
            let executable = fixture.bin.join("edit-port");
            fs::write(&executable, "#!/bin/sh\nprintf 'project: allocation-changed\\napiPort: 53996\\n' > \"$1\"\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            succeeded(&Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "editor_override_worker", "--nocapture"])
                .env("HOME", &fixture.home)
                .env("PATH", format!("{}:{}", fixture.bin.display(), std::env::var("PATH").unwrap()))
                .env_remove("DOCKER_CONTEXT").env("DOCKER_HOST", "unix:///allocation-fixture.sock")
                .env("EDITOR", executable).env("ALLOCATION_EDITOR_ROOT", &root).output().unwrap());
        } else {
            succeeded(&fixture.run(&root, &["config", "set", "apiPort", "53996"]));
        }
        let changed = allocation(&root);
        assert_eq!(changed["generated"], false);
        assert_eq!(changed["explicitValue"], 53996);
        assert_eq!(changed["port"], generated["port"]);
        assert_eq!(changed["key"], generated["key"]);
        assert_eq!(fixture.reservations(), reservations);
        succeeded(&fixture.run(&root, &["setup"]));
        assert_eq!(config::get(&root, "apiPort").unwrap()["value"], 53996);
        assert_eq!(fixture.reservations(), reservations);
    }
}

#[test]
fn secure_local_reads_reject_symlinks_without_touching_the_target() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".dockstride")).unwrap();
    let target = directory.path().join("other.json");
    fs::write(&target, "{\"apiPort\":54050}\n").unwrap();
    std::os::unix::fs::symlink(&target, directory.path().join(".dockstride/ports.json")).unwrap();
    assert!(allocations::read_local(directory.path()).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "{\"apiPort\":54050}\n");
}

#[test]
fn editor_override_worker() {
    let Some(root) = std::env::var_os("ALLOCATION_EDITOR_ROOT") else { return; };
    dockstride::runtime::pin_invocation();
    config::edit(Path::new(&root)).unwrap();
}

#[test]
fn required_generated_port_is_allocated_on_initial_setup_and_after_release() {
    let fixture = Fixture::new();
    let root = fixture.checkout("required");
    let model = fs::read_to_string(root.join("compose.ncl")).unwrap();
    fs::write(root.join("compose.ncl"), model.replace("apiPort | Dyn | default = null", "apiPort | Number")).unwrap();
    succeeded(&fixture.run(&root, &["setup", "--set", "project=allocation-required"]));
    let first = allocation(&root)["port"].as_u64().unwrap();
    assert!((54000..=54100).contains(&first));
    succeeded(&fixture.run(&root, &["ports", "release", "--yes"]));
    assert!(config::read_env(&root).unwrap().get("apiPort").is_none());
    succeeded(&fixture.run(&root, &["setup"]));
    let next = config::get(&root, "apiPort").unwrap()["value"].as_u64().unwrap();
    assert!((54000..=54100).contains(&next));
    assert_eq!(allocation(&root)["generated"], true);
    let inventory = fixture.run(&root, &["env", "list"]);
    succeeded(&inventory);
    let report: Value = serde_json::from_slice(&inventory.stdout).unwrap();
    assert_eq!(report["result"]["environments"][0]["allocatedEndpoints"]["api"], format!("http://localhost:{next}"));
}
