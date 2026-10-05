//! Saved configured environments. Inventory never evaluates project code.
use crate::{output::Output, publication, registry, runtime, state};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, io::Read, os::unix::process::CommandExt, path::{Component, Path, PathBuf}, process::{Command, Stdio}, thread};

const OWNER: &str = "io.dockstride.owner";
const LIMIT: usize = 1024 * 1024;

/// A failed forget retains the complete read-only safety report.
#[derive(Debug)]
pub struct ForgetBlocked(pub Value);
impl std::fmt::Display for ForgetBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Environment cannot be forgotten: {}", self.0["blockers"])
    }
}
impl std::error::Error for ForgetBlocked {}

fn required<'a>(entry: &'a Value, key: &str) -> Result<&'a str> {
    entry[key].as_str().filter(|text| !text.is_empty()).with_context(|| format!("Registry entry lacks {key}"))
}

fn path_observation(path: &Path) -> Value {
    match fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({"status":"missing","path":path}),
        Err(error) => json!({"status":"unreadable","path":path,"error":error.to_string()}),
        Ok(metadata) if !metadata.is_dir() => json!({"status":"stale","path":path,"reason":"recorded checkout is not a directory"}),
        Ok(_) => match fs::read_dir(path) {
            Err(error) => json!({"status":"unreadable","path":path,"error":error.to_string()}),
            Ok(_) => match fs::canonicalize(path) {
                Ok(actual) if actual != path => json!({"status":"stale","path":path,"actualRoot":actual,"reason":"recorded root resolves elsewhere; ownership is not transferred"}),
                Err(error) => json!({"status":"unreadable","path":path,"error":error.to_string()}),
                _ => json!({"status":"present","path":path}),
            },
        },
    }
}

