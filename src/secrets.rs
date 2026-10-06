//! Secret values exist only in process memory, private files, or Docker's stdin.
//! Replacing storage is not credential rotation: apply requires a project procedure.
//! File access is explicitly host UID/GID based. Native rootless Docker maps
//! container root UID/GID to the invoking host IDs: non-root consumers may use
//! an explicitly authorized container group 0 with a host-group-readable 0640
//! file. No file is world-readable; unknown userns/Desktop/remote mappings fail.
//! Journals retain references only. Missing recorded objects never regenerate.
use crate::{
    config, nickel,
    output::Output,
    runtime::{self, Docker},
    state,
};
use anyhow::{Context, Result, bail, ensure};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{IsTerminal, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd},
    },
    path::{Component, Path, PathBuf},
};

const MARKER: &[u8] = b"dockstride-private-secrets-v1\n";
const OWNER: &str = "io.dockstride.owner";

/// Explicit initial input; ignored once the logical secret has a valid reference.
#[derive(Clone, Debug)]
pub enum SecretInput {
    File(PathBuf),
    Stdin,
}
// Private provenance is intentionally not Debug and never included in reports.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileSource {
    kind: String,
    canonical_path: PathBuf,
    origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keyed_digest: Option<String>,
}
struct Input {
    bytes: Vec<u8>,
    source: Option<FileSource>,
}
impl Input {
    fn plain(bytes: Vec<u8>) -> Self { Self { bytes, source: None } }
}

/// Structured context; the original error remains in the anyhow chain.
#[derive(Debug)]
pub struct SyncFailed(pub Value);
impl std::fmt::Display for SyncFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("secret sync failed; inspect committed storage and application state before retry")
    }
}
impl std::error::Error for SyncFailed {}
#[derive(Clone, Serialize, Deserialize)]
struct Revision {
    logical: String,
    revision: String,
    reference: Value,
    backend: String,
    owner: String,
    context: String,
    cluster: Option<String>,
    #[serde(default)]
    previous: Option<Value>,
    #[serde(default)]
    pending: bool,
    #[serde(default)]
    deleted: bool,
    #[serde(default, rename = "fileSource", skip_serializing_if = "Option::is_none")]
    file_source: Option<FileSource>,
}
#[derive(Default, Serialize, Deserialize)]
struct History {
    #[serde(default)]
    revisions: Vec<Revision>,
}
struct Session {
    snapshot: crate::sources::EnvironmentSnapshot,
    metadata: Value,
    fields: Vec<crate::model::Field>,
    docker: Docker,
    owner: String,
    context: String,
    cluster: Option<String>,
    backend: String,
    project: String,
}

pub fn provision(
    root: &Path,
    non_interactive: bool,
    inputs: &BTreeMap<String, SecretInput>,
    output: &Output,
) -> Result<Value> {
    preflight_inputs(root, inputs, None)?;
    let _lifecycle = state::lock(root, "lifecycle")?;
    let _global = state::global_lock()?;
    let _config = state::lock(root, "config")?;
    crate::publication::recover_locked(root)?;
    let mut session = session(root, output)?;
    let mut policies = policies(&session.metadata)?;
    for field in session.fields.iter().filter(|f| f.kind == "secret") {
        let name = field
            .path
            .strip_prefix("secrets.")
            .context("secret fields must be declared under Config.secrets")?;
        valid_name(name)?;
        policies
            .entry(name.into())
            .or_insert_with(|| json!({"kind":"prompt"}));
    }
    for name in inputs.keys() {
        valid_name(name)?;
        ensure!(
            policies.contains_key(name)
                || session.snapshot.values.pointer(&format!("/secrets/{name}")).is_some(),
            "unknown initial secret input name {name}"
        );
    }
    for (name, input) in inputs {
        ensure!(!(policies.get(name).is_some_and(|policy| policy_kind(policy) == "reference")
            && matches!(input, SecretInput::Stdin)),
            "reference secret {name} requires a private file path; stdin cannot supply a reference");
        if policies.get(name).is_some_and(|policy| policy_kind(policy) == "reference")
            && session.snapshot.values.pointer(&format!("/secrets/{name}")).is_none() {
            if let SecretInput::File(path) = input {
                let path = if path.is_absolute() { path.clone() } else { std::env::current_dir()?.join(path) };
                reference_file(&json!({"file":path}))?;
            }
        }
    }
    ensure!(
        inputs
            .values()
            .filter(|input| matches!(input, SecretInput::Stdin))
            .count()
            <= 1,
        "only one explicit secret input may consume stdin"
    );
    let _sources = crate::sources::lock_paths(session.snapshot.fingerprints.keys().cloned())?;
    session.snapshot.verify().context(
        "environment changed while waiting for secret publication locks; rerun setup with current configuration",
    )?;
    let _lock = state::lock(root, "secrets")?;
    let identity =
        state::ensure_identity(root, &session.project, &session.backend, &session.context)?;
    session.owner = identity["id"]
        .as_str()
        .context("identity has no owner id")?
        .to_owned();
    if runtime::managed_invocation() {
        crate::registry::check_setup_identity(root, &session.project, &session.owner, &session.docker)?;
    }
    let mut history = history(root)?;
    reconcile(root, &mut session, &mut history)?;
    let mut results = Vec::new();
    let existing: Vec<_> = references(&session.snapshot.values).map(|(name, reference)| (name.clone(), reference.clone())).collect();
    for (name, reference) in existing {
        if session.backend == "swarm" && reference.get("file").is_some() {
            let policy = policies.get(&name).cloned().unwrap_or_else(|| json!({"kind":"reference"}));
            publish_reference(root, &mut session, &mut history, &name, &reference, &policy)?;
            results.push(json!({"name":name,"status":"provisioned","reference":session.snapshot.values.pointer(&format!("/secrets/{name}"))}));
        } else {
            valid_name(&name)?;
            validate_reference(&reference, &session)?;
            let owned = history
                .revisions
                .iter()
                .find(|r| r.reference == reference && !r.deleted);
            verify_reference(&reference, &session, owned)?;
            results.push(json!({"name":name,"status":"reused","reference":reference}));
        }
    }
    let missing: Vec<_> = policies
        .keys()
        .filter(|name| session.snapshot.values.pointer(&format!("/secrets/{name}")).is_none())
        .cloned()
        .collect();
    let stdin_count = missing
        .iter()
        .filter(|name| match inputs.get(*name) {
            Some(SecretInput::Stdin) => true,
            Some(SecretInput::File(_)) => false,
            None => policy_kind(&policies[*name]) == "stdin",
        })
        .count();
    ensure!(
        stdin_count <= 1,
        "only one missing secret may consume stdin per invocation"
    );
    if non_interactive {
        let unresolved: Vec<_> = missing
            .iter()
            .filter(|name| !inputs.contains_key(*name) && matches!(policy_kind(&policies[*name]), "prompt" | "reference"))
            .cloned()
            .collect();
        if !unresolved.is_empty() {
            return Err(config::MissingInputs { fields: unresolved.iter().map(|name| {
                session.fields.iter().find(|f| f.path.strip_prefix("secrets.") == Some(name.as_str())).cloned().unwrap_or_else(|| crate::model::Field {
                    path:format!("secrets.{name}"),kind:"secret".into(),doc:Some(if policy_kind(&policies[name]) == "reference" {
                        "Supply --secret-file NAME=PATH to an existing private file, or use the hidden FILE PATH prompt.".into()
                    } else { "Supply --secret-file NAME=PATH or --secret-stdin NAME, or use a hidden terminal prompt.".into() }),default:None,required:true,choices:vec![],
                })
            }).collect() }.into());
        }
    }
    let mut stdin_used = false;
    for name in missing {
        let policy = &policies[&name];
        if policy_kind(policy) == "reference" {
            let reference = obtain_reference(inputs.get(&name), non_interactive)?;
            let reference = publish_reference(root, &mut session, &mut history, &name, &reference, policy)?;
            results.push(json!({"name":name,"status":"provisioned","reference":reference}));
            continue;
        }
        let input = match inputs.get(&name) {
            Some(input) => obtain_input(input, &mut stdin_used)?,
            None => obtain(root, policy, non_interactive, &mut stdin_used)?,
        };
        let reference = create_revision(root, &mut session, &mut history, &name, &input.bytes, policy, input.source)?;
        results.push(json!({"name":name,"status":"provisioned","reference":reference}));
        output.event(
            "secrets",
            &format!("{name} provisioned; only its reference was saved"),
        )?;
    }
    let deferred = if session.backend == "compose" && runtime::managed_invocation() {
        true
    } else {
        let values = config::effective(&session.snapshot.values, &session.fields)?;
        let missing = session.fields.iter().any(|field| field.required && !field.path.starts_with("secrets.")
            && field.path.split('.').try_fold(&values, |value, part| value.get(part)).is_none());
        if missing {
            let allocations = crate::allocations::eligible_fields(root, &session.snapshot.values, &session.fields)?;
            session.fields.iter().any(|field| field.required && allocations.contains(&field.path)
                && field.path.split('.').try_fold(&values, |value, part| value.get(part)).is_none())
        } else { false }
    };
    if !deferred { validate_consumers_with_session(root, &session)?; }
    if !results.is_empty() {
        state::mark_resources(root, true)?;
        if let Some(cluster) = &session.cluster {
            state::pin_secret_cluster(root, cluster)?;
        }
    }
    Ok(json!({"secrets":results,"consumerValidation":if deferred {"deferred"} else {"validated"}}))
}

