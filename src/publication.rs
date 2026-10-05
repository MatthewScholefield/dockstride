//! Recoverable publication of ordinary settings and ownership/reference state.
//! Callers hold lifecycle -> global -> optional allocation -> config guards.
//! Recovery acquires recorded canonical file guards; publishing never reacquires them.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs::{self, File, OpenOptions}, io::{Read, Write}, os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}};

const LOCAL: &str = "publication";
const GLOBAL: &str = "publication-pending";
const LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum Kind { Replace, Create, Remove }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Change {
    path: PathBuf,
    kind: Kind,
    before: Option<String>,
    after: Option<String>,
    payload: Option<String>,
    mode: u32,
}

fn digest(bytes: &[u8]) -> String { hex::encode(Sha256::digest(bytes)) }

/// Canonicalize the parent, never follow a leaf symlink, including a dangling one.
fn canonical_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().context("publication path has no filename")?;
    let canonical = fs::canonicalize(parent)?.join(name);
    if let Ok(metadata) = fs::symlink_metadata(&canonical) {
        ensure!(metadata.is_file() && !metadata.file_type().is_symlink(), "publication requires an ordinary file: {}", canonical.display());
    }
    Ok(canonical)
}

fn fingerprint(path: &Path) -> Result<Option<String>> {
    let mut file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read publication file {}", path.display())),
    };
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() }, "publication file must be regular and owned by the current user: {}", path.display());
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        hasher.update(&buffer[..count]);
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

impl Change {
    pub fn replace(path: &Path, after: &[u8], mode: u32) -> Result<Self> { Self::new(path, Some(after), mode, Kind::Replace) }
    pub fn create(path: &Path, after: &[u8], mode: u32) -> Result<Self> { Self::new(path, Some(after), mode, Kind::Create) }
    pub fn remove(path: &Path) -> Result<Self> { Self::new(path, None, 0o600, Kind::Remove) }
    /// Bind this transition to the exact secure observation already validated by
    /// its planner, rather than accepting a newer file seen by the constructor.
    pub fn expect_before(&self, expected: &Option<String>) -> Result<()> {
        ensure!(&self.before == expected, "publication file changed after validation: {}", self.path.display());
        Ok(())
    }
    fn new(path: &Path, after: Option<&[u8]>, mode: u32, kind: Kind) -> Result<Self> {
        let path = canonical_path(path)?;
        validate_target(&path)?;
        let before = if path.exists() {
            let text = read_ordinary(&path)?;
            validate_payload(&path, &text, false)?;
            Some(digest(text.as_bytes()))
        } else { None };
        ensure!(kind != Kind::Create || before.is_none(), "create-new publication file already exists: {}", path.display());
        ensure!(kind != Kind::Remove || before.is_some(), "publication file to remove is absent: {}", path.display());
        let payload = after.map(|bytes| std::str::from_utf8(bytes).context("ordinary publication payload must be UTF-8").map(str::to_owned)).transpose()?;
        let change = Self { path, kind, before, after: after.map(digest), payload, mode };
        change.validate()?;
        Ok(change)
    }
    fn validate(&self) -> Result<()> {
        ensure!(normalized(&self.path), "publication path is not canonical: {}", self.path.display());
        validate_target(&self.path)?;
        ensure!(self.mode == 0o600, "ordinary publication files require mode 0600");
        for hash in [&self.before, &self.after].into_iter().flatten() {
            ensure!(hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()), "invalid publication fingerprint for {}", self.path.display());
        }
        match self.kind {
            Kind::Remove => ensure!(self.before.is_some() && self.after.is_none() && self.payload.is_none(), "invalid removal transition for {}", self.path.display()),
            Kind::Create | Kind::Replace => {
                ensure!(self.kind != Kind::Create || self.before.is_none(), "create-new transition has a before fingerprint");
                let payload = self.payload.as_ref().context("publication transition is missing ordinary payload")?;
                ensure!(payload.len() <= LIMIT && self.after.as_ref() == Some(&digest(payload.as_bytes())), "invalid publication payload fingerprint for {}", self.path.display());
                validate_payload(&self.path, payload, self.kind == Kind::Create)?;
            }
        }
        Ok(())
    }
}