fn configuration_presence(root: &Path) -> Value {
    let files: Vec<Value> = ["compose.ncl", "env.yaml"].into_iter().map(|name| {
        let path = root.join(name);
        match fs::File::open(&path) {
            Ok(file) => match file.metadata() {
                Ok(metadata) if metadata.is_file() => json!({"path":path,"status":"present"}),
                Ok(_) => json!({"path":path,"status":"unreadable","reason":"not an ordinary configuration file"}),
                Err(error) => json!({"path":path,"status":"unreadable","error":error.to_string()}),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({"path":path,"status":"missing"}),
            Err(error) => json!({"path":path,"status":"unreadable","error":error.to_string()}),
        }
    }).collect();
    let status = if files.iter().any(|file| file["status"] == "unreadable") { "unreadable" }
        else if files.iter().all(|file| file["status"] == "present") { "present" } else { "absent" };
    json!({"status":status,"files":files})
}

fn related_pending(operation: &Value, entry: &Value) -> bool {
    let claims = &operation["claims"];
    operation["root"] == entry["root"] || claims["root"] == entry["root"]
        || (claims["ownerId"].is_string() && claims["ownerId"] == entry["ownerId"])
        || (claims["daemonId"].is_string() && claims["daemonId"] == entry["daemonId"] && claims["project"] == entry["project"])
}

/// All registered repositories are included, without Docker or Nickel access.
pub fn list(root: &Path, worktrees: bool) -> Result<Value> {
    let pending = publication::pending_global()?;
    let operations = pending["operations"].as_array().context("Invalid pending operation inventory")?;
    let mut entries = registry::entries()?;
    for entry in &mut entries {
        let path = PathBuf::from(required(entry, "root")?);
        let checkout = path_observation(&path);
        let configuration = configuration_presence(&path);
        let mut status = checkout["status"].as_str().unwrap_or("unreadable").to_owned();
        let mut identity_error = None;
        if status == "present" {
            match state::read(&path, "identity") {
                Ok(identity) if identity["id"] == entry["ownerId"]
                    && identity["root"] == entry["root"] && identity["project"] == entry["project"]
                    && identity["backend"] == entry["backend"] && identity["context"] == entry["connection"] => {
                    if configuration["status"] == "absent" { status = "stale".into(); }
                    if configuration["status"] == "unreadable" { status = "unreadable".into(); }
                }
                Ok(_) => { status = "stale".into(); identity_error = Some("checkout owner identity is missing or differs from saved registration".to_owned()); }
                Err(error) => { status = "unreadable".into(); identity_error = Some(error.to_string()); }
            }
        }
        let related: Vec<Value> = operations.iter().filter(|operation| related_pending(operation, entry)).cloned().collect();
        entry["checkout"] = checkout;
        entry["configuration"] = configuration;
        entry["status"] = json!(status);
        entry["identityError"] = json!(identity_error);
        entry["pendingOperations"] = json!(related);
        if !related.is_empty() { entry["state"] = json!("pending"); }
    }
    // Pending registrations without a committed entry remain visible as claims.
    let mut result = json!({"schemaVersion":1,"environments":entries,"pendingOperations":operations,"liveObservations":null});
    if worktrees { result["worktreeDiscovery"] = discover_worktrees(root, &entries)?; }
    Ok(result)
}

fn git_capture(root: &Path, args: &[&str]) -> Result<(bool, Vec<u8>)> {
    runtime::interrupted()?;
    let mut child = Command::new("git").args(args).current_dir(root).process_group(0)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("Start optional Git worktree discovery")?;
    let mut stdout = child.stdout.take().unwrap();
    let out = thread::spawn(move || -> std::io::Result<(Vec<u8>, bool)> {
        let mut bytes = Vec::new();
        let mut exceeded = false;
        let mut buffer = [0u8; 8192];
        loop {
            let count = stdout.read(&mut buffer)?;
            if count == 0 { break; }
            let accepted = count.min(LIMIT.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&buffer[..accepted]);
            exceeded |= accepted < count;
        }
        Ok((bytes, exceeded))
    });
    let err = runtime::tail_bytes(child.stderr.take().unwrap());
    let waited = runtime::Docker::new(root, Output::default()).wait(&mut child, 10);
    let (bytes, exceeded) = out.join().map_err(|_| anyhow::anyhow!("Git discovery reader failed"))??;
    let _ = err.join();
    ensure!(!exceeded, "Git worktree discovery exceeds 1 MiB");
    Ok((waited?.success(), bytes))
}

fn discover_worktrees(root: &Path, entries: &[Value]) -> Result<Value> {
    match git_capture(root, &["rev-parse", "--git-dir"]) {
        Ok((false, _)) => return Ok(json!({"status":"not-git","worktrees":[]})),
        Err(error) => return Ok(json!({"status":"unavailable","error":error.to_string(),"worktrees":[]})),
        _ => {},
    }
    let (success, bytes) = git_capture(root, &["worktree", "list", "--porcelain", "-z"])?;
    ensure!(success, "Git worktree discovery failed");
    let mut paths = BTreeSet::new();
    for field in bytes.split(|byte| *byte == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            let path = PathBuf::from(std::str::from_utf8(path).context("Git worktree path is not UTF-8")?);
            paths.insert(select_path(&path)?);
        }
    }
    let rows: Vec<Value> = paths.into_iter().map(|path| {
        let registered = entries.iter().any(|entry| entry["root"].as_str() == path.to_str());
        let configuration = configuration_presence(&path);
        json!({"root":path,"registration":if registered {"registered"} else {"unregistered"},
            "status":if registered {"configured"} else if configuration["status"] == "present" {"configuration-present"} else {"unconfigured"},
            "checkout":path_observation(&path),"configuration":configuration})
    }).collect();
    Ok(json!({"status":"available","scope":"invoking-repository","worktrees":rows}))
}

// Missing roots remain selectable by their saved absolute path. Canonicalize the
// nearest existing ancestor, not a moved replacement guessed from its project.
fn select_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => { normalized.pop(); },
            part => normalized.push(part.as_os_str()),
        }
    }
    let mut ancestor = normalized.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(ancestor.file_name().context("Cannot resolve registry path")?.to_os_string());
        ancestor = ancestor.parent().context("Cannot resolve registry path")?;
    }
    let mut canonical = fs::canonicalize(ancestor)?;
    for part in missing.into_iter().rev() { canonical.push(part); }
    Ok(canonical)
}

