//! Missing ordinary settings may be proposed by one trusted project command.
//! This boundary validates the whole response and never publishes files or settings.
use crate::{commands, nickel, output::Output, runtime::Docker, sources};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::{Component, Path, PathBuf}};

#[derive(Debug)]
pub struct SourceDescriptor {
    pub path: PathBuf,
    pub create_if_missing: bool,
}

#[derive(Debug)]
pub struct Proposal {
    pub values: Value,
    pub sources: Option<Vec<SourceDescriptor>>,
    pub invoked: bool,
}

impl Proposal {
    fn empty() -> Self {
        Self { values: json!({}), sources: None, invoked: false }
    }
}

struct Declaration {
    command: String,
    fields: Vec<String>,
    sources: bool,
}

fn ordinary_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty() && path.split('.').all(|key| !key.is_empty() && !key.contains(['\n', '\r', '\0'])), "Invalid defaults field path {path:?}");
    ensure!(path != "secrets" && !path.starts_with("secrets.") && path != "_dockstride" && !path.starts_with("_dockstride."), "Defaults cannot propose reserved or secret field {path}");
    Ok(())
}

fn declaration(metadata: &Value) -> Result<Option<Declaration>> {
    let Some(defaults) = metadata.get("setup").and_then(|setup| setup.get("defaults")).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = defaults.as_object().context("setup.defaults must be a record")?;
    ensure!(object.keys().all(|key| matches!(key.as_str(), "command" | "fields" | "sources")), "Unsupported setup.defaults declaration key");
    let command = defaults["command"].as_str().context("setup.defaults.command must name a project command")?.to_owned();
    commands::declaration(metadata, &command)?;
    let fields = match defaults.get("fields") {
        Some(value) => value.as_array().context("setup.defaults.fields must be an array")?.iter()
            .map(|value| value.as_str().map(str::to_owned).context("setup.defaults.fields entries must be paths"))
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    for (index, path) in fields.iter().enumerate() {
        ordinary_path(path)?;
        ensure!(!fields[..index].iter().any(|other| other == path || other.starts_with(&format!("{path}.")) || path.starts_with(&format!("{other}."))), "Conflicting defaults allowlist paths at {path}");
    }
    let sources = match defaults.get("sources") {
        Some(value) => value.as_bool().context("setup.defaults.sources must be a boolean")?,
        None => false,
    };
    Ok(Some(Declaration { command, fields, sources }))
}

fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |value, key| value.as_object()?.get(key))
}

fn validate_values(value: &Value, prefix: &str, allowed: &[String]) -> Result<()> {
    let object = value.as_object().context("Defaults values must be a nested configuration mapping")?;
    for (key, value) in object {
        ensure!(!key.contains('.') && !key.is_empty(), "Defaults values must use nested keys, not dotted paths");
        let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
        ordinary_path(&path)?;
        if allowed.contains(&path) {
            continue;
        }
        ensure!(allowed.iter().any(|allowed| allowed.starts_with(&format!("{path}."))), "Defaults proposed non-allowlisted field {path}");
        ensure!(value.as_object().is_some_and(|object| !object.is_empty()), "Defaults proposed conflicting or empty parent field {path}");
        validate_values(value, &path, allowed)?;
    }
    Ok(())
}