pub fn list(root: &Path) -> Result<Value> {
    let output = Output {
        json: false,
        quiet: true,
    };
    let session = session(root, &output)?;
    let history = history(root)?;
    let consumers = consumers(root)?;
    let mut secrets = Vec::new();
    for (name, reference) in references(&session.snapshot.values) {
        let revision = history
            .revisions
            .iter()
            .rev()
            .find(|r| r.reference == *reference && !r.deleted);
        let present = verify_reference(reference, &session, revision).is_ok();
        secrets.push(json!({"name":name,"backend":session.backend,"present":present,"reference":reference,
            "revision":revision.map(|r| &r.revision),"owned":revision.is_some_and(|r| r.owner == session.owner),
            "consumers":consumers.get(name).cloned().unwrap_or_default(),
            "retained":retained(&history,name,reference,&session.owner)}));
    }
    Ok(json!({"secrets":secrets}))
}

pub fn doctor(root: &Path) -> Result<Value> {
    let output = Output {
        json: false,
        quiet: true,
    };
    let session = session(root, &output)?;
    let mut issues = Vec::new();
    let history = history(root)?;
    for (name, reference) in references(&session.snapshot.values) {
        let revision = history
            .revisions
            .iter()
            .find(|r| r.reference == *reference && !r.deleted);
        if let Err(error) = verify_reference(reference, &session, revision) {
            issues.push(json!({"secret":name,"error":error.to_string()}));
        }
    }
    for revision in history.revisions.iter().filter(|r| r.pending) {
        issues.push(json!({"secret":revision.logical,"error":"interrupted operation; run setup to reconcile its owned revision","revision":revision.revision}));
    }
    if let Err(error) = validate_consumers_with_session(root, &session) {
        issues.push(json!({"error":error.to_string()}));
    }
    Ok(
        json!({"ok":issues.is_empty(),"issues":issues,"context":session.context,"backend":session.backend}),
    )
}

pub fn replace(
    root: &Path,
    name: &str,
    input: Option<&SecretInput>,
    apply: bool,
    non_interactive: bool,
    output: &Output,
) -> Result<Value> {
    valid_name(name)?;
    if let Some(input) = input {
        preflight_inputs(root, &BTreeMap::from([(name.to_owned(), input.clone())]), None)?;
    }
    let _lifecycle = state::lock(root, "lifecycle")?;
    let global = state::global_lock()?;
    let config = state::lock(root, "config")?;
    crate::publication::recover_locked(root)?;
    let mut session = session(root, output)?;
    let sources = crate::sources::lock_paths(session.snapshot.fingerprints.keys().cloned())?;
    session.snapshot.verify()?;
    let lock = state::lock(root, "secrets")?;
    let identity =
        state::ensure_identity(root, &session.project, &session.backend, &session.context)?;
    session.owner = identity["id"]
        .as_str()
        .context("identity has no owner id")?
        .to_owned();
    if runtime::managed_invocation() {
        crate::registry::check_setup_identity(root, &session.project, &session.owner, &session.docker)?;
    }
    let mut history = history(root)?;
    reconcile(root, &mut session, &mut history)?;
    let old = session
        .snapshot.values
        .pointer(&format!("/secrets/{name}"))
        .context("secret has no existing reference; run setup first")?
        .clone();
    let owned = history
        .revisions
        .iter()
        .find(|r| r.reference == old && !r.deleted);
    verify_reference(&old, &session, owned)?;
    let rotation = if apply {
        let project = nickel::evaluate(root, None)?;
        Some(rotation_scope(&project, &session.metadata, name)?)
    } else { None };
    let policy = policies(&session.metadata)?
        .remove(name)
        .unwrap_or_else(|| json!({"kind":"prompt"}));
    let reference = if policy_kind(&policy) == "reference" {
        let reference = obtain_reference(input, non_interactive)?;
        publish_reference(root, &mut session, &mut history, name, &reference, &policy)?
    } else {
        let input = match input {
            Some(input) => obtain_input(input, &mut false)?,
            None => obtain(root, &policy, non_interactive, &mut false)?,
        };
        create_revision(root, &mut session, &mut history, name, &input.bytes, &policy, input.source)?
    };
    validate_consumers_with_session(root, &session)?;
    let affected = consumers(root)?.remove(name).unwrap_or_default();
    // Storage is committed before trusted project commands run. Keep only lifecycle
    // serialization; commands must not hold allocation/configuration publication locks.
    drop(lock);
    drop(sources);
    drop(config);
    drop(global);
    if apply {
        let (workflow, services) = rotation.as_ref().unwrap();
        let project = nickel::evaluate(root, None)?;
        runtime::execute_actions(&project, workflow, services, &session.docker, output)
            .context("storage replacement committed and previous revision retained; declared application rotation failed, inspect/recover application before retry")?;
    }
    Ok(
        json!({"name":name,"reference":reference,"previous":old,"consumers":affected,"applied":apply,"rotation":"application-specific; no universal regeneration or rollback"}),
    )
}

struct SyncSelection {
    name: String,
    policy: Value,
    previous: Value,
    source: FileSource,
    input: Option<Vec<u8>>,
    unchanged: bool,
    baseline: bool,
    prior_comparable: bool,
    owned_index: Option<usize>,
    rotation: Option<(String, Vec<String>)>,
}

fn rotation_scope(project: &crate::model::Project, metadata: &Value, name: &str) -> Result<(String, Vec<String>)> {
    let rotation = metadata.pointer(&format!("/setup/rotations/{name}"))
        .context("no safe rotation procedure declared for this secret; storage replacement alone cannot rotate application credentials")?;
    let workflow = rotation.get("workflow").and_then(Value::as_str)
        .context("rotation workflow must be declared")?.to_owned();
    let services = rotation.get("services").and_then(Value::as_array)
        .context("rotation must explicitly declare affected services")?.iter()
        .map(|value| value.as_str().map(str::to_owned).context("rotation services must be strings"))
        .collect::<Result<Vec<_>>>()?;
    ensure!(!services.is_empty(), "rotation must explicitly name affected services");
    for service in &services {
        ensure!(project.services()?.contains_key(service), "rotation refers to unknown service {service}");
    }
    ensure!(runtime::planned_actions(project, &workflow, &services)?.iter().any(|action|
        action["workflows"].as_array().is_some_and(|workflows|
            workflows.iter().any(|entry| entry.as_str() == Some(workflow.as_str())))),
        "rotation workflow has no explicitly declared actions in its planned service scope");
    Ok((workflow, services))
}

fn sync_report(rows: &[Value], plan_only: bool, committed: &[String], applied: &[String], names: &[String]) -> Value {
    let uncommitted: Vec<_> = names.iter().filter(|name| !committed.contains(name)
        && !rows.iter().any(|row| row["name"].as_str() == Some(name.as_str()) && row["status"] == "unchanged")).collect();
    json!({"operation":"secrets-sync","sideEffects":!plan_only,
        "comparisonsDeferred":plan_only,"secrets":rows,
        "committed":committed,"uncommitted":uncommitted,"applied":applied})
}

