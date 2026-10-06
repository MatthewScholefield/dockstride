//! Configuration inventory for the invoking Git repository; never evaluates project code.
use crate::{output::Output, runtime};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, io::Read, os::unix::process::CommandExt, path::{Component, Path, PathBuf}, process::{Command, Stdio}, thread};

const LIMIT: usize = 1024 * 1024;

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

pub fn list(root: &Path) -> Result<Value> {
    let mut result = match discover_worktrees(root) {
        Ok(result) => result,
        Err(error) => json!({"status":"unavailable","error":error.to_string(),"worktrees":[]}),
    };
    result["schemaVersion"] = json!(1);
    result["scope"] = json!("invoking-repository");
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

fn discover_worktrees(root: &Path) -> Result<Value> {
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
        let configuration = configuration_presence(&path);
        json!({"root":path,"checkout":path_observation(&path),"configuration":configuration})
    }).collect();
    Ok(json!({"status":"available","scope":"invoking-repository","worktrees":rows}))
}

// Missing/pruned worktrees retain their reported path, canonicalizing its existing ancestor.
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
        missing.push(ancestor.file_name().context("Cannot resolve worktree path")?.to_os_string());
        ancestor = ancestor.parent().context("Cannot resolve worktree path")?;
    }
    let mut canonical = fs::canonicalize(ancestor)?;
    for part in missing.into_iter().rev() { canonical.push(part); }
    Ok(canonical)
}