fn state_name(path: &Path) -> Option<&str> {
    (path.parent()?.file_name()? == ".dockstride").then(|| path.file_name()?.to_str()).flatten()
}

fn normalized(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|part| matches!(part, std::path::Component::CurDir | std::path::Component::ParentDir))
}

fn validate_target(path: &Path) -> Result<()> {
    if let Some(name) = state_name(path) {
        ensure!(matches!(name, "ports.json" | "identity.json" | "port-reservations.json" | "environment-registry.json"), "state file is not an ordinary publication target: {}", path.display());
    } else {
        ensure!(!path.components().any(|part| part.as_os_str() == ".dockstride"), "private credential/state files cannot be ordinary settings targets: {}", path.display());
    }
    Ok(())
}

fn validate_payload(path: &Path, payload: &str, create: bool) -> Result<()> {
    if state_name(path).is_some() {
        validate_target(path)?;
        let value: Value = serde_json::from_str(payload).context("invalid ordinary reference-state payload")?;
        ensure!(value.is_object(), "ordinary reference state must be a mapping");
        validate_reference_state(&value)?;
        return Ok(());
    }
    validate_target(path)?;
    let local = path.file_name().is_some_and(|name| name == "env.yaml");
    let value = crate::sources::parse(payload, path, !local)?;
    if local { crate::config::check_secrets(&value)?; }
    if create && !local { ensure!(value == json!({}), "shared source creation permits only an empty mapping: {}", path.display()); }
    Ok(())
}

fn validate_reference_state(value: &Value) -> Result<()> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                ensure!(!matches!(key.to_ascii_lowercase().as_str(), "secrets" | "password" | "credential" | "credentials" | "contents" | "content" | "payload" | "privatekey" | "token"), "credential contents are forbidden in ordinary reference state");
                validate_reference_state(child)?;
            }
        }
        Value::Array(values) => for child in values { validate_reference_state(child)?; },
        _ => {},
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Journal {
    schema_version: u32,
    id: String,
    root: PathBuf,
    operation: String,
    changes: Vec<Change>,
    claims: Value,
}

impl Journal {
    fn validate(&self, root: &Path, global: &Path) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported publication journal schema {}", self.schema_version);
        ensure!(self.id.len() == 32 && self.id.bytes().all(|b| b.is_ascii_hexdigit()), "invalid publication operation ID");
        ensure!(self.root == root && normalized(root), "publication journal checkout mismatch");
        ensure!(!self.operation.is_empty() && self.operation.len() <= 128 && self.operation.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "invalid publication operation name");
        ensure!(self.claims.is_object(), "publication claims must be a mapping");
        validate_claims(&self.claims)?;
        let mut paths = BTreeSet::new();
        let mut rank = 0;
        for change in &self.changes {
            change.validate()?;
            ensure!(paths.insert(&change.path), "duplicate publication path: {}", change.path.display());
            if let Some(name) = state_name(&change.path) {
                let expected = if matches!(name, "port-reservations.json" | "environment-registry.json") { global } else { root };
                ensure!(change.path.parent() == Some(expected.join(".dockstride").as_path()), "reference state is outside its publication owner: {}", change.path.display());
            } else if change.path != root.join("env.yaml") && change.kind != Kind::Remove {
                let value = crate::sources::parse(change.payload.as_ref().context("shared transition is missing payload")?, &change.path, true)?;
                if change.kind == Kind::Create { ensure!(value == json!({}), "shared source creation permits only an empty mapping"); }
            }
            let next = change_rank(change);
            ensure!(next >= rank, "global reservations must precede local publication and registry commit must be last");
            rank = next;
        }
        Ok(())
    }
    fn summary(&self) -> Value {
        json!({"schemaVersion":1,"pending":true,"id":self.id,"root":self.root,"operation":self.operation,"claims":self.claims,"paths":self.changes.iter().map(|change| json!({"path":change.path,"kind":change.kind})).collect::<Vec<_>>()})
    }
}