fn normalized(root: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() { path.to_owned() } else { root.join(path) };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => { normalized.pop(); },
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn response(root: &Path, mut value: Value, declaration: &Declaration, sources_selected: bool) -> Result<Proposal> {
    let object = value.as_object().context("Defaults response must be a JSON object")?;
    ensure!(object.keys().all(|key| matches!(key.as_str(), "schemaVersion" | "values" | "sources")), "Unsupported defaults response key");
    ensure!(value["schemaVersion"] == 1, "Unsupported defaults schemaVersion");
    let values = value.get("values").context("Defaults response requires values")?;
    validate_values(values, "", &declaration.fields)?;
    let sources = if let Some(value) = value.get("sources") {
        ensure!(declaration.sources && !sources_selected, "Defaults cannot replace explicitly selected sources or propose undeclared sources");
        let descriptors = value.as_array().context("Defaults sources must be an array")?;
        let mut seen = BTreeSet::new();
        let mut sources = Vec::new();
        for descriptor in descriptors {
            let object = descriptor.as_object().context("Defaults source descriptor must be an object")?;
            ensure!(object.keys().all(|key| matches!(key.as_str(), "path" | "createIfMissing")), "Unsupported defaults source descriptor key");
            let path = descriptor["path"].as_str().context("Defaults source path must be a string")?;
            ensure!(!path.is_empty() && !path.contains(['\n', '\r', '\0']), "Defaults source path must be nonempty and contain no control delimiters");
            let create_if_missing = match descriptor.get("createIfMissing") {
                Some(value) => value.as_bool().context("Defaults createIfMissing must be a boolean")?,
                None => false,
            };
            let path = PathBuf::from(path);
            ensure!(seen.insert(normalized(root, &path)), "Defaults proposed duplicate source paths");
            sources.push(SourceDescriptor { path, create_if_missing });
        }
        Some(sources)
    } else { None };
    let values = value.as_object_mut().unwrap().remove("values").unwrap();
    Ok(Proposal { values, sources, invoked: true })
}

struct Prepared {
    metadata: Value,
    declaration: Declaration,
    context: Value,
    sources_selected: bool,
}

fn prepare(root: &Path, local: &Value, purpose: &str) -> Result<Option<Prepared>> {
    let metadata = nickel::bootstrap_metadata(root, Some(local))?;
    let Some(declaration) = declaration(&metadata)? else { return Ok(None); };
    let snapshot = sources::snapshot(root, Some(local))?;
    let fields = nickel::schema(root, Some(local))?;
    let sources_selected = snapshot.local.get("_dockstride").and_then(|metadata| metadata.get("sources")).is_some();
    let mut context = commands::context_snapshot(root, purpose, &snapshot, &fields, &[])?;
    let missing: Vec<&str> = declaration.fields.iter().filter(|path| at(&context["settings"], path).is_none()).map(String::as_str).collect();
    context["missingFields"] = json!(missing);
    Ok(Some(Prepared { metadata, declaration, context, sources_selected }))
}

pub(crate) fn command_context(root: &Path, local: &Value, purpose: &str) -> Result<Value> {
    match prepare(root, local, purpose)? {
        Some(prepared) => Ok(prepared.context),
        None => commands::context_for(root, local, purpose, &[]),
    }
}

/// Preview discovery using lazy metadata only; this never starts the command.
pub fn plan(root: &Path, local: &Value) -> Result<Value> {
    let Some(prepared) = prepare(root, local, "setup")? else {
        return Ok(json!({"wouldRun":false,"missingFields":[],"discoverSources":false,"command":null}));
    };
    let discover_sources = prepared.declaration.sources && !prepared.sources_selected;
    Ok(json!({"wouldRun":discover_sources || !prepared.context["missingFields"].as_array().unwrap().is_empty(),
        "missingFields":prepared.context["missingFields"],"discoverSources":discover_sources,"command":prepared.declaration.command}))
}

/// Run at most one command and return a fully validated, unpublished proposal.
/// A caller applies only gaps after resolving proposed sources and re-reading local
/// settings under its publication locks; it must not rerun this hook after a race.
pub fn propose_for(root: &Path, local: &Value, purpose: &str, timeout: u64, output: &Output) -> Result<Proposal> {
    let Some(prepared) = prepare(root, local, purpose)? else { return Ok(Proposal::empty()); };
    if prepared.context["missingFields"].as_array().unwrap().is_empty() && (!prepared.declaration.sources || prepared.sources_selected) {
        return Ok(Proposal::empty());
    }
    let result = commands::named(root, &prepared.metadata, &prepared.declaration.command, &Docker::new(root, output.clone()), output, &prepared.context, timeout)
        .context("Project setup defaults command failed")?;
    response(root, result, &prepared.declaration, prepared.sources_selected)
}

pub fn propose(root: &Path, local: &Value, timeout: u64, output: &Output) -> Result<Proposal> {
    propose_for(root, local, "setup", timeout, output)
}