pub fn sync(
    root: &Path,
    names: &[String],
    plan_only: bool,
    confirmed: bool,
    apply: bool,
    output: &Output,
) -> Result<Value> {
    ensure!(!names.is_empty(), "secret sync requires explicit names");
    ensure!(plan_only || confirmed, "secret sync requires explicit confirmation");
    let mut unique = std::collections::BTreeSet::new();
    for name in names {
        valid_name(name)?;
        ensure!(unique.insert(name), "duplicate secret sync selection {name}");
    }
    let _lifecycle = if plan_only { None } else { Some(state::lock(root, "lifecycle")?) };
    let global = if plan_only { None } else { Some(state::global_lock()?) };
    let config = if plan_only { None } else { Some(state::lock(root, "config")?) };
    if !plan_only { crate::publication::recover_locked(root)?; }
    let mut session = session(root, output)?;
    let sources = if plan_only { None } else {
        let locks = crate::sources::lock_paths(session.snapshot.fingerprints.keys().cloned())?;
        session.snapshot.verify()?;
        Some(locks)
    };
    let lock = if plan_only { None } else { Some(state::lock(root, "secrets")?) };
    let mut history = history(root)?;
    if !plan_only {
        ensure!(!session.owner.is_empty(), "secret sync requires a recorded ownership identity");
        reconcile(root, &mut session, &mut history)?;
    }
    let policies = policies(&session.metadata)?;
    let mut selections = Vec::with_capacity(names.len());
    let mut rows = Vec::with_capacity(names.len());
    // Resolve every selection before reading any credential or creating the HMAC key.
    for name in names {
        let previous = session.snapshot.values.pointer(&format!("/secrets/{name}"))
            .with_context(|| format!("secret {name} has no existing reference; run setup first"))?.clone();
        let owned_index = history.revisions.iter().rposition(|revision|
            revision.logical == *name && revision.reference == previous && !revision.deleted);
        let owned = owned_index.map(|index| &history.revisions[index]);
        verify_reference(&previous, &session, owned)?;
        let policy = policies.get(name).cloned().unwrap_or_else(|| json!({"kind":"prompt"}));
        let (path, origin) = if policy_kind(&policy) == "reference" {
            if let Some(path) = previous.get("file").and_then(Value::as_str) {
                (PathBuf::from(path), "reference-file")
            } else {
                let source = owned.and_then(|revision| revision.file_source.as_ref())
                    .context("external named secret has no file source to sync")?;
                if source.origin == "shared-reference" {
                    let path = session.snapshot.shared_values.pointer(&format!("/secrets/{name}/file"))
                        .and_then(Value::as_str).context("shared provider file reference is absent; do not reuse an obsolete source")?;
                    (PathBuf::from(path), "shared-reference")
                } else {
                    ensure!(source.origin == "reference-file", "secret has no authoritative reference file source");
                    (source.canonical_path.clone(), "reference-file")
                }
            }
        } else if policy_kind(&policy) == "file" {
            (declared_path(root, &policy)?, "declared-file")
        } else {
            let source = owned.and_then(|revision| revision.file_source.as_ref())
                .filter(|source| source.kind == "file" && source.origin == "cli-file")
                .with_context(|| format!("secret {name} has no current declared or explicit CLI file source"))?;
            (source.canonical_path.clone(), "cli-file")
        };
        // Metadata-only planning does not open or read source credential content.
        reject_symlinks(&path)?;
        let canonical_path = path.canonicalize().context("cannot resolve secret sync source")?;
        let source = FileSource { kind: "file".into(), canonical_path, origin: origin.into(), keyed_digest: None };
        rows.push(json!({"name":name,"source":{"kind":"file","canonicalPath":source.canonical_path,
            "origin":source.origin},"status":"comparison-deferred","reference":previous}));
        selections.push(SyncSelection { name: name.clone(), policy, previous, source, input: None,
            unchanged: false, baseline: false, prior_comparable: false, owned_index, rotation: None });
    }
    if plan_only { return Ok(sync_report(&rows, true, &[], &[], names)); }
    let key = if selections.iter().any(|selection| policy_kind(&selection.policy) != "reference" || session.backend == "swarm") { Some(source_key()?) } else { None };
    // Preflight all sources and exact bytes before the first new revision is published.
    for selection in &mut selections {
        if policy_kind(&selection.policy) == "reference" && session.backend == "compose" {
            reference_file(&json!({"file":selection.source.canonical_path}))?;
            continue;
        }
        let path = if selection.source.origin == "declared-file" {
            declared_path(root, &selection.policy)?
        } else { selection.source.canonical_path.clone() };
        let (file, canonical_path) = if policy_kind(&selection.policy) == "reference" {
            reference_file(&json!({"file":path}))?
        } else { open_input(&path, None)? };
        ensure!(canonical_path == selection.source.canonical_path, "secret sync source identity changed");
        let bytes = read_bounded(file)?;
        let mac = content_mac(key.as_ref().unwrap(), &bytes);
        let revision = selection.owned_index.map(|index| &history.revisions[index]);
        if let Some(digest) = revision.and_then(|revision| revision.file_source.as_ref())
            .and_then(|source| source.keyed_digest.as_deref()) {
            if let Some(matches) = digest_matches(&mac, digest) {
                selection.prior_comparable = true;
                selection.unchanged = matches;
            }
        }
        if !selection.prior_comparable && session.backend == "compose" && revision.is_some() {
            let path = Path::new(selection.previous["file"].as_str().context("managed reference has no file")?);
            let (file, _) = open_input(path, Some(access(&session, &selection.name)?))?;
            let baseline = read_bounded(file)?;
            selection.prior_comparable = true;
            selection.unchanged = bytes == baseline;
            selection.baseline = selection.unchanged;
        }
        selection.source.keyed_digest = Some(hex::encode(mac.finalize().into_bytes()));
        selection.input = Some(bytes);
    }
    let project = if apply && selections.iter().any(|selection| !selection.unchanged) {
        Some(nickel::evaluate(root, None)?)
    } else { None };
    if let Some(project) = &project {
        for selection in selections.iter_mut().filter(|selection| !selection.unchanged) {
            selection.rotation = Some(rotation_scope(project, &session.metadata, &selection.name)?);
        }
    }
    let mut committed = Vec::new();
    let mut applied = Vec::new();
    for (selection, row) in selections.iter().zip(&mut rows) {
        if selection.unchanged {
            row["status"] = json!("unchanged");
            row["consumerRestartNeeded"] = json!(false);
            if selection.baseline { row["baselineEstablished"] = json!(true); }
        } else {
            row["status"] = json!("not-published");
            row["priorContentComparable"] = json!(selection.prior_comparable);
        }
    }
    // Source provenance may change even when storage contents do not.
    let mut provenance_changed = false;
    for selection in selections.iter().filter(|selection| selection.unchanged) {
        if let Some(index) = selection.owned_index {
            if history.revisions[index].file_source.as_ref() != Some(&selection.source) {
                history.revisions[index].file_source = Some(selection.source.clone());
                provenance_changed = true;
            }
        }
    }
    if provenance_changed {
        save_history(root, &history).map_err(|error|
            error.context(SyncFailed(sync_report(&rows, false, &committed, &applied, names))))?;
    }
    for (index, selection) in selections.iter().enumerate().filter(|(_, selection)| !selection.unchanged) {
        let created = if policy_kind(&selection.policy) == "reference" && session.backend == "compose" {
            Ok(selection.previous.clone())
        } else {
            create_revision(root, &mut session, &mut history, &selection.name,
                selection.input.as_deref().unwrap(), &selection.policy, Some(selection.source.clone()))
        };
        match created {
            Ok(reference) => {
                committed.push(selection.name.clone());
                rows[index]["status"] = json!("replaced");
                rows[index]["reference"] = reference;
                rows[index]["previous"] = selection.previous.clone();
                rows[index]["applied"] = json!(false);
                rows[index]["consumerRestartNeeded"] = json!(true);
                rows[index]["priorContentComparable"] = json!(selection.prior_comparable);
            }
            Err(error) => {
                // Publication may fail after env.yaml was durably changed. Observe
                // that exact transition rather than calling a failed batch atomic.
                if let Some(record) = history.revisions.last()
                    && record.logical == selection.name && record.reference != selection.previous
                    && (!record.pending || config::read_env(root).is_ok_and(|local|
                        local.pointer(&format!("/secrets/{}", selection.name)) == Some(&record.reference))) {
                    committed.push(selection.name.clone());
                    rows[index]["status"] = json!("replaced");
                    rows[index]["reference"] = record.reference.clone();
                    rows[index]["previous"] = selection.previous.clone();
                    rows[index]["applied"] = json!(false);
                    rows[index]["consumerRestartNeeded"] = json!(true);
                }
                return Err(error.context(SyncFailed(sync_report(&rows, false, &committed, &applied, names))));
            }
        }
    }
    validate_consumers_with_session(root, &session).map_err(|error|
        error.context(SyncFailed(sync_report(&rows, false, &committed, &applied, names))))?;
    // Trusted rotation commands do not need retained plaintext in this process.
    for selection in &mut selections { selection.input = None; }
    drop(lock);
    drop(sources);
    drop(config);
    drop(global);
    if apply && !committed.is_empty() {
        let project = nickel::evaluate(root, None).map_err(|error|
            error.context(SyncFailed(sync_report(&rows, false, &committed, &applied, names))))?;
        for (index, selection) in selections.iter().enumerate().filter(|(_, selection)| !selection.unchanged) {
            let (workflow, services) = selection.rotation.as_ref().unwrap();
            runtime::execute_actions(&project, workflow, services, &session.docker, output).map_err(|error|
                error.context(SyncFailed(sync_report(&rows, false, &committed, &applied, names))))?;
            applied.push(selection.name.clone());
            rows[index]["applied"] = json!(true);
            rows[index]["consumerRestartNeeded"] = json!(false);
        }
    }
    Ok(sync_report(&rows, false, &committed, &applied, names))
}