fn capture(root: &Path, entry: &Value, args: &[&str]) -> Result<String> {
    runtime::capture_pinned(root, required(entry, "connection")?, &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

fn resource_evidence(root: &Path, entry: &Value) -> Result<(Vec<Value>, Vec<Value>)> {
    let info: Value = serde_json::from_str(&capture(root, entry, &["info", "--format", "{{json .}}"])?).context("Docker info did not return valid daemon evidence")?;
    ensure!(info["ID"].as_str().filter(|id| !id.is_empty()) == Some(required(entry, "daemonId")?),
        "Recorded Docker connection no longer identifies the registered daemon; refusing ownership transfer");
    let swarm = info["Swarm"]["LocalNodeState"].as_str().context("Docker info lacks Swarm state; resource absence cannot be proved")?;
    ensure!(matches!(swarm, "inactive" | "active"), "Docker Swarm state {swarm} cannot prove resource absence");
    if swarm == "active" {
        ensure!(info["Swarm"]["ControlAvailable"] == true, "Recorded daemon is a Swarm worker; owned services/secrets/configs require a manager to prove absence");
    }
    let owner_filter = format!("label={OWNER}={}", required(entry, "ownerId")?);
    let mut resources = Vec::new();
    let mut failures = Vec::new();
    for kind in ["container", "network", "volume", "service", "secret", "config"] {
        if matches!(kind, "service" | "secret" | "config") && swarm == "inactive" { continue; }
        let mut args = vec![kind, "ls"];
        if kind == "container" { args.push("--all"); }
        if matches!(kind, "container" | "network") { args.push("--no-trunc"); }
        args.extend(["--filter", &owner_filter, "--format", "{{json .}}"]);
        let text = match capture(root, entry, &args) {
            Ok(text) => text,
            Err(error) => {
                failures.push(json!({"kind":"resource-evidence-failure","resourceKind":kind,"error":format!("{error:#}")}));
                continue;
            },
        };
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let observation: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(error) => {
                    failures.push(json!({"kind":"resource-evidence-failure","resourceKind":kind,"error":format!("Invalid Docker resource evidence: {error}")}));
                    continue;
                },
            };
            let id = if kind == "volume" { observation["Name"].as_str() } else { observation["ID"].as_str() };
            let Some(id) = id.filter(|id| !id.is_empty()) else {
                failures.push(json!({"kind":"resource-evidence-failure","resourceKind":kind,"error":"Docker resource evidence lacks identifier"}));
                continue;
            };
            resources.push(json!({"kind":kind,"id":id,"name":observation["Name"].as_str().or_else(|| observation["Names"].as_str()),"observation":observation}));
        }
    }
    Ok((resources, failures))
}

fn reservation_evidence(entry: &Value) -> Result<Vec<Value>> {
    let value = state::read(&state::global_root()?, "port-reservations")?;
    let value = if value.is_null() { json!({}) } else { value };
    let records = if value.get("schemaVersion").is_some() {
        ensure!(value["schemaVersion"] == 1, "Unsupported port reservation schema; forgetting is unsafe");
        value["reservations"].as_object().context("Versioned port reservations lack reservation map")?
    } else { value.as_object().context("Port reservations must be a mapping")? };
    let mut blockers = Vec::new();
    for (key, reservation) in records {
        let owned = if let Some(path) = reservation.as_str() { entry["root"].as_str() == Some(path) }
            else if reservation.is_object() {
                ensure!(reservation["root"].as_str().is_some_and(|root| !root.is_empty())
                    || reservation["ownerId"].as_str().is_some_and(|owner| !owner.is_empty()),
                    "Port reservation {key} lacks ownership evidence; forgetting is unsafe");
                reservation["root"] == entry["root"] || (reservation["ownerId"].is_string() && reservation["ownerId"] == entry["ownerId"])
            } else { anyhow::bail!("Unrecognized port reservation {key}; forgetting is unsafe"); };
        if owned { blockers.push(json!({"kind":"reservation","key":key,"record":reservation})); }
    }
    if let Some(allocations) = entry.get("allocations") {
        for (field, allocation) in allocations.as_object().context("Registry allocations must be a mapping")? {
            blockers.push(json!({"kind":"registered-allocation","field":field,"record":allocation}));
        }
    }
    let path = Path::new(required(entry, "root")?);
    // Local saved allocations also block if the global reservation is missing.
    if path.is_dir() {
        let saved = state::read(path, "ports")?;
        let saved = if saved.is_null() { json!({}) } else { saved };
        let records = if saved.get("schemaVersion").is_some() {
            ensure!(saved["schemaVersion"] == 1, "Unsupported local port allocation schema; forgetting is unsafe");
            saved["allocations"].as_object().context("Versioned local ports lack allocation map")?
        } else { saved.as_object().context("Local ports must be a mapping")? };
        for (field, allocation) in records { blockers.push(json!({"kind":"local-allocation","field":field,"record":allocation})); }
    }
    Ok(blockers)
}