// Claims are collision/reservation intents, not arbitrary project-command output.
fn validate_claims(value: &Value) -> Result<()> {
    let object = value.as_object().context("publication claims must be a mapping")?;
    ensure!(object.keys().all(|key| matches!(key.as_str(), "project" | "daemon" | "daemonId" | "owner" | "ownerId" | "backend" | "context" | "connection" | "root" | "sources" | "endpoints" | "reservations")), "unsupported publication claim field");
    validate_reference_state(value)?;
    for (key, value) in object {
        if matches!(key.as_str(), "sources" | "endpoints" | "reservations") { continue; }
        ensure!(value.is_string(), "publication claim {key} must be a string");
    }
    Ok(())
}

fn change_rank(change: &Change) -> u8 {
    match state_name(&change.path) { Some("port-reservations.json") => 0, Some("environment-registry.json") => 2, _ => 1 }
}

fn global_journals(global: &Path) -> Result<Vec<Journal>> {
    let value = crate::state::read(global, GLOBAL)?;
    if value.is_null() { return Ok(Vec::new()); }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields, rename_all = "camelCase")]
    struct Pending { schema_version: u32, operations: Vec<Journal> }
    let pending: Pending = serde_json::from_value(value).context("malformed global publication intents")?;
    ensure!(pending.schema_version == 1, "unsupported global publication schema {}", pending.schema_version);
    let mut roots = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for journal in &pending.operations {
        journal.validate(&journal.root, global)?;
        ensure!(roots.insert(&journal.root) && ids.insert(&journal.id), "duplicate global publication intent");
    }
    Ok(pending.operations)
}

fn save_global(global: &Path, journals: &[Journal]) -> Result<()> {
    crate::state::save(global, GLOBAL, &json!({"schemaVersion":1,"operations":journals}))
}

fn load(root: &Path, global: &Path) -> Result<Option<Journal>> {
    let local = crate::state::read(root, LOCAL)?;
    let local = if local.is_null() { None } else { Some(serde_json::from_value::<Journal>(local).context("malformed local publication journal")?) };
    if let Some(journal) = &local { journal.validate(root, global)?; }
    let remote = global_journals(global)?.into_iter().find(|journal| journal.root == root);
    if let (Some(local), Some(remote)) = (&local, &remote) { ensure!(local == remote, "local and global publication intents disagree for {}; pending claims retained", root.display()); }
    Ok(local.or(remote))
}

fn check_transitions(journal: &Journal) -> Result<()> {
    for change in &journal.changes {
        let current = check_transition(change)?;
        if change.kind == Kind::Remove && current.is_some() && state_name(&change.path).is_none() && change.path != journal.root.join("env.yaml") {
            crate::sources::parse(&read_ordinary(&change.path)?, &change.path, true)?;
        }
    }
    Ok(())
}

fn check_transition(change: &Change) -> Result<Option<String>> {
    ensure!(canonical_path(&change.path)? == change.path, "pending publication path identity changed at {}; pending claims retained", change.path.display());
    let current = fingerprint(&change.path)?;
    if change.kind == Kind::Remove && current.is_some() {
        let text = read_ordinary(&change.path)?;
        ensure!(current.as_ref() == Some(&digest(text.as_bytes())), "pending publication file changed while validating deletion at {}; pending claims retained", change.path.display());
        validate_payload(&change.path, &text, false)?;
    }
    ensure!(current == change.before || current == change.after, "pending publication conflict at {}: file matches neither recorded before nor after state; restore the intended file or resolve the external edit before retrying; pending claims retained", change.path.display());
    Ok(current)
}