pub fn gc(
    root: &Path,
    names: &[String],
    plan_only: bool,
    confirmed: bool,
    output: &Output,
) -> Result<Value> {
    ensure!(
        plan_only || confirmed,
        "secret deletion requires explicit confirmation"
    );
    ensure!(
        plan_only || !names.is_empty(),
        "select exact revision identifiers from secrets gc --plan; no implicit deletion"
    );
    let _lifecycle = if plan_only {
        None
    } else {
        Some(state::lock(root, "lifecycle")?)
    };
    let _global = if plan_only {
        None
    } else {
        Some(state::global_lock()?)
    };
    let _config = if plan_only {
        None
    } else {
        Some(state::lock(root, "config")?)
    };
    if !plan_only { crate::publication::recover_locked(root)?; }
    let mut session = session(root, output)?;
    let _sources = if plan_only {
        None
    } else {
        let locks = crate::sources::lock_paths(session.snapshot.fingerprints.keys().cloned())?;
        session.snapshot.verify()?;
        Some(locks)
    };
    let _lock = if plan_only {
        None
    } else {
        Some(state::lock(root, "secrets")?)
    };
    let mut history = history(root)?;
    if !plan_only {
        ensure!(
            !session.owner.is_empty(),
            "secret deletion requires a recorded ownership identity"
        );
        reconcile(root, &mut session, &mut history)?;
    }
    let mut protection = crate::secret_protection::observe(root)?;
    let current: Vec<_> = references(&session.snapshot.values)
        .map(|(_, value)| value.clone())
        .collect();
    let mut plan = Vec::new();
    let mut selected = Vec::new();
    for (index, revision) in history
        .revisions
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.deleted)
    {
        if !names.is_empty()
            && !names.iter().any(|n| {
                n == &revision.revision
                    || revision.reference.get("name").and_then(Value::as_str) == Some(n)
                    || revision.reference.get("file").and_then(Value::as_str) == Some(n)
            })
        {
            continue;
        }
        let reason = if revision.pending {
            Some("pending journal")
        } else if current.contains(&revision.reference) {
            Some("current environment reference")
        } else if revision.owner != session.owner {
            Some("foreign ownership")
        } else if referenced_snapshot(root, &revision.reference)? {
            Some("retained deployment snapshot")
        } else {
            protection.reason(&revision.reference)
        };
        let mut eligible = reason.is_none();
        let mut detail = reason.map(str::to_owned);
        if eligible {
            if let Err(error) = verify_reference(&revision.reference, &session, Some(revision)) {
                eligible = false;
                detail = Some(error.to_string());
            } else if revision.backend == "swarm" && swarm_consumed(&session, revision)? {
                eligible = false;
                detail = Some("active Swarm service consumer".into());
            } else if revision.backend == "compose" && compose_consumed(&session, revision)? {
                eligible = false;
                detail = Some("active container mount consumer".into());
            }
        }
        plan.push(json!({"name":revision.logical,"revision":revision.revision,"reference":revision.reference,"eligible":eligible,"reason":detail}));
        if eligible {
            selected.push(index);
        } else if !plan_only {
            bail!(
                "selected revision {} is not eligible: {}",
                revision.revision,
                detail.unwrap_or_default()
            );
        }
    }
    for name in names {
        ensure!(
            plan.iter()
                .any(|item| item["revision"].as_str() == Some(name)
                    || item["reference"]["name"].as_str() == Some(name)
                    || item["reference"]["file"].as_str() == Some(name)),
            "unknown retained revision {name}"
        );
    }
    if !plan_only {
        for index in selected {
            session.snapshot.verify()?;
            protection.verify()?;
            let revision = &mut history.revisions[index];
            if revision.backend == "swarm" {
                session.docker.run(
                    &[
                        "secret".into(),
                        "rm".into(),
                        revision.reference["name"].as_str().unwrap().into(),
                    ],
                    None,
                )?;
            } else {
                remove_private(&revision.reference, &session)?;
            }
            revision.deleted = true;
            save_history(root, &history)?;
            protection.refresh_own_history(root)?;
        }
        output.event(
            "secrets",
            "explicitly selected eligible owned revisions deleted",
        )?;
    }
    Ok(json!({"plan":plan,"applied":!plan_only}))
}

fn session(root: &Path, output: &Output) -> Result<Session> {
    let snapshot = crate::sources::snapshot(root, None)?;
    let layered = &snapshot.values;
    let metadata = nickel::setup_metadata_values(root, layered)?;
    let fields = nickel::schema_values(root, layered)?;
    let effective = |path: &str| {
        layered.get(path).or_else(|| {
            fields
                .iter()
                .find(|field| field.path == path)
                .and_then(|field| field.default.as_ref())
        })
    };
    let backend = effective("backend")
        .map(|value| value.as_str().context("backend must be a string"))
        .transpose()?
        .unwrap_or("compose")
        .to_owned();
    ensure!(
        backend == "compose" || backend == "swarm",
        "unsupported secret backend"
    );
    let project = effective("project")
        .and_then(Value::as_str)
        .context("project name is required before provisioning")?
        .to_owned();
    crate::model::validate_project_name(&project)?;
    let docker = Docker::new(
        root,
        Output {
            json: output.json,
            quiet: output.quiet,
        },
    );
    let context = docker.context()?;
    let cluster = if backend == "swarm" {
        let info: Value = serde_json::from_str(&docker.capture(
            &["info".into(), "--format".into(), "{{json .Swarm}}".into()],
            None,
        )?)?;
        ensure!(
            info["LocalNodeState"].as_str() == Some("active")
                && info["ControlAvailable"].as_bool() == Some(true),
            "Swarm secrets require an initialized manager; initialize explicitly"
        );
        Some(
            info.pointer("/Cluster/ID")
                .and_then(Value::as_str)
                .context("Docker did not report Swarm cluster identity")?
                .to_owned(),
        )
    } else {
        None
    };
    let identity = state::read(root, "identity")?;
    if identity.get("id").is_some() {
        ensure!(
            identity["project"].as_str() == Some(&project)
                && identity["backend"].as_str() == Some(&backend)
                && identity["context"].as_str() == Some(&context),
            "secret ownership scope differs from project/backend/Docker context"
        );
        if let Some(recorded_root) = identity["root"].as_str() {
            ensure!(
                Path::new(recorded_root) == fs::canonicalize(root)?,
                "ownership identity belongs to a different checkout"
            );
        }
    }
    if let Some(recorded) = identity.get("secretCluster").and_then(Value::as_str) {
        ensure!(
            cluster.as_deref() == Some(recorded),
            "Swarm cluster differs from recorded secret scope; restore credentials deliberately, never adopt a same-named object in another cluster"
        );
    }
    let owner = identity
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Ok(Session {
        snapshot,
        metadata,
        fields,
        docker,
        owner,
        context,
        cluster,
        backend,
        project,
    })
}
fn history(root: &Path) -> Result<History> {
    let value = state::read(root, "secrets")?;
    if value.is_null() || value.as_object().is_some_and(|v| v.is_empty()) {
        Ok(History::default())
    } else {
        Ok(serde_json::from_value(value)?)
    }
}
fn save_history(root: &Path, history: &History) -> Result<()> {
    state::save(root, "secrets", &serde_json::to_value(history)?)
}
fn policies(metadata: &Value) -> Result<BTreeMap<String, Value>> {
    let Some(value) = metadata.pointer("/setup/secrets") else {
        return Ok(BTreeMap::new());
    };
    let record = value
        .as_object()
        .context("setup.secrets must be a record")?;
    for name in record.keys() {
        valid_name(name)?;
    }
    Ok(record.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}
fn policy_kind(policy: &Value) -> &str {
    policy
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or(if policy.get("bytes").is_some() {
            "generate"
        } else {
            "prompt"
        })
}
pub(crate) fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "invalid secret/resource name; only ASCII letters, digits, '-' and '_' allowed"
    );
    Ok(())
}
fn references(env: &Value) -> impl Iterator<Item = (&String, &Value)> {
    env.get("secrets")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|record| record.iter())
}

pub(crate) fn validate_reference_shape(reference: &Value) -> Result<()> {
    let record = reference.as_object().context("secret reference must be a record, never plaintext")?;
    if let Some(path) = record.get("file").and_then(Value::as_str) {
        ensure!(record.len() == 1 && Path::new(path).is_absolute(),
            "file secret reference requires exactly {{file: absolute-path}}");
        ensure!(!Path::new(path).components().any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
            "secret paths must not contain traversal components");
    } else {
        ensure!(record.len() == 2 && record.get("external").and_then(Value::as_bool) == Some(true),
            "secret reference requires {{file: absolute-path}} or {{external: true, name: object}}");
        valid_name(record.get("name").and_then(Value::as_str).context("external secret name missing")?)?;
    }
    Ok(())
}

fn reference_file(reference: &Value) -> Result<(File, PathBuf)> {
    validate_reference_shape(reference)?;
    let path = Path::new(reference["file"].as_str().context("file reference required")?);
    let file = open_secure_file(path, libc::O_RDONLY | libc::O_NOATIME, 0)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o007 == 0 && metadata.mode() & 0o022 == 0
        && metadata.len() > 0 && metadata.len() <= 1_048_576,
        "referenced secret file must be nonempty, private, regular, and owned by the current user");
    Ok((file, path.to_owned()))
}

