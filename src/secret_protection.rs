//! Read-only retention evidence. Callers hold the global publication guard; this
//! module never takes foreign checkout locks or opens credential contents.
use crate::{nickel, registry, sources, state};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, ffi::CString, fs::{self, File, OpenOptions}, io::Read,
    os::{fd::{AsRawFd, FromRawFd}, unix::{ffi::OsStrExt, fs::{MetadataExt, OpenOptionsExt}}},
    path::{Component, Path, PathBuf}};

#[derive(Clone, PartialEq, Eq)]
struct Identity { device: u64, inode: u64 }
impl Identity {
    fn of(metadata: &fs::Metadata) -> Self { Self { device: metadata.dev(), inode: metadata.ino() } }
}
#[derive(PartialEq, Eq)]
struct Version { size: u64, mode: u32, uid: u32, gid: u32, mtime: (i64, i64), ctime: (i64, i64) }
impl Version {
    fn of(metadata: &fs::Metadata) -> Self {
        Self { size: metadata.len(), mode: metadata.mode(), uid: metadata.uid(), gid: metadata.gid(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()), ctime: (metadata.ctime(), metadata.ctime_nsec()) }
    }
}
struct PathEvidence { path: PathBuf, canonical: PathBuf, identity: Identity, version: Version }
impl PathEvidence {
    fn observe(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "retention evidence requires an absolute path");
        let canonical = fs::canonicalize(path).context("cannot resolve retention path")?;
        let metadata = fs::metadata(&canonical).context("cannot inspect retention path")?;
        Ok(Self { path: path.to_owned(), canonical, identity: Identity::of(&metadata), version: Version::of(&metadata) })
    }
    fn verify(&self) -> Result<()> {
        let now = Self::observe(&self.path)?;
        ensure!(now.canonical == self.canonical && now.identity == self.identity && now.version == self.version,
            "secret retention path changed; rerun GC");
        Ok(())
    }
    fn matches(&self, path: &Path) -> bool {
        path == self.path || path == self.canonical || Self::observe(path).is_ok_and(|now|
            now.canonical == self.canonical || now.identity == self.identity)
    }
}
struct Retained { reference: Option<Value>, path: Option<PathEvidence>, reason: String }
struct PrivateFile { path: PathBuf, fingerprint: Option<String>, history: Option<Value> }
struct Directory { path: PathBuf, identity: Identity, entries: Vec<PathBuf> }
struct Checkout { root: PathEvidence, snapshot: sources::EnvironmentSnapshot, metadata: Value }

