//! Saved configured environments and verified Docker identity reservations.
//! Mutation callers hold lifecycle -> global -> config -> canonical source guards.
use crate::{model::Project, output::Output, publication, runtime::Docker, sources, state};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, path::{Path, PathBuf}};

pub struct Registration {
    /// Identity transition followed by the registry commit; append after local changes.
    pub changes: Vec<publication::Change>,
    pub claims: Value,
    pub owner_id: String,
}

fn validate(value: &Value) -> Result<()> {
    ensure!(value["schemaVersion"] == 1, "unsupported environment registry schema");
    let entries = value["environments"].as_object().context("environment registry environments must be a mapping")?;
    for (root, entry) in entries {
        for field in ["root", "ownerId", "project", "backend", "connection", "daemonId", "state"] {
            ensure!(entry[field].as_str().is_some_and(|text| !text.is_empty()), "invalid environment registry {field} for {root}");
        }
        ensure!(entry["root"].as_str() == Some(root) && Path::new(root).is_absolute(), "environment registry root mismatch for {root}");
        ensure!(entry["state"] == "committed", "invalid saved environment registry state for {root}");
        ensure!(matches!(entry["backend"].as_str(), Some("compose" | "swarm")), "invalid registry backend for {root}");
        ensure!(entry["sourceFiles"].as_array().is_some_and(|paths| paths.iter().all(|path| path.as_str().is_some_and(|path| Path::new(path).is_absolute()))), "invalid registry source files for {root}");
        ensure!(entry["allocatedEndpoints"].is_object(), "invalid registry allocated endpoints for {root}");
    }
    Ok(())
}

fn observed() -> Result<(Value, Option<String>)> {
    let (value, fingerprint) = state::read_fingerprinted(&state::global_root()?, "environment-registry")?;
    let value = if value.is_null() { json!({"schemaVersion":1,"environments":{}}) } else { value };
    validate(&value)?;
    Ok((value, fingerprint))
}

/// Pure saved-state reads: no directory creation, evaluation, recovery or Docker.
pub fn read() -> Result<Value> { Ok(observed()?.0) }
pub fn entries() -> Result<Vec<Value>> {
    Ok(read()?["environments"].as_object().unwrap().values().cloned().collect())
}

fn replacement(value: &Value, fingerprint: &Option<String>) -> Result<publication::Change> {
    let path = state::global_root()?.join(".dockstride/environment-registry.json");
    let change = publication::Change::replace(&path, &serde_json::to_vec_pretty(value)?, 0o600)?;
    change.expect_before(fingerprint)?;
    Ok(change)
}

/// Caller has the global guard and has proved resource/reservation/pending absence.
pub fn remove_change(root: &Path) -> Result<publication::Change> {
    // Forget also accepts an absent checkout; do not canonicalize through its files.
    let key = root.to_str().context("registry checkout path must be UTF-8")?;
    let (mut value, fingerprint) = observed()?;
    ensure!(value["environments"].as_object_mut().unwrap().remove(key).is_some(), "checkout is not registered: {}", root.display());
    replacement(&value, &fingerprint)
}

/// Append last to an allocation publication under the global guard.
pub fn endpoint_change(root: &Path, endpoints: &Value, allocations: &Value) -> Result<publication::Change> {
    ensure!(endpoints.is_object() && allocations.is_object(), "registry endpoints and allocations must be mappings");
    let key = fs::canonicalize(root)?.to_str().context("checkout path must be UTF-8")?.to_owned();
    let (mut value, fingerprint) = observed()?;
    let entry = value["environments"].get_mut(&key).context("cannot publish endpoints for an unregistered environment")?;
    entry["allocatedEndpoints"] = endpoints.clone();
    entry["allocations"] = allocations.clone();
    replacement(&value, &fingerprint)
}

/// GC changes saved allocation metadata only; absent checkout paths stay selectable.
pub fn prune_allocations_change(removed: &[(String, String)]) -> Result<publication::Change> {
    let (mut value, fingerprint) = observed()?;
    for (root, entry) in value["environments"].as_object_mut().unwrap() {
        let keys: Vec<&str> = removed.iter().filter(|(path, _)| path == root).map(|(_, key)| key.as_str()).collect();
        if keys.is_empty() { continue; }
        let ports = keys.iter().filter_map(|key| key.rsplit_once(':')
            .and_then(|(_, suffix)| suffix.split_once('/'))
            .and_then(|(port, _)| port.parse::<u64>().ok())).collect::<std::collections::BTreeSet<_>>();
        if let Some(allocations) = entry["allocations"].as_object_mut() {
            allocations.retain(|_, allocation| {
                if let Some(key) = allocation["key"].as_str() {
                    return !keys.contains(&key);
                }
                allocation.as_u64().or_else(|| allocation["port"].as_u64())
                    .is_none_or(|port| !ports.contains(&port))
            });
        }
        entry["allocatedEndpoints"] = crate::ports::endpoints_without(&entry["allocatedEndpoints"], &ports);
    }
    replacement(&value, &fingerprint)
}