pub fn preflight_inputs(root: &Path, inputs: &BTreeMap<String, SecretInput>, candidate: Option<&Value>) -> Result<()> {
    if !inputs.values().any(|input| matches!(input, SecretInput::Stdin)) { return Ok(()); }
    let snapshot = crate::sources::snapshot(root, candidate)?;
    let mut policies = policies(&nickel::setup_metadata_values(root, &snapshot.values)?)?;
    for field in nickel::schema_values(root, &snapshot.values)?.into_iter().filter(|field| field.kind == "secret") {
        if let Some(name) = field.path.strip_prefix("secrets.") {
            policies.entry(name.to_owned()).or_insert_with(|| json!({"kind":"prompt"}));
        }
    }
    for (name, input) in inputs {
        ensure!(!matches!(input, SecretInput::Stdin) || policies.contains_key(name)
            || snapshot.values.pointer(&format!("/secrets/{name}")).is_some(),
            "unknown initial secret input name {name}");
        ensure!(!(matches!(input, SecretInput::Stdin) && policies.get(name).is_some_and(|policy| policy_kind(policy) == "reference")),
            "reference secret {name} requires a private file path; stdin cannot supply a reference");
    }
    Ok(())
}

pub(crate) fn validate_config_transition(root: &Path, before: &Value, candidate: &Value, effective: &Value, managed: bool) -> Result<()> {
    config::check_secrets(candidate)?;
    let mut policies = policies(&nickel::setup_metadata_values(root, effective)?)?;
    let mut prior_values = effective.clone();
    let mut prior_overlay = before.clone();
    prior_overlay.as_object_mut().context("configuration must be a mapping")?.remove("_dockstride");
    crate::sources::merge(&mut prior_values, &prior_overlay);
    for (name, policy) in self::policies(&nickel::setup_metadata_values(root, &prior_values)?)? {
        if policy_kind(&policy) == "generate" { policies.insert(name, policy); }
    }
    for (name, policy) in &policies {
        if !managed && policy_kind(policy) == "generate" {
            ensure!(before.pointer(&format!("/secrets/{name}")) == candidate.pointer(&format!("/secrets/{name}")),
                "generated secret {name} is managed by setup/replace, not native configuration edits");
        }
    }
    let default_fields = if effective.get("backend").is_none()
        && references(candidate).any(|(name, reference)| reference.get("file").is_none()
            && before.pointer(&format!("/secrets/{name}")) != Some(reference)) {
        nickel::schema_values(root, effective)?
    } else { Vec::new() };
    let backend = effective.get("backend").or_else(|| default_fields.iter()
        .find(|field| field.path == "backend").and_then(|field| field.default.as_ref()))
        .and_then(Value::as_str).unwrap_or("compose");
    for (name, reference) in references(candidate) {
        if before.pointer(&format!("/secrets/{name}")) == Some(reference) { continue; }
        if reference.get("file").is_some() {
            if !managed { reference_file(reference)?; }
        } else {
            ensure!(backend == "swarm",
                "external named secrets require the Swarm backend");
            if !managed {
                let docker = Docker::new(root, Output { json: false, quiet: true });
                let info: Value = serde_json::from_str(&docker.capture(&["secret".into(), "inspect".into(),
                    reference["name"].as_str().unwrap().into()], None)?)?;
                ensure!(info.get(0).and_then(|item| item.pointer("/Spec/Name")) == Some(&reference["name"]),
                    "external secret reference is absent or mismatched in the current Docker context");
            }
        }
    }
    if !managed && references(candidate).any(|(name, reference)| before.pointer(&format!("/secrets/{name}")) != Some(reference)) {
        let fields = nickel::schema_values(root, effective)?;
        let values = config::effective(effective, &fields)?;
        let complete = fields.iter().filter(|field| field.required).all(|field|
            field.path.split('.').try_fold(&values, |value, part| value.get(part)).is_some());
        if complete && values.get("backend").and_then(Value::as_str).unwrap_or("compose") == "compose" {
            let mut session = session(root, &Output { json: false, quiet: true })?;
            session.snapshot.values = effective.clone();
            session.metadata = nickel::setup_metadata_values(root, effective)?;
            validate_consumers_with_session(root, &session)?;
        }
    }
    Ok(())
}

fn obtain_reference(input: Option<&SecretInput>, non_interactive: bool) -> Result<Value> {
    let path = match input {
        Some(SecretInput::File(path)) => path.clone(),
        Some(SecretInput::Stdin) => bail!("reference secrets require a private file path, not stdin"),
        None => {
            ensure!(!non_interactive && std::io::stdin().is_terminal(), "reference secret requires --secret-file NAME=PATH");
            PathBuf::from(rpassword::prompt_password("Private secret FILE PATH (hidden): ")?)
        }
    };
    let path = if path.is_absolute() { path } else { std::env::current_dir()?.join(path) };
    let reference = json!({"file":path});
    reference_file(&reference)?;
    Ok(reference)
}

fn publish_reference(root: &Path, session: &mut Session, history: &mut History, name: &str, reference: &Value, policy: &Value) -> Result<Value> {
    let (file, path) = reference_file(reference)?;
    if session.backend == "compose" {
        local_scope(session)?;
        session.snapshot.verify()?;
        config::set_secret_reference_locked(root, name, reference)?;
        session.snapshot = crate::sources::snapshot(root, None)?;
        Ok(reference.clone())
    } else {
        let bytes = read_bounded(file)?;
        let origin = if session.snapshot.provenance.get(&format!("secrets.{name}")).is_some_and(|origin| origin.file != session.snapshot.local_file) { "shared-reference" } else { "reference-file" };
        let source = FileSource { kind: "file".into(), canonical_path: path, origin: origin.into(),
            keyed_digest: Some(content_digest(&source_key()?, &bytes)) };
        create_revision(root, session, history, name, &bytes, policy, Some(source))
    }
}
fn retained(history: &History, logical: &str, current: &Value, owner: &str) -> Vec<Value> {
    let mut result: Vec<Value> = history
        .revisions
        .iter()
        .filter(|r| r.logical == logical && !r.deleted && r.reference != *current)
        .map(|r| json!({"revision":r.revision,"reference":r.reference,"owned":r.owner == owner}))
        .collect();
    for previous in history
        .revisions
        .iter()
        .filter(|r| r.logical == logical)
        .filter_map(|r| r.previous.as_ref())
    {
        if previous != current
            && !history.revisions.iter().any(|r| r.reference == *previous)
            && !result.iter().any(|r| r["reference"] == *previous)
        {
            result.push(json!({"revision":null,"reference":previous,"owned":false}));
        }
    }
    result
}
fn obtain_input(input: &SecretInput, stdin_used: &mut bool) -> Result<Input> {
    match input {
        SecretInput::File(path) => import_file(path, "cli-file"),
        SecretInput::Stdin => Ok(Input::plain(read_stdin(stdin_used)?)),
    }
}
fn read_stdin(stdin_used: &mut bool) -> Result<Vec<u8>> {
    ensure!(
        !*stdin_used,
        "only one secret may consume stdin per invocation"
    );
    ensure!(
        !std::io::stdin().is_terminal(),
        "stdin secret input requires piped input"
    );
    *stdin_used = true;
    let mut bytes = Vec::new();
    std::io::stdin().take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= 1_048_576,
        "secret must contain 1..1048576 bytes"
    );
    Ok(bytes)
}
fn obtain(
    root: &Path,
    policy: &Value,
    non_interactive: bool,
    stdin_used: &mut bool,
) -> Result<Input> {
    let bytes = match policy_kind(policy) {
        "generate" => {
            let size = policy.get("bytes").and_then(Value::as_u64).unwrap_or(32);
            ensure!(
                (16..=65536).contains(&size),
                "generated secret bytes must be 16..65536"
            );
            let mut bytes = vec![0; size as usize];
            getrandom::fill(&mut bytes)
                .map_err(|_| anyhow::anyhow!("cryptographic random generation failed"))?;
            match policy
                .get("encoding")
                .and_then(Value::as_str)
                .unwrap_or("hex")
            {
                "hex" => hex::encode(bytes).into_bytes(),
                "base64" => base64(&bytes).into_bytes(),
                _ => bail!("secret encoding must be hex or base64"),
            }
        }
        "file" => {
            let path = declared_path(root, policy)?;
            return import_file(&path, "declared-file");
        }
        "stdin" => read_stdin(stdin_used)?,
        "prompt" => {
            ensure!(
                !non_interactive && std::io::stdin().is_terminal(),
                "secret input required; declare file/stdin policy or use a terminal prompt"
            );
            rpassword::prompt_password("Secret (hidden): ")
                .context("unable to read hidden secret prompt")?
                .into_bytes()
        }
        _ => bail!("unknown secret provisioning policy"),
    };
    ensure!(
        !bytes.is_empty() && bytes.len() <= 1_048_576,
        "secret must contain 1..1048576 bytes"
    );
    Ok(Input::plain(bytes))
}

fn declared_path(root: &Path, policy: &Value) -> Result<PathBuf> {
    let path = Path::new(policy.get("path").and_then(Value::as_str)
        .context("file secret policy requires path")?);
    Ok(if path.is_absolute() { path.to_owned() } else { root.join(path) })
}