pub(crate) struct Protection {
    own_root: PathBuf,
    own_history_path: PathBuf,
    retained: Vec<Retained>,
    blockers: Vec<String>,
    files: Vec<PrivateFile>,
    directories: Vec<Directory>,
    checkouts: Vec<Checkout>,
}
impl Protection {
    pub(crate) fn reason(&self, reference: &Value) -> Option<&str> {
        self.retained.iter().find(|item| {
            item.reference.as_ref().is_some_and(|saved| saved == reference
                || saved.get("name").and_then(Value::as_str).is_some_and(|name|
                    reference.get("name").and_then(Value::as_str) == Some(name)))
                || item.path.as_ref().is_some_and(|path|
                    reference.get("file").and_then(Value::as_str).is_some_and(|file| path.matches(Path::new(file))))
        }).map(|item| item.reason.as_str()).or_else(|| self.blockers.first().map(String::as_str))
    }
    /// Rechecks the exact private observations and source graph, not Docker or
    /// credential bytes. A new/removed bookkeeping file also invalidates proof.
    pub(crate) fn verify(&self) -> Result<()> {
        for file in &self.files {
            let (_, fingerprint) = read_private(&file.path)?;
            ensure!(fingerprint == file.fingerprint, "secret retention bookkeeping changed; rerun GC");
        }
        for directory in &self.directories {
            let file = open_directory(&directory.path)?;
            ensure!(Identity::of(&file.metadata()?) == directory.identity
                && directory_entries(&directory.path, &file)? == directory.entries,
                "secret retention bookkeeping directory changed; rerun GC");
        }
        for checkout in &self.checkouts {
            checkout.root.verify()?;
            checkout.snapshot.verify()?;
            let snapshot = sources::snapshot(&checkout.root.canonical, None)?;
            ensure!(snapshot.fingerprints == checkout.snapshot.fingerprints,
                "secret retention source graph changed; rerun GC");
            let metadata = nickel::setup_metadata_values(&checkout.root.canonical, &snapshot.values)?;
            snapshot.verify()?;
            ensure!(metadata == checkout.metadata, "secret retention policies changed; rerun GC");
        }
        for retained in &self.retained {
            if let Some(path) = &retained.path { path.verify()?; }
        }
        Ok(())
    }
    /// Accept exactly one journal transition caused by this GC's deletion. All
    /// other history edits (including a protected revision's deletion) fail.
    pub(crate) fn refresh_own_history(&mut self, root: &Path) -> Result<()> {
        let root = fs::canonicalize(root)?;
        ensure!(root == self.own_root, "GC cannot refresh another checkout's retention history");
        let path = self.own_history_path.clone();
        let index = self.files.iter().position(|file| file.path == path)
            .context("invoking secret history was not observed")?;
        let (after, fingerprint) = read_private(&path)?;
        let before = self.files[index].history.as_ref().context("secret history was not observed")?;
        let old = before["revisions"].as_array().context("invalid observed secret history")?;
        let new = after["revisions"].as_array().context("invalid updated secret history")?;
        ensure!(old.len() == new.len(), "secret history changed outside GC deletion");
        let mut changed = 0;
        for (old, new) in old.iter().zip(new) {
            if old == new { continue; }
            ensure!(old["deleted"] == false && new["deleted"] == true
                && old.as_object().is_some_and(|old| new.as_object().is_some_and(|new|
                    old.len() == new.len() && old.iter().all(|(key, value)|
                        key == "deleted" || new.get(key) == Some(value))))
                && self.reason(&old["reference"]).is_none(),
                "secret history changed outside an unprotected GC deletion");
            changed += 1;
        }
        ensure!(changed == 1 && before.as_object().is_some_and(|old|
            after.as_object().is_some_and(|new| old.len() == new.len()
                && old.iter().all(|(key, value)| key == "revisions" || new.get(key) == Some(value)))),
            "secret history changed outside GC deletion");
        self.files[index].history = Some(after);
        self.files[index].fingerprint = fingerprint;
        self.verify()
    }
    fn file(&mut self, path: &Path) -> Result<Value> {
        let (value, fingerprint) = read_private(path)?;
        let history = (path == self.own_history_path).then(|| value.clone());
        self.files.push(PrivateFile { path: path.to_owned(), fingerprint, history });
        Ok(value)
    }
    fn reference(&mut self, reference: &Value, reason: String) -> Result<()> {
        let record = reference.as_object().context("unsafe deployed secret reference")?;
        let path = if let Some(file) = reference.get("file") {
            ensure!(record.len() == 1, "unsupported deployed file reference");
            let file = file.as_str().context("unsafe deployed file reference")?;
            let path = PathEvidence::observe(Path::new(file))?;
            ensure!(fs::metadata(&path.canonical)?.is_file(), "secret retention path is not a regular file");
            Some(path)
        } else {
            ensure!(record.len() == 2 && reference["external"] == true
                && reference.get("name").and_then(Value::as_str).is_some_and(|s| !s.is_empty()),
                "unsupported deployed secret reference");
            None
        };
        self.retained.push(Retained { reference: Some(reference.clone()), path, reason });
        Ok(())
    }
    fn source(&mut self, path: &Path, reason: String) -> Result<()> {
        let path = PathEvidence::observe(path)?;
        ensure!(fs::metadata(&path.canonical)?.is_file(), "secret source is not a regular file");
        self.retained.push(Retained { reference: None, path: Some(path), reason });
        Ok(())
    }
    fn checkout(&mut self, root: &Path, entry: Option<&Value>) -> Result<()> {
        let root_evidence = PathEvidence::observe(root).context("cannot resolve registered checkout path")?;
        ensure!(root_evidence.canonical == root && fs::metadata(root)?.is_dir(), "registered checkout moved or aliased");
        let identity = self.file(&root.join(".dockstride/identity.json")).context("cannot read private checkout ownership state")?;
        if let Some(entry) = entry {
            ensure!(identity["root"].as_str() == root.to_str() && identity["id"] == entry["ownerId"],
                "registered checkout ownership state is absent or moved");
            ensure!(matches!(identity["backend"].as_str(), Some("compose" | "swarm")), "unsupported checkout backend");
        }
        let snapshot = sources::snapshot(root, None).context("cannot resolve current layered configuration")?;
        ensure!(snapshot.fingerprints.get(&snapshot.local_file).is_some_and(Option::is_some),
            "checkout configuration is missing; current secret consumers are unknown");
        let metadata = nickel::setup_metadata_values(root, &snapshot.values).context("cannot evaluate current secret policy metadata")?;
        snapshot.verify()?;
        let refs = match snapshot.values.get("secrets") {
            Some(value) => Some(value.as_object().context("unsafe current secret references")?),
            None => None,
        };
        if let Some(refs) = refs {
            for reference in refs.values() {
                self.reference(reference, format!("current environment reference in {}", root.display()))?;
            }
        }
        for (source, value) in &snapshot.shared_documents {
            if let Some(refs) = value.get("secrets").and_then(Value::as_object) {
                for reference in refs.values() {
                    self.reference(reference, format!("shared environment reference in {}", source.display()))?;
                }
            }
        }
        let history = self.file(&root.join(".dockstride/secrets.json")).context("cannot read private secret revision history")?;
        let revisions = match history.get("revisions") {
            Some(value) => value.as_array().context("unsafe secret revision history")?.as_slice(),
            None => { ensure!(history.is_null() || history.as_object().is_some_and(|v| v.is_empty()), "unsafe secret revision history"); &[] }
        };
        for revision in revisions {
            ensure!(revision.is_object() && revision["logical"].is_string() && revision["reference"].is_object(), "unsafe secret revision history");
            ensure!(revision.get("pending").is_none_or(Value::is_boolean) && revision.get("deleted").is_none_or(Value::is_boolean), "unsafe secret revision flags");
            if revision["pending"] == true && revision["deleted"] != true {
                self.reference(&revision["reference"], format!("pending secret journal in {}", root.display()))?;
            }
        }
        if let Some(refs) = refs {
            for (logical, reference) in refs {
                // Only the current revision supplies origin, never an older
                // revision after a manual stdin replacement.
                if let Some(revision) = revisions.iter().rev().find(|revision|
                    revision["logical"].as_str() == Some(logical) && revision["reference"] == *reference && revision["deleted"] != true)
                {
                    if let Some(source) = revision.get("fileSource").filter(|source| !source.is_null()) {
                        ensure!(source["kind"] == "file" && matches!(source["origin"].as_str(), Some("declared-file" | "cli-file" | "reference-file" | "shared-reference")), "unsafe current secret source provenance");
                        let path = source["canonicalPath"].as_str().context("unsafe current secret source path")?;
                        self.source(Path::new(path), format!("current imported source for {logical} in {}", root.display()))?;
                    }
                }
            }
        }
        if let Some(policies) = metadata.pointer("/setup/secrets") {
            for (logical, policy) in policies.as_object().context("unsafe secret policy metadata")? {
                ensure!(policy.is_object(), "unsafe secret policy");
                if policy["kind"] == "file" {
                    let path = Path::new(policy["path"].as_str().context("unsafe declared secret source path")?);
                    let path = if path.is_absolute() { path.to_owned() } else { root.join(path) };
                    self.source(&path, format!("declared file source for {logical} in {}", root.display()))?;
                }
            }
        }
        self.scan(&root.join(".dockstride"), root, true).context("cannot verify private retained operation bookkeeping")?;
        self.checkouts.push(Checkout { root: root_evidence, snapshot, metadata });
        Ok(())
    }
    fn scan(&mut self, directory: &Path, root: &Path, top: bool) -> Result<()> {
        let file = open_directory(directory)?;
        let entries = directory_entries(directory, &file)?;
        for path in &entries {
            let name = path.file_name().context("missing bookkeeping filename")?;
            if top && (name == "locks" || name == "secrets.json") { continue; }
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(!metadata.file_type().is_symlink(), "unsafe bookkeeping symlink");
            if metadata.is_dir() {
                // Never enumerate the mode-0300 credential store, even if placed
                // within .dockstride. Unknown inaccessible state fails closed.
                ensure!(name != "secrets" && metadata.mode() & 0o400 != 0, "credential directory is not operation bookkeeping");
                self.scan(&path, root, false)?;
            } else if matches!(path.extension().and_then(|s| s.to_str()), Some("json" | "jsonl" | "yaml" | "yml")) {
                let value = self.file(&path)?;
                self.snapshot_references(&value, root)?;
            } else {
                ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0, "unsafe operation bookkeeping file");
            }
        }
        self.directories.push(Directory { path: directory.to_owned(), identity: Identity::of(&file.metadata()?), entries });
        Ok(())
    }
    fn snapshot_references(&mut self, value: &Value, root: &Path) -> Result<()> {
        match value {
            Value::Object(values) => {
                if value.get("file").is_some() || value.get("external") == Some(&Value::Bool(true)) && value.get("name").is_some() {
                    self.reference(value, format!("retained deployment or operation snapshot in {}", root.display()))?;
                }
                for value in values.values() { self.snapshot_references(value, root)?; }
            }
            Value::Array(values) => for value in values { self.snapshot_references(value, root)?; },
            _ => {}
        }
        Ok(())
    }
}

