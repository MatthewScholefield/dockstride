//! Live, recursive ordinary-settings layers. Raw documents stay separate from effective values.
use anyhow::{Context, Result, ensure, bail};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, fs, io, path::{Component, Path, PathBuf}};

#[derive(Clone, Debug, Serialize)]
pub struct Origin { pub file: PathBuf, pub path: String }
#[derive(Clone, Debug, Serialize)]
pub struct Provenance { pub file: PathBuf, pub path: String, pub overridden: Vec<Origin> }
#[derive(Clone, Debug)]
pub struct EnvironmentSnapshot {
    pub local: Value,
    pub local_file: PathBuf,
    pub values: Value,
    pub sources: Vec<PathBuf>,
    pub provenance: BTreeMap<String, Provenance>,
    pub fingerprints: BTreeMap<PathBuf, Option<String>>,
    pub(crate) shared_values: Value,
    pub(crate) shared_documents: BTreeMap<PathBuf, Value>,
}

impl EnvironmentSnapshot {
    pub fn verify(&self) -> Result<()> {
        ensure!(unchanged(self)?, "configuration or shared sources changed; rerun with current settings");
        Ok(())
    }

    /// Ensure an editable raw document belongs to this observation, not a
    /// different read performed before or after the snapshot was captured.
    pub(crate) fn verify_text(&self, path: &Path, text: &str) -> Result<()> {
        let expected = self.fingerprints.get(path)
            .context("editable document is outside the configuration snapshot")?;
        let matches = match expected {
            Some(expected) => *expected == hex::encode(Sha256::digest(text.as_bytes())),
            None => path == self.local_file && text.is_empty(),
        };
        ensure!(matches, "configuration or shared sources changed; rerun with current settings");
        Ok(())
    }
}

pub fn expand_home(path: &Path) -> Result<PathBuf> {
    expand_home_with(path, std::env::var_os("HOME").as_deref().map(Path::new))
}

fn expand_home_with(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    if !path.as_os_str().as_encoded_bytes().starts_with(b"~/") { return Ok(path.to_owned()); }
    let home = home.filter(|home| !home.as_os_str().is_empty() && home.is_absolute())
        .context("expanding ~/ requires HOME to be a nonempty absolute path")?;
    Ok(home.join(path.strip_prefix("~")?))
}

pub fn portable_home(path: &Path) -> PathBuf {
    portable_home_with(path, std::env::var_os("HOME").as_deref().map(Path::new))
}

fn portable_home_with(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home.filter(|home| home.is_absolute()) else { return path.to_owned(); };
    if !path.is_absolute() || path.components().chain(home.components()).any(|part| part == Component::ParentDir) {
        return path.to_owned();
    }
    match path.strip_prefix(home) {
        Ok(suffix) => PathBuf::from("~/").join(suffix),
        Err(_) => path.to_owned(),
    }
}

pub fn relative_path(base: &Path, target: &Path) -> PathBuf {
    if !base.is_absolute() || !target.is_absolute()
        || base.components().chain(target.components()).any(|part| part == Component::ParentDir) {
        return target.to_owned();
    }
    let base = base.components().collect::<Vec<_>>();
    let target_parts = target.components().collect::<Vec<_>>();
    let common = base.iter().zip(&target_parts).take_while(|(left, right)| left == right).count();
    if common == 0 { return target.to_owned(); }
    let mut relative = PathBuf::new();
    for _ in &base[common..] { relative.push(".."); }
    for part in &target_parts[common..] { relative.push(part.as_os_str()); }
    if relative.as_os_str().is_empty() { relative.push("."); }
    relative
}

pub(crate) fn identity(path: &Path) -> Result<PathBuf> {
    if path.exists() { Ok(fs::canonicalize(path)?) }
    else { Ok(fs::canonicalize(path.parent().context("configuration path has no parent")?)?.join(path.file_name().context("configuration path has no name")?)) }
}

pub fn fingerprint(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(hex::encode(Sha256::digest(bytes)))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading configuration {}", path.display())),
    }
}

