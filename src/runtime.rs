//! Docker CLI execution and explicit, ownership-scoped Compose workflows.
use crate::{model::Project, output::Output, state};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::{
    collections::{BTreeSet, HashSet},
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, UdpSocket},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, LazyLock, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const OWNER: &str = "io.dockstride.owner";
const PROJECT: &str = "io.dockstride.project";
static CANCEL: LazyLock<std::result::Result<Arc<AtomicBool>, String>> = LazyLock::new(|| {
    let flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, flag.clone())
        .map_err(|e| e.to_string())?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, flag.clone())
        .map_err(|e| e.to_string())?;
    Ok(flag)
});

#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Interrupted: foreground work stopped; detached containers and completed setup remain. Use dks down to stop owned containers."
        )
    }
}
impl std::error::Error for Cancelled {}
#[derive(Debug)]
pub struct DockerError {
    pub args: Vec<String>,
    pub status: i32,
    pub stderr: String,
}
impl std::fmt::Display for DockerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "docker {} failed (exit {}): {}",
            self.args.join(" "),
            self.status,
            self.stderr.trim()
        )
    }
}
impl std::error::Error for DockerError {}
fn safe_args(args: &[String]) -> Vec<String> {
    // Diagnostics retain operation names/options, never argument values such as
    // --env passwords, registry credentials, or project command arguments.
    const WORDS: &[&str] = &[
        "compose",
        "container",
        "service",
        "stack",
        "volume",
        "network",
        "image",
        "secret",
        "context",
        "version",
        "info",
        "show",
        "inspect",
        "ls",
        "ps",
        "up",
        "run",
        "exec",
        "logs",
        "watch",
        "create",
        "update",
        "deploy",
        "rm",
        "build",
        "push",
        "pull",
        "login",
        "logout",
    ];
    args.iter()
        .map(|s| {
            if WORDS.contains(&s.as_str()) || (s.starts_with('-') && !s.contains('=')) {
                s.clone()
            } else {
                "<argument>".into()
            }
        })
        .collect()
}
fn docker_error(
    args: &[String],
    status: std::process::ExitStatus,
    stderr: String,
) -> anyhow::Error {
    DockerError {
        args: safe_args(args),
        status: status.code().unwrap_or(128 + status.signal().unwrap_or(9)),
        stderr,
    }
    .into()
}
fn cancellation() -> Result<Arc<AtomicBool>> {
    CANCEL
        .as_ref()
        .map(Arc::clone)
        .map_err(|error| anyhow::anyhow!("Cannot install cancellation handler: {error}"))
}
fn interrupted() -> Result<()> {
    if cancellation()?.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    Ok(())
}
fn terminate(child: &mut Child) {
    terminate_mode(child, true);
}
fn terminate_mode(child: &mut Child, isolated: bool) {
    let target = if isolated {
        -(child.id() as i32)
    } else {
        child.id() as i32
    };
    unsafe {
        libc::kill(target, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(target, libc::SIGKILL);
            }
            let _ = child.wait();
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    // Only managed subprocesses own a separate process group. Never signal the
    // native command's shared foreground group (which includes this process).
    if isolated {
        unsafe {
            libc::kill(target, libc::SIGKILL);
        }
    }
}
fn tail_bytes<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buf[..n]);
            if kept.len() > 128 * 1024 {
                kept.drain(..kept.len() - 128 * 1024);
            }
        }
        kept
    })
}
fn stream_output<R: Read>(
    reader: R,
    output: Output,
    phase: &'static str,
    retain_tail: bool,
) -> Vec<u8> {
    // read_until is capped by take, including malformed giant output lines.
    let mut reader = BufReader::new(reader);
    let mut bytes = Vec::new();
    let mut tail = Vec::new();
    loop {
        bytes.clear();
        match std::io::Read::by_ref(&mut reader)
            .take(8192)
            .read_until(b'\n', &mut bytes)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let message = String::from_utf8_lossy(&bytes);
                let _ = output.event(phase, message.trim_end());
                if retain_tail {
                    let discard = (tail.len() + bytes.len()).saturating_sub(128 * 1024);
                    tail.drain(..discard);
                    tail.extend_from_slice(&bytes);
                }
            }
        }
    }
    tail
}
fn streaming<R: Read + Send + 'static>(
    reader: R,
    output: Output,
    phase: &'static str,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        stream_output(reader, output, phase, false);
    })
}