fn check_claims(registry: &Value, pending: &Value, root: &str, owner: &str, daemon: &str, project: &str) -> Result<()> {
    for (saved_root, entry) in registry["environments"].as_object().unwrap() {
        if saved_root == root { continue; }
        ensure!(entry["ownerId"].as_str() != Some(owner), "ownership identity belongs to registered checkout {saved_root}; moved checkouts require explicit resolution");
        ensure!(entry["daemonId"].as_str() != Some(daemon) || entry["project"].as_str() != Some(project), "project {project} is already registered on Docker daemon {daemon} by {saved_root}; stopped and stale environments retain their claim");
    }
    for operation in pending["operations"].as_array().context("invalid pending registry operations")? {
        let claims = &operation["claims"];
        ensure!(operation["root"].as_str() != Some(root), "checkout has a pending publication; recover it before registering");
        ensure!(claims["ownerId"].as_str() != Some(owner), "ownership identity is reserved by pending publication {}", operation["id"]);
        // Missing target evidence is not proof of a different daemon.
        if claims["project"].as_str() == Some(project) {
            ensure!(claims["daemonId"].as_str().is_some_and(|id| id != daemon), "project {project} overlaps pending publication {} at {}; pending claims retained", operation["id"], operation["root"]);
        }
    }
    Ok(())
}

fn verified_daemon(docker: &Docker) -> Result<String> {
    let daemon = docker.capture_timeout(&["info".into(), "--format".into(), "{{.ID}}".into()], None, 30)?;
    let daemon = daemon.trim();
    ensure!(!daemon.is_empty() && daemon != "<no value>" && !daemon.chars().any(char::is_whitespace), "Docker target did not return a verifiable daemon ID; cannot acquire an environment claim");
    Ok(daemon.to_owned())
}

/// Before provisioning a missing secret, check authoritative claims without
/// evaluating the still-incomplete model. Caller retains the global guard until
/// reference publication, so another setup cannot claim between these steps.
pub(crate) fn check_setup_identity(root: &Path, project: &str, owner: &str, docker: &Docker) -> Result<()> {
    crate::model::validate_project_name(project)?;
    ensure!(!owner.is_empty(), "setup ownership identity is missing");
    let root = fs::canonicalize(root)?.to_str().context("checkout path must be UTF-8")?.to_owned();
    let connection = docker.context()?;
    let daemon = verified_daemon(docker)?;
    let registry = read()?;
    if let Some(entry) = registry["environments"].get(&root) {
        ensure!(entry["ownerId"].as_str() == Some(owner), "registered ownership identity differs from checkout ownership state");
        ensure!(entry["connection"].as_str() == Some(&connection) && entry["daemonId"].as_str() == Some(&daemon), "Docker target differs from registered environment; refusing to transfer its claim");
    }
    check_claims(&registry, &publication::pending_global()?, &root, owner, &daemon, project)
}

pub fn prepare(project: &Project, source_files: &[PathBuf], allocations: Option<&Value>) -> Result<Registration> {
    let docker = Docker::new(&project.root, Output::default());
    prepare_with_docker(project, source_files, &docker, allocations)
}