fn read_ordinary(path: &Path) -> Result<String> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)
        .with_context(|| format!("read ordinary publication file {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() }, "ordinary publication file must be regular and current-user owned: {}", path.display());
    let mut bytes = Vec::new();
    file.take((LIMIT + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= LIMIT, "ordinary publication file exceeds 1 MiB: {}", path.display());
    String::from_utf8(bytes).with_context(|| format!("ordinary publication file is not UTF-8: {}", path.display()))
}

/// Stage a durable operation without applying any file transition. Caller owns all
/// publication guards, including canonical file guards. Intended global claims
/// become blockers before the local copy; a crash between copies remains recoverable.
pub fn stage_locked(root: &Path, operation: &str, changes: Vec<Change>, claims: Value) -> Result<()> {
    stage_at(root, &crate::state::global_root()?, operation, changes, claims)
}

fn stage_at(root: &Path, global: &Path, operation: &str, mut changes: Vec<Change>, claims: Value) -> Result<()> {
    let root = fs::canonicalize(root)?;
    crate::state::prepare(global)?;
    let global = fs::canonicalize(global)?;
    ensure!(load(&root, &global)?.is_none(), "pending publication for {}; recover it before starting another operation", root.display());
    changes.sort_by_key(change_rank);
    let journal = Journal { schema_version: 1, id: crate::state::random_id()?, root, operation: operation.to_owned(), changes, claims };
    journal.validate(&journal.root, &global)?;
    // Fresh staging must match before, not merely an after value.
    for change in &journal.changes {
        ensure!(canonical_path(&change.path)? == change.path, "publication path identity changed before staging: {}", change.path.display());
        ensure!(fingerprint(&change.path)? == change.before, "publication file changed before staging: {}", change.path.display());
    }
    let mut journals = global_journals(&global)?;
    for pending in &journals {
        ensure!(!pending.changes.iter().any(|old| journal.changes.iter().any(|new| old.path == new.path)),
            "publication overlaps pending operation {} in {}; recover its recorded files before continuing", pending.id, pending.root.display());
    }
    journals.push(journal.clone());
    save_global(&global, &journals)?;
    crate::state::save(&journal.root, LOCAL, &serde_json::to_value(&journal)?)
}

/// Publish under the caller's source guards; no reentrant guard acquisition.
pub fn publish(root: &Path, operation: &str, changes: Vec<Change>, claims: Value) -> Result<Value> {
    stage_locked(root, operation, changes, claims)?;
    let root = fs::canonicalize(root)?;
    let global = fs::canonicalize(crate::state::global_root()?)?;
    let journal = load(&root, &global)?.context("staged publication disappeared")?;
    complete(&journal, &global)
}

/// Caller owns lifecycle/global/config guards but NOT canonical source guards.
pub fn recover_locked(root: &Path) -> Result<Value> {
    recover_at(root, &crate::state::global_root()?)
}

fn recover_at(root: &Path, global: &Path) -> Result<Value> {
    let root = fs::canonicalize(root)?;
    let global = if global.exists() { fs::canonicalize(global)? } else { global.to_owned() };
    let Some(journal) = load(&root, &global)? else { return Ok(json!({"pending":false,"recovered":false})); };
    let _files = crate::sources::lock_paths(journal.changes.iter().map(|change| change.path.clone()))?;
    complete(&journal, &global)
}

fn complete(journal: &Journal, global: &Path) -> Result<Value> {
    // These are after source guards in the lock hierarchy. Journal state names
    // are excluded from Change, so saving the mirror cannot reacquire a guard.
    let mut state_paths = journal.changes.iter().filter(|change| state_name(&change.path).is_some()).map(|change| &change.path).collect::<Vec<_>>();
    state_paths.sort();
    let _states = state_paths.into_iter().map(|path| {
        let owner = path.parent().unwrap().parent().unwrap();
        let name = path.file_stem().unwrap().to_str().context("non-UTF-8 reference-state name")?;
        crate::state::lock(owner, &format!("state-{name}"))
    }).collect::<Result<Vec<_>>>()?;
    // Validate every transition before continuing even one partially applied file.
    check_transitions(journal)?;
    let mut journals = global_journals(global)?;
    if !journals.iter().any(|entry| entry.id == journal.id) {
        journals.push(journal.clone());
        save_global(global, &journals)?;
    }
    crate::state::save(&journal.root, LOCAL, &serde_json::to_value(journal)?)?;
    for change in &journal.changes {
        let current = check_transition(change)?;
        if current == change.after { continue; }
        match change.kind {
            Kind::Remove => {
                fs::remove_file(&change.path).with_context(|| format!("remove publication file {}", change.path.display()))?;
                File::open(change.path.parent().unwrap())?.sync_all()?;
            }
            Kind::Replace => crate::state::atomic_write(&change.path, change.payload.as_ref().unwrap().as_bytes(), change.mode)?,
            Kind::Create => create_new(change)?,
        }
    }
    check_transitions(journal)?;
    for change in &journal.changes { ensure!(fingerprint(&change.path)? == change.after, "publication final verification conflict at {}; pending claims retained", change.path.display()); }
    // Clear the local copy first: an interruption still has a global blocker and
    // a complete recovery mirror, including the committed registry transition.
    remove_state(&journal.root, LOCAL)?;
    journals.retain(|entry| entry.id != journal.id);
    save_global(global, &journals)?;
    Ok(json!({"pending":false,"recovered":true,"operation":journal.operation,"id":journal.id}))
}

fn remove_state(root: &Path, name: &str) -> Result<()> {
    let _guard = crate::state::lock(root, &format!("state-{name}"))?;
    let path = root.join(".dockstride").join(format!("{name}.json"));
    match fs::remove_file(&path) {
        Ok(()) => File::open(path.parent().unwrap())?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn create_new(change: &Change) -> Result<()> {
    let parent = change.path.parent().unwrap();
    let temporary = parent.join(format!(".dks-create-{}", crate::state::random_id()?));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(change.mode).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(change.mode))?;
        file.write_all(change.payload.as_ref().unwrap().as_bytes())?;
        file.sync_all()?;
        // Hard linking an already durable inode gives create-new publication
        // without a crash window exposing a partially written destination.
        fs::hard_link(&temporary, &change.path).with_context(|| format!("create-new publication {}", change.path.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let cleanup = fs::remove_file(&temporary);
    if cleanup.is_ok() { File::open(parent)?.sync_all()?; }
    result
}

/// Read-only, sanitized status: ordinary claims/paths, never payload or fingerprints.
pub fn pending(root: &Path) -> Result<Value> {
    let root = fs::canonicalize(root)?;
    let global_path = crate::state::global_root()?;
    let global = if global_path.exists() { fs::canonicalize(&global_path)? } else { global_path };
    Ok(match load(&root, &global)? { Some(journal) => journal.summary(), None => Value::Null })
}

pub fn pending_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let root = fs::canonicalize(root)?;
    let global_path = crate::state::global_root()?;
    let global = if global_path.exists() { fs::canonicalize(&global_path)? } else { global_path };
    Ok(load(&root, &global)?.map(|journal| journal.changes.into_iter().map(|change| change.path).collect()).unwrap_or_default())
}

pub fn pending_global() -> Result<Value> {
    let global_path = crate::state::global_root()?;
    let global = if global_path.exists() { fs::canonicalize(&global_path)? } else { global_path };
    Ok(json!({"schemaVersion":1,"operations":global_journals(&global)?.iter().map(Journal::summary).collect::<Vec<_>>()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("checkout");
        let global = directory.path().join("global");
        fs::create_dir(&root)?;
        crate::state::prepare(&root)?;
        crate::state::prepare(&global)?;
        Ok((directory, root, global))
    }

    fn changes(root: &Path, global: &Path) -> Result<Vec<Change>> {
        Ok(vec![
            Change::replace(&global.join(".dockstride/port-reservations.json"), b"{\"127.0.0.1:4200/tcp\":\"checkout\"}\n", 0o600)?,
            Change::create(&root.join("shared.yaml"), b"{}\n", 0o600)?,
            Change::replace(&root.join("env.yaml"), b"project: example\nport: 4200\n", 0o600)?,
            Change::replace(&global.join(".dockstride/environment-registry.json"), b"{\"example\":{\"root\":\"checkout\"}}\n", 0o600)?,
        ])
    }

    fn apply_prefix(journal: &Journal, count: usize) -> Result<()> {
        for change in journal.changes.iter().take(count) {
            match change.kind {
                Kind::Create => create_new(change)?,
                Kind::Replace => crate::state::atomic_write(&change.path, change.payload.as_ref().unwrap().as_bytes(), change.mode)?,
                Kind::Remove => {
                    fs::remove_file(&change.path)?;
                    File::open(change.path.parent().unwrap())?.sync_all()?;
                }
            }
        }
        Ok(())
    }

    #[test]
    fn interrupted_file_prefixes_resume_without_losing_claims() -> Result<()> {
        for count in 0..=4 {
            let (_directory, root, global) = fixture()?;
            stage_at(&root, &global, "setup", changes(&root, &global)?, json!({"project":"example","daemonId":"daemon-one"}))?;
            let journal = load(&root, &global)?.unwrap();
            apply_prefix(&journal, count)?;
            assert_eq!(global_journals(&global)?[0].claims["project"], "example");
            assert_eq!(recover_at(&root, &global)?["recovered"], true);
            assert!(load(&root, &global)?.is_none());
            assert!(global_journals(&global)?.is_empty());
            assert_eq!(fs::read_to_string(root.join("env.yaml"))?, "project: example\nport: 4200\n");
            assert_eq!(fs::read_to_string(root.join("shared.yaml"))?, "{}\n");
            assert_eq!(crate::state::read(&global, "environment-registry")?["example"]["root"], "checkout");
        }
        Ok(())
    }

    #[test]
    fn either_durable_mirror_recovers_and_clear_interruption_is_idempotent() -> Result<()> {
        for (global_only, applied) in [(true, false), (false, false), (true, true)] {
            let (_directory, root, global) = fixture()?;
            stage_at(&root, &global, "setup", changes(&root, &global)?, json!({}))?;
            let journal = load(&root, &global)?.unwrap();
            if applied { apply_prefix(&journal, journal.changes.len())?; }
            if global_only { remove_state(&root, LOCAL)?; } else { remove_state(&global, GLOBAL)?; }
            assert!(load(&root, &global)?.is_some());
            recover_at(&root, &global)?;
            assert!(load(&root, &global)?.is_none());
            assert_eq!(fs::read_to_string(root.join("env.yaml"))?, "project: example\nport: 4200\n");
        }
        Ok(())
    }

    #[test]
    fn external_edit_blocks_every_continuation_and_preserves_both_mirrors() -> Result<()> {
        let (_directory, root, global) = fixture()?;
        stage_at(&root, &global, "setup", changes(&root, &global)?, json!({"project":"example"}))?;
        let local_before = crate::state::read(&root, LOCAL)?;
        let global_before = crate::state::read(&global, GLOBAL)?;
        fs::write(root.join("env.yaml"), "project: external\n")?;
        let error = recover_at(&root, &global).unwrap_err().to_string();
        assert!(error.contains(root.join("env.yaml").to_str().unwrap()));
        assert!(error.contains("pending claims retained"));
        assert!(!global.join(".dockstride/port-reservations.json").exists());
        assert!(!root.join("shared.yaml").exists());
        assert_eq!(fs::read_to_string(root.join("env.yaml"))?, "project: external\n");
        assert_eq!(crate::state::read(&root, LOCAL)?, local_before);
        assert_eq!(crate::state::read(&global, GLOBAL)?, global_before);
        fs::remove_file(root.join("env.yaml"))?;
        recover_at(&root, &global)?;
        assert_eq!(fs::read_to_string(root.join("env.yaml"))?, "project: example\nport: 4200\n");
        Ok(())
    }

    #[test]
    fn create_new_never_truncates_external_source_and_deletion_resumes() -> Result<()> {
        let (_directory, root, global) = fixture()?;
        let source = root.join("shared.yaml");
        stage_at(&root, &global, "config-sources", vec![Change::create(&source, b"{}\n", 0o600)?], json!({}))?;
        fs::write(&source, "project: external\n")?;
        assert!(recover_at(&root, &global).is_err());
        assert_eq!(fs::read_to_string(&source)?, "project: external\n");
        fs::remove_file(&source)?;
        recover_at(&root, &global)?;
        let ports = root.join(".dockstride/ports.json");
        crate::state::atomic_write(&ports, b"{\"port\":4200}", 0o600)?;
        stage_at(&root, &global, "ports-release", vec![Change::remove(&ports)?], json!({}))?;
        fs::remove_file(&ports)?;
        recover_at(&root, &global)?;
        assert!(!ports.exists());
        assert!(load(&root, &global)?.is_none());
        Ok(())
    }

    #[test]
    fn malformed_transitions_and_credentials_are_rejected_without_staging() -> Result<()> {
        let (_directory, root, global) = fixture()?;
        assert!(Change::replace(&root.join(".dockstride/credentials.json"), b"{\"password\":\"private\"}", 0o600).is_err());
        assert!(Change::remove(&root.join(".dockstride/locks")).is_err());
        assert!(Change::replace(&root.join("env.yaml"), b"secrets:\n  password: private\n", 0o600).is_err());
        assert!(Change::create(&root.join("shared.yaml"), b"password: private\n", 0o600).is_err());
        assert!(Change::replace(&root.join(".dockstride/identity.json"), b"{\"password\":\"private\"}", 0o600).is_err());
        let change = Change::replace(&root.join("env.yaml"), b"project: example\n", 0o600)?;
        assert!(stage_at(&root, &global, "setup", vec![change.clone(), change.clone()], json!({})).is_err());
        let mut invalid = change.clone();
        invalid.payload = Some("project: changed\n".into());
        assert!(stage_at(&root, &global, "setup", vec![invalid], json!({})).is_err());
        assert!(load(&root, &global)?.is_none());
        stage_at(&root, &global, "setup", vec![change], json!({}))?;
        let before = crate::state::read(&root, LOCAL)?;
        let mut invalid = crate::state::read(&global, GLOBAL)?;
        invalid["operations"][0]["schemaVersion"] = json!(9);
        crate::state::save(&global, GLOBAL, &invalid)?;
        assert!(recover_at(&root, &global).is_err());
        assert_eq!(crate::state::read(&root, LOCAL)?, before);
        assert_eq!(crate::state::read(&global, GLOBAL)?, invalid);
        assert!(!root.join("env.yaml").exists());
        Ok(())
    }

    #[test]
    fn sanitized_reads_retain_claims_and_overlapping_operations_are_blocked() -> Result<()> {
        let (_directory, root, global) = fixture()?;
        stage_at(&root, &global, "setup", changes(&root, &global)?, json!({"project":"example","daemonId":"one"}))?;
        let before = crate::state::read(&global, GLOBAL)?;
        let journal = load(&root, &global)?.unwrap();
        let summary = journal.summary();
        assert_eq!(summary["claims"]["daemonId"], "one");
        assert_eq!(summary["paths"][0]["path"], global.join(".dockstride/port-reservations.json").to_str().unwrap());
        assert!(summary.get("payload").is_none());
        assert!(summary["paths"][0].get("before").is_none());
        assert!(summary["paths"][0].get("after").is_none());
        assert_eq!(crate::state::read(&global, GLOBAL)?, before);
        let second = root.parent().unwrap().join("second");
        fs::create_dir(&second)?;
        let conflicting = Change::replace(&global.join(".dockstride/port-reservations.json"), b"{}", 0o600)?;
        assert!(stage_at(&second, &global, "setup", vec![conflicting], json!({})).is_err());
        assert_eq!(crate::state::read(&global, GLOBAL)?, before);
        assert!(!second.join(".dockstride").exists());
        Ok(())
    }

    #[test]
    fn secure_observation_rejects_newer_state_and_symlinked_journals() -> Result<()> {
        let (_directory, root, global) = fixture()?;
        crate::state::save(&root, "ports", &json!({"port":4200}))?;
        let (observed, before) = crate::state::read_fingerprinted(&root, "ports")?;
        assert_eq!(observed["port"], 4200);
        crate::state::save(&root, "ports", &json!({"port":4201}))?;
        let stale = Change::replace(&root.join(".dockstride/ports.json"), b"{\"port\":4202}", 0o600)?;
        assert!(stale.expect_before(&before).is_err());
        assert_eq!(crate::state::read(&root, "ports")?["port"], 4201);
        assert!(load(&root, &global)?.is_none());
        stage_at(&root, &global, "setup", changes(&root, &global)?, json!({}))?;
        let global_before = crate::state::read(&global, GLOBAL)?;
        let local = root.join(".dockstride/publication.json");
        let target = root.join("external");
        fs::write(&target, "{}")?;
        fs::remove_file(&local)?;
        std::os::unix::fs::symlink(&target, &local)?;
        assert!(recover_at(&root, &global).is_err());
        assert_eq!(fs::read_to_string(&target)?, "{}");
        assert_eq!(crate::state::read(&global, GLOBAL)?, global_before);
        assert!(!root.join("env.yaml").exists());
        Ok(())
    }
}