const DOCKER_ENV: [&str; 6] = [
    "DOCKER_CONTEXT",
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS",
];
struct DockerEnvironment {
    values: [Option<OsString>; 6],
    // Resolution needs the construction-time environment and Docker's runtime
    // default, so this intentionally is not a static LazyLock.
    connection: OnceLock<std::result::Result<Connection, String>>,
}
enum Selection {
    Context(String),
    Host(OsString),
}
struct Connection {
    selection: Selection,
    fingerprint: String,
}
#[derive(Clone)]
pub struct Docker {
    pub root: PathBuf,
    pub output: Output,
    environment: Arc<DockerEnvironment>,
}
impl Docker {
    pub fn new(root: &Path, output: Output) -> Self {
        Self {
            root: root.to_path_buf(),
            output,
            environment: Arc::new(DockerEnvironment {
                values: DOCKER_ENV.map(std::env::var_os),
                connection: OnceLock::new(),
            }),
        }
    }
    fn base_command(&self) -> Command {
        let mut command = Command::new("docker");
        command.current_dir(&self.root).process_group(0);
        for (key, value) in DOCKER_ENV.iter().zip(&self.environment.values) {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
    }
    fn connection(&self) -> Result<&Connection> {
        let connection = self
            .environment
            .connection
            .get_or_init(|| self.resolve_connection().map_err(|e| format!("{e:#}")));
        if connection.is_err() {
            interrupted()?;
        }
        connection
            .as_ref()
            .map_err(|error| anyhow::anyhow!("Resolving Docker connection: {error}"))
    }
    fn resolve_connection(&self) -> Result<Connection> {
        if let Some(context) = self.environment.values[0]
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            return self.named_connection(
                context
                    .to_str()
                    .context("DOCKER_CONTEXT must be UTF-8")?
                    .to_owned(),
            );
        }
        if let Some(host) = self.environment.values[1]
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            return Ok(Connection {
                selection: Selection::Host(host.clone()),
                fingerprint: format!(
                    "host;DOCKER_HOST={}",
                    host.to_str().context("DOCKER_HOST must be UTF-8")?
                ),
            });
        }
        let args = strings(&["context", "show"]);
        let mut command = self.base_command();
        command
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST")
            .args(&args);
        let name = self
            .capture_command(&args, command, None, 30)?
            .trim()
            .to_owned();
        ensure!(!name.is_empty(), "Docker returned an empty current context");
        self.named_connection(name)
    }
    fn named_connection(&self, name: String) -> Result<Connection> {
        let args = strings(&[
            "context",
            "inspect",
            &name,
            "--format",
            "{{.Endpoints.docker.Host}}",
        ]);
        let mut command = self.base_command();
        command
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST")
            .args(["--context", &name])
            .args(&args);
        let endpoint = self.capture_command(&args, command, None, 30)?;
        ensure!(
            !endpoint.trim().is_empty(),
            "Docker context {name} has no Engine endpoint"
        );
        Ok(Connection {
            fingerprint: format!("{name};{}", endpoint.trim()),
            selection: Selection::Context(name),
        })
    }
    fn command(&self, args: &[String]) -> Result<Command> {
        let mut command = self.base_command();
        command
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST");
        match &self.connection()?.selection {
            Selection::Context(name) => {
                command.args(["--context", name]);
            }
            Selection::Host(host) => {
                command.env("DOCKER_HOST", host);
            }
        }
        command.args(args);
        Ok(command)
    }
    fn inherit_connection(&self, command: &mut Command) -> Result<()> {
        for (key, value) in DOCKER_ENV.iter().zip(&self.environment.values) {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST");
        match &self.connection()?.selection {
            Selection::Context(name) => {
                command.env("DOCKER_CONTEXT", name);
            }
            Selection::Host(host) => {
                command.env("DOCKER_HOST", host);
            }
        }
        Ok(())
    }
    fn recorded_command(&self, fingerprint: &str) -> Result<Command> {
        let (name, endpoint) = fingerprint
            .split_once(';')
            .context("Recorded Docker connection lacks an endpoint fingerprint")?;
        let mut command = self.base_command();
        command
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST");
        if let Some(host) = endpoint.strip_prefix("DOCKER_HOST=") {
            ensure!(!host.is_empty(), "Recorded Docker host is empty");
            command.env("DOCKER_HOST", host);
        } else {
            let current = self.named_connection(name.to_owned())?;
            ensure!(
                current.fingerprint == fingerprint,
                "Recorded Docker context endpoint changed; refusing to query a different daemon"
            );
            command.args(["--context", name]);
        }
        Ok(command)
    }
    pub fn capture(&self, args: &[String], stdin: Option<&[u8]>) -> Result<String> {
        self.capture_timeout(args, stdin, 300)
    }
    pub fn capture_timeout(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        timeout: u64,
    ) -> Result<String> {
        self.capture_command(args, self.command(args)?, stdin, timeout)
    }
    fn capture_command(
        &self,
        args: &[String],
        mut command: Command,
        stdin: Option<&[u8]>,
        timeout: u64,
    ) -> Result<String> {
        interrupted()?;
        let mut child = command
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Cannot start Docker; install Docker Engine and the Compose plugin")?;
        let stdout = child.stdout.take().unwrap();
        // Captured stdout is semantic data, not a bounded log tail.
        let out = thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut stream = stdout;
            stream.read_to_end(&mut bytes).map(|_| bytes)
        });
        let err = tail_bytes(child.stderr.take().unwrap());
        thread::scope(|scope| {
            let writer = stdin.map(|bytes| {
                let mut pipe = child.stdin.take().unwrap();
                scope.spawn(move || pipe.write_all(bytes))
            });
            let wait = self.wait(&mut child, timeout);
            let bytes = out
                .join()
                .map_err(|_| anyhow::anyhow!("Docker output reader failed"))??;
            let error = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
            let status = wait?;
            if !status.success() {
                return Err(docker_error(args, status, error));
            }
            if let Some(writer) = writer {
                writer
                    .join()
                    .map_err(|_| anyhow::anyhow!("Docker stdin writer failed"))??;
            }
            String::from_utf8(bytes).context("Docker returned non-UTF-8 output")
        })
    }
    pub fn run(&self, args: &[String], stdin: Option<&[u8]>) -> Result<()> {
        self.run_timeout(args, stdin, 0)
    }
    pub fn run_timeout(&self, args: &[String], stdin: Option<&[u8]>, timeout: u64) -> Result<()> {
        interrupted()?;
        let mut child = self
            .command(args)?
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Cannot start Docker")?;
        let out = streaming(child.stdout.take().unwrap(), self.output.clone(), "docker");
        let stderr = child.stderr.take().unwrap();
        let output = self.output.clone();
        let err = thread::spawn(move || stream_output(stderr, output, "docker", true));
        thread::scope(|scope| {
            let writer = stdin.map(|bytes| {
                let mut pipe = child.stdin.take().unwrap();
                scope.spawn(move || pipe.write_all(bytes))
            });
            let result = self.wait(&mut child, timeout);
            let _ = out.join();
            let stderr = err
                .join()
                .map_err(|_| anyhow::anyhow!("Docker stderr reader failed"))?;
            let status = result?;
            if !status.success() {
                return Err(docker_error(
                    args,
                    status,
                    String::from_utf8_lossy(&stderr).into_owned(),
                ));
            }
            if let Some(writer) = writer {
                writer
                    .join()
                    .map_err(|_| anyhow::anyhow!("Docker stdin writer failed"))??;
            }
            Ok(())
        })
    }
    /// Preserve raw output and the foreground TTY for explicitly native commands.
    pub fn native(&self, args: &[String], stdin: Option<&[u8]>) -> Result<()> {
        if self.output.json {
            return self.run(args, stdin);
        }
        interrupted()?;
        let group = unsafe { libc::getpgrp() };
        let mut child = self
            .command(args)?
            .process_group(group)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context("Cannot start native Docker command")?;
        thread::scope(|scope| {
            let writer = stdin.map(|bytes| {
                let mut pipe = child.stdin.take().unwrap();
                scope.spawn(move || pipe.write_all(bytes))
            });
            let result = self.wait_mode(&mut child, 0, false);
            let status = result?;
            if !status.success() {
                return Err(docker_error(
                    args,
                    status,
                    "Native Docker command failed".into(),
                ));
            }
            if let Some(writer) = writer {
                writer
                    .join()
                    .map_err(|_| anyhow::anyhow!("Docker stdin writer failed"))??;
            }
            Ok(())
        })
    }
    fn wait(&self, child: &mut Child, timeout: u64) -> Result<std::process::ExitStatus> {
        self.wait_mode(child, timeout, true)
    }
    fn wait_mode(
        &self,
        child: &mut Child,
        timeout: u64,
        isolated: bool,
    ) -> Result<std::process::ExitStatus> {
        let start = Instant::now();
        let flag = cancellation()?;
        loop {
            if flag.load(Ordering::Relaxed) {
                terminate_mode(child, isolated);
                interrupted()?;
            }
            if timeout > 0 && start.elapsed() >= Duration::from_secs(timeout) {
                terminate_mode(child, isolated);
                bail!(
                    "Docker command exceeded {timeout}s; detached containers and completed operations remain"
                );
            }
            if let Some(status) = child.try_wait()? {
                // Commands cannot leave pipe-holding background descendants behind.
                if isolated {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                }
                return Ok(status);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    pub fn context(&self) -> Result<String> {
        Ok(self.connection()?.fingerprint.clone())
    }
    pub fn check(&self) -> Result<Value> {
        let engine = self
            .capture(&strings(&["version", "--format", "{{json .}}"]), None)
            .context("Docker daemon unavailable in the selected context")?;
        let compose = self
            .capture(&strings(&["compose", "version", "--short"]), None)
            .context("Docker Compose plugin required")?;
        let raw = compose.trim().trim_start_matches('v');
        let mut version = raw.split('.');
        let major: u32 = version.next().unwrap_or("0").parse().unwrap_or(0);
        let minor: u32 = version.next().unwrap_or("0").parse().unwrap_or(0);
        ensure!(
            major > 2 || major == 2 && minor >= 24,
            "Docker Compose >=2.24 required for watch and native dependency handling; found {}",
            compose.trim()
        );
        Ok(
            json!({"context":self.context()?,"engine":serde_json::from_str::<Value>(&engine)?,"compose":compose.trim()}),
        )
    }
}
pub fn connection_fingerprint(root: &Path) -> Result<String> {
    Docker::new(root, Output::default()).context()
}
pub fn pinned_command(root: &Path, fingerprint: &str) -> Result<Command> {
    Docker::new(root, Output::default()).recorded_command(fingerprint)
}
pub fn capture_pinned(root: &Path, fingerprint: &str, args: &[String]) -> Result<String> {
    let docker = Docker::new(root, Output::default());
    let mut command = docker.recorded_command(fingerprint)?;
    command.args(args);
    docker.capture_command(args, command, None, 30)
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}
pub fn compose_args(project: &Project, file: &Path) -> Result<Vec<String>> {
    Ok(vec![
        "compose".into(),
        "--project-directory".into(),
        project.root.to_string_lossy().into_owned(),
        "--project-name".into(),
        project.name()?.into(),
        "--file".into(),
        file.to_string_lossy().into_owned(),
    ])
}

fn check_ownership(project: &Project, docker: &Docker, creating: bool) -> Result<Value> {
    let context = docker.context()?;
    let root = fs::canonicalize(&project.root)?
        .to_string_lossy()
        .into_owned();
    let mut identity = state::read(&project.root, "identity")?;
    let missing = identity.get("id").and_then(Value::as_str).is_none();
    if missing {
        ensure!(
            creating,
            "No Dockstride ownership identity: refusing to operate on resources by project name alone"
        );
        identity = json!({"id":"","project":project.name()?,"backend":project.backend()?,"context":context,"root":root,"resources":false});
    } else {
        ensure!(
            identity["root"].as_str() == Some(root.as_str()),
            "This environment identity belongs to another checkout; create a separately configured environment"
        );
        ensure!(
            identity["project"].as_str() == Some(project.name()?)
                && identity["backend"].as_str() == Some(project.backend()?),
            "Project/backend differs from persisted identity; an explicit transition is required"
        );
        if identity["context"].as_str().is_some_and(|s| !s.is_empty()) {
            ensure!(
                identity["context"].as_str() == Some(context.as_str()),
                "Docker context differs from the environment's recorded context; refusing cross-context operations"
            );
        } else {
            identity["context"] = json!(context);
        }
    }
    let id = identity["id"].as_str().unwrap().to_owned();
    for (kind, filter, listing) in [
        ("container", "com.docker.compose.project", vec!["ps", "-aq"]),
        (
            "volume",
            "com.docker.compose.project",
            vec!["volume", "ls", "-q"],
        ),
        (
            "network",
            "com.docker.compose.project",
            vec!["network", "ls", "-q"],
        ),
    ] {
        let mut args = strings(&listing);
        args.extend(strings(&[
            "--filter",
            &format!("label={filter}={}", project.name()?),
        ]));
        let found = docker.capture(&args, None)?;
        for resource in found.split_whitespace() {
            let labels = docker.capture(
                &strings(&[
                    kind,
                    "inspect",
                    resource,
                    "--format",
                    if kind == "container" {
                        "{{json .Config.Labels}}"
                    } else {
                        "{{json .Labels}}"
                    },
                ]),
                None,
            )?;
            let labels: Value = serde_json::from_str(&labels)?;
            ensure!(
                labels[OWNER].as_str() == Some(&id),
                "Unrelated {kind} {resource} uses project {}; refusing to reuse or delete it",
                project.name()?
            );
        }
    }
    let model = project.compose()?;
    for (field, kind) in [("volumes", "volume"), ("networks", "network")] {
        let mut definitions = model[field].as_object().cloned().unwrap_or_default();
        if field == "networks" && !definitions.contains_key("default") {
            definitions.insert("default".into(), json!({}));
        }
        let existing = docker.capture(&strings(&[kind, "ls", "--format", "{{.Name}}"]), None)?;
        let existing: HashSet<&str> = existing.lines().collect();
        for (logical, definition) in definitions {
            if definition["external"] == true {
                continue;
            }
            let expected = definition["name"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{}_{}", project.name().unwrap(), logical));
            if existing.contains(expected.as_str()) {
                let labels: Value = serde_json::from_str(&docker.capture(
                    &strings(&[kind, "inspect", &expected, "--format", "{{json .Labels}}"]),
                    None,
                )?)?;
                ensure!(
                    !id.is_empty() && labels[OWNER].as_str() == Some(&id),
                    "Unrelated {kind} {expected} occupies a declared managed resource name; use external=true for intentional sharing"
                );
            }
        }
    }
    if project.backend()? == "swarm" {
        let info = docker.capture(
            &strings(&["info", "--format", "{{.Swarm.LocalNodeState}}"]),
            None,
        )?;
        ensure!(
            info.trim() == "active",
            "Selected Docker context is not an active Swarm"
        );
        let ids = docker.capture(
            &strings(&[
                "service",
                "ls",
                "-q",
                "--filter",
                &format!("label=com.docker.stack.namespace={}", project.name()?),
            ]),
            None,
        )?;
        for resource in ids.split_whitespace() {
            let labels: Value = serde_json::from_str(&docker.capture(
                &strings(&[
                    "service",
                    "inspect",
                    resource,
                    "--format",
                    "{{json .Spec.Labels}}",
                ]),
                None,
            )?)?;
            ensure!(
                labels[OWNER].as_str() == Some(&id),
                "Unrelated Swarm service {resource} occupies stack {}",
                project.name()?
            );
        }
    }
    Ok(identity)
}
pub fn validate_ownership(project: &Project, docker: &Docker, creating: bool) -> Result<String> {
    let _lock = state::lock(&project.root, "identity")?;
    let checked = check_ownership(project, docker, creating)?;
    let identity = state::ensure_identity(
        &project.root,
        project.name()?,
        project.backend()?,
        checked["context"].as_str().unwrap(),
    )?;
    Ok(identity["id"]
        .as_str()
        .context("Missing ownership id")?
        .to_owned())
}
pub fn doctor(project: &Project, output: &Output) -> Result<Value> {
    let docker = Docker::new(&project.root, output.clone());
    let checked = check_ownership(project, &docker, true)?;
    if project.backend()? == "compose" {
        detect_ports(project, &[], &docker)?;
    }
    Ok(
        json!({"ownership":"unambiguous","context":checked["context"],"ports":"no detected conflicts","remotePortCaveat":!local_context(checked["context"].as_str().unwrap_or(""))}),
    )
}
fn add_labels(record: &mut Value, id: &str, name: &str) -> Result<()> {
    let record = record
        .as_object_mut()
        .context("Docker resource must be an object")?;
    let value = record.entry("labels").or_insert_with(|| json!({}));
    if let Some(labels) = value.as_array() {
        let mut map = Map::new();
        for label in labels {
            let label = label.as_str().context("Label list must contain strings")?;
            let (key, value) = label.split_once('=').unwrap_or((label, ""));
            map.insert(key.into(), json!(value));
        }
        *value = Value::Object(map);
    }
    let labels = value
        .as_object_mut()
        .context("Labels must be a map or string list")?;
    labels.insert(OWNER.into(), json!(id));
    labels.insert(PROJECT.into(), json!(name));
    Ok(())
}
pub fn render_document(project: &Project, target: &str) -> Result<Value> {
    let mut model = match target {
        "compose" => project.compose()?,
        "swarm" => project.swarm()?,
        _ => bail!("Unknown render target {target}"),
    };
    let identity = state::read(&project.root, "identity")?;
    let Some(id) = identity["id"].as_str() else {
        return Ok(model);
    };
    let root = fs::canonicalize(&project.root)?
        .to_string_lossy()
        .into_owned();
    ensure!(
        identity["root"].as_str() == Some(root.as_str())
            && identity["project"].as_str() == Some(project.name()?)
            && identity["backend"].as_str() == Some(project.backend()?),
        "Rendered environment differs from its recorded ownership identity"
    );
    for service in model["services"]
        .as_object_mut()
        .context("Missing services")?
        .values_mut()
    {
        add_labels(service, id, project.name()?)?;
        if target == "swarm" {
            let deploy = service
                .as_object_mut()
                .unwrap()
                .entry("deploy")
                .or_insert_with(|| json!({}));
            add_labels(deploy, id, project.name()?)?;
        }
    }
    // Label managed volumes/networks only. Docker external objects are never commandeered.
    for field in ["volumes", "networks"] {
        if field == "networks" && model.get(field).is_none() {
            model[field] = json!({"default":{}});
        }
        if let Some(resources) = model[field].as_object_mut() {
            for resource in resources.values_mut() {
                if resource.is_null() {
                    *resource = json!({});
                }
                if resource["external"] != true {
                    add_labels(resource, id, project.name()?)?;
                }
            }
        }
    }
    Ok(model)
}
pub fn write_render(project: &Project, target: &str) -> Result<PathBuf> {
    ensure!(
        state::read(&project.root, "identity")?["id"]
            .as_str()
            .is_some(),
        "Ownership identity must be validated before rendering an executable snapshot"
    );
    let model = render_document(project, target)?;
    let path = project
        .root
        .join(".dockstride")
        .join(format!("render-{target}.yaml"));
    state::atomic_write(&path, serde_yaml::to_string(&model)?.as_bytes(), 0o600)?;
    Ok(path)
}

fn selected_services(project: &Project, selected: &[String]) -> Result<Vec<String>> {
    let services = project.services()?;
    let initial: Vec<String> = if selected.is_empty() {
        services
            .iter()
            .filter(|(_, service)| service["profiles"].as_array().is_none_or(Vec::is_empty))
            .map(|(name, _)| name.clone())
            .collect()
    } else {
        selected.to_vec()
    };
    let mut needed = BTreeSet::new();
    fn visit(
        name: &str,
        services: &Map<String, Value>,
        needed: &mut BTreeSet<String>,
        visiting: &mut HashSet<String>,
    ) -> Result<()> {
        ensure!(services.contains_key(name), "Unknown service {name}");
        if needed.contains(name) {
            return Ok(());
        }
        ensure!(
            visiting.insert(name.into()),
            "Dependency cycle involving {name}"
        );
        let depends = &services[name]["depends_on"];
        if let Some(map) = depends.as_object() {
            for dependency in map.keys() {
                visit(dependency, services, needed, visiting)?;
            }
        } else if let Some(list) = depends.as_array() {
            for dependency in list {
                visit(
                    dependency
                        .as_str()
                        .context("depends_on entries must be strings")?,
                    services,
                    needed,
                    visiting,
                )?;
            }
        }
        visiting.remove(name);
        needed.insert(name.into());
        Ok(())
    }
    for name in initial {
        visit(&name, services, &mut needed, &mut HashSet::new())?;
    }
    Ok(needed.into_iter().collect())
}
fn applicable(action: &Value, workflow: &str, selected: &[String]) -> bool {
    let workflows = action["workflows"].as_array();
    let services = action["services"].as_array();
    workflows
        .is_none_or(|list| list.is_empty() || list.iter().any(|v| v.as_str() == Some(workflow)))
        && (selected.is_empty()
            || services.is_none_or(|list| {
                list.is_empty()
                    || list
                        .iter()
                        .any(|v| v.as_str().is_some_and(|s| selected.iter().any(|n| n == s)))
            }))
}
fn actions<'a>(
    project: &'a Project,
    workflow: &str,
    selected: &[String],
    stage: Option<&str>,
) -> Result<Vec<&'a Value>> {
    let Some(value) = project.metadata.get("actions") else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .context("dockstride.actions must be an ordered array")?;
    for action in list {
        ensure!(
            action["name"].as_str().is_some_and(|s| !s.is_empty()),
            "Every action needs a nonempty name"
        );
        let kind = action["kind"].as_str().context("Action needs kind")?;
        ensure!(
            ["up", "run", "exec", "command"].contains(&kind),
            "Unsupported action kind {kind}"
        );
        ensure!(
            ["before", "after"].contains(&action["stage"].as_str().unwrap_or("before")),
            "Action stage must be before or after"
        );
        for scope in ["workflows", "services"] {
            if let Some(value) = action.get(scope) {
                ensure!(
                    value
                        .as_array()
                        .is_some_and(|list| list.iter().all(Value::is_string)),
                    "Action {scope} must be a string list"
                );
            }
        }
        if kind == "command" || kind == "exec" {
            ensure!(
                !argv(&action["argv"])?.is_empty(),
                "Action argv cannot be empty"
            );
        }
        if kind != "command" {
            ensure!(
                action["service"].as_str().is_some_and(|name| project
                    .services()
                    .is_ok_and(|services| services.contains_key(name))),
                "Action references an unknown service"
            );
        }
    }
    Ok(list
        .iter()
        .filter(|a| {
            applicable(a, workflow, selected)
                && stage.is_none_or(|stage| a["stage"].as_str().unwrap_or("before") == stage)
        })
        .collect())
}
fn argv(value: &Value) -> Result<Vec<String>> {
    value
        .as_array()
        .context("argv must be an array, never a shell string")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .context("argv entries must be strings")
        })
        .collect()
}
fn project_command(
    project: &Project,
    args: &[String],
    docker: &Docker,
    output: &Output,
    timeout: u64,
) -> Result<()> {
    ensure!(!args.is_empty(), "Project command argv cannot be empty");
    interrupted()?;
    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .current_dir(&project.root)
        .process_group(0)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    docker.inherit_connection(&mut command)?;
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot start project command {}", args[0]))?;
    let out = streaming(child.stdout.take().unwrap(), output.clone(), "hook");
    let err = streaming(child.stderr.take().unwrap(), output.clone(), "hook");
    let result = docker.wait(&mut child, timeout);
    let _ = out.join();
    let _ = err.join();
    ensure!(result?.success(), "Project command {} failed", args[0]);
    Ok(())
}
fn execute_action(
    project: &Project,
    action: &Value,
    docker: &Docker,
    output: &Output,
    timeout: u64,
) -> Result<()> {
    let name = action["name"]
        .as_str()
        .context("Each action requires a name")?;
    output.event("action", name)?;
    let kind = action["kind"]
        .as_str()
        .context("Each action requires kind")?;
    if kind == "command" {
        return project_command(project, &argv(&action["argv"])?, docker, output, timeout)
            .with_context(|| format!("Action {name} failed"));
    }
    ensure!(
        project.backend()? == "compose",
        "Action {name}: Compose {kind} is not a Swarm prerequisite; use an explicit project argv command"
    );
    let file = write_render(project, "compose")?;
    let mut args = compose_args(project, &file)?;
    let service = action["service"]
        .as_str()
        .context("Compose actions require service")?;
    ensure!(
        project.services()?.contains_key(service),
        "Action {name} references unknown service {service}"
    );
    match kind {
        "up" => {
            args.extend(strings(&["up", "--detach", service]));
        }
        "run" => {
            args.extend(strings(&["run", "--rm", "-T", service]));
            if action.get("argv").is_some() {
                args.extend(argv(&action["argv"])?);
            }
        }
        "exec" => {
            args.extend(strings(&["exec", "-T", service]));
            args.extend(argv(&action["argv"])?);
        }
        _ => bail!("Unknown action kind {kind}"),
    }
    docker
        .run_timeout(&args, None, timeout)
        .with_context(|| format!("Action {name} failed"))?;
    if kind == "up" {
        wait_ready(project, &[service.to_owned()], docker, output, timeout)?;
    }
    Ok(())
}
pub fn execute_actions(
    project: &Project,
    workflow: &str,
    selected: &[String],
    docker: &Docker,
    output: &Output,
) -> Result<()> {
    for action in actions(project, workflow, selected, None)? {
        execute_action(
            project,
            action,
            docker,
            output,
            action["timeout"].as_u64().unwrap_or(300),
        )?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct Port {
    service: String,
    host: String,
    published: u16,
    protocol: String,
}
fn published_ports(project: &Project, selected: &[String]) -> Result<Vec<Port>> {
    let mut result = Vec::new();
    for name in selected_services(project, selected)? {
        if let Some(ports) = project.services()?[&name]["ports"].as_array() {
            for port in ports {
                let (host, published, protocol) = if let Some(port) = port.as_str() {
                    let (port, protocol) = port.split_once('/').unwrap_or((port, "tcp"));
                    // Compose IPv6 literals are bracketed; split published/target from the right.
                    let Some((prefix, _target)) = port.rsplit_once(':') else {
                        continue;
                    };
                    let (host, published) = prefix.rsplit_once(':').unwrap_or(("0.0.0.0", prefix));
                    (
                        host.trim_matches(['[', ']']).to_owned(),
                        published.to_owned(),
                        protocol.to_owned(),
                    )
                } else {
                    let Some(published) = port.get("published") else {
                        continue;
                    };
                    (
                        port["host_ip"].as_str().unwrap_or("0.0.0.0").to_owned(),
                        published
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| published.to_string()),
                        port["protocol"].as_str().unwrap_or("tcp").to_owned(),
                    )
                };
                let (start, end) = published
                    .split_once('-')
                    .unwrap_or((&published, &published));
                let start: u16 = start
                    .parse()
                    .with_context(|| format!("Invalid published port {published}"))?;
                let end: u16 = end.parse()?;
                ensure!(
                    start > 0 && end >= start,
                    "Invalid published port range {published}"
                );
                for published in start..=end {
                    result.push(Port {
                        service: name.clone(),
                        host: host.clone(),
                        published,
                        protocol: protocol.clone(),
                    });
                }
            }
        }
    }
    Ok(result)
}
fn bindable(host: &str, port: u16, protocol: &str) -> bool {
    if protocol == "udp" {
        UdpSocket::bind((host, port)).is_ok()
    } else {
        TcpListener::bind((host, port)).is_ok()
    }
}
fn local_context(context: &str) -> bool {
    context.contains(";unix://") || context.contains(";DOCKER_HOST=unix://")
}
fn container_rows(project: &Project, docker: &Docker) -> Result<Vec<Value>> {
    container_rows_timeout(project, docker, 300)
}
fn container_rows_timeout(project: &Project, docker: &Docker, timeout: u64) -> Result<Vec<Value>> {
    let started = Instant::now();
    let ids = docker.capture_timeout(
        &strings(&[
            "ps",
            "-aq",
            "--filter",
            &format!("label=com.docker.compose.project={}", project.name()?),
        ]),
        None,
        timeout.max(1),
    )?;
    if ids.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut args = strings(&["container", "inspect"]);
    args.extend(ids.split_whitespace().map(str::to_owned));
    serde_json::from_str(&docker.capture_timeout(
        &args,
        None,
        timeout.saturating_sub(started.elapsed().as_secs()).max(1),
    )?)
    .context("Invalid Docker container inspection")
}
fn detect_ports(project: &Project, selected: &[String], docker: &Docker) -> Result<()> {
    let context = docker.context()?;
    let ours = container_rows(project, docker)?;
    let mut owned = HashSet::new();
    for row in &ours {
        if row["State"]["Running"] == true
            && let Some(bindings) = row["NetworkSettings"]["Ports"].as_object()
        {
            for (key, list) in bindings {
                for binding in list.as_array().into_iter().flatten() {
                    if let Some(port) = binding["HostPort"]
                        .as_str()
                        .and_then(|p| p.parse::<u16>().ok())
                    {
                        owned.insert((port, key.split('/').nth(1).unwrap_or("tcp").to_owned()));
                    }
                }
            }
        }
    }
    let ids = docker.capture(&strings(&["ps", "-q"]), None)?;
    let mut foreign = HashSet::new();
    if !ids.trim().is_empty() {
        let mut args = strings(&["container", "inspect"]);
        args.extend(ids.split_whitespace().map(str::to_owned));
        let rows: Vec<Value> = serde_json::from_str(&docker.capture(&args, None)?)?;
        for row in rows {
            if row["Config"]["Labels"]["com.docker.compose.project"].as_str()
                == Some(project.name()?)
            {
                continue;
            }
            if let Some(bindings) = row["NetworkSettings"]["Ports"].as_object() {
                for (key, list) in bindings {
                    for binding in list.as_array().into_iter().flatten() {
                        if let Some(port) = binding["HostPort"]
                            .as_str()
                            .and_then(|p| p.parse::<u16>().ok())
                        {
                            foreign
                                .insert((port, key.split('/').nth(1).unwrap_or("tcp").to_owned()));
                        }
                    }
                }
            }
        }
    }
    let mut seen: Vec<Port> = Vec::new();
    for port in published_ports(project, selected)? {
        ensure!(
            !seen.iter().any(|other| other.published == port.published
                && other.protocol == port.protocol
                && (other.host == port.host
                    || ["0.0.0.0", "::"].contains(&other.host.as_str())
                    || ["0.0.0.0", "::"].contains(&port.host.as_str()))),
            "Overlapping published port {}:{}/{} in service {}",
            port.host,
            port.published,
            port.protocol,
            port.service
        );
        seen.push(port.clone());
        ensure!(
            !foreign.contains(&(port.published, port.protocol.clone())),
            "Port {}/{} needed by {} is published by an unrelated container; choose another port (nothing was stopped)",
            port.published,
            port.protocol,
            port.service
        );
        if local_context(&context) && !owned.contains(&(port.published, port.protocol.clone())) {
            ensure!(
                bindable(&port.host, port.published, &port.protocol),
                "Port {}:{}/{} needed by {} is occupied; refusing to stop/reuse another process",
                port.host,
                port.published,
                port.protocol,
                port.service
            );
        }
    }
    Ok(())
}
pub fn allocate_ports(project: &Project) -> Result<bool> {
    allocate_ports_with_docker(project, &Docker::new(&project.root, Output::default()))
}
fn allocate_ports_with_docker(project: &Project, docker: &Docker) -> Result<bool> {
    let Some(policies) = project.metadata["setup"]["ports"].as_object() else {
        return Ok(false);
    };
    let raw_env = crate::config::read_env(&project.root)?;
    let home = std::env::var_os("HOME").context("HOME required for allocation coordination")?;
    let global = PathBuf::from(home).join(".local/share/dockstride");
    let _global = state::lock(&global, "port-allocation")?;
    let _local = state::lock(&project.root, "port-allocation")?;
    let mut saved = state::read(&project.root, "ports")?;
    if !saved.is_object() {
        saved = json!({});
    }
    let mut changed = false;
    for (field, policy) in policies {
        let current = field.split('.').try_fold(&raw_env, |v, key| v.get(key));
        let persisted = saved[field].as_u64();
        if let Some(value) = current.filter(|v| !v.is_null()) {
            if let Some(port) = persisted {
                ensure!(
                    value.as_u64() == Some(port),
                    "Allocated port {field} was changed from persisted {port}; endpoint transitions must be explicit"
                );
            }
            continue;
        }
        let service = policy["service"]
            .as_str()
            .context("Port allocation requires service")?;
        ensure!(
            project.services()?.contains_key(service),
            "Port policy {field} names unknown service {service}"
        );
        ensure!(
            policy["target"]
                .as_u64()
                .is_some_and(|p| p > 0 && p <= 65535),
            "Port policy {field} requires a valid container target port"
        );
        ensure!(
            local_context(&docker.context()?),
            "Automatic port allocation currently requires a local Unix-socket Docker context; configure fixed ports for a remote daemon"
        );
        let port = if let Some(port) = persisted {
            u16::try_from(port)?
        } else {
            let start = u16::try_from(policy["from"].as_u64().unwrap_or(49152))?;
            let end = u16::try_from(policy["to"].as_u64().unwrap_or(65535))?;
            ensure!(
                start > 0 && end >= start,
                "Invalid allocation range for {field}"
            );
            let host = policy["host"].as_str().unwrap_or("127.0.0.1");
            let protocol = policy["protocol"].as_str().unwrap_or("tcp");
            // Persist a global reservation so independently configured checkouts never race
            // for a not-yet-bound endpoint. Reservations intentionally outlive down.
            let mut reservations = state::read(&global, "port-reservations")?;
            if !reservations.is_object() {
                reservations = json!({});
            }
            let port = (start..=end)
                .find(|port| {
                    !reservations
                        .as_object()
                        .unwrap()
                        .keys()
                        .any(|key| key.ends_with(&format!(":{port}/{protocol}")))
                        && bindable(host, *port, protocol)
                })
                .context("No unreserved available port in declared range")?;
            reservations[format!("{host}:{port}/{protocol}")] =
                json!(fs::canonicalize(&project.root)?.to_string_lossy());
            state::save(&global, "port-reservations", &reservations)?;
            port
        };
        saved[field] = json!(port);
        state::save(&project.root, "ports", &saved)?;
        crate::config::set(&project.root, field, json!(port))?;
        changed = true;
    }
    Ok(changed)
}

fn one_shot(project: &Project, service: &str) -> Result<bool> {
    for definition in project.services()?.values() {
        if definition["depends_on"][service]["condition"] == "service_completed_successfully" {
            return Ok(true);
        }
    }
    Ok(project.metadata["oneshots"]
        .as_array()
        .is_some_and(|list| list.iter().any(|s| s.as_str() == Some(service))))
}
fn inspect_state(project: &Project, service: &str, rows: &[Value]) -> Result<(bool, String)> {
    let containers: Vec<&Value> = rows
        .iter()
        .filter(|row| {
            row["Config"]["Labels"]["com.docker.compose.service"].as_str() == Some(service)
                && row["Config"]["Labels"]["com.docker.compose.oneoff"].as_str() != Some("True")
        })
        .collect();
    if containers.is_empty() {
        return Ok((false, "not created".into()));
    }
    let oneshot = one_shot(project, service)?;
    let mut ready = true;
    let mut description = String::new();
    for container in containers {
        let state = &container["State"];
        let status = state["Status"].as_str().unwrap_or("unknown");
        if status == "exited" || status == "dead" {
            let code = state["ExitCode"].as_i64().unwrap_or(-1);
            ensure!(
                oneshot && code == 0,
                "Service {service} {status} with exit code {code}; inspect dks logs {service}"
            );
            description = "completed successfully".into();
            continue;
        }
        if oneshot {
            ready = false;
            description = "one-shot still running".into();
            continue;
        }
        ensure!(
            state["OOMKilled"] != true,
            "Service {service} was killed by the kernel (out of memory)"
        );
        if state["Running"] != true {
            ready = false;
            description = status.into();
            continue;
        }
        let declared = project.services()?[service]["healthcheck"].is_object()
            && project.services()?[service]["healthcheck"]["disable"] != true
            && project.services()?[service]["healthcheck"]["test"][0] != "NONE";
        match state["Health"]["Status"].as_str() {
            Some("healthy") => {
                description = "healthy".into();
            }
            Some("unhealthy") => {
                bail!("Service {service} is unhealthy; inspect dks logs {service}")
            }
            Some(other) => {
                ready = false;
                description = format!("health: {other}");
            }
            None if declared => {
                ready = false;
                description = "declared healthcheck has no result yet".into();
            }
            None => {
                description = "running (no healthcheck; application readiness not asserted)".into();
            }
        }
    }
    Ok((ready, description))
}
fn readiness_check(
    project: &Project,
    service: &str,
    docker: &Docker,
    output: &Output,
    remaining: u64,
) -> Result<bool> {
    let Some(check) = project.metadata["readiness"].get(service) else {
        return Ok(true);
    };
    ensure!(
        check.is_object(),
        "Readiness for {service} must be an object"
    );
    ensure!(
        check.get("url").is_some() || check.get("command").is_some(),
        "Readiness for {service} requires url or command"
    );
    if let Some(url) = check["url"].as_str() {
        // The library handles HTTP(S), redirects, and certificate verification; errors
        // while an application boots are retried within the same bounded wait.
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(remaining.clamp(1, 5))))
            .http_status_as_error(false)
            .build();
        let agent: ureq::Agent = config.into();
        let Ok(mut response) = agent.get(url).call() else {
            return Ok(false);
        };
        if response.status().as_u16() as u64 != check["status"].as_u64().unwrap_or(200) {
            return Ok(false);
        }
        let body = match response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_string()
        {
            Ok(body) => body,
            Err(_) => return Ok(false),
        };
        if let Some(contains) = check["contains"].as_str()
            && !body.contains(contains)
        {
            return Ok(false);
        }
        if let Some(expected) = check.get("json") {
            let actual: Value = match serde_json::from_str(&body) {
                Ok(value) => value,
                Err(_) => return Ok(false),
            };
            fn matches(actual: &Value, expected: &Value) -> bool {
                if let Some(map) = expected.as_object() {
                    map.iter().all(|(key, value)| {
                        actual.get(key).is_some_and(|actual| matches(actual, value))
                    })
                } else {
                    actual == expected
                }
            }
            if !matches(&actual, expected) {
                return Ok(false);
            }
        }
    }
    if let Some(command) = check.get("command")
        && project_command(
            project,
            &argv(command)?,
            docker,
            output,
            remaining.clamp(1, 5),
        )
        .is_err()
    {
        interrupted()?;
        return Ok(false);
    }
    Ok(true)
}
struct LogChild {
    child: Child,
    out: Option<thread::JoinHandle<()>>,
    err: Option<thread::JoinHandle<()>>,
}
impl Drop for LogChild {
    fn drop(&mut self) {
        terminate(&mut self.child);
        if let Some(out) = self.out.take() {
            let _ = out.join();
        }
        if let Some(err) = self.err.take() {
            let _ = err.join();
        }
    }
}
fn startup_logs(
    project: &Project,
    selected: &[String],
    docker: &Docker,
    output: &Output,
) -> Result<LogChild> {
    let file = write_render(project, "compose")?;
    let mut args = compose_args(project, &file)?;
    args.extend(strings(&["logs", "--follow", "--tail", "30", "--no-color"]));
    args.extend_from_slice(selected);
    let mut child = docker
        .command(&args)?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = streaming(child.stdout.take().unwrap(), output.clone(), "startup-log");
    let err = streaming(child.stderr.take().unwrap(), output.clone(), "startup-log");
    Ok(LogChild {
        child,
        out: Some(out),
        err: Some(err),
    })
}
fn wait_ready(
    project: &Project,
    selected: &[String],
    docker: &Docker,
    output: &Output,
    timeout: u64,
) -> Result<Value> {
    let started = Instant::now();
    let deadline = Duration::from_secs(timeout.max(1));
    let mut previous = Map::new();
    loop {
        interrupted()?;
        ensure!(
            started.elapsed() < deadline,
            "Readiness exceeded {timeout}s; containers remain running. Inspect dks status and dks logs"
        );
        let rows = container_rows_timeout(
            project,
            docker,
            timeout.saturating_sub(started.elapsed().as_secs()).max(1),
        )?;
        let mut ready = true;
        for service in selected {
            let (container_ready, description) = inspect_state(project, service, &rows)?;
            if previous.get(service).and_then(Value::as_str) != Some(&description) {
                output.event("readiness", &format!("{service}: {description}"))?;
                previous.insert(service.clone(), json!(description));
            }
            if !container_ready {
                ready = false;
                continue;
            }
            if !readiness_check(
                project,
                service,
                docker,
                output,
                timeout.saturating_sub(started.elapsed().as_secs()).max(1),
            )? {
                ready = false;
            }
        }
        if ready {
            return Ok(json!(selected.iter().map(|name| json!({"name":name,"status":if project.metadata["readiness"].get(name).is_some() {"application readiness verified"} else {previous[name].as_str().unwrap_or("unknown")},"containerReady":true,"applicationReady":project.metadata["readiness"].get(name).map(|_| true)})).collect::<Vec<_>>()));
        }
        thread::sleep(Duration::from_millis(500));
    }
}
fn journal(project: &Project, workflow: &str, phase: &str, selected: &[String]) -> Result<()> {
    state::save(
        &project.root,
        "operation",
        &json!({"workflow":workflow,"phase":phase,"services":selected,"time":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()}),
    )
}
pub fn lifecycle(
    project: &Project,
    workflow: &str,
    selected: &[String],
    plan_only: bool,
    confirmed: bool,
    timeout: u64,
    output: &Output,
) -> Result<Value> {
    ensure!(
        project.backend()? == "compose",
        "Use dks deploy for the Swarm backend"
    );
    ensure!(
        ["up", "dev", "down", "destroy"].contains(&workflow),
        "Unknown lifecycle {workflow}"
    );
    let selected_full = selected_services(project, selected)?;
    let planned = actions(project, workflow, selected, None)?;
    let plan = json!({"backend":"compose","project":project.name()?,"workflow":workflow,"services":selected_full,"actions":planned,"readiness":project.metadata["readiness"],"portAllocation":project.metadata["setup"]["ports"],"effects":if workflow == "destroy" {"remove owned containers/networks and managed volumes; retain secret references"} else if workflow == "down" {"stop/remove owned containers/networks; preserve volumes and secrets"} else {"allocate declared ports, build/start, run declared actions, verify readiness"},"endpoints":project.endpoints()});
    let mut plan = plan;
    plan["devLoop"] = project.metadata["dev"].clone();
    plan["operations"] = if workflow == "down" || workflow == "destroy" {
        json!([{"kind":"ownership-check"},{"kind":"remove-containers","scope":"recorded owner + project labels","services":selected},{"kind":"remove-networks","scope":"owned only","onlyFullProject":true},{"kind":"remove-volumes","enabled":workflow == "destroy","scope":"owned only","onlyFullProject":true}])
    } else {
        let mut command = compose_args(
            project,
            &project.root.join(".dockstride/render-compose.yaml"),
        )?;
        command.extend(strings(&["up", "--detach", "--build"]));
        command.extend_from_slice(selected);
        json!([{"kind":"check-docker-context-ownership-ports"},{"kind":"allocate-declared-ports","policy":project.metadata["setup"]["ports"]},{"kind":"actions-before","actions":actions(project,workflow,selected,Some("before"))?},{"kind":"docker","argv":command},{"kind":"wait-readiness","services":selected_full,"timeoutSeconds":timeout},{"kind":"actions-after","actions":actions(project,workflow,selected,Some("after"))?},{"kind":"compose-watch","enabled":workflow == "dev","requiresDeclaredWatch":true}])
    };
    if plan_only {
        return Ok(plan);
    }
    ensure!(
        workflow != "destroy" || confirmed,
        "Destruction requires explicit confirmation (--yes)"
    );
    let _lock = state::lock(&project.root, "lifecycle")?;
    let fresh = crate::nickel::evaluate(&project.root, None)?;
    ensure!(
        fresh.name()? == project.name()? && fresh.backend()? == project.backend()?,
        "Environment identity changed while waiting for lifecycle lock; rerun the command with current configuration"
    );
    let project = &fresh;
    let docker = Docker::new(&project.root, output.clone());
    output.event(
        "check",
        &format!("{} ({})", project.name()?, project.backend()?),
    )?;
    docker.check()?;
    validate_ownership(project, &docker, workflow == "up" || workflow == "dev")?;
    if workflow == "down" || workflow == "destroy" {
        return teardown(project, workflow == "destroy", selected, &docker, output);
    }
    let evaluated;
    let project = if allocate_ports_with_docker(project, &docker)? {
        evaluated = crate::nickel::evaluate(&project.root, None)?;
        &evaluated
    } else {
        project
    };
    detect_ports(project, selected, &docker)?;
    let selected_full = selected_services(project, selected)?;
    let file = write_render(project, "compose")?;
    journal(project, workflow, "starting", &selected_full)?;
    state::mark_resources(&project.root, true)?;
    for action in actions(project, workflow, selected, Some("before"))? {
        execute_action(project, action, &docker, output, timeout)?;
    }
    let mut up = compose_args(project, &file)?;
    up.extend(strings(&["up", "--detach", "--build"]));
    up.extend_from_slice(selected);
    if let Err(error) = docker.run_timeout(&up, None, timeout) {
        if !error.is::<Cancelled>() {
            let mut logs = compose_args(project, &file)?;
            logs.extend(strings(&["logs", "--tail", "30", "--no-color"]));
            logs.extend_from_slice(&selected_full);
            let _ = docker.run_timeout(&logs, None, 10);
            let _ = journal(project, workflow, "failed", &selected_full);
        }
        return Err(error.context("Compose startup failed; one-shot prerequisites must exit successfully. Detached containers may remain"));
    }
    journal(project, workflow, "waiting", &selected_full)?;
    let logs = startup_logs(project, &selected_full, &docker, output)?;
    let readiness = wait_ready(project, &selected_full, &docker, output, timeout)?;
    drop(logs);
    for action in actions(project, workflow, selected, Some("after"))? {
        execute_action(project, action, &docker, output, timeout)?;
    }
    journal(project, workflow, "ready", &selected_full)?;
    let result = json!({"project":project.name()?,"services":readiness,"endpoints":project.endpoints(),"detached":true});
    output.event(
        "ready",
        &format!(
            "{} started. Endpoints: {}",
            project.name()?,
            project.endpoints()
        ),
    )?;
    if workflow == "dev" {
        if let Some(command) = project.metadata["dev"].get("argv") {
            output.event("dev","Starting declared development command; Ctrl-C stops the command, not detached containers")?;
            project_command(project, &argv(command)?, &docker, output, 0)?;
        } else {
            let services = project.services()?;
            let watched: Vec<String> = selected_full
                .iter()
                .filter(|name| {
                    services[*name]["develop"]["watch"]
                        .as_array()
                        .is_some_and(|w| !w.is_empty())
                })
                .cloned()
                .collect();
            if watched.is_empty() {
                output.event("dev","No Compose watch declared; containers remain running. Bind mounts work natively; automatic synchronization is not enabled.")?;
            } else {
                output.event(
                    "dev",
                    "Starting Compose watch; Ctrl-C stops watch, not detached containers",
                )?;
                let mut args = compose_args(project, &file)?;
                args.extend(strings(&["watch", "--no-up"]));
                args.extend(watched);
                docker.run(&args, None)?;
            }
        }
    }
    Ok(result)
}
fn teardown(
    project: &Project,
    destroy: bool,
    selected: &[String],
    docker: &Docker,
    output: &Output,
) -> Result<Value> {
    let id = validate_ownership(project, docker, false)?;
    let rows = container_rows(project, docker)?;
    let names: HashSet<&str> = selected.iter().map(String::as_str).collect();
    let mut removed = Vec::new();
    for row in rows {
        let service = row["Config"]["Labels"]["com.docker.compose.service"]
            .as_str()
            .unwrap_or("");
        if !names.is_empty() && !names.contains(service) {
            continue;
        }
        ensure!(
            row["Config"]["Labels"][OWNER].as_str() == Some(&id),
            "Container ownership changed; refusing removal"
        );
        let resource = row["Id"].as_str().context("Missing container id")?;
        docker.run(&strings(&["container", "rm", "--force", resource]), None)?;
        removed.push(resource.to_owned());
    }
    if selected.is_empty() {
        for kind in if destroy {
            vec!["network", "volume"]
        } else {
            vec!["network"]
        } {
            let resources = docker.capture(
                &strings(&[
                    kind,
                    "ls",
                    "-q",
                    "--filter",
                    &format!("label={OWNER}={id}"),
                    "--filter",
                    &format!("label={PROJECT}={}", project.name()?),
                ]),
                None,
            )?;
            for resource in resources.split_whitespace() {
                // No force: attached external/other-project resources must survive.
                docker.run(&strings(&[kind, "rm", resource]), None)?;
                removed.push(resource.into());
            }
        }
        // Down deliberately retains data and identity; destroying volumes still
        // does not transfer authority over persistent secret revisions.
        if destroy && project.env["secrets"].as_object().is_none_or(Map::is_empty) {
            state::mark_resources(&project.root, false)?;
        }
    }
    journal(
        project,
        if destroy { "destroy" } else { "down" },
        "complete",
        selected,
    )?;
    output.event("teardown",if destroy {"Removed owned containers/networks/volumes; secret references and allocated endpoints preserved"} else {"Removed owned containers/networks; volumes, secrets, and allocated endpoints preserved"})?;
    Ok(json!({"removed":removed,"volumesPreserved":!destroy,"secretsPreserved":true}))
}
pub fn inspect(project: &Project, output: &Output) -> Result<Value> {
    let docker = Docker::new(&project.root, output.clone());
    check_ownership(project, &docker, true)?;
    let rows = container_rows(project, &docker)?;
    let mut services = Vec::new();
    for name in project.services()?.keys() {
        let state = match inspect_state(project, name, &rows) {
            Ok((ready, status)) => {
                json!({"name":name,"containerReady":ready,"applicationReady":null,"status":status})
            }
            Err(error) => {
                json!({"name":name,"containerReady":false,"applicationReady":null,"status":"failed","error":error.to_string()})
            }
        };
        services.push(state);
    }
    Ok(
        json!({"project":project.name()?,"context":docker.context()?,"services":services,"endpoints":project.endpoints(),"operation":state::read(&project.root,"operation")?}),
    )
}
pub fn passthrough(
    project: &Project,
    namespace: &str,
    args: &[String],
    output: &Output,
) -> Result<Value> {
    let docker = Docker::new(&project.root, output.clone());
    let mut command = match namespace {
        "docker" => Vec::new(),
        "stack" => strings(&["stack"]),
        "compose" | "logs" | "exec" => {
            validate_ownership(project, &docker, true)?;
            let file = write_render(project, "compose")?;
            compose_args(project, &file)?
        }
        _ => bail!("Passthrough must explicitly select docker, compose, or stack"),
    };
    if namespace == "stack"
        && args.len() == 1
        && ["services", "ps", "rm"].contains(&args[0].as_str())
    {
        command.extend_from_slice(args);
        command.push(project.name()?.into());
        docker.native(&command, None)?;
        return Ok(json!({"exitCode":0}));
    }
    if namespace == "logs" || namespace == "exec" {
        command.push(namespace.into());
    }
    command.extend_from_slice(args);
    ensure!(!command.is_empty(), "Docker passthrough requires arguments");
    docker.native(&command, None)?;
    Ok(json!({"exitCode":0}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn project(root: &Path, services: Value, metadata: Value) -> Project {
        Project {
            root: root.into(),
            env: json!({"project":"fixture","backend":"compose"}),
            model: json!({"services":services}),
            metadata,
            fields: Vec::new(),
        }
    }
    fn container(service: &str, state: Value) -> Value {
        json!({"Config":{"Labels":{"com.docker.compose.service":service,"com.docker.compose.oneoff":"False"}},"State":state})
    }
    #[test]
    fn successful_prerequisite_is_not_a_crashed_application() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(
            dir.path(),
            json!({"db":{"image":"postgres"},"migrate":{"image":"migration"},"api":{"depends_on":{"migrate":{"condition":"service_completed_successfully"}}}}),
            json!({}),
        );
        let row = container("migrate", json!({"Status":"exited","ExitCode":0}));
        assert_eq!(
            inspect_state(&project, "migrate", &[row]).unwrap(),
            (true, "completed successfully".into())
        );
        let failed = container("migrate", json!({"Status":"exited","ExitCode":7}));
        let error = inspect_state(&project, "migrate", &[failed])
            .unwrap_err()
            .to_string();
        assert!(error.contains("migrate") && error.contains("7"));
        let crashed = container("api", json!({"Status":"exited","ExitCode":0}));
        assert!(inspect_state(&project, "api", &[crashed]).is_err());
    }
    #[test]
    fn health_is_never_invented_and_declared_checks_are_honored() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(
            dir.path(),
            json!({"plain":{},"checked":{"healthcheck":{"test":["CMD","true"]}}}),
            json!({}),
        );
        let state = json!({"Status":"running","Running":true});
        let plain = inspect_state(&project, "plain", &[container("plain", state.clone())]).unwrap();
        assert!(plain.0 && plain.1.contains("no healthcheck"));
        assert!(
            !inspect_state(&project, "checked", &[container("checked", state.clone())])
                .unwrap()
                .0
        );
        let mut healthy = state.clone();
        healthy["Health"] = json!({"Status":"healthy"});
        assert!(
            inspect_state(&project, "checked", &[container("checked", healthy)])
                .unwrap()
                .0
        );
        let mut unhealthy = state;
        unhealthy["Health"] = json!({"Status":"unhealthy"});
        assert!(inspect_state(&project, "checked", &[container("checked", unhealthy)]).is_err());
    }
    #[test]
    fn plans_and_refused_destructive_operations_leave_no_state() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(
            dir.path(),
            json!({"api":{"build":"."}}),
            json!({"actions":[{"name":"seed","kind":"command","argv":["false"],"workflows":["dev"]}],"setup":{"ports":{"apiPort":{"service":"api","target":8000}}}}),
        );
        let plan = lifecycle(&project, "dev", &[], true, false, 1, &Output::default()).unwrap();
        assert_eq!(plan["actions"][0]["name"], "seed");
        assert_eq!(plan["operations"][3]["kind"], "docker");
        assert!(!dir.path().join(".dockstride").exists());
        let error = lifecycle(
            &project,
            "destroy",
            &[],
            false,
            false,
            1,
            &Output::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("confirmation"));
        assert!(!dir.path().join(".dockstride").exists());
    }
    #[test]
    fn service_selection_includes_prerequisites_not_unrelated_services() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project(
            dir.path(),
            json!({"api":{"depends_on":{"db":{"condition":"service_healthy"}}},"db":{},"worker":{}}),
            json!({"actions":[{"name":"api-seed","kind":"command","argv":["true"],"workflows":["dev"],"services":["api"]},{"name":"worker-seed","kind":"command","argv":["false"],"workflows":["dev"],"services":["worker"]}]}),
        );
        assert_eq!(
            selected_services(&project, &["api".into()]).unwrap(),
            vec!["api", "db"]
        );
        assert_eq!(
            actions(&project, "dev", &["api".into()], None)
                .unwrap()
                .iter()
                .map(|a| a["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["api-seed"]
        );
        project.model["services"]["db"]["depends_on"] = json!(["api"]);
        assert!(selected_services(&project, &["api".into()]).is_err());
    }
    #[test]
    fn render_labels_only_owned_objects_and_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = project(
            dir.path(),
            json!({"api":{"image":"image","labels":["application=yes"]}}),
            json!({}),
        );
        project.model["volumes"] = json!({"data":{},"shared":{"external":true}});
        let raw = render_document(&project, "compose").unwrap();
        assert_eq!(raw["services"]["api"]["labels"], json!(["application=yes"]));
        assert!(!dir.path().join(".dockstride").exists());
        let identity =
            state::ensure_identity(dir.path(), "fixture", "compose", "test-context").unwrap();
        let rendered = render_document(&project, "compose").unwrap();
        assert_eq!(rendered["services"]["api"]["labels"][OWNER], identity["id"]);
        assert_eq!(rendered["services"]["api"]["labels"]["application"], "yes");
        assert_eq!(rendered["volumes"]["data"]["labels"][OWNER], identity["id"]);
        assert!(rendered["volumes"]["shared"].get("labels").is_none());
        assert!(!dir.path().join(".dockstride/render-compose.yaml").exists());
    }
    #[test]
    fn published_port_ranges_and_ipv6_are_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(
            dir.path(),
            json!({"api":{"ports":["[::1]:8080:80","9000-9001:9000-9001/udp",{"target":8000,"published":"8081","host_ip":"127.0.0.1"}]}}),
            json!({}),
        );
        let ports = published_ports(&project, &[]).unwrap();
        assert_eq!(
            ports
                .iter()
                .map(|p| (p.host.as_str(), p.published, p.protocol.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("::1", 8080, "tcp"),
                ("0.0.0.0", 9000, "udp"),
                ("0.0.0.0", 9001, "udp"),
                ("127.0.0.1", 8081, "tcp")
            ]
        );
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    #[test]
    fn an_http_success_from_the_wrong_application_is_not_ready() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for body in [
                r#"{"application":"unrelated","ready":true}"#,
                r#"{"application":"fixture","ready":true}"#,
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut socket);
                let mut line = String::new();
                loop {
                    line.clear();
                    assert!(
                        reader.read_line(&mut line).unwrap() > 0,
                        "incomplete HTTP headers"
                    );
                    if line == "\r\n" {
                        break;
                    }
                }
                drop(reader);
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let project = Project {
            root: dir.path().into(),
            env: json!({"project":"fixture"}),
            model: json!({"services":{"api":{}}}),
            metadata: json!({"readiness":{"api":{"url":format!("http://{address}/ready"),"json":{"application":"fixture","ready":true}}}}),
            fields: Vec::new(),
        };
        let docker = Docker::new(&project.root, Output::default());
        assert!(!readiness_check(&project, "api", &docker, &Output::default(), 5).unwrap());
        assert!(readiness_check(&project, "api", &docker, &Output::default(), 5).unwrap());
        server.join().unwrap();
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn default_context_changes_cannot_redirect_a_checked_operation() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("docker");
        fs::write(&executable,r#"#!/bin/sh
context=""
if [ "$1" = "--context" ]; then context="$2"; shift 2; fi
if [ -z "$context" ]; then context=$(cat "$DOCKER_CONFIG/current"); fi
case "$1 $2" in
  "context show") cat "$DOCKER_CONFIG/current" ;;
  "context inspect") printf 'unix:///%s.sock\n' "$3" ;;
  "version --format") printf 'beta\n' > "$DOCKER_CONFIG/current"; printf '{"Client":{},"Server":{}}\n' ;;
  "compose version") printf '2.30.0\n' ;;
  "container rm") rm "$DOCKER_CONFIG/$context-owned"; printf '%s\n' "$context" ;;
  "compose exec") printf 'prompt> '; IFS= read -r line; printf 'received:%s\n' "$line" ;;
  *) exit 42 ;;