fn open_input(path: &Path, managed_mode: Option<(u32, u32, u32)>) -> Result<(File, PathBuf)> {
    // Open the original spelling before canonicalization, with every component
    // anchored and no-follow. Canonicalizing first would launder symlinks.
    let path = if path.is_absolute() { path.to_owned() } else { std::env::current_dir()?.join(path) };
    let file = open_secure_file(&path, libc::O_RDONLY, 0).context("cannot open secret input file")?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "secret input must be a regular file");
    ensure!(metadata.uid() == unsafe { libc::geteuid() }, "secret input must be owned by the current user");
    match managed_mode {
        Some((uid, gid, mode)) => ensure!(metadata.uid() == uid && metadata.gid() == gid
            && metadata.mode() & 0o777 == mode, "managed baseline access differs from declared policy"),
        None => ensure!(metadata.mode() & 0o077 == 0,
            "secret input/recovery file must not be accessible to group or other users"),
    }
    let canonical = path.canonicalize().context("cannot resolve secret input identity")?;
    let target = open_secure_file(&canonical, libc::O_RDONLY, 0)?.metadata()?;
    ensure!(metadata.dev() == target.dev() && metadata.ino() == target.ino(),
        "secret input identity changed during import");
    Ok((file, canonical))
}

fn read_bounded(mut file: File) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(!bytes.is_empty() && bytes.len() <= 1_048_576, "secret input size invalid");
    Ok(bytes)
}

// All callers that create/read this key hold the shared global publication guard.
fn source_key() -> Result<[u8; 32]> {
    let directory = state::global_root()?.join(".dockstride");
    let path = directory.join("secret-source-key");
    let mut key = [0u8; 32];
    match open_secure_file(&path, libc::O_RDONLY, 0) {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600 && metadata.len() == 32,
                "secret source key ownership, permissions, or length invalid");
            file.read_exact(&mut key)?;
        }
        Err(error) if error.downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {
            getrandom::fill(&mut key).map_err(|_| anyhow::anyhow!("cryptographic random generation failed"))?;
            let mut file = open_secure_file(&path, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?;
            file.write_all(&key)?;
            file.sync_all()?;
            File::open(directory)?.sync_all()?;
        }
        Err(error) => return Err(error),
    }
    Ok(key)
}

fn content_mac(key: &[u8; 32], bytes: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts 32-byte keys");
    mac.update(bytes);
    mac
}
fn content_digest(key: &[u8; 32], bytes: &[u8]) -> String {
    hex::encode(content_mac(key, bytes).finalize().into_bytes())
}
fn digest_matches(mac: &Hmac<Sha256>, digest: &str) -> Option<bool> {
    let mut decoded = [0u8; 32];
    hex::decode_to_slice(digest, &mut decoded).ok()?;
    Some(mac.clone().verify_slice(&decoded).is_ok())
}
fn import_file(path: &Path, origin: &str) -> Result<Input> {
    let (file, canonical_path) = open_input(path, None)?;
    let bytes = read_bounded(file)?;
    let keyed_digest = Some(content_digest(&source_key()?, &bytes));
    Ok(Input { bytes, source: Some(FileSource {
        kind: "file".into(), canonical_path, origin: origin.into(), keyed_digest,
    }) })
}
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as usize;
        let b = chunk.get(1).copied().unwrap_or(0) as usize;
        let c = chunk.get(2).copied().unwrap_or(0) as usize;
        result.push(TABLE[a >> 2] as char);
        result.push(TABLE[((a & 3) << 4) | (b >> 4)] as char);
        result.push(if chunk.len() > 1 {
            TABLE[((b & 15) << 2) | (c >> 6)] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            TABLE[c & 63] as char
        } else {
            '='
        });
    }
    result
}
fn creation_error(mut error: anyhow::Error, bytes: &[u8]) -> anyhow::Error {
    fn redact(mut text: String, bytes: &[u8]) -> String {
        let encoded = base64(bytes);
        if !encoded.is_empty() && text.contains(&encoded) {
            text = text.replace(&encoded, "[REDACTED]");
        }
        if let Ok(secret) = std::str::from_utf8(bytes)
            && !secret.is_empty()
            && text.contains(secret)
        {
            text = text.replace(secret, "[REDACTED]");
        }
        text
    }
    if let Some(docker) = error.downcast_mut::<runtime::DockerError>() {
        docker.stderr = redact(std::mem::take(&mut docker.stderr), bytes);
    } else {
        error = anyhow::anyhow!(redact(format!("{error:#}"), bytes));
    }
    error.context("Docker secret creation failed; operation journal retained for scope/ownership-safe recovery")
}