fn forget_report(invoking_root: &Path, entry: &Value, plan: bool) -> Result<Value> {
    let pending = publication::pending_global()?;
    let mut blockers: Vec<Value> = pending["operations"].as_array().context("Invalid pending operation inventory")?.iter()
        .filter(|operation| related_pending(operation, entry)).map(|operation| json!({"kind":"pending-operation","operation":operation})).collect();
    match reservation_evidence(entry) {
        Ok(reservations) => blockers.extend(reservations),
        Err(error) => blockers.push(json!({"kind":"reservation-evidence-failure","error":format!("{error:#}")})),
    }
    let target = Path::new(required(entry, "root")?);
    if target.is_dir() {
        match publication::pending(target) {
            Ok(operation) if !operation.is_null() => {
                if !blockers.iter().any(|blocker| blocker["operation"]["id"] == operation["id"]) {
                    blockers.push(json!({"kind":"pending-operation","operation":operation}));
                }
            },
            Err(error) => blockers.push(json!({"kind":"pending-evidence-failure","error":format!("{error:#}")})),
            _ => {},
        }
    }
    // Failed evidence is itself a blocker, not an empty daemon.
    let resources = match resource_evidence(invoking_root, entry) {
        Ok((resources, failures)) => { blockers.extend(failures); resources },
        Err(error) => { blockers.push(json!({"kind":"resource-evidence-failure","error":format!("{error:#}")})); Vec::new() },
    };
    blockers.extend(resources.iter().map(|resource| json!({"kind":"owned-resource","resource":resource})));
    Ok(json!({"schemaVersion":1,"operation":"env-forget","root":entry["root"],"ownerId":entry["ownerId"],
        "plan":plan,"sideEffects":false,"allowed":blockers.is_empty(),"blockers":blockers,"resources":resources,"forgotten":false,
        "retains":["checkout files","secret revisions","Docker resources"]}))
}

/// Remove registration only, after recorded-target resource absence is proved.
/// No other checkout lifecycle lock is acquired, including for missing roots.
pub fn forget(invoking_root: &Path, path: &Path, plan: bool, confirmed: bool, _output: &Output) -> Result<Value> {
    ensure!(plan || confirmed, "env forget requires --yes (or --plan)");
    let target = select_path(path)?;
    let find = || -> Result<Value> {
        registry::entries()?.into_iter().find(|entry| entry["root"].as_str() == target.to_str())
            .with_context(|| format!("No registered environment at {}; moved roots are not adopted automatically", target.display()))
    };
    // A plan is strictly read-only, including no lock-directory creation/recovery.
    if plan { return forget_report(invoking_root, &find()?, true); }
    let _lifecycle = state::lock(invoking_root, "lifecycle")?;
    let _global = state::global_lock()?;
    let _config = state::lock(invoking_root, "config")?;
    let invoking_pending = publication::pending(invoking_root)?;
    ensure!(invoking_pending.is_null() || invoking_pending["pending"] != true,
        "Invoking checkout has a pending publication; recover it before forgetting an environment");
    let entry = find()?;
    let mut report = forget_report(invoking_root, &entry, false)?;
    if report["allowed"] != true { return Err(ForgetBlocked(report).into()); }
    let change = registry::remove_change(&target)?;
    publication::publish(invoking_root, "forget-environment", vec![change], json!({"root":target,"ownerId":entry["ownerId"],"project":entry["project"],"daemonId":entry["daemonId"]}))?;
    report["sideEffects"] = json!(true);
    report["forgotten"] = json!(true);
    Ok(report)
}