esac
"#).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(dir.path().join("current"), "alpha\n").unwrap();
        fs::write(dir.path().join("alpha-owned"), "owned").unwrap();
        fs::write(dir.path().join("beta-owned"), "unrelated").unwrap();
        let path = format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        // A subprocess provides isolated environment overrides; concurrent tests
        // never mutate this test runner's PATH or Docker connection variables.
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::connection_tests::pinned_default_context_child",
                "--ignored",
            ])
            .env("PATH", path)
            .env("DOCKER_CONFIG", dir.path())
            .env("DKS_CONTEXT_TEST_ROOT", dir.path())
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_HOST")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(String::from_utf8_lossy(&child.stdout).contains("prompt> received:hello\n"));
        assert!(!String::from_utf8_lossy(&child.stderr).contains("received:hello"));
        assert!(!dir.path().join("alpha-owned").exists());
        assert!(dir.path().join("beta-owned").exists());
    }
    #[test]
    #[ignore = "isolated helper invoked by default_context_changes_cannot_redirect_a_checked_operation"]
    fn pinned_default_context_child() {
        let root = PathBuf::from(std::env::var_os("DKS_CONTEXT_TEST_ROOT").unwrap());
        let docker = Docker::new(&root, Output::default());
        let checked = docker.check().unwrap();
        assert_eq!(checked["context"], "alpha;unix:///alpha.sock");
        assert_eq!(fs::read_to_string(root.join("current")).unwrap(), "beta\n");
        docker
            .capture(&strings(&["container", "rm", "owned"]), None)
            .unwrap();
        docker
            .native(
                &strings(&["compose", "exec", "api", "sh"]),
                Some(b"hello\n"),
            )
            .unwrap();
        assert_eq!(docker.context().unwrap(), "alpha;unix:///alpha.sock");
    }
}