pub(crate) fn prepare_with_docker(project: &Project, source_files: &[PathBuf], docker: &Docker, allocations: Option<&Value>) -> Result<Registration> {
    let root = fs::canonicalize(&project.root)?.to_str().context("checkout path must be UTF-8")?.to_owned();
    let name = project.name()?;
    let backend = project.backend()?;
    let connection = docker.context()?;
    let verified = verified_daemon(docker)?;
    let daemon = verified.as_str();
    let (mut registry, registry_fingerprint) = observed()?;
    let previous = registry["environments"].get(&root).cloned().unwrap_or(Value::Null);
    let (mut identity, identity_fingerprint) = state::read_fingerprinted(&project.root, "identity")?;
    let old_identity = identity.clone();
    if !identity.is_null() {
        ensure!(identity["id"].as_str().is_some_and(|id| !id.is_empty()), "invalid persisted ownership identity");
        ensure!(identity["root"].as_str() == Some(&root), "ownership identity belongs to another checkout; moved checkouts require explicit resolution");
        ensure!(identity["context"].as_str() == Some(&connection), "Docker connection differs from the environment's recorded connection; refusing cross-context registration");
    }
    if !previous.is_null() {
        ensure!(previous["connection"].as_str() == Some(&connection) && previous["daemonId"].as_str() == Some(daemon), "Docker target differs from registered environment; refusing to transfer its claim");
        ensure!(identity.is_null() || identity["id"] == previous["ownerId"], "registered ownership identity differs from checkout ownership state");
    }
    let owner = if !previous.is_null() { previous["ownerId"].as_str().unwrap().to_owned() }
        else if let Some(owner) = identity["id"].as_str() { owner.to_owned() }
        else { state::random_id()? };
    check_claims(&registry, &publication::pending_global()?, &root, &owner, daemon, name)?;
    if identity.is_null() && !previous.is_null() {
        identity = json!({"id":owner,"root":root,"project":previous["project"],"backend":previous["backend"],"context":connection,"resources":false});
    }
    if !identity.is_null() && (identity["project"].as_str() != Some(name) || identity["backend"].as_str() != Some(backend)) {
        ensure!(identity["resources"] == false, "cannot transition project/backend while owned resources exist");
        check_previous_resources(&project.root, &identity)?;
    }
    crate::runtime::check_registry_resources(project, docker, &owner)?;
    if identity.is_null() { identity = json!({"id":owner,"root":root,"resources":false}); }
    identity["project"] = json!(name);
    identity["backend"] = json!(backend);
    identity["context"] = json!(connection);
    let endpoints = project.endpoints();
    let mut entry = json!({"root":root,"ownerId":owner,"project":name,"backend":backend,"connection":connection,"daemonId":daemon,"sourceFiles":source_files,"allocatedEndpoints":endpoints,"state":"committed"});
    entry["allocations"] = match allocations {
        Some(allocations) => {
            ensure!(allocations.is_object(), "registry allocations must be a mapping");
            allocations.clone()
        }
        None => {
            let (mut local, _) = crate::allocations::read_local(&project.root)?;
            local.as_object_mut().context("invalid allocation state")?
                .remove("allocations").context("allocation state lacks allocations")?
        }
    };
    let claims = json!({"root":root,"ownerId":owner,"project":name,"backend":backend,"connection":connection,"daemonId":daemon,"sources":source_files,"endpoints":entry["allocatedEndpoints"]});
    let mut changes = Vec::new();
    if identity != old_identity {
        let change = publication::Change::replace(&project.root.join(".dockstride/identity.json"), &serde_json::to_vec_pretty(&identity)?, 0o600)?;
        change.expect_before(&identity_fingerprint)?;
        changes.push(change);
    }
    if entry != previous {
        registry["environments"][&root] = entry;
        changes.push(replacement(&registry, &registry_fingerprint)?);
    }
    Ok(Registration { changes, claims, owner_id: owner })
}

pub(crate) fn check_previous_resources(root: &Path, identity: &Value) -> Result<()> {
    let owner = identity["id"].as_str().context("previous owner ID missing")?;
    let connection = identity["context"].as_str().context("previous Docker connection missing")?;
    let mut kinds = vec![vec!["ps", "-aq"], vec!["volume", "ls", "-q"], vec!["network", "ls", "-q"]];
    if identity["backend"] == "swarm" { kinds.extend([vec!["service", "ls", "-q"], vec!["secret", "ls", "-q"], vec!["config", "ls", "-q"]]); }
    for kind in kinds {
        let mut args = kind.into_iter().map(str::to_owned).collect::<Vec<_>>();
        args.extend(["--filter".to_owned(), format!("label=io.dockstride.owner={owner}")]);
        let found = crate::runtime::capture_pinned(root, connection, &args)?;
        ensure!(found.trim().is_empty(), "cannot transition project/backend: previous owned resources remain: {}", found.trim());
    }
    Ok(())
}

/// Caller owns the invoking lifecycle guard. No project process executes under these guards.
pub fn reconcile(project: &Project, docker: &Docker) -> Result<()> {
    let _global = state::global_lock()?;
    let _config = state::lock(&project.root, "config")?;
    publication::recover_locked(&project.root)?;
    let snapshot = sources::snapshot(&project.root, None)?;
    let _sources = sources::lock_paths(snapshot.fingerprints.keys().cloned())?;
    snapshot.verify()?;
    let current = crate::nickel::evaluate_values(&project.root, &snapshot.values)?;
    ensure!(current.env == project.env, "environment changed since lifecycle evaluation; retry the command");
    let registration = prepare_with_docker(&current, &snapshot.sources, docker, None)?;
    snapshot.verify()?;
    if !registration.changes.is_empty() {
        publication::publish(&project.root, "environment-reconcile", registration.changes, registration.claims)?;
    }
    Ok(())
}