fn create_revision(
    root: &Path,
    session: &mut Session,
    history: &mut History,
    name: &str,
    bytes: &[u8],
    policy: &Value,
    file_source: Option<FileSource>,
) -> Result<Value> {
    valid_name(name)?;
    session.snapshot.verify()?;
    let revision = state::random_id()?;
    let filename = format!("{name}--{revision}");
    let reference = if session.backend == "swarm" {
        // Docker limits object names independently of the full project/logical
        // names preserved in labels. Keep the complete cryptographic revision.
        let available = 64usize
            .checked_sub(revision.len() + 4)
            .context("revision does not fit a Swarm secret name")?;
        let prefix = available / 2;
        ensure!(
            prefix > 0,
            "Swarm revision leaves no room for readable name prefixes"
        );
        let object_name = format!(
            "{}-{}--{revision}",
            &session.project[..session.project.len().min(prefix)],
            &name[..name.len().min(prefix)]
        );
        json!({"external":true,"name":object_name})
    } else {
        local_scope(session)?;
        let directory = private_directory(session, true)?;
        json!({"file":directory.join(&filename)})
    };
    if session.backend == "swarm"
        && policy_kind(policy) == "generate"
        && file_source.is_none()
        && policy
            .get("durable")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    {
        let recovery = policy.get("recoveryFile").and_then(Value::as_str).context("durable generated Swarm credentials require a private recoveryFile before plaintext can be discarded; declare a revision-specific path containing {revision}")?;
        ensure!(
            recovery.contains("{revision}"),
            "recoveryFile must include {revision} to retain previous credentials"
        );
        let recovery = PathBuf::from(recovery.replace("{revision}", &revision));
        write_recovery(&recovery, bytes)?;
    }
    let record = Revision {
        logical: name.into(),
        revision,
        reference: reference.clone(),
        backend: session.backend.clone(),
        owner: session.owner.clone(),
        context: session.context.clone(),
        cluster: session.cluster.clone(),
        previous: session.snapshot.values.pointer(&format!("/secrets/{name}")).cloned(),
        pending: true,
        deleted: false,
        file_source,
    };
    history.revisions.push(record.clone());
    save_history(root, history)?;
    state::mark_resources(root, true)?;
    if let Some(cluster) = &session.cluster {
        state::pin_secret_cluster(root, cluster)?;
    }
    if session.backend == "swarm" {
        let args = vec![
            "secret".into(),
            "create".into(),
            "--label".into(),
            format!("{OWNER}={}", session.owner),
            "--label".into(),
            format!("io.dockstride.project={}", session.project),
            "--label".into(),
            format!("io.dockstride.secret={name}"),
            "--label".into(),
            format!("io.dockstride.revision={}", record.revision),
            reference["name"].as_str().unwrap().into(),
            "-".into(),
        ];
        session
            .docker
            .capture(&args, Some(bytes))
            .map_err(|error| creation_error(error, bytes))?;
    } else {
        write_private(&reference, bytes, session, name)?;
    }
    verify_reference(&reference, session, Some(&record))?;
    session.snapshot.verify()?;
    config::set_secret_reference_locked(root, name, &reference)?;
    history.revisions.last_mut().unwrap().pending = false;
    save_history(root, history)?;
    session.snapshot = crate::sources::snapshot(root, None)?;
    Ok(reference)
}
fn reconcile(root: &Path, session: &mut Session, history: &mut History) -> Result<()> {
    for index in 0..history.revisions.len() {
        let record = &history.revisions[index];
        if !record.pending || record.deleted {
            continue;
        }
        session.snapshot.verify()?;
        ensure!(
            record.owner == session.owner
                && record.context == session.context
                && record.cluster == session.cluster,
            "interrupted secret operation belongs to a different owner/context/cluster"
        );
        verify_reference(&record.reference, session, Some(record)).context("interrupted secret revision is absent or unverifiable; recover from the declared private recovery source, do not regenerate an established credential")?;
        let current = session.snapshot.values.pointer(&format!("/secrets/{}", record.logical));
        ensure!(
            current == Some(&record.reference) || current == record.previous.as_ref(),
            "secret reference changed during interrupted operation; manual recovery required"
        );
        if current != Some(&record.reference) {
            session.snapshot.verify()?;
            config::set_secret_reference_locked(root, &record.logical, &record.reference)?;
        }
        history.revisions[index].pending = false;
        save_history(root, history)?;
        session.snapshot = crate::sources::snapshot(root, None)?;
    }
    Ok(())
}
fn validate_reference(reference: &Value, session: &Session) -> Result<()> {
    validate_reference_shape(reference)?;
    let record = reference
        .as_object()
        .context("secret reference must be a Docker-native record, never plaintext")?;
    if session.backend == "compose" {
        ensure!(
            record.len() == 1 && record.get("file").and_then(Value::as_str).is_some(),
            "Compose secret requires exactly {{file: absolute-path}}"
        );
        let path = Path::new(record["file"].as_str().unwrap());
        ensure!(path.is_absolute(), "secret file path must be absolute");
        reject_symlinks(path)?;
    } else {
        ensure!(
            record.len() == 2 && record.get("external").and_then(Value::as_bool) == Some(true),
            "Swarm secret requires {{external: true, name: revision-name}}"
        );
        valid_name(
            record
                .get("name")
                .and_then(Value::as_str)
                .context("Swarm secret name missing")?,
        )?;
    }
    Ok(())
}
fn verify_reference(
    reference: &Value,
    session: &Session,
    revision: Option<&Revision>,
) -> Result<()> {
    validate_reference(reference, session)?;
    if let Some(revision) = revision {
        ensure!(
            revision.context == session.context
                && revision.cluster == session.cluster
                && revision.backend == session.backend
                && revision.owner == session.owner,
            "secret revision belongs to a different backend/context/cluster/owner"
        );
    }
    if session.backend == "swarm" {
        let info: Value = serde_json::from_str(&session.docker.capture(&["secret".into(), "inspect".into(), reference["name"].as_str().unwrap().into()], None).context("recorded Swarm secret is absent; restore from recovery source, not regeneration")?)?;
        let spec = info
            .get(0)
            .and_then(|v| v.get("Spec"))
            .context("invalid Docker secret inspection response")?;
        ensure!(
            spec["Name"] == reference["name"],
            "Swarm secret name mismatch"
        );
        if let Some(revision) = revision {
            ensure!(
                spec["Labels"][OWNER].as_str() == Some(&session.owner)
                    && spec["Labels"]["io.dockstride.project"].as_str() == Some(&session.project)
                    && spec["Labels"]["io.dockstride.secret"].as_str() == Some(&revision.logical)
                    && spec["Labels"]["io.dockstride.revision"].as_str()
                        == Some(&revision.revision),
                "Swarm secret ownership labels mismatch; refusing to adopt/delete it"
            );
        }
    } else {
        if revision.is_none() {
            reference_file(reference)?;
        }
        let path = Path::new(reference["file"].as_str().unwrap());
        let file = open_secure_file(path, libc::O_RDONLY, 0).context("recorded secret file is absent or unreadable; restore it from recovery source, never regenerate")?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() > 0 && metadata.len() <= 1_048_576
                && metadata.mode() & 0o007 == 0 && metadata.mode() & 0o022 == 0,
            "secret file ownership/permissions are unsafe"
        );
        if let Some(revision) = revision {
            let directory = private_directory(session, false)?;
            ensure!(
                path.parent() == Some(directory.as_path())
                    && path.file_name().and_then(|s| s.to_str())
                        == Some(&format!("{}--{}", revision.logical, revision.revision)),
                "file is not the owned journaled revision"
            );
            let (uid, gid, mode) = access(session, &revision.logical)?;
            ensure!(
                metadata.uid() == uid && metadata.gid() == gid && metadata.mode() & 0o777 == mode,
                "owned file permissions/owner differ from declared access strategy"
            );
        }
    }
    Ok(())
}
fn local_scope(session: &Session) -> Result<()> {
    let context_name = session
        .context
        .split(';')
        .next()
        .context("missing Docker context name")?;
    let info: Value = serde_json::from_str(&session.docker.capture(
        &["context".into(), "inspect".into(), context_name.into()],
        None,
    )?)?;
    let context_host = info
        .get(0)
        .and_then(|v| v.pointer("/Endpoints/docker/Host"))
        .and_then(Value::as_str)
        .context("Docker context has no host endpoint")?;
    let host = session
        .context
        .split(";DOCKER_HOST=")
        .nth(1)
        .unwrap_or(context_host);
    ensure!(
        host.starts_with("unix://") && !context_name.contains("desktop"),
        "host-file secrets require a native local Docker context; remote/Desktop mounts are not portable, use Swarm or an explicitly local environment"
    );
    docker_rootless(session)?;
    Ok(())
}
fn docker_rootless(session: &Session) -> Result<bool> {
    let security: Value = serde_json::from_str(&session.docker.capture(
        &[
            "info".into(),
            "--format".into(),
            "{{json .SecurityOptions}}".into(),
        ],
        None,
    )?)?;
    let items = security
        .as_array()
        .context("Docker did not report security options")?;
    let rootless = items
        .iter()
        .any(|item| item.as_str().is_some_and(|s| s.contains("rootless")));
    ensure!(
        rootless
            || !items
                .iter()
                .any(|item| item.as_str().is_some_and(|s| s.contains("userns"))),
        "userns-remap host-file ownership cannot be validated; use Swarm or a native local Docker context"
    );
    Ok(rootless)
}
fn reject_symlinks(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for part in path.components() {
        ensure!(
            !matches!(part, Component::ParentDir | Component::CurDir),
            "secret paths must not contain traversal components"
        );
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "secret path contains a symlink"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn private_directory(session: &Session, create: bool) -> Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let explicit = session
        .metadata
        .pointer("/setup/secretDirectory")
        .and_then(Value::as_str);
    let parent = match explicit {
        Some(path) => PathBuf::from(path),
        None => {
            let data = match std::env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty()) {
                Some(path) => PathBuf::from(path),
                None => PathBuf::from(
                    std::env::var_os("HOME")
                        .context("HOME missing; declare setup.secretDirectory")?,
                )
                .join(".local/share"),
            };
            data.join("dockstride/secrets")
        }
    };
    ensure!(parent.is_absolute(), "secretDirectory must be absolute");
    reject_symlinks(&parent)?;
    if !parent.exists() {
        ensure!(
            create,
            "managed secret directory is absent; do not recreate missing credential storage implicitly"
        );
        ensure!(
            explicit != Some("/opt/secrets") && !parent.starts_with("/opt"),
            "shared /opt secret parent requires explicit administrator provisioning with ownership marker; use the user-private default instead"
        );
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&parent)?;
        let mut marker = open_secure_file(
            &parent.join(".dockstride-owner"),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        marker.write_all(MARKER)?;
        marker.sync_all()?;
        File::open(&parent)?.sync_all()?;
    }
    let metadata = fs::symlink_metadata(&parent)?;
    ensure!(
        metadata.is_dir()
            && (metadata.uid() == uid || metadata.uid() == 0)
            && metadata.mode() & 0o022 == 0,
        "secret parent has incompatible ownership/permissions; refusing to commandeer it"
    );
    let marker = open_secure_file(&parent.join(".dockstride-owner"), libc::O_RDONLY, 0).context("existing secret parent lacks Dockstride marker; choose an alternate managed directory or provision explicitly")?;
    ensure!(
        marker.metadata()?.uid() == metadata.uid()
            && marker.metadata()?.is_file()
            && marker.metadata()?.mode() & 0o022 == 0,
        "secret ownership marker unsafe"
    );
    let mut marker_bytes = Vec::new();
    marker.take(128).read_to_end(&mut marker_bytes)?;
    ensure!(marker_bytes == MARKER, "secret ownership marker mismatch");
    let directory = parent.join(format!("u{uid}"));
    use std::os::unix::fs::DirBuilderExt;
    if create {
        match fs::DirBuilder::new().mode(0o300).create(&directory) {
        Ok(()) => File::open(&parent)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
        Err(error) => return Err(error).context("administrator must provision the per-user secret directory; the CLI never elevates implicitly"),
    }
    }
    let metadata = fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == uid
            && metadata.mode() & 0o777 == 0o300,
        "per-user secret directory must be owned by invoking user and mode 0300"
    );
    Ok(directory)
}
fn access(session: &Session, name: &str) -> Result<(u32, u32, u32)> {
    let policy = session
        .metadata
        .pointer(&format!("/setup/secretAccess/{name}"));
    let uid = policy
        .and_then(|p| p.get("uid"))
        .and_then(Value::as_u64)
        .unwrap_or(unsafe { libc::geteuid() } as u64);
    let gid = policy
        .and_then(|p| p.get("gid"))
        .and_then(Value::as_u64)
        .unwrap_or(unsafe { libc::getegid() } as u64);
    ensure!(
        uid <= u32::MAX as u64 && gid <= u32::MAX as u64,
        "secret access uid/gid out of range"
    );
    let mode = if policy.and_then(|p| p.get("gid")).is_some() {
        0o640
    } else {
        0o600
    };
    Ok((uid as u32, gid as u32, mode))
}
fn open_directory(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute(),
        "managed secret directory must be absolute"
    );
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(name) => {
                let name = std::ffi::CString::new(name.as_bytes())?;
                let descriptor = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                ensure!(
                    descriptor >= 0,
                    "secret directory is absent, a symlink, or not traversable: {}",
                    std::io::Error::last_os_error()
                );
                directory = unsafe { File::from_raw_fd(descriptor) };
            }
            _ => bail!("managed secret path contains traversal"),
        }
    }
    Ok(directory)
}
fn open_secure_file(path: &Path, flags: i32, mode: u32) -> Result<File> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    reject_symlinks(&absolute)?;
    let directory = open_directory(absolute.parent().context("secret file parent missing")?)?;
    let name = std::ffi::CString::new(
        absolute
            .file_name()
            .context("secret filename missing")?
            .as_bytes(),
    )?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("secure secret file access failed");
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}
fn write_private(reference: &Value, bytes: &[u8], session: &Session, logical: &str) -> Result<()> {
    let path = Path::new(reference["file"].as_str().unwrap());
    let directory = open_directory(path.parent().unwrap())?;
    let name = std::ffi::CString::new(path.file_name().unwrap().as_bytes())?;
    let temporary = std::ffi::CString::new(format!(".pending-{}", state::random_id()?))?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(
        descriptor >= 0,
        "exclusive private secret creation failed: {}",
        std::io::Error::last_os_error()
    );
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    let (uid, gid, mode) = access(session, logical)?;
    let write = (|| -> Result<()> {
        if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0 {
            bail!(
                "declared container-readable ownership cannot be applied; provision authorized ownership/group explicitly: {}",
                std::io::Error::last_os_error()
            );
        }
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        ensure!(
            unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    directory.as_raw_fd() as libc::c_long,
                    temporary.as_ptr(),
                    directory.as_raw_fd() as libc::c_long,
                    name.as_ptr(),
                    libc::RENAME_NOREPLACE as libc::c_long,
                )
            } == 0,
            "atomic exclusive secret publication failed: {}",
            std::io::Error::last_os_error()
        );
        // Mode 0300 intentionally prevents O_RDONLY directory handles. Flush the
        // same filesystem through the still-open secret fd, without a chmod window.
        ensure!(
            unsafe { libc::syncfs(file.as_raw_fd()) } == 0,
            "secret publication durability failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    })();
    if write.is_err() {
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    write?;
    Ok(())
}
fn write_recovery(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(path.is_absolute(), "recoveryFile must be an absolute path");
    reject_symlinks(path)?;
    let parent = path.parent().context("invalid recovery path")?;
    let metadata =
        fs::metadata(parent).context("recovery directory must already be provisioned privately")?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "recovery directory must be user-owned and private (0700)"
    );
    let mut file = open_secure_file(path, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)
        .context("recoveryFile must be exclusive, not an existing file")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn remove_private(reference: &Value, session: &Session) -> Result<()> {
    let path = Path::new(
        reference["file"]
            .as_str()
            .context("invalid owned file reference")?,
    );
    ensure!(
        path.parent() == Some(private_directory(session, false)?.as_path()),
        "refusing to delete outside managed directory"
    );
    let directory = open_directory(path.parent().unwrap())?;
    let file = open_secure_file(path, libc::O_RDONLY, 0)?;
    let name = std::ffi::CString::new(path.file_name().unwrap().as_bytes())?;
    ensure!(
        unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == 0,
        "private secret deletion failed: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        unsafe { libc::syncfs(file.as_raw_fd()) } == 0,
        "secret deletion durability failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
fn consumers(root: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let project = nickel::evaluate(root, None)?;
    let mut result: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, service) in project.services()? {
        if let Some(secrets) = service.get("secrets").and_then(Value::as_array) {
            for secret in secrets {
                if let Some(logical) = secret
                    .as_str()
                    .or_else(|| secret.get("source").and_then(Value::as_str))
                {
                    result.entry(logical.into()).or_default().push(name.clone());
                }
            }
        }
    }
    Ok(result)
}
/// Finish consumer permission validation after native allocations complete setup.
pub fn validate_consumers(root: &Path, output: &Output) -> Result<()> {
    validate_consumers_with_session(root, &session(root, output)?)
}

fn validate_consumers_with_session(root: &Path, session: &Session) -> Result<()> {
    if session.backend != "compose" {
        return Ok(());
    }
    let project = nickel::evaluate_values(root, &session.snapshot.values)?;
    let services = project.services()?;
    if !services.values().any(|service| service.get("secrets").and_then(Value::as_array)
        .is_some_and(|grants| !grants.is_empty())) {
        return Ok(());
    }
    local_scope(session)?;
    let rootless = docker_rootless(session)?;
    for (service_name, service) in services {
        let user = service.get("user").and_then(Value::as_str);
        for secret in service
            .get("secrets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let logical = secret
                .as_str()
                .or_else(|| secret.get("source").and_then(Value::as_str))
                .context("invalid service secret grant")?;
            let reference = session
                .snapshot.values
                .pointer(&format!("/secrets/{logical}"))
                .context("service secret grant has no reference")?;
            let metadata = open_secure_file(Path::new(reference["file"].as_str()
                .context("invalid file secret reference")?), libc::O_RDONLY, 0)?.metadata()?;
            if let Some(user) = user {
                let mut parts = user.split(':');
                let uid: u32 = parts.next().unwrap().parse().with_context(|| format!("service {service_name}: named container users cannot be validated against host-file ownership; use numeric user and explicit setup.secretAccess"))?;
                let gid = parts
                    .next()
                    .map(str::parse::<u32>)
                    .transpose()
                    .context("container group must be numeric for host-file secrets")?;
                let can_read = if rootless {
                    let host_uid = unsafe { libc::geteuid() };
                    let host_gid = unsafe { libc::getegid() };
                    let explicit_group = session
                        .metadata
                        .pointer(&format!("/setup/secretAccess/{logical}/gid"))
                        .and_then(Value::as_u64)
                        == Some(host_gid as u64);
                    (uid == 0 && metadata.uid() == host_uid)
                        || (gid == Some(0)
                            && explicit_group
                            && metadata.gid() == host_gid
                            && metadata.mode() & 0o040 != 0)
                } else {
                    uid == 0
                        || uid == metadata.uid()
                        || (gid == Some(metadata.gid()) && metadata.mode() & 0o040 != 0)
                };
                ensure!(
                    can_read,
                    "service {service_name} user cannot read {logical}; rootless non-root consumers require explicit user UID:0 and setup.secretAccess host invoking GID (0640); native Docker requires matching numeric host UID/GID"
                );
            } else {
                bail!(
                    "service {service_name} has no explicit numeric user; its image USER cannot be assumed root. Declare user for host-file secret {logical}"
                );
            }
        }
    }
    Ok(())
}
fn swarm_consumed(session: &Session, revision: &Revision) -> Result<bool> {
    let ids = session
        .docker
        .capture(&["service".into(), "ls".into(), "--quiet".into()], None)?;
    for id in ids.split_whitespace() {
        let info: Value = serde_json::from_str(
            &session
                .docker
                .capture(&["service".into(), "inspect".into(), id.into()], None)?,
        )?;
        if info
            .get(0)
            .and_then(|v| v.pointer("/Spec/TaskTemplate/ContainerSpec/Secrets"))
            .and_then(Value::as_array)
            .is_some_and(|secrets| {
                secrets
                    .iter()
                    .any(|s| s["SecretName"] == revision.reference["name"])
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}
fn compose_consumed(session: &Session, revision: &Revision) -> Result<bool> {
    let ids = session
        .docker
        .capture(&["ps".into(), "--all".into(), "--quiet".into()], None)?;
    for id in ids.split_whitespace() {
        let info: Value = serde_json::from_str(
            &session
                .docker
                .capture(&["inspect".into(), id.into()], None)?,
        )?;
        if info
            .get(0)
            .and_then(|v| v.get("Mounts"))
            .and_then(Value::as_array)
            .is_some_and(|mounts| {
                mounts
                    .iter()
                    .any(|m| m["Source"] == revision.reference["file"])
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}
fn referenced_snapshot(root: &Path, reference: &Value) -> Result<bool> {
    // Only inspect operation bookkeeping, never enumerate the mode-0300 secret directory.
    fn contains(value: &Value, reference: &Value) -> bool {
        value == reference
            || match value {
                Value::Array(values) => values.iter().any(|v| contains(v, reference)),
                Value::Object(values) => values.values().any(|v| contains(v, reference)),
                _ => false,
            }
    }
    fn scan(path: &Path, reference: &Value) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "secrets.json" || name == "locks" {
                continue;
            }
            let metadata = entry.file_type()?;
            ensure!(
                !metadata.is_symlink(),
                "operation state contains a symlink; ownership/retention cannot be safely verified"
            );
            if metadata.is_dir() {
                if scan(&entry.path(), reference)? {
                    return Ok(true);
                }
            } else if entry.path().extension().is_some_and(|e| e == "json") {
                let value: Value = serde_json::from_slice(&fs::read(entry.path())?)
                    .context("cannot verify retained deployment bookkeeping")?;
                if contains(&value, reference) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    scan(&root.join(".dockstride"), reference)
}