pub(crate) fn observe(root: &Path) -> Result<Protection> {
    let own = fs::canonicalize(root).context("cannot resolve invoking checkout")?;
    let mut protection = Protection { own_history_path: own.join(".dockstride/secrets.json"), own_root: own.clone(),
        retained: Vec::new(), blockers: Vec::new(), files: Vec::new(), directories: Vec::new(), checkouts: Vec::new() };
    let global = state::global_root()?;
    let registry_path = global.join(".dockstride/environment-registry.json");
    let saved = match protection.file(&registry_path) {
        Ok(saved) => saved,
        Err(_) => {
            protection.blockers.push("environment registry cannot be safely read; registered secret consumers are unknown".into());
            return Ok(protection);
        }
    };
    // Reuse authoritative schema validation, but bind its read to our secure
    // fingerprint before relying on its roots.
    let registry = match registry::read() {
        Ok(value) => value,
        Err(_) => {
            protection.blockers.push("environment registry cannot be safely read; registered secret consumers are unknown".into());
            return Ok(protection);
        }
    };
    let (now, fingerprint) = read_private(&registry_path)?;
    ensure!(now == saved && protection.files[0].fingerprint == fingerprint, "environment registry changed during secret retention observation");
    let mut roots = BTreeMap::new();
    for entry in registry["environments"].as_object().context("unsafe environment registry")?.values() {
        roots.insert(PathBuf::from(entry["root"].as_str().context("unsafe registered checkout path")?), Some(entry));
    }
    roots.entry(own).or_insert(None);
    for (root, entry) in roots {
        if let Err(error) = protection.checkout(&root, entry) {
            protection.blockers.push(format!("checkout {} has unavailable or unsafe retention state ({error}); cannot exclude secret consumers", root.display()));
        }
    }
    protection.verify()?;
    Ok(protection)
}

