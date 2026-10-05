//! Docker CLI execution and explicit, ownership-scoped Compose workflows.
use crate::{model::Project, output::Output, state};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
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
static MANAGED_INVOCATION: AtomicBool = AtomicBool::new(false);
static INVOCATION_DOCKER: LazyLock<Arc<DockerEnvironment>> = LazyLock::new(|| Arc::new(DockerEnvironment {
    values: DOCKER_ENV.map(std::env::var_os),
    connection: OnceLock::new(),
}));

/// Pin one CLI invocation without contacting Docker or forcing configuration.
pub fn pin_invocation() {
    LazyLock::force(&INVOCATION_DOCKER);
    MANAGED_INVOCATION.store(true, Ordering::Relaxed);
}

pub(crate) fn managed_invocation() -> bool {
    MANAGED_INVOCATION.load(Ordering::Relaxed)
}
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

#[derive(Debug)]
pub(crate) struct DeadlineExceeded;
impl std::fmt::Display for DeadlineExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Command deadline exceeded; stopped consumers remain stopped and completed operations remain")
    }
}
impl std::error::Error for DeadlineExceeded {}

/// A service exit is an observed prerequisite failure, not a Docker CLI error.
#[derive(Debug)]
pub struct PrerequisiteFailed(pub Value);
impl std::fmt::Display for PrerequisiteFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Prerequisite {} failed with observed exit code {}; inspect dks logs {}",
            self.0["service"].as_str().unwrap_or("unknown"),
            self.0["exitCode"],
            self.0["service"].as_str().unwrap_or("unknown"))
    }
}
impl std::error::Error for PrerequisiteFailed {}
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
pub(crate) fn interrupted() -> Result<()> {
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
pub(crate) fn tail_bytes<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
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
pub(crate) fn streaming<R: Read + Send + 'static>(
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
    // Resolve once against construction-time environment and share across a CLI
    // invocation; low-level library configuration does not imply managed setup.
    connection: OnceLock<Connection>,
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
    deadline: Option<Instant>,
    profiles: Option<Arc<Vec<String>>>,
}
impl Docker {
    pub fn new(root: &Path, output: Output) -> Self {
        Self {
            root: root.to_path_buf(),
            output,
            deadline: None,
            profiles: None,
            environment: if managed_invocation() {
                Arc::clone(&INVOCATION_DOCKER)
            } else {
                Arc::new(DockerEnvironment {
                    values: DOCKER_ENV.map(std::env::var_os),
                    connection: OnceLock::new(),
                })
            },
        }
    }
    pub(crate) fn with_deadline(&self, deadline: Instant) -> Self {
        let mut docker = self.clone();
        docker.deadline = Some(self.deadline.map_or(deadline, |existing| existing.min(deadline)));
        docker
    }
    /// Diagnostics have one fresh, bounded phase on the captured connection,
    /// even when the lifecycle phase exhausted its own deadline.
    pub(crate) fn for_diagnostics(&self, timeout: u64) -> Self {
        let mut docker = self.clone();
        docker.deadline = Some(Instant::now() + Duration::from_secs(timeout.min(10)));
        docker
    }
    pub(crate) fn remaining(&self, limit: Duration) -> Result<Duration> {
        interrupted()?;
        let remaining = self.deadline.map_or(limit, |deadline| {
            limit.min(deadline.saturating_duration_since(Instant::now()))
        });
        if remaining.is_zero() { return Err(DeadlineExceeded.into()); }
        Ok(remaining)
    }
    fn with_profiles(&self, profiles: &[String]) -> Self {
        let mut docker = self.clone();
        docker.profiles = Some(Arc::new(profiles.to_vec()));
        docker
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
        if let Some(connection) = self.environment.connection.get() {
            return Ok(connection);
        }
        // Cache only successful resolution: retain typed Docker failures and
        // never poison a later observation with a shorter operation's deadline.
        let connection = self.resolve_connection()?;
        let _ = self.environment.connection.set(connection);
        Ok(self.environment.connection.get().unwrap())
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
        if args.first().is_some_and(|arg| arg == "compose")
            && let Some(profiles) = &self.profiles
        {
            command.env("COMPOSE_PROFILES", profiles.join(",")).arg("compose");
            for profile in profiles.iter() {
                command.args(["--profile", profile]);
            }
            command.args(&args[1..]);
        } else {
            command.args(args);
        }
        Ok(command)
    }
    pub(crate) fn inherit_connection(&self, command: &mut Command) -> Result<()> {
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
        self.remaining(Duration::from_secs(timeout.max(1)))?;
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
        self.remaining(Duration::from_secs(timeout.max(1)))?;
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
    pub(crate) fn wait(&self, child: &mut Child, timeout: u64) -> Result<std::process::ExitStatus> {
        self.wait_mode(child, timeout, true)
    }
    fn wait_mode(
        &self,
        child: &mut Child,
        timeout: u64,
        isolated: bool,
    ) -> Result<std::process::ExitStatus> {
        let deadline = if timeout == 0 {
            self.deadline
        } else {
            let local = Instant::now() + Duration::from_secs(timeout);
            Some(self.deadline.map_or(local, |deadline| deadline.min(local)))
        };
        let flag = cancellation()?;
        loop {
            if flag.load(Ordering::Relaxed) {
                terminate_mode(child, isolated);
                interrupted()?;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                // At the absolute deadline there is no remaining grace budget.
                let target = if isolated { -(child.id() as i32) } else { child.id() as i32 };
                unsafe { libc::kill(target, libc::SIGKILL); }
                let _ = child.wait();
                return Err(DeadlineExceeded.into());
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
            let pause = deadline.map_or(Duration::from_millis(50), |deadline| {
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now()))
            });
            thread::sleep(pause);
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
    check_ownership_with_identity(project, docker, creating, None)
}

/// Verify proposed ownership without publishing or temporarily changing identity.
pub(crate) fn check_registry_resources(project: &Project, docker: &Docker, owner: &str) -> Result<Value> {
    let identity = json!({"id":owner,"project":project.name()?,"backend":project.backend()?,
        "context":docker.context()?,"root":fs::canonicalize(&project.root)?,"resources":false});
    check_ownership_with_identity(project, docker, true, Some(&identity))
}

fn check_ownership_with_identity(project: &Project, docker: &Docker, creating: bool, proposed: Option<&Value>) -> Result<Value> {
    let context = docker.context()?;
    let root = fs::canonicalize(&project.root)?
        .to_string_lossy()
        .into_owned();
    let mut identity = match proposed {
        Some(identity) => identity.clone(),
        None => state::read(&project.root, "identity")?,
    };
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
    let name = project.name()?;
    let swarm = project.backend()? == "swarm";
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
        let filters: &[&str] = if swarm {
            &["com.docker.compose.project", "com.docker.stack.namespace", PROJECT]
        } else {
            &[filter]
        };
        let found = namespace_resources(docker, &listing, name, filters)?;
        for resource in &found {
            let labels = docker.capture(
                &strings(&[
                    kind,
                    "inspect",
                    resource.as_str(),
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
            let mut owned = labels[OWNER].as_str() == Some(&id);
            if !owned && swarm && kind == "container" && labels.get(OWNER).is_none()
                && let (Some(service_id), Some(task_id)) = (
                    labels["com.docker.swarm.service.id"].as_str(),
                    labels["com.docker.swarm.task.id"].as_str(),
                )
            {
                let service: Value = serde_json::from_str(&docker.capture(
                    &strings(&["service", "inspect", service_id]), None,
                )?)?;
                let task: Value = serde_json::from_str(&docker.capture(
                    &strings(&["inspect", "--type", "task", task_id]), None,
                )?)?;
                let container_id = docker.capture(
                    &strings(&["container", "inspect", resource, "--format", "{{.Id}}"]), None,
                )?;
                owned = service[0]["ID"].as_str() == Some(service_id)
                    && service[0]["Spec"]["Labels"][OWNER].as_str() == Some(&id)
                    && service[0]["Spec"]["Labels"][PROJECT].as_str() == Some(name)
                    && task[0]["ID"].as_str() == Some(task_id)
                    && task[0]["ServiceID"].as_str() == Some(service_id)
                    && task[0]["Status"]["ContainerStatus"]["ContainerID"].as_str()
                        == Some(container_id.trim());
            }
            ensure!(
                owned,
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
    if swarm {
        let info = docker.capture(
            &strings(&["info", "--format", "{{.Swarm.LocalNodeState}}"]),
            None,
        )?;
        ensure!(
            info.trim() == "active",
            "Selected Docker context is not an active Swarm"
        );
        for kind in ["service", "secret", "config"] {
            let resources = namespace_resources(docker, &[kind, "ls", "-q"], name,
                &["com.docker.stack.namespace", PROJECT])?;
            for resource in &resources {
                let labels: Value = serde_json::from_str(&docker.capture(
                    &strings(&[kind, "inspect", resource, "--format", "{{json .Spec.Labels}}"]),
                    None,
                )?)?;
                ensure!(labels[OWNER].as_str() == Some(&id),
                    "Unrelated Swarm {kind} {resource} occupies stack {name}");
            }
        }
    }
    Ok(identity)
}
/// The invoking checkout lifecycle lock must already be held.
pub(crate) fn recover_publication(root: &Path) -> Result<()> {
    let _global = state::global_lock()?;
    let _config = state::lock(root, "config")?;
    crate::publication::recover_locked(root)?;
    Ok(())
}

/// The invoking checkout lifecycle lock must already be held.
pub fn validate_ownership(project: &Project, docker: &Docker, creating: bool) -> Result<String> {
    let _global = state::global_lock()?;
    let _config = state::lock(&project.root, "config")?;
    crate::publication::recover_locked(&project.root)?;
    let snapshot = crate::sources::snapshot(&project.root, None)?;
    let _sources = crate::sources::lock_paths(snapshot.fingerprints.keys().cloned())?;
    snapshot.verify()?;
    if !creating {
        ensure!(state::read(&project.root, "identity")?["id"].as_str().is_some(),
            "No Dockstride ownership identity: refusing to operate on resources by project name alone");
    }
    let registration = crate::registry::prepare_with_docker(project, &snapshot.sources, docker, None)?;
    snapshot.verify()?;
    crate::publication::publish(&project.root, "register-environment", registration.changes, registration.claims)?;
    Ok(registration.owner_id)
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
        if let Some(resources) = model.get_mut(field).and_then(Value::as_object_mut) {
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

struct Dependency<'a> {
    service: &'a str,
    condition: &'a str,
    required: bool,
    restart: bool,
}
fn dependencies(service: &Value) -> Result<Vec<Dependency<'_>>> {
    let Some(depends) = service.get("depends_on") else { return Ok(Vec::new()) };
    if let Some(list) = depends.as_array() {
        return list.iter().map(|name| Ok(Dependency {
            service: name.as_str().context("depends_on entries must be strings")?,
            condition: "service_started", required: true, restart: false,
        })).collect();
    }
    depends.as_object().context("depends_on must be a mapping or list")?.iter().map(|(name, value)| {
        ensure!(value.is_object(), "Dependency {name} must be an object");
        for field in ["required", "restart"] {
            ensure!(value.get(field).is_none_or(Value::is_boolean), "Dependency {name} {field} must be boolean");
        }
        let condition = value.get("condition").map_or(Ok("service_started"), |v| v.as_str().context("Dependency condition must be a string"))?;
        ensure!(["service_started", "service_healthy", "service_completed_successfully"].contains(&condition), "Unsupported dependency condition {condition}");
        Ok(Dependency { service: name, condition, required: value["required"] != false, restart: value["restart"] == true })
    }).collect()
}
fn dependency_order(project: &Project, scope: &[String]) -> Result<Vec<String>> {
    dependency_order_scoped(project, scope, scope)
}
fn dependency_order_scoped(project: &Project, roots: &[String], optional_scope: &[String]) -> Result<Vec<String>> {
    fn visit(name: &str, services: &Map<String, Value>, scope: &HashSet<&str>, visiting: &mut HashSet<String>, done: &mut HashSet<String>, ordered: &mut Vec<String>) -> Result<()> {
        ensure!(services.contains_key(name), "Unknown service {name}");
        if done.contains(name) { return Ok(()) }
        ensure!(visiting.insert(name.into()), "Dependency cycle involving {name}");
        for dependency in dependencies(&services[name])? {
            if dependency.required || scope.contains(dependency.service) {
                visit(dependency.service, services, scope, visiting, done, ordered)?;
            }
        }
        visiting.remove(name);
        done.insert(name.into());
        ordered.push(name.into());
        Ok(())
    }
    let mut ordered = Vec::new();
    let mut done = HashSet::new();
    let scope_set = optional_scope.iter().map(String::as_str).collect();
    for name in roots {
        visit(name, project.services()?, &scope_set, &mut HashSet::new(), &mut done, &mut ordered)
            .context("Invalid Compose dependency configuration")?;
    }
    Ok(ordered)
}
fn valid_profile(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}
fn active_profiles(project: &Project, flags: &[String], saved_fallback: bool) -> Result<Vec<String>> {
    let environment = std::env::var_os("COMPOSE_PROFILES");
    let mut profiles: BTreeSet<String> = flags.iter().cloned().collect();
    if let Some(value) = &environment {
        let value = value.to_str().context("COMPOSE_PROFILES must be UTF-8")?;
        profiles.extend(value.split(',').map(str::trim).filter(|name| !name.is_empty()).map(str::to_owned));
    } else if flags.is_empty() && saved_fallback {
        let operation = state::read(&project.root, "operation")?;
        if let Some(saved) = operation.get("appliedProfiles") {
            profiles.extend(argv(saved).context("Invalid recorded active profiles")?);
        }
    }
    for name in &profiles {
        ensure!(name == "*" || valid_profile(name), "Invalid Compose profile {name:?}");
    }
    Ok(profiles.into_iter().collect())
}
fn selected_services_with_profiles(project: &Project, selected: &[String], profiles: &[String]) -> Result<Vec<String>> {
    let services = project.services()?;
    for (name, service) in services {
        if let Some(value) = service.get("profiles") {
            let declared = value.as_array().with_context(|| format!("Service {name} profiles must be a string list"))?;
            for profile in declared {
                ensure!(profile.as_str().is_some_and(valid_profile), "Service {name} has invalid profile {profile}");
            }
        }
    }
    // Validate every declared edge, including inactive services, before mutation.
    let all: Vec<_> = services.keys().cloned().collect();
    dependency_order_scoped(project, &all, &all)?;
    let initial: Vec<String> = if selected.is_empty() {
        services.iter().filter(|(_, service)| {
            service["profiles"].as_array().is_none_or(|declared| {
                declared.is_empty() || profiles.iter().any(|active| active == "*"
                    || declared.iter().any(|profile| profile.as_str() == Some(active.as_str())))
            })
        }).map(|(name, _)| name.clone()).collect()
    } else { selected.to_vec() };
    let mut scope = dependency_order(project, &initial)?;
    // Optional edges only participate when both endpoints are already required.
    dependency_order(project, &scope)?;
    scope.sort();
    Ok(scope)
}
fn selected_services(project: &Project, selected: &[String]) -> Result<Vec<String>> {
    selected_services_with_profiles(project, selected, &active_profiles(project, &[], false)?)
}
fn applicable(action: &Value, workflow: &str, selected: &[String]) -> bool {
    let workflows = action["workflows"].as_array();
    let services = action["services"].as_array();
    workflows
        .is_none_or(|list| list.is_empty() || list.iter().any(|v| v.as_str() == Some(workflow)))
        && services.is_none_or(|list| {
            list.is_empty()
                || list.iter().any(|v| v.as_str().is_some_and(|s| selected.iter().any(|n| n == s)))
        })
}
/// Validate declarations without side effects and select actions against the
/// required Compose dependency scope (Swarm retains explicit selection).
pub fn planned_actions<'a>(project: &'a Project, workflow: &str, selected: &[String]) -> Result<Vec<&'a Value>> {
    validate_actions(project, workflow, selected).context("Invalid workflow configuration")
}
fn validate_actions<'a>(project: &'a Project, workflow: &str, selected: &[String]) -> Result<Vec<&'a Value>> {
    let compose = project.backend()? == "compose";
    let scope = if compose {
        // Validate even inactive portions before any action can stop consumers.
        let all: Vec<_> = project.services()?.keys().cloned().collect();
        dependency_order_scoped(project, &all, &[])?;
        selected_services(project, selected)?
    } else {
        for name in selected { ensure!(project.services()?.contains_key(name), "Unknown service {name}"); }
        selected.to_vec()
    };
    let Some(value) = project.metadata.get("actions") else { return Ok(Vec::new()) };
    let list = value.as_array().context("dockstride.actions must be an ordered array")?;
    let applies = |action: &Value| {
        if !compose && workflow == "deploy" && !action["workflows"].as_array().is_some_and(|list| list.iter().any(|v| v.as_str() == Some("deploy"))) {
            return false;
        }
        if compose || !selected.is_empty() { applicable(action, workflow, &scope) }
        else {
            action["workflows"].as_array().is_none_or(|list| list.is_empty() || list.iter().any(|v| v.as_str() == Some(workflow)))
        }
    };
    let mut names = HashSet::new();
    let mut prerequisites = HashSet::new();
    for action in list {
        let name = action["name"].as_str().filter(|s| !s.is_empty()).context("Every action needs a nonempty name")?;
        ensure!(names.insert(name), "Duplicate action name {name}");
        let kind = action["kind"].as_str().context("Action needs kind")?;
        ensure!(["up", "run", "exec", "command", "stop", "prerequisite"].contains(&kind), "Unsupported action kind {kind}");
        let stage = action.get("stage").map_or(Ok("before"), |v| v.as_str().context("Action stage must be a string"))?;
        ensure!(["before", "after"].contains(&stage), "Action stage must be before or after");
        for field in ["workflows", "services"] {
            if let Some(value) = action.get(field) {
                ensure!(value.as_array().is_some_and(|list| list.iter().all(Value::is_string)), "Action {field} must be a string list");
                if field == "services" {
                    for name in value.as_array().unwrap() {
                        ensure!(project.services()?.contains_key(name.as_str().unwrap()), "Action applicability references unknown service {name}");
                    }
                }
            }
        }
        if let Some(timeout) = action.get("timeout") {
            ensure!(timeout.as_u64().is_some_and(|n| n > 0), "Action timeout must be a positive integer");
        }
        if ["command", "exec", "run"].contains(&kind) && (kind != "run" || action.get("argv").is_some()) {
            ensure!(!argv(&action["argv"])?.is_empty(), "Action argv cannot be empty");
        }
        if kind == "stop" {
            let targets = argv(&action["targets"]).context("Stop action requires targets")?;
            ensure!(!targets.is_empty(), "Stop action targets cannot be empty");
            for target in targets { ensure!(project.services()?.contains_key(&target), "Stop action references unknown service {target}"); }
        } else if kind != "command" {
            let service = action["service"].as_str().context("Compose actions require service")?;
            ensure!(project.services()?.contains_key(service), "Action references unknown service {service}");
            if kind == "prerequisite" {
                ensure!(stage == "before", "Prerequisite action must use stage before");
                ensure!(action["fresh"].is_boolean(), "Prerequisite action requires explicit boolean fresh");
                ensure!(one_shot(project, service)?, "Prerequisite service {service} must be a declared one-shot");
                if applies(action) {
                    ensure!(prerequisites.insert(service), "Duplicate prerequisite for service {service}");
                }
            }
        }
    }
    let planned: Vec<_> = list.iter().filter(|action| applies(action)).collect();
    for action in &planned {
        ensure!(compose || action["kind"] == "command", "Action {}: Compose {} is not supported on Swarm; use an explicit project argv command", action["name"], action["kind"]);
    }
    if compose && !prerequisites.is_empty() {
        validate_prerequisite_order(project, &planned, &scope)?;
    }
    Ok(planned)
}
fn actions<'a>(project: &'a Project, workflow: &str, selected: &[String], stage: Option<&str>) -> Result<Vec<&'a Value>> {
    Ok(planned_actions(project, workflow, selected)?.into_iter().filter(|a| stage.is_none_or(|stage| a["stage"].as_str().unwrap_or("before") == stage)).collect())
}
fn validate_prerequisite_order(project: &Project, planned: &[&Value], scope: &[String]) -> Result<()> {
    let prerequisites: HashSet<_> = planned.iter().filter(|a| a["kind"] == "prerequisite").filter_map(|a| a["service"].as_str()).collect();
    let mut completed = HashSet::new();
    let before = planned.iter().filter(|a| a["stage"].as_str().unwrap_or("before") == "before");
    let after = planned.iter().filter(|a| a["stage"] == "after");
    for action in before.chain(after) {
        if action["kind"] == "up" || action["kind"] == "prerequisite" {
            let service = action["service"].as_str().context("Action requires service")?;
            for dependency in dependency_order_scoped(project, &[service.into()], scope)? {
                if prerequisites.contains(dependency.as_str())
                    && !(action["kind"] == "prerequisite" && dependency == service)
                {
                    ensure!(completed.contains(dependency.as_str()), "Prerequisite {dependency} must precede action {}", action["name"]);
                }
            }
            if action["kind"] == "prerequisite" { completed.insert(service); }
        }
    }
    Ok(())
}
fn restart_consumers(project: &Project, roots: &BTreeSet<String>, running: &BTreeSet<String>) -> Result<BTreeSet<String>> {
    let mut affected = roots.clone();
    loop {
        let mut changed = false;
        for (name, definition) in project.services()? {
            if running.contains(name) && !affected.contains(name)
                && dependencies(definition)?.iter().any(|dep| dep.restart && affected.contains(dep.service))
            {
                changed |= affected.insert(name.clone());
            }
        }
        if !changed { break }
    }
    Ok(affected.intersection(running).cloned().collect())
}
fn running_services(rows: &[Value]) -> BTreeSet<String> {
    rows.iter().filter(|row| row["State"]["Running"] == true
        && row["Config"]["Labels"]["com.docker.compose.oneoff"].as_str() != Some("True"))
        .filter_map(|row| row["Config"]["Labels"]["com.docker.compose.service"].as_str().map(str::to_owned))
        .collect()
}
fn preflight_native_scope(project: &Project, planned: &[&Value], scope: &[String], docker: &Docker) -> Result<()> {
    let mut affected = BTreeSet::new();
    for action in planned.iter().filter(|a| a["stage"].as_str().unwrap_or("before") == "before") {
        if action["kind"] == "stop" { affected.extend(argv(&action["targets"])?); }
        if action["kind"] == "prerequisite" {
            affected.insert(action["service"].as_str().context("Prerequisite requires service")?.into());
        }
    }
    let running = running_services(&container_rows(project, docker)?);
    let mut expanded = scope.to_vec();
    expanded.extend(restart_consumers(project, &affected, &running)?);
    let expanded = selected_services(project, &expanded)?;
    validate_prerequisite_order(project, planned, &expanded).context("Invalid native workflow configuration")
}

#[derive(Default)]
struct NativeOperation {
    completed: BTreeMap<String, Vec<String>>,
    started: BTreeSet<String>,
    stopped: BTreeSet<String>,
    prerequisites: BTreeSet<String>,
    scope: Vec<String>,
}
fn service_containers<'a>(rows: &'a [Value], service: &str) -> Vec<&'a Value> {
    rows.iter().filter(|row| {
        row["Config"]["Labels"]["com.docker.compose.service"].as_str() == Some(service)
            && row["Config"]["Labels"]["com.docker.compose.oneoff"].as_str() != Some("True")
    }).collect()
}
fn completed_ids(rows: &[Value], service: &str) -> Result<Vec<String>> {
    let containers = service_containers(rows, service);
    ensure!(!containers.is_empty(), "Prerequisite {service} has no retained container");
    let mut ids = Vec::new();
    for container in containers {
        ensure!(container["State"]["Status"] == "exited" && container["State"]["ExitCode"] == 0 && container["State"]["OOMKilled"] != true,
            "Prerequisite {service} did not complete successfully; inspect dks logs {service}");
        ids.push(container["Id"].as_str().context("Container inspection lacks ID")?.to_owned());
    }
    ids.sort();
    Ok(ids)
}
fn revalidate_completed(project: &Project, docker: &Docker, operation: &NativeOperation) -> Result<()> {
    if operation.completed.is_empty() { return Ok(()) }
    let rows = container_rows(project, docker)?;
    for (service, ids) in &operation.completed {
        ensure!(completed_ids(&rows, service)? == *ids, "Successful prerequisite {service} container identity changed; consumers remain stopped");
    }
    Ok(())
}
fn stop_running(project: &Project, targets: &[String], docker: &Docker, operation: &mut NativeOperation) -> Result<()> {
    if targets.is_empty() { return Ok(()) }
    check_ownership(project, docker, false)?;
    let rows = container_rows(project, docker)?;
    let running = running_services(&rows);
    let targets: BTreeSet<_> = targets.iter().cloned().collect();
    let running = if operation.prerequisites.is_empty() {
        targets.intersection(&running).cloned().collect()
    } else {
        restart_consumers(project, &targets, &running)?
    };
    if running.is_empty() { return Ok(()) }
    let file = write_render(project, "compose")?;
    let mut args = compose_args(project, &file)?;
    args.push("stop".into());
    args.extend(running.iter().cloned());
    docker.run(&args, None)?;
    for service in running {
        operation.started.remove(&service);
        operation.stopped.insert(service);
    }
    Ok(())
}
fn wait_dependency(project: &Project, dependency: &Dependency<'_>, docker: &Docker) -> Result<()> {
    loop {
        docker.remaining(Duration::from_secs(300))?;
        let rows = container_rows(project, docker)?;
        let containers = service_containers(&rows, dependency.service);
        let mut ready = !containers.is_empty();
        for container in &containers {
            let state = &container["State"];
            if dependency.condition == "service_completed_successfully" && state["OOMKilled"] == true {
                return Err(prerequisite_failure(dependency.service, &containers, container));
            }
            ensure!(state["OOMKilled"] != true, "Dependency {} was killed by the kernel (out of memory)", dependency.service);
            let status = state["Status"].as_str().unwrap_or("unknown");
            match dependency.condition {
                "service_completed_successfully" => {
                    if status == "exited" || status == "dead" {
                        if status != "exited" || state["ExitCode"] != 0 {
                            return Err(prerequisite_failure(dependency.service, &containers, container));
                        }
                    } else { ready = false; }
                }
                "service_healthy" => {
                    ensure!(status != "exited" && status != "dead", "Dependency {} stopped before becoming healthy", dependency.service);
                    ensure!(state["Health"]["Status"] != "unhealthy", "Dependency {} is unhealthy", dependency.service);
                    ready &= state["Running"] == true && state["Health"]["Status"] == "healthy";
                }
                _ => {
                    if status == "exited" || status == "dead" {
                        ensure!(status == "exited" && state["ExitCode"] == 0 && one_shot(project, dependency.service)?,
                            "Dependency {} stopped before its consumer started", dependency.service);
                    } else {
                        ready &= state["Running"] == true;
                    }
                }
            }
        }
        if ready { return Ok(()) }
        thread::sleep(docker.remaining(Duration::from_millis(100))?);
    }
}
fn prerequisite_failure(service: &str, containers: &[&Value], failed: &Value) -> anyhow::Error {
    PrerequisiteFailed(json!({
        "service": service,
        "container": failed["Id"],
        "exitCode": failed["State"]["ExitCode"],
        "status": failed["State"]["Status"],
        "oomKilled": failed["State"]["OOMKilled"],
        "containers": containers.iter().map(|row| json!({
            "id":row["Id"], "status":row["State"]["Status"],
            "exitCode":row["State"]["ExitCode"], "running":row["State"]["Running"],
            "oomKilled":row["State"]["OOMKilled"]
        })).collect::<Vec<_>>()
    })).into()
}
fn start_ordered(project: &Project, scope: &[String], docker: &Docker, operation: &mut NativeOperation) -> Result<()> {
    for service in dependency_order_scoped(project, scope, &operation.scope)? {
        if operation.completed.contains_key(&service) { continue }
        ensure!(!operation.prerequisites.contains(&service), "Prerequisite {service} must complete in its declared action order before starting consumers");
        for dependency in dependencies(&project.services()?[&service])? {
            if dependency.required || operation.scope.iter().any(|name| name == dependency.service) {
                wait_dependency(project, &dependency, docker)?;
            }
        }
        if operation.started.contains(&service) { continue }
        revalidate_completed(project, docker, operation)?;
        let file = write_render(project, "compose")?;
        let mut args = compose_args(project, &file)?;
        args.extend(strings(&["up", "--detach", "--build", "--no-deps", &service]));
        docker.run(&args, None)?;
        operation.started.insert(service);
    }
    Ok(())
}
fn execute_native_action(project: &Project, action: &Value, docker: &Docker, output: &Output, operation: &mut NativeOperation, timeout: u64) -> Result<()> {
    let bounded = docker.with_deadline(Instant::now() + Duration::from_secs(action["timeout"].as_u64().unwrap_or(timeout).max(1)));
    let docker = &bounded;
    let kind = action["kind"].as_str().context("Action needs kind")?;
    if kind == "stop" {
        output.event("action", action["name"].as_str().unwrap_or("stop"))?;
        return stop_running(project, &argv(&action["targets"])?, docker, operation);
    }
    if kind == "up" {
        output.event("action", action["name"].as_str().unwrap_or("up"))?;
        let service = action["service"].as_str().context("Up action requires service")?;
        start_ordered(project, &[service.into()], docker, operation)?;
        return wait_ready(project, &[service.into()], docker, output, timeout).map(|_| ());
    }
    if kind != "prerequisite" { return execute_action(project, action, docker, output, timeout) }
    output.event("action", action["name"].as_str().unwrap_or("prerequisite"))?;
    let service = action["service"].as_str().context("Prerequisite requires service")?;
    if operation.completed.contains_key(service) { return revalidate_completed(project, docker, operation) }
    // Compose restart relationships are only applied to consumers already in
    // scope or actually running; never create unrelated, absent services.
    let restart: Vec<_> = project.services()?.iter().filter_map(|(name, definition)| {
        match dependencies(definition) {
            Ok(deps) if deps.iter().any(|dep| dep.service == service && dep.restart) => Some(Ok(name.clone())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        }
    }).collect::<Result<_>>()?;
    stop_running(project, &restart, docker, operation)?;
    let deps: Vec<_> = dependencies(&project.services()?[service])?.into_iter().filter(|dep| dep.required || operation.scope.iter().any(|name| name == dep.service)).collect();
    let dependency_scope: Vec<_> = deps.iter().map(|dep| dep.service.to_owned()).collect();
    start_ordered(project, &dependency_scope, docker, operation)?;
    for dependency in &deps { wait_dependency(project, dependency, docker)?; }
    revalidate_completed(project, docker, operation)?;
    let rows = container_rows(project, docker)?;
    let existing = service_containers(&rows, service);
    let fresh = action["fresh"] == true;
    let retained_success = !existing.is_empty() && existing.iter().all(|row| row["State"]["Status"] == "exited" && row["State"]["ExitCode"] == 0 && row["State"]["OOMKilled"] != true);
    let already_running = existing.iter().any(|row| row["State"]["Running"] == true);
    if fresh || (!retained_success && !already_running) {
        let file = write_render(project, "compose")?;
        let mut args = compose_args(project, &file)?;
        args.extend(strings(&["up", "--detach", "--build", "--no-deps"]));
        if fresh { args.push("--force-recreate".into()); }
        args.push(service.into());
        docker.run(&args, None).with_context(|| format!("Prerequisite {service} startup failed"))?;
    }
    wait_dependency(project, &Dependency { service, condition: "service_completed_successfully", required: true, restart: false }, docker)?;
    let ids = completed_ids(&container_rows(project, docker)?, service)?;
    operation.completed.insert(service.into(), ids);
    operation.started.insert(service.into());
    let mut saved = state::read(&project.root, "operation")?;
    if !saved.is_object() { saved = json!({}); }
    saved["prerequisites"] = json!(operation.completed);
    state::save(&project.root, "operation", &saved)?;
    Ok(())
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
fn execute_action(
    project: &Project,
    action: &Value,
    docker: &Docker,
    output: &Output,
    timeout: u64,
) -> Result<()> {
    let bounded = docker.with_deadline(Instant::now() + Duration::from_secs(timeout.max(1)));
    let docker = &bounded;
    let name = action["name"]
        .as_str()
        .context("Each action requires a name")?;
    output.event("action", name)?;
    let kind = action["kind"]
        .as_str()
        .context("Each action requires kind")?;
    if kind == "command" {
        return crate::commands::streamed(&project.root, &argv(&action["argv"])?, docker, output, timeout)
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
    let planned = planned_actions(project, workflow, selected)?;
    execute_validated_sequence(project, selected, &planned, docker, output)
}
/// Execute a borrowed subset without manufacturing a second Project. Selection
/// and all declarations are still preflighted before the first command.
pub(crate) fn execute_action_sequence(
    project: &Project,
    workflow: &str,
    selected: &[String],
    sequence: &[&Value],
    docker: &Docker,
    output: &Output,
) -> Result<()> {
    let applicable = planned_actions(project, workflow, selected)?;
    for action in sequence {
        ensure!(applicable.iter().any(|candidate| std::ptr::eq(*candidate, *action)), "Action {} is outside the planned workflow configuration scope", action["name"]);
    }
    execute_validated_sequence(project, selected, sequence, docker, output)
}
fn execute_validated_sequence(
    project: &Project,
    selected: &[String],
    planned: &[&Value],
    docker: &Docker,
    output: &Output,
) -> Result<()> {
    let native = planned.iter().any(|action| action["kind"] == "prerequisite");
    let budget = planned.iter().map(|action| action["timeout"].as_u64().unwrap_or(300)).max().unwrap_or(300);
    let docker = docker.with_deadline(Instant::now() + Duration::from_secs(budget));
    let mut operation = NativeOperation {
        scope: if native { selected_services(project, selected)? } else { Vec::new() },
        prerequisites: planned.iter().filter(|a| a["kind"] == "prerequisite").filter_map(|a| a["service"].as_str().map(str::to_owned)).collect(),
        ..Default::default()
    };
    if native { preflight_native_scope(project, planned, &operation.scope, &docker)?; }
    for action in planned.iter().filter(|action| !native || action["stage"].as_str().unwrap_or("before") == "before") {
        if native || action["kind"] == "stop" {
            execute_native_action(project, action, &docker, output, &mut operation, budget)?;
        } else {
            execute_action(project, action, &docker, output, action["timeout"].as_u64().unwrap_or(300))?;
        }
    }
    if native {
        let mut scope = operation.scope.clone();
        scope.extend(operation.stopped.iter().cloned());
        scope = selected_services(project, &scope)?;
        operation.scope = scope.clone();
        start_ordered(project, &scope, &docker, &mut operation)?;
        wait_ready(project, &scope, &docker, output, budget)?;
        revalidate_completed(project, &docker, &operation)?;
        for action in planned.iter().filter(|a| a["stage"] == "after") {
            execute_native_action(project, action, &docker, output, &mut operation, budget)?;
        }
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
pub(crate) fn bindable(host: &str, port: u16, protocol: &str) -> bool {
    if protocol == "udp" {
        UdpSocket::bind((host, port)).is_ok()
    } else {
        TcpListener::bind((host, port)).is_ok()
    }
}
pub(crate) fn local_context(context: &str) -> bool {
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
/// The invoking checkout lifecycle lock must already be held.
pub fn allocate_ports(root: &Path) -> Result<bool> {
    crate::allocations::allocate(root, &Docker::new(root, Output::default()))
}

pub(crate) fn one_shot(project: &Project, service: &str) -> Result<bool> {
    if project.backend()? == "compose" {
        for definition in project.services()?.values() {
            if definition["depends_on"][service]["condition"] == "service_completed_successfully" {
                return Ok(true);
            }
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
        ensure!(
            state["OOMKilled"] != true,
            "Service {service} was killed by the kernel (out of memory)"
        );
        let status = state["Status"].as_str().unwrap_or("unknown");
        if status == "exited" || status == "dead" {
            let code = state["ExitCode"].as_i64().unwrap_or(-1);
            ensure!(
                status == "exited" && oneshot && code == 0,
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
pub(crate) fn validate_readiness(project: &Project, service: &str) -> Result<()> {
    let Some(check) = project.metadata["readiness"].get(service) else { return Ok(()) };
    ensure!(check.is_object(), "Readiness for {service} must be an object");
    ensure!(check.get("url").is_some() || check.get("command").is_some(), "Readiness for {service} requires url or command");
    if let Some(url) = check.get("url") {
        let url = url.as_str().context("Readiness URL must be a string")?;
        ensure!(url.starts_with("http://") || url.starts_with("https://"), "Readiness URL must use HTTP(S)");
    }
    if let Some(command) = check.get("command") {
        ensure!(!argv(command)?.is_empty(), "Readiness command must not be empty");
    }
    if let Some(status) = check.get("status") {
        ensure!(status.as_u64().is_some_and(|code| (100..=599).contains(&code)), "Readiness status must be an HTTP status code");
    }
    if let Some(contains) = check.get("contains") {
        ensure!(contains.is_string(), "Readiness contains must be a string");
    }
    Ok(())
}
pub(crate) fn readiness_check(
    project: &Project,
    service: &str,
    docker: &Docker,
    output: &Output,
    remaining: u64,
) -> Result<bool> {
    validate_readiness(project, service)?;
    let Some(check) = project.metadata["readiness"].get(service) else {
        return Ok(true);
    };
    if let Some(url) = check["url"].as_str() {
        // The library handles HTTP(S), redirects, and certificate verification; errors
        // while an application boots are retried within the same bounded wait.
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(docker.remaining(Duration::from_secs(remaining.clamp(1, 5)))?))
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
        && crate::commands::streamed(
            &project.root,
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
    let bounded = docker.with_deadline(started + Duration::from_secs(timeout.max(1)));
    let docker = &bounded;
    let mut previous = Map::new();
    let mut observations: BTreeMap<String, Value> = selected.iter().map(|service| (
        service.clone(), json!({"name":service,"observed":false,
            "containerReady":false,"applicationReady":null,"unhealthy":false}),
    )).collect();
    let result = (|| -> Result<Value> {
    loop {
        interrupted()?;
        docker.remaining(Duration::from_secs(timeout.max(1)))?;
        let rows = container_rows_timeout(
            project,
            docker,
            timeout.saturating_sub(started.elapsed().as_secs()).max(1),
        )?;
        let mut ready = true;
        for service in selected {
            let observation = observations.get_mut(service).unwrap();
            observation["observed"] = json!(true);
            observation["containerReady"] = json!(false);
            observation["unhealthy"] = json!(service_containers(&rows, service).iter()
                .any(|row| row["State"]["Health"]["Status"] == "unhealthy"));
            let (container_ready, description) = inspect_state(project, service, &rows)?;
            observation["containerReady"] = json!(container_ready);
            if !container_ready { observation["applicationReady"] = Value::Null; }
            if previous.get(service).and_then(Value::as_str) != Some(&description) {
                output.event("readiness", &format!("{service}: {description}"))?;
                previous.insert(service.clone(), json!(description));
            }
            if !container_ready {
                ready = false;
                continue;
            }
            let application_ready = readiness_check(
                project,
                service,
                docker,
                output,
                timeout.saturating_sub(started.elapsed().as_secs()).max(1),
            )?;
            observation["applicationReady"] = if project.metadata["readiness"].get(service).is_some() {
                json!(application_ready)
            } else { Value::Null };
            if !application_ready {
                ready = false;
            }
        }
        if ready {
            docker.remaining(Duration::from_secs(timeout.max(1)))?;
            return Ok(json!(selected.iter().map(|name| json!({"name":name,"status":if project.metadata["readiness"].get(name).is_some() {"application readiness verified"} else {previous[name].as_str().unwrap_or("unknown")},"containerReady":true,"applicationReady":project.metadata["readiness"].get(name).map(|_| true)})).collect::<Vec<_>>()));
        }
        thread::sleep(docker.remaining(Duration::from_millis(500))?);
    }
    })();
    result.map_err(|error| error.context(crate::status::StatusReport(json!({
        "backend":"compose","project":project.name().ok(),"ready":false,
        "services":observations.into_values().collect::<Vec<_>>(),
    }))))
}
fn journal(project: &Project, workflow: &str, phase: &str, selected: &[String]) -> Result<()> {
    let mut saved = state::read(&project.root, "operation")?;
    if !saved.is_object() { saved = json!({}); }
    if phase == "starting" {
        saved.as_object_mut().unwrap().remove("prerequisites");
    }
    saved["workflow"] = json!(workflow);
    saved["phase"] = json!(phase);
    saved["services"] = json!(selected);
    saved["time"] = json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs());
    state::save(&project.root, "operation", &saved)
}
/// Only attach secondary evidence; neither hook failures nor a new diagnostic
/// deadline may replace the original lifecycle error or cancellation.
pub(crate) fn startup_diagnostics(
    error: anyhow::Error,
    project: &Project,
    selected: &[String],
    timeout: u64,
    docker: &Docker,
    output: &Output,
) -> anyhow::Error {
    if error.is::<Cancelled>() || interrupted().is_err() {
        return error;
    }
    let report = crate::diagnostics::run(
        project, selected, "startup-failed", timeout, docker, output, Some(&error),
    );
    error.context(crate::diagnostics::DiagnosticReport(report))
}

/// Read the existing identity without creating/adopting an environment.
fn diagnostic_owner(project: &Project, docker: &Docker) -> Result<(String, String)> {
    docker.remaining(Duration::from_secs(10))?;
    let context = docker.context()?;
    let identity = state::read(&project.root, "identity")?;
    let owner = identity["id"].as_str().filter(|id| !id.is_empty())
        .context("Diagnostics require an existing ownership identity")?;
    let root = fs::canonicalize(&project.root)?;
    let root_key = root.to_str().context("Diagnostic checkout path must be UTF-8")?;
    ensure!(identity["root"].as_str() == Some(root_key)
        && identity["project"].as_str() == Some(project.name()?)
        && identity["backend"].as_str() == Some(project.backend()?),
        "Diagnostic ownership identity does not match this environment");
    ensure!(identity["context"].as_str() == Some(context.as_str()),
        "Diagnostic Docker connection differs from the recorded ownership identity");
    let registry = crate::registry::read()?;
    if let Some(entry) = registry["environments"].get(root_key) {
        ensure!(entry["ownerId"].as_str() == Some(owner)
            && entry["project"].as_str() == Some(project.name()?)
            && entry["backend"].as_str() == Some(project.backend()?)
            && entry["connection"].as_str() == Some(context.as_str()),
            "Diagnostic environment differs from its saved registration");
        let daemon = docker.capture_timeout(&strings(&["info", "--format", "{{.ID}}"]), None, 10)?;
        ensure!(entry["daemonId"].as_str() == Some(daemon.trim()),
            "Diagnostic Docker daemon differs from its saved registration");
    }
    Ok((owner.into(), context))
}

pub(crate) fn diagnostic_container_state(container: &Value) -> Value {
    let state = &container["State"];
    let status = state["Status"].as_str().filter(|value|
        ["created", "running", "paused", "restarting", "removing", "exited", "dead"].contains(value));
    let health = state["Health"]["Status"].as_str().filter(|value|
        ["starting", "healthy", "unhealthy", "none"].contains(value));
    json!({"status":status,"running":state["Running"].as_bool(),
        "exitCode":state["ExitCode"].as_i64(),"oomKilled":state["OOMKilled"].as_bool(),
        "health":health})
}

pub(crate) fn diagnostic_resource_id(value: &Value) -> Option<&str> {
    value.as_str().filter(|id| !id.is_empty() && id.len() <= 64
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

/// A single read-only observation pass. Never pass Config, environment, health
/// logs, task error strings, or credential bytes across the hook boundary.
pub(crate) fn diagnostic_observations(
    project: &Project,
    selected: &[String],
    docker: &Docker,
) -> Result<Value> {
    let (owner, context) = diagnostic_owner(project, docker)?;
    if project.backend()? == "swarm" {
        return crate::deploy::diagnostic_observations(project, selected, docker, &owner, &context);
    }
    let fallback_profiles;
    let profiles = match &docker.profiles {
        Some(profiles) => profiles.as_slice(),
        None => {
            fallback_profiles = active_profiles(project, &[], selected.is_empty())?;
            fallback_profiles.as_slice()
        }
    };
    let scope = selected_services_with_profiles(project, selected, profiles)?;
    ensure!(!scope.is_empty(), "Diagnostics have no required Compose services");
    let containers = container_rows(project, docker)?;
    let mut services = Vec::new();
    for service in scope {
        docker.remaining(Duration::from_secs(10))?;
        let rows = service_containers(&containers, &service);
        let mut row = json!({"service":service,"observed":true,"present":!rows.is_empty(),
            "containerReady":false,"unhealthy":false,"applicationReady":null,
            "oneShot":one_shot(project, &service)?,"verifiedContainerIds":[],
            "containers":[],"failures":[]});
        let mut all_owned = true;
        for container in rows {
            let labels = &container["Config"]["Labels"];
            let id = diagnostic_resource_id(&container["Id"]);
            if labels[OWNER].as_str() != Some(owner.as_str())
                || labels[PROJECT].as_str() != Some(project.name()?)
                || labels["com.docker.compose.project"].as_str() != Some(project.name()?)
                || id.is_none() {
                all_owned = false;
                row["failures"].as_array_mut().unwrap().push(json!("Container ownership or immutable identity could not be verified"));
                continue;
            }
            let id = id.unwrap();
            row["verifiedContainerIds"].as_array_mut().unwrap().push(json!(id));
            let mut state = diagnostic_container_state(container);
            state["id"] = json!(id);
            if state["health"] == "unhealthy" { row["unhealthy"] = json!(true); }
            row["containers"].as_array_mut().unwrap().push(state);
        }
        if all_owned {
            row["containerReady"] = json!(inspect_state(project, &service, &containers)
                .map(|(ready, _)| ready).unwrap_or(false));
        } else {
            row["observed"] = json!(false);
            // Mixed ownership must not make even a partial service target usable.
            row["verifiedContainerIds"] = json!([]);
            row["containers"] = json!([]);
        }
        services.push(row);
    }
    Ok(json!({"backend":"compose","project":project.name()?,"context":context,
        "activeProfiles":profiles,"services":services}))
}

pub fn lifecycle(
    project: &Project,
    workflow: &str,
    selected: &[String],
    profiles: &[String],
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
    let active_profiles = active_profiles(project, profiles, false)?;
    let selected_full = selected_services_with_profiles(project, selected, &active_profiles)?;
    if workflow == "up" || workflow == "dev" {
        ensure!(!selected_full.is_empty(), "No required Compose services; select services or activate profiles");
    }
    if workflow == "up" || workflow == "dev" {
        crate::diagnostics::validate(project)?;
        for service in &selected_full { validate_readiness(project, service)?; }
    }
    let planned = actions(project, workflow, &selected_full, None)?;
    if plan_only {
        let native = planned.iter().any(|action| action["kind"] == "prerequisite");
        let operations = if workflow == "down" || workflow == "destroy" {
            json!([{"kind":"ownership-check"},{"kind":"remove-containers","scope":"recorded owner + project labels","services":selected},{"kind":"remove-networks","scope":"owned only","onlyFullProject":true},{"kind":"remove-volumes","enabled":workflow == "destroy","scope":"owned only","onlyFullProject":true}])
        } else {
            let startup = if native {
                json!({"kind":"dependency-ordered-startup","services":dependency_order(project, &selected_full)?,"noDependencies":true,"retainPrerequisites":true,"restartStoppedConsumers":true})
            } else {
                let mut command = compose_args(project, &project.root.join(".dockstride/render-compose.yaml"))?;
                for profile in &active_profiles { command.extend(strings(&["--profile", profile])); }
                command.extend(strings(&["up", "--detach", "--build"]));
                command.extend_from_slice(&selected_full);
                json!({"kind":"docker","argv":command})
            };
            json!([{"kind":"check-docker-context-ownership-ports"},{"kind":"allocate-declared-ports","policy":project.metadata["setup"]["ports"]},{"kind":"actions-before","actions":actions(project,workflow,&selected_full,Some("before"))?},startup,{"kind":"wait-readiness","services":selected_full,"timeoutSeconds":timeout},{"kind":"actions-after","actions":actions(project,workflow,&selected_full,Some("after"))?},{"kind":"compose-watch","enabled":workflow == "dev","requiresDeclaredWatch":true}])
        };
        return Ok(json!({"backend":"compose","project":project.name()?,"workflow":workflow,"services":selected_full,"actions":planned,"readiness":project.metadata["readiness"],"portAllocation":project.metadata["setup"]["ports"],"effects":if workflow == "destroy" {"remove owned containers/networks and managed volumes; retain secret references"} else if workflow == "down" {"stop/remove owned containers/networks; preserve volumes and secrets"} else {"allocate declared ports, build/start, run declared actions, verify readiness"},"endpoints":project.endpoints(),"devLoop":project.metadata["dev"],"operations":operations}));
    }
    ensure!(
        workflow != "destroy" || confirmed,
        "Destruction requires explicit confirmation (--yes)"
    );
    let _lock = state::lock(&project.root, "lifecycle")?;
    recover_publication(&project.root)?;
    let fresh = crate::nickel::evaluate(&project.root, None)?;
    ensure!(
        fresh.name()? == project.name()? && fresh.backend()? == project.backend()?,
        "Environment identity changed while waiting for lifecycle lock; rerun the command with current configuration"
    );
    let project = &fresh;
    // Revalidate the effective scope after waiting for the lifecycle lock.
    let initial_scope = selected_services_with_profiles(project, selected, &active_profiles)?;
    let initial_actions = planned_actions(project, workflow, &initial_scope)?;
    if workflow == "up" || workflow == "dev" {
        ensure!(!initial_scope.is_empty(), "No required Compose services; select services or activate profiles");
        crate::diagnostics::validate(project)?;
        for service in &initial_scope { validate_readiness(project, service)?; }
    }
    drop(initial_actions);
    let mut docker = Docker::new(&project.root, output.clone()).with_profiles(&active_profiles);
    if workflow == "up" || workflow == "dev" {
        docker = docker.with_deadline(Instant::now() + Duration::from_secs(timeout.max(1)));
    }
    output.event(
        "check",
        &format!("{} ({})", project.name()?, project.backend()?),
    )?;
    let preflight = (|| -> Result<()> {
        docker.check()?;
        validate_ownership(project, &docker, workflow == "up" || workflow == "dev")?;
        Ok(())
    })();
    if let Err(error) = preflight {
        return Err(if workflow == "up" || workflow == "dev" {
            startup_diagnostics(error, project, &initial_scope, timeout, &docker, output)
        } else { error });
    }
    if workflow == "down" || workflow == "destroy" {
        return teardown(project, workflow == "destroy", selected, &docker, output);
    }
    let evaluated;
    let allocated = crate::allocations::allocate(&project.root, &docker).map_err(|error| {
        if error.is::<DockerError>() || error.is::<DeadlineExceeded>() {
            startup_diagnostics(error, project, &initial_scope, timeout, &docker, output)
        } else { error }
    })?;
    let project = if allocated {
        evaluated = crate::nickel::evaluate(&project.root, None)?;
        &evaluated
    } else {
        project
    };
    let mut selected_full = selected_services_with_profiles(project, selected, &active_profiles)?;
    crate::diagnostics::validate(project)?;
    for service in &selected_full { validate_readiness(project, service)?; }
    detect_ports(project, &selected_full, &docker)
        .map_err(|error| startup_diagnostics(error, project, &selected_full, timeout, &docker, output))?;
    let planned = planned_actions(project, workflow, &selected_full)?;
    let native = planned.iter().any(|action| action["kind"] == "prerequisite");
    let mut operation = NativeOperation {
        scope: selected_full.clone(),
        prerequisites: planned.iter().filter(|a| a["kind"] == "prerequisite").filter_map(|a| a["service"].as_str().map(str::to_owned)).collect(),
        ..Default::default()
    };
    if native {
        preflight_native_scope(project, &planned, &operation.scope, &docker).map_err(|error| {
            if error.is::<DockerError>() || error.is::<DeadlineExceeded>() {
                startup_diagnostics(error, project, &selected_full, timeout, &docker, output)
            } else { error }
        })?;
    }
    let file = write_render(project, "compose")?;
    journal(project, workflow, "starting", &selected_full)?;
    state::mark_resources(&project.root, true)?;
    let startup = (|| -> Result<Value> {
        for action in planned.iter().filter(|a| a["stage"].as_str().unwrap_or("before") == "before") {
            if native || action["kind"] == "stop" {
                execute_native_action(project, action, &docker, output, &mut operation, timeout)?;
            } else {
                execute_action(project, action, &docker, output, action["timeout"].as_u64().unwrap_or(timeout))?;
            }
        }
        if native {
            selected_full.extend(operation.stopped.iter().cloned());
            selected_full = selected_services(project, &selected_full)?;
            operation.scope = selected_full.clone();
            start_ordered(project, &selected_full, &docker, &mut operation)?;
        } else {
            let mut up = compose_args(project, &file)?;
            up.extend(strings(&["up", "--detach", "--build"]));
            up.extend_from_slice(&selected_full);
            docker.run_timeout(&up, None, timeout)?;
        }
        journal(project, workflow, "waiting", &selected_full)?;
        let mut applied = state::read(&project.root, "operation")?;
        applied["appliedProfiles"] = json!(active_profiles);
        state::save(&project.root, "operation", &applied)?;
        let logs = startup_logs(project, &selected_full, &docker, output)?;
        let readiness = wait_ready(project, &selected_full, &docker, output, timeout)?;
        drop(logs);
        revalidate_completed(project, &docker, &operation)?;
        for action in planned.iter().filter(|a| a["stage"] == "after") {
            if native || action["kind"] == "stop" {
                execute_native_action(project, action, &docker, output, &mut operation, timeout)?;
            } else {
                execute_action(project, action, &docker, output, action["timeout"].as_u64().unwrap_or(timeout))?;
            }
        }
        Ok(readiness)
    })();
    let readiness = match startup {
        Ok(readiness) => readiness,
        Err(error) => {
            let _ = journal(project, workflow, if error.is::<Cancelled>() { "cancelled" } else { "failed" }, &selected_full);
            if !error.is::<Cancelled>() && docker.remaining(Duration::from_secs(10)).is_ok() {
                let mut logs = compose_args(project, &file)?;
                logs.extend(strings(&["logs", "--tail", "30", "--no-color"]));
                logs.extend_from_slice(&selected_full);
                logs.extend(operation.prerequisites.iter().filter(|service| !selected_full.contains(service)).cloned());
                let _ = docker.run_timeout(&logs, None, 10);
            }
            let error = error.context("Compose startup failed; one-shot prerequisites must exit successfully. Stopped consumers remain stopped; detached containers and prerequisite logs remain");
            return Err(startup_diagnostics(error, project, &selected_full, timeout, &docker, output));
        }
    };
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
        docker.deadline = None;
        if let Some(command) = project.metadata["dev"].get("argv") {
            output.event("dev","Starting declared development command; Ctrl-C stops the command, not detached containers")?;
            crate::commands::streamed(&project.root, &argv(command)?, &docker, output, 0)?;
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
pub fn status(
    project: &Project,
    selected: &[String],
    profiles: &[String],
    inspect_only: bool,
    timeout: u64,
    output: &Output,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(timeout.min(10).max(1));
    let docker = Docker::new(&project.root, output.clone()).with_deadline(deadline);
    let mut report = json!({
        "backend":"compose", "project":project.name().ok(), "context":null,
        "inspectOnly":inspect_only, "ready":false, "deadlineExceeded":false,
        "requiredServices":[], "excludedServices":[], "services":[],
        "endpoints":project.endpoints()
    });
    let observation = (|| -> Result<()> {
        (|| -> Result<()> {
        ensure!(project.backend()? == "compose", "Compose status requires the Compose backend");
        let names: Vec<_> = project.services()?.keys().cloned().collect();
        report["services"] = json!(names.iter().map(|name| json!({
            "name":name, "required":false, "observed":false,
            "containerReady":false, "applicationReady":null, "ready":false,
            "status":"unobserved", "containers":[]
        })).collect::<Vec<_>>());
        let active = active_profiles(project, profiles, selected.is_empty())?;
        report["activeProfiles"] = json!(active);
        let scope = selected_services_with_profiles(project, selected, &active)?;
        report["requiredServices"] = json!(scope);
        report["excludedServices"] = json!(names.iter().filter(|name| !scope.contains(name)).collect::<Vec<_>>());
        for row in report["services"].as_array_mut().unwrap() {
            let name = row["name"].as_str().unwrap();
            validate_readiness(project, name)?;
            let required = scope.iter().any(|service| service == name);
            row["required"] = json!(required);
            if !required { row["status"] = json!("excluded"); }
        }
        ensure!(!scope.is_empty(), "No required Compose services; select services or activate profiles");
            Ok(())
        })().map_err(|error| error.context(crate::status::StatusConfiguration))?;
        report["context"] = json!(docker.context()?);
        let identity = check_ownership(project, &docker, true)?;
        report["owner"] = identity["id"].clone();
        report["ownershipVerified"] = json!(true);
        let containers = container_rows(project, &docker)?;
        // Record all container observations before probing any application so
        // slow probes never hide later Docker/container results.
        for row in report["services"].as_array_mut().unwrap() {
            let name = row["name"].as_str().unwrap().to_owned();
            row["observed"] = json!(true);
            row["containers"] = json!(service_containers(&containers, &name).iter().map(|container| json!({
                "id":container["Id"], "status":container["State"]["Status"],
                "running":container["State"]["Running"], "exitCode":container["State"]["ExitCode"],
                "oomKilled":container["State"]["OOMKilled"], "health":container["State"]["Health"]["Status"]
            })).collect::<Vec<_>>());
            match inspect_state(project, &name, &containers) {
                Ok((ready, description)) => {
                    row["containerReady"] = json!(ready);
                    if row["required"] == true { row["status"] = json!(description); }
                }
                Err(error) => {
                    row["error"] = json!(error.to_string());
                    if row["required"] == true { row["status"] = json!("failed"); }
                }
            }
        }
        for row in report["services"].as_array_mut().unwrap() {
            if row["required"] != true || row["containerReady"] != true { continue }
            let name = row["name"].as_str().unwrap();
            if project.metadata["readiness"].get(name).is_none() {
                row["ready"] = json!(true);
                continue;
            }
            if inspect_only {
                row["status"] = json!("inspected");
                continue;
            }
            if Instant::now() >= deadline {
                row["status"] = json!("application readiness unobserved: deadline exceeded");
                continue;
            }
            let application_ready = readiness_check(project, name, &docker, output, 10)?;
            row["applicationReady"] = json!(application_ready);
            row["ready"] = json!(application_ready);
            row["status"] = json!(if application_ready { "application readiness verified" } else { "application readiness failed" });
        }
        Ok(())
    })();
    report["deadlineExceeded"] = json!(Instant::now() >= deadline);
    report["ready"] = json!(!report["requiredServices"].as_array().unwrap().is_empty()
        && report["services"].as_array().unwrap().iter().filter(|row| row["required"] == true).all(|row| row["ready"] == true)
        && report["deadlineExceeded"] != true);
    if let Err(error) = observation {
        // Deadline exhaustion is an incomplete readiness observation. Genuine
        // transport/configuration/ownership failures retain their original type.
        if report["deadlineExceeded"] == true && error.is::<DeadlineExceeded>() {
            for row in report["services"].as_array_mut().unwrap() {
                if row["required"] == true && row["ready"] != true && row["status"] == "unobserved" {
                    row["status"] = json!("unobserved: deadline exceeded");
                }
            }
            return crate::status::finish(report);
        }
        return Err(error.context(crate::status::StatusReport(report)));
    }
    crate::status::finish(report)
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
            let _lifecycle = state::lock(&project.root, "lifecycle")?;
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
    fn owned_sparse_render_keeps_optional_resource_sections_valid() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(dir.path(), json!({"app":{"image":"python:3.13-alpine"}}), json!({}));
        state::save(dir.path(), "identity", &json!({
            "id":"owner","root":fs::canonicalize(dir.path()).unwrap(),
            "project":"fixture","backend":"compose"
        })).unwrap();
        let rendered = render_document(&project, "compose").unwrap();
        for field in ["volumes", "networks"] {
            assert!(rendered.get(field).is_none_or(Value::is_object));
        }
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
        let plan = lifecycle(&project, "dev", &[], &[], true, false, 1, &Output::default()).unwrap();
        assert_eq!(plan["actions"][0]["name"], "seed");
        assert_eq!(plan["operations"][3]["kind"], "docker");
        assert!(!dir.path().join(".dockstride").exists());
        let error = lifecycle(
            &project,
            "destroy",
            &[],
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

fn namespace_resources(docker: &Docker, listing: &[&str], project: &str, filters: &[&str]) -> Result<BTreeSet<String>> {
    let mut resources = BTreeSet::new();
    for filter in filters {
        let mut args = strings(listing);
        args.extend(strings(&["--filter", &format!("label={filter}={project}")]));
        resources.extend(docker.capture(&args, None)?.split_whitespace().map(str::to_owned));
    }
    Ok(resources)
}