/// Bind the observed value to the exact bytes used for its fingerprint.
fn read_document(path: &Path, shared: bool) -> Result<(Value, Option<String>)> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if !shared && error.kind() == io::ErrorKind::NotFound => return Ok((json!({}), None)),
        Err(error) => return Err(error).with_context(|| format!("reading configuration {}", path.display())),
    };
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("configuration {} is not UTF-8", path.display()))?;
    let value = if shared { parse(text, path)? } else { crate::config::parse_document(text)? };
    Ok((value, Some(hex::encode(Sha256::digest(&bytes)))))
}

pub fn parse(text: &str, path: &Path) -> Result<Value> {
    let mut value: Value = serde_yaml::from_str(text).with_context(|| format!("invalid configuration {}", path.display()))?;
    if value.is_null() { value = json!({}); }
    ensure!(value.is_object(), "{} must contain a configuration mapping", path.display());
    crate::config::check_secrets(&value)?;
    descriptors(&value)?;
    ensure!(value.pointer("/_dockstride/swarmSecrets").is_none(), "Swarm secret bindings must remain checkout-local, not in shared source {}", path.display());
    Ok(value)
}

pub fn descriptors(local: &Value) -> Result<Option<Vec<PathBuf>>> {
    let Some(metadata) = local.get("_dockstride") else { return Ok(None) };
    let metadata = metadata.as_object().context("_dockstride must be a mapping")?;
    ensure!(metadata.keys().all(|key| key == "sources" || key == "swarmSecrets"), "unknown _dockstride metadata key");
    swarm_bindings(local)?;
    let Some(sources) = metadata.get("sources") else { return Ok(None) };
    let sources = sources.as_array().context("_dockstride.sources must be a list")?;
    sources.iter().map(|source| {
        let descriptor = source.as_object().context("each shared source must be a {path: file} mapping")?;
        ensure!(descriptor.len() == 1 && descriptor.contains_key("path"), "persisted source descriptors permit only path");
        let path = descriptor["path"].as_str().filter(|path| !path.is_empty()).context("source path must be a nonempty string")?;
        Ok(PathBuf::from(path))
    }).collect::<Result<Vec<_>>>().map(Some)
}

/// Current immutable Docker objects, kept only in the local raw document.
pub fn swarm_bindings(local: &Value) -> Result<BTreeMap<String, String>> {
    let Some(bindings) = local.pointer("/_dockstride/swarmSecrets") else { return Ok(BTreeMap::new()); };
    bindings.as_object().context("_dockstride.swarmSecrets must be a mapping")?.iter().map(|(name, object)| {
        crate::secrets::valid_name(name)?;
        let object = object.as_str().context("Swarm secret binding must be a Docker object name")?;
        crate::secrets::valid_name(object)?;
        Ok((name.clone(), object.to_owned()))
    }).collect()
}

pub fn merge(target: &mut Value, overlay: &Value) {
    if let (Some(target), Some(overlay)) = (target.as_object_mut(), overlay.as_object()) {
        for (key, value) in overlay {
            if key == "secrets" {
                if let (Some(old), Some(refs)) = (target.get_mut(key).and_then(Value::as_object_mut), value.as_object()) {
                    old.extend(refs.iter().map(|(name, reference)| (name.clone(), reference.clone())));
                    continue;
                }
            }
            match target.get_mut(key) { Some(old) => merge(old, value), None => { target.insert(key.clone(), value.clone()); } }
        }
    } else { *target = overlay.clone(); }
}