fn open_directory(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "operation bookkeeping path must be absolute");
    let mut anchor = OpenOptions::new().read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC).open("/")?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            ensure!(matches!(component, Component::RootDir), "unsafe bookkeeping path component");
            continue;
        };
        let name = CString::new(name.as_bytes())?;
        let access = if components.peek().is_none() { libc::O_RDONLY } else { libc::O_PATH };
        let fd = unsafe { libc::openat(anchor.as_raw_fd(), name.as_ptr(),
            access | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 { return Err(std::io::Error::last_os_error().into()); }
        anchor = unsafe { File::from_raw_fd(fd) };
    }
    let file = anchor;
    let metadata = file.metadata()?;
    ensure!(metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "operation bookkeeping directory must be private and current-user owned");
    Ok(file)
}
fn directory_entries(path: &Path, file: &File) -> Result<Vec<PathBuf>> {
    let anchored = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    let mut entries = fs::read_dir(anchored)?.map(|entry| entry.map(|entry| path.join(entry.file_name())))
        .collect::<std::io::Result<Vec<_>>>()?;
    // Lock creation/removal does not change retained reference evidence.
    entries.retain(|path| path.file_name().is_none_or(|name| name != "locks"));
    entries.sort();
    Ok(entries)
}
fn read_private(path: &Path) -> Result<(Value, Option<String>)> {
    let parent = path.parent().context("bookkeeping parent is missing")?;
    let directory = match open_directory(parent) {
        Ok(directory) => directory,
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => return Ok((Value::Null, None)),
        Err(error) => return Err(error),
    };
    let name = CString::new(path.file_name().context("bookkeeping filename is missing")?.as_bytes())?;
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound { return Ok((Value::Null, None)); }
        return Err(error.into());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "operation bookkeeping file must be private and current-user owned");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let fingerprint = Some(hex::encode(Sha256::digest(&bytes)));
    let value = match path.extension().and_then(|s| s.to_str()) {
        Some("jsonl") => {
            let values = bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty())
                .map(serde_json::from_slice).collect::<std::result::Result<Vec<Value>, _>>()
                .context("unsafe operation journal")?;
            Value::Array(values)
        }
        Some("yaml" | "yml") => serde_yaml::from_slice(&bytes).context("unsafe deployment bookkeeping")?,
        _ => serde_json::from_slice(&bytes).context("unsafe secret retention bookkeeping")?,
    };
    Ok((value, fingerprint))
}
