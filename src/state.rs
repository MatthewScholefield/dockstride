use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

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

pub(crate) fn private_dir(path: &Path) -> Result<()> {
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