fn origin_paths(value: &Value, prefix: &str, result: &mut Vec<String>) {
    if !prefix.is_empty() { result.push(prefix.to_owned()); }
    if let Some(object) = value.as_object() {
        for (key, value) in object { origin_paths(value, &if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") }, result); }
    }
}

fn apply(snapshot: &mut EnvironmentSnapshot, value: &Value, file: &Path) {
    let mut value = value.clone();
    value.as_object_mut().unwrap().remove("_dockstride");
    let mut paths = Vec::new();
    origin_paths(&value, "", &mut paths);
    // A scalar/list/null replaces the entire subtree, including its winning origins.
    for path in &paths {
        let selected = path.split('.').try_fold(&value, |value, key| value.get(key));
        if selected.is_some_and(|value| !value.is_object()) {
            snapshot.provenance.retain(|old, _| !old.strip_prefix(path.as_str()).is_some_and(|suffix| suffix.starts_with('.')));
        }
    }
    for path in paths {
        let mut overridden = Vec::new();
        if let Some(old) = snapshot.provenance.remove(&path) {
            overridden = old.overridden;
            overridden.push(Origin { file: old.file, path: old.path });
        }
        snapshot.provenance.insert(path.clone(), Provenance { file: file.to_owned(), path, overridden });
    }
    merge(&mut snapshot.values, &value);
}

fn resolve(snapshot: &mut EnvironmentSnapshot, file: &Path, value: Value, stack: &mut Vec<PathBuf>, overrides: &BTreeMap<PathBuf, Value>) -> Result<()> {
    if stack.iter().any(|entry| entry == file) {
        let chain = stack.iter().map(PathBuf::as_path).chain(std::iter::once(file)).map(|path| path.display().to_string()).collect::<Vec<_>>().join(" -> ");
        bail!("shared source cycle: {chain}");
    }
    stack.push(file.to_owned());
    for source in descriptors(&value)?.unwrap_or_default() {
        let source = expand_home(&source)?;
        let source = if source.is_absolute() { source } else { file.parent().unwrap().join(source) };
        let canonical = identity(&source).with_context(|| format!("resolving shared source {} declared by {}", source.display(), file.display()))?;
        let (child, fingerprint) = if let Some(value) = overrides.get(&canonical) {
            (value.clone(), fingerprint(&canonical)?)
        } else {
            read_document(&canonical, true)
                .with_context(|| format!("reading shared source {} declared by {}", canonical.display(), file.display()))?
        };
        crate::config::check_secrets(&child)?;
        ensure!(child.pointer("/_dockstride/swarmSecrets").is_none(), "Swarm secret bindings must remain checkout-local, not in shared source {}", canonical.display());
        snapshot.shared_documents.insert(canonical.clone(), child.clone());
        snapshot.fingerprints.insert(canonical.clone(), fingerprint);
        if !snapshot.sources.contains(&canonical) { snapshot.sources.push(canonical.clone()); }
        resolve(snapshot, &canonical, child, stack, overrides)?;
    }
    if file == snapshot.local_file.as_path() {
        snapshot.shared_values = snapshot.values.clone();
    }
    apply(snapshot, &value, file);
    stack.pop();
    Ok(())
}

pub fn snapshot(root: &Path, candidate: Option<&Value>) -> Result<EnvironmentSnapshot> {
    snapshot_with_overrides(root, candidate, &BTreeMap::new())
}

pub(crate) fn snapshot_with_overrides(root: &Path, candidate: Option<&Value>, overrides: &BTreeMap<PathBuf, Value>) -> Result<EnvironmentSnapshot> {
    let root = fs::canonicalize(root).context("opening checkout for shared configuration")?;
    let file = root.join("env.yaml");
    let (local, fingerprint) = match candidate {
        Some(candidate) => (candidate.clone(), fingerprint(&file)?),
        None => read_document(&file, false)?,
    };
    ensure!(local.is_object(), "env.yaml must contain a configuration mapping");
    let mut snapshot = EnvironmentSnapshot { local: local.clone(), local_file: file.clone(), values: json!({}), sources: Vec::new(), provenance: BTreeMap::new(), fingerprints: BTreeMap::from([(file.clone(), fingerprint)]), shared_values: json!({}), shared_documents: BTreeMap::new() };
    resolve(&mut snapshot, &file, local, &mut Vec::new(), overrides)?;
    if snapshot.shared_documents.values().any(|value| value.get("secrets").is_some()) {
        let metadata = crate::nickel::setup_metadata_values(&root, &snapshot.values)?;
        if let Some(policies) = metadata.pointer("/setup/secrets").and_then(Value::as_object) {
            for (name, policy) in policies {
                if policy["kind"] == "generate" || (policy.get("kind").is_none() && policy.get("bytes").is_some()) {
                    for (source, value) in &snapshot.shared_documents {
                        ensure!(value.pointer(&format!("/secrets/{name}")).is_none(),
                            "generated secret {name} must remain checkout-local, not inherited from shared source {}", source.display());
                    }
                }
            }
        }
    }
    Ok(snapshot)
}

pub fn unchanged(snapshot: &EnvironmentSnapshot) -> Result<bool> {
    for (path, before) in &snapshot.fingerprints { if fingerprint(path)? != *before { return Ok(false); } }
    Ok(true)
}

/// Canonical identities, sorted independently of source declaration/precedence order.
/// Caller first holds the invoking checkout's lifecycle and config guards.
/// Include the local document and every source in both the current and proposed
/// graph. This acquires only canonical file guards, never another lifecycle lock.
/// Verify the snapshot after acquisition before publishing any document.
pub(crate) fn lock_paths(paths: impl IntoIterator<Item=PathBuf>) -> Result<Vec<crate::state::Lock>> {
    let mut canonical = BTreeSet::new();
    for path in paths {
        let path = identity(&path)?;
        canonical.insert(path);
    }
    canonical.into_iter().map(|path| {
        let name = format!("source-{}", hex::encode(Sha256::digest(path.as_os_str().as_encoded_bytes())));
        crate::state::lock(path.parent().unwrap(), &name)
    }).collect()
}

#[cfg(test)]
mod path_tests {
    use super::{expand_home_with, portable_home_with, relative_path};
    use std::path::Path;

    #[test]
    fn home_expansion_is_explicit_and_requires_an_absolute_home() {
        let home = Some(Path::new("/home/test"));
        assert_eq!(expand_home_with(Path::new("~/settings.yaml"), home).unwrap(), Path::new("/home/test/settings.yaml"));
        assert_eq!(expand_home_with(Path::new("~/"), home).unwrap(), Path::new("/home/test"));
        for path in ["~", "~user/settings.yaml", "$HOME/settings.yaml", "relative.yaml", "/absolute.yaml"] {
            assert_eq!(expand_home_with(Path::new(path), None).unwrap(), Path::new(path));
        }
        for home in [None, Some(Path::new("")), Some(Path::new("relative/home"))] {
            assert!(expand_home_with(Path::new("~/settings.yaml"), home).is_err());
        }
    }

    #[test]
    fn home_compaction_respects_components_and_preserves_traversal() {
        let home = Some(Path::new("/home/test"));
        assert_eq!(portable_home_with(Path::new("/home/test/settings.yaml"), home), Path::new("~/settings.yaml"));
        assert_eq!(portable_home_with(Path::new("/home/test"), home), Path::new("~/"));
        for path in ["/outside/settings.yaml", "/home/test-sibling/settings.yaml", "/home/test/../outside.yaml", "relative.yaml", "~/settings.yaml"] {
            assert_eq!(portable_home_with(Path::new(path), home), Path::new(path));
        }
        for home in [None, Some(Path::new("")), Some(Path::new("home/test")), Some(Path::new("/home/test/../test"))] {
            assert_eq!(portable_home_with(Path::new("/home/test/settings.yaml"), home), Path::new("/home/test/settings.yaml"));
        }
    }

    #[test]
    fn lexical_relative_paths_support_descendants_siblings_and_fallbacks() {
        for (base, target, expected) in [
            ("/checkout", "/checkout/shared/settings.yaml", "shared/settings.yaml"),
            ("/checkout", "/sibling/settings.yaml", "../sibling/settings.yaml"),
            ("/checkout/nested", "/checkout", ".."),
            ("/checkout", "/checkout", "."),
            ("relative", "/target", "/target"),
            ("/checkout", "relative", "relative"),
            ("/checkout/../other", "/target", "/target"),
            ("/checkout", "/target/../other", "/target/../other"),
        ] {
            assert_eq!(relative_path(Path::new(base), Path::new(target)), Path::new(expected));
        }
    }
}
