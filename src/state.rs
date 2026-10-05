use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// An advisory cross-process lock. Dropping it releases the lock, including on errors.
pub struct Lock {
    file: File,
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid state name {name:?}"
    );
    Ok(())
}

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{} must be an ordinary directory",
        path.display()
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "{} must be owned by the current user",
        path.display()
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Prepare private scratch storage without retaining a publication lock.
pub(crate) fn prepare(root: &Path) -> Result<()> {
    private_dir(&root.join(".dockstride"))
}

pub fn lock(root: &Path, name: &str) -> Result<Lock> {
    valid_name(name)?;
    private_dir(&root.join(".dockstride"))?;
    let directory = root.join(".dockstride/locks");
    private_dir(&directory)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join(format!("{name}.lock")))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() },
        "lock file must be a regular file owned by the current user"
    );
    file.lock_exclusive()
        .context("acquire project operation lock")?;
    Ok(Lock { file })
}

/// Shared registry/allocation coordination. Keep HOME-based compatibility explicit.
pub fn global_root() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME required for environment/allocation coordination")?)
        .join(".local/share/dockstride"))
}

/// Caller must hold its own checkout lifecycle lock before taking this lock.
/// Never acquire a different checkout lifecycle lock while holding it.
pub fn global_lock() -> Result<Lock> {
    lock(&global_root()?, "environment-allocation")
}

pub fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("operating-system randomness: {error}"))?;
    Ok(hex::encode(bytes))
}

/// Publish a complete file on the same filesystem, durably and without a partial reader view.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        directory.is_dir(),
        "parent directory does not exist: {}",
        directory.display()
    );
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "refusing to replace non-regular file {}",
            path.display()
        );
    }
    let temporary = directory.join(format!(".dks-write-{}", random_id()?));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))?;
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn state_path(root: &Path, name: &str) -> Result<PathBuf> {
    valid_name(name)?;
    Ok(root.join(".dockstride").join(format!("{name}.json")))
}

fn read_private(root: &Path, path: &Path) -> Result<Option<Vec<u8>>> {
    let directory = root.join(".dockstride");
    let directory_file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&directory)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("open private state directory {}", directory.display()));
        }
    };
    let metadata = directory_file.metadata()?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "{} must be a private ordinary directory owned by the current user",
        directory.display()
    );
    let filename = CString::new(
        path.file_name()
            .context("state filename is missing")?
            .as_bytes(),
    )?;
    // openat pins the checked directory even if its path is concurrently replaced.
    let descriptor = unsafe {
        libc::openat(
            directory_file.as_raw_fd(),
            filename.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("open private state {}", path.display()));
    }
    // SAFETY: openat returned a new owned descriptor that is transferred exactly once.
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "{} must be a private regular file owned by the current user",
        path.display()
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

pub fn read(root: &Path, name: &str) -> Result<Value> {
    let path = state_path(root, name)?;
    match read_private(root, &path)? {
        Some(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("read state {}", path.display()))
        }
        None => Ok(Value::Null),
    }
}

/// Parse and fingerprint the same secure observation for publication planning.
pub(crate) fn read_fingerprinted(root: &Path, name: &str) -> Result<(Value, Option<String>)> {
    let path = state_path(root, name)?;
    match read_private(root, &path)? {
        Some(bytes) => {
            let value = serde_json::from_slice(&bytes)
                .with_context(|| format!("read state {}", path.display()))?;
            Ok((value, Some(hex::encode(Sha256::digest(&bytes)))))
        }
        None => Ok((Value::Null, None)),
    }
}

fn save_unlocked(root: &Path, name: &str, value: &Value) -> Result<()> {
    private_dir(&root.join(".dockstride"))?;
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write(&state_path(root, name)?, &bytes, 0o600)
}

pub fn save(root: &Path, name: &str, value: &Value) -> Result<()> {
    let _lock = lock(root, &format!("state-{name}"))?;
    save_unlocked(root, name, value)
}

/// Persist an ownership token independently of the human-facing project name.
pub fn ensure_identity(root: &Path, project: &str, backend: &str, context: &str) -> Result<Value> {
    let _lock = lock(root, "state-identity")?;
    let mut identity = read(root, "identity")?;
    let canonical = fs::canonicalize(root)?.to_string_lossy().into_owned();
    if identity.is_null() {
        identity = json!({"id": random_id()?, "root": canonical, "project": project, "backend": backend, "context": context, "resources": false});
    } else {
        ensure!(
            identity["id"].as_str().is_some(),
            "invalid persisted ownership identity"
        );
        for (field, supplied) in [
            ("root", canonical.as_str()),
            ("project", project),
            ("backend", backend),
            ("context", context),
        ] {
            if identity[field].as_str() != Some(supplied) {
                ensure!(
                    identity["resources"].as_bool() == Some(false),
                    "{field} changed while owned resources exist; explicitly tear down the previous deployment before transitioning"
                );
                identity[field] = json!(supplied);
            }
        }
    }
    save_unlocked(root, "identity", &identity)?;
    Ok(identity)
}

pub fn mark_resources(root: &Path, present: bool) -> Result<()> {
    let _lock = lock(root, "state-identity")?;
    let mut identity = read(root, "identity")?;
    ensure!(
        identity.is_object(),
        "ownership identity must be established before recording resources"
    );
    identity["resources"] = json!(present);
    save_unlocked(root, "identity", &identity)
}

/// Pin production secret ownership to its original Swarm without nested flock acquisition.
pub fn pin_secret_cluster(root: &Path, cluster: &str) -> Result<()> {
    ensure!(!cluster.is_empty(), "Swarm cluster identity is required");
    let _lock = lock(root, "state-identity")?;
    let mut identity = read(root, "identity")?;
    ensure!(
        identity["id"].as_str().is_some(),
        "ownership identity must be established before pinning a secret cluster"
    );
    ensure!(
        identity
            .get("secretCluster")
            .is_none_or(|previous| previous.as_str() == Some(cluster)),
        "secret ownership is pinned to another Swarm cluster; recover or remove its secrets before transitioning"
    );
    identity["secretCluster"] = json!(cluster);
    save_unlocked(root, "identity", &identity)
}

/// Append a durable machine-readable event. Interrupted operations retain completed events.
pub fn journal(root: &Path, operation: &str, event: &Value) -> Result<()> {
    valid_name(operation)?;
    let _lock = lock(root, &format!("journal-{operation}"))?;
    let path = root
        .join(".dockstride")
        .join(format!("journal-{operation}.jsonl"));
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "operation journal must be a private regular file owned by the current user"
    );
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let mut bytes =
        serde_json::to_vec(&json!({"timestamp_ms":timestamp,"operation":operation,"event":event}))?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn journal_events(root: &Path, operation: &str) -> Result<Vec<Value>> {
    valid_name(operation)?;
    let path = root
        .join(".dockstride")
        .join(format!("journal-{operation}.jsonl"));
    let contents = match read_private(root, &path)? {
        Some(bytes) => String::from_utf8(bytes).context("operation journal is not UTF-8")?,
        None => return Ok(Vec::new()),
    };
    let mut events = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        match serde_json::from_str(line) {
            Ok(event) => events.push(event),
            Err(_) => bail!(
                "operation journal {operation} contains an incomplete event at line {}; inspect it before recovery",
                index + 1
            ),
        }
    }
    Ok(events)
}
