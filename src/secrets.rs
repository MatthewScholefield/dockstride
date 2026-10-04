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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
}
#[derive(Default, Serialize, Deserialize)]
struct History {
    #[serde(default)]
    revisions: Vec<Revision>,
}
struct Session {
    env: Value,
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
    let mut session = session(root, output, false)?;
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
                || session.env.pointer(&format!("/secrets/{name}")).is_some(),
            "unknown initial secret input name {name}"
        );
    }
    ensure!(
        inputs
            .values()
            .filter(|input| matches!(input, SecretInput::Stdin))
            .count()
            <= 1,
        "only one explicit secret input may consume stdin"
    );
    // Initial revision creation must not race a project/backend transition.
    let _lifecycle = state::lock(root, "lifecycle")?;
    let _lock = state::lock(root, "secrets")?;
    ensure!(
        config::read_env(root)? == session.env,
        "environment changed while waiting for secret lifecycle lock; rerun setup with current configuration"
    );
    let identity =
        state::ensure_identity(root, &session.project, &session.backend, &session.context)?;
    session.owner = identity["id"]
        .as_str()
        .context("identity has no owner id")?
        .to_owned();
    let mut history = history(root)?;
    reconcile(root, &mut session, &mut history)?;
    let mut results = Vec::new();
    if let Some(refs) = session.env.get("secrets").and_then(Value::as_object) {
        for (name, reference) in refs {
            valid_name(name)?;
            validate_reference(reference, &session)?;
            let owned = history
                .revisions
                .iter()
                .find(|r| r.reference == *reference && !r.deleted);
            verify_reference(reference, &session, owned)?;
            results.push(json!({"name":name,"status":"reused","reference":reference}));
        }
    }
    let missing: Vec<_> = policies
        .keys()
        .filter(|name| session.env.pointer(&format!("/secrets/{name}")).is_none())
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
            .filter(|name| !inputs.contains_key(*name) && policy_kind(&policies[*name]) == "prompt")
            .cloned()
            .collect();
        if !unresolved.is_empty() {
            return Err(config::MissingInputs { fields: unresolved.iter().map(|name| {
                session.fields.iter().find(|f| f.path.strip_prefix("secrets.") == Some(name.as_str())).cloned().unwrap_or_else(|| crate::model::Field {
                    path:format!("secrets.{name}"),kind:"secret".into(),doc:Some("Supply --secret-file NAME=PATH or --secret-stdin NAME, or use a hidden terminal prompt.".into()),default:None,required:true,choices:vec![],
                })
            }).collect() }.into());
        }
    }
    let mut stdin_used = false;
    for name in missing {
        let policy = &policies[&name];
        let bytes = match inputs.get(&name) {
            Some(input) => obtain_input(input, &mut stdin_used)?,
            None => obtain(root, policy, non_interactive, &mut stdin_used)?,
        };
        let reference = create_revision(root, &mut session, &mut history, &name, &bytes, policy)?;
        results.push(json!({"name":name,"status":"provisioned","reference":reference}));
        output.event(
            "secrets",
            &format!("{name} provisioned; only its reference was saved"),
        )?;
    }
    validate_consumers(root, &session)?;
    if !results.is_empty() {
        state::mark_resources(root, true)?;
        if let Some(cluster) = &session.cluster {
            state::pin_secret_cluster(root, cluster)?;
        }
    }
    Ok(json!({"secrets":results}))
}

pub fn list(root: &Path) -> Result<Value> {
    let output = Output {
        json: false,
        quiet: true,
    };
    let session = session(root, &output, false)?;
    let history = history(root)?;
    let consumers = consumers(root)?;
    let mut secrets = Vec::new();
    for (name, reference) in references(&session.env) {
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
    let session = session(root, &output, false)?;
    let mut issues = Vec::new();
    let history = history(root)?;
    for (name, reference) in references(&session.env) {
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
    if let Err(error) = validate_consumers(root, &session) {
        issues.push(json!({"error":error.to_string()}));
    }
    Ok(
        json!({"ok":issues.is_empty(),"issues":issues,"context":session.context,"backend":session.backend}),
    )
}

pub fn replace(
    root: &Path,
    name: &str,
    input: Option<&[u8]>,
    apply: bool,
    trusted: bool,
    non_interactive: bool,
    output: &Output,
) -> Result<Value> {
    valid_name(name)?;
    if apply {
        ensure!(
            trusted,
            "secret apply executes the project-declared rotation procedure and requires --trust"
        );
    }
    let _lifecycle = state::lock(root, "lifecycle")?;
    let _lock = state::lock(root, "secrets")?;
    let mut session = session(root, output, true)?;
    let mut history = history(root)?;
    reconcile(root, &mut session, &mut history)?;
    let old = session
        .env
        .pointer(&format!("/secrets/{name}"))
        .context("secret has no existing reference; run setup first")?
        .clone();
    let owned = history
        .revisions
        .iter()
        .find(|r| r.reference == old && !r.deleted);
    verify_reference(&old, &session, owned)?;
    let rotation = session
        .metadata
        .pointer(&format!("/setup/rotations/{name}"))
        .cloned();
    if apply {
        let rotation = rotation.as_ref().context("no safe rotation procedure declared for this secret; storage replacement alone cannot rotate application credentials")?;
        ensure!(
            rotation.get("workflow").and_then(Value::as_str).is_some(),
            "rotation workflow must be declared"
        );
        ensure!(
            rotation.get("services").and_then(Value::as_array).is_some(),
            "rotation must explicitly declare affected services"
        );
        let project = nickel::evaluate(root, None)?;
        let services = rotation["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().context("rotation services must be strings"))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !services.is_empty(),
            "rotation must explicitly name affected services"
        );
        for service in services {
            ensure!(
                project.services()?.contains_key(service),
                "rotation refers to unknown service {service}"
            );
        }
        let workflow = rotation["workflow"].as_str().unwrap();
        ensure!(
            project
                .metadata
                .get("actions")
                .and_then(Value::as_array)
                .is_some_and(|actions| actions.iter().any(|action| action
                    .get("workflows")
                    .and_then(Value::as_array)
                    .is_some_and(|w| w.iter().any(|w| w.as_str() == Some(workflow))))),
            "rotation workflow has no explicitly declared actions"
        );
        output.event("plan", &format!("replace {name}, retain previous revision, execute declared rotation workflow {workflow}"))?;
    }
    let policy = policies(&session.metadata)?
        .remove(name)
        .unwrap_or_else(|| json!({"kind":"prompt"}));
    let bytes = match input {
        Some(bytes) => {
            ensure!(
                !bytes.is_empty() && bytes.len() <= 1_048_576,
                "secret input must contain 1..1048576 bytes"
            );
            std::borrow::Cow::Borrowed(bytes)
        }
        None => std::borrow::Cow::Owned(obtain(root, &policy, non_interactive, &mut false)?),
    };
    let reference = create_revision(root, &mut session, &mut history, name, &bytes, &policy)?;
    validate_consumers(root, &session)?;
    let affected = consumers(root)?.remove(name).unwrap_or_default();
    if apply {
        let rotation = rotation.as_ref().unwrap();
        let services = rotation["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .context("rotation services must be strings")
            })
            .collect::<Result<Vec<_>>>()?;
        let project = nickel::evaluate(root, None)?;
        for service in &services {
            ensure!(
                project.services()?.contains_key(service),
                "rotation refers to unknown service {service}"
            );
        }
        runtime::execute_actions(&project, rotation["workflow"].as_str().unwrap(), &services, &session.docker, output)
            .context("storage replacement committed and previous revision retained; declared application rotation failed, inspect/recover application before retry")?;
    }
    Ok(
        json!({"name":name,"reference":reference,"previous":old,"consumers":affected,"applied":apply,"rotation":"application-specific; no universal regeneration or rollback"}),
    )
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
    let _lock = if plan_only {
        None
    } else {
        Some(state::lock(root, "secrets")?)
    };
    let mut session = session(root, output, false)?;
    let mut history = history(root)?;
    if !plan_only {
        ensure!(
            !session.owner.is_empty(),
            "secret deletion requires a recorded ownership identity"
        );
        reconcile(root, &mut session, &mut history)?;
    }
    let current: Vec<_> = references(&session.env)
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
            None
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
        }
        output.event(
            "secrets",
            "explicitly selected eligible owned revisions deleted",
        )?;
    }
    Ok(json!({"plan":plan,"applied":!plan_only}))
}

fn session(root: &Path, output: &Output, create_identity: bool) -> Result<Session> {
    let env = config::read_env(root)?;
    let metadata = nickel::setup_metadata(root, Some(&env))?;
    let fields = nickel::schema(root, Some(&env))?;
    let effective = |path: &str| {
        env.get(path).or_else(|| {
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
    let identity = if create_identity {
        state::ensure_identity(root, &project, &backend, &context)?
    } else {
        state::read(root, "identity")?
    };
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
        env,
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
fn valid_name(name: &str) -> Result<()> {
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
fn obtain_input(input: &SecretInput, stdin_used: &mut bool) -> Result<Vec<u8>> {
    match input {
        SecretInput::File(path) => {
            ensure!(
                path.is_absolute(),
                "explicit secret input paths must be absolute"
            );
            read_input(path)
        }
        SecretInput::Stdin => read_stdin(stdin_used),
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
) -> Result<Vec<u8>> {
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
            let path = Path::new(
                policy
                    .get("path")
                    .and_then(Value::as_str)
                    .context("file secret policy requires path")?,
            );
            read_input(&if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            })?
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
    Ok(bytes)
}
pub fn read_input(path: &Path) -> Result<Vec<u8>> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    reject_symlinks(&path)?;
    let mut file =
        open_secure_file(&path, libc::O_RDONLY, 0).context("cannot open secret input file")?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "secret input must be a regular file");
    ensure!(
        metadata.mode() & 0o077 == 0,
        "secret input/recovery file must not be accessible to group or other users"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(1_048_577)
        .read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= 1_048_576,
        "secret input size invalid"
    );
    Ok(bytes)
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
) -> Result<Value> {
    valid_name(name)?;
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
        previous: session.env.pointer(&format!("/secrets/{name}")).cloned(),
        pending: true,
        deleted: false,
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
    config::set_secret_reference(root, name, &reference)?;
    history.revisions.last_mut().unwrap().pending = false;
    save_history(root, history)?;
    session.env = config::read_env(root)?;
    Ok(reference)
}
fn reconcile(root: &Path, session: &mut Session, history: &mut History) -> Result<()> {
    for index in 0..history.revisions.len() {
        let record = &history.revisions[index];
        if !record.pending || record.deleted {
            continue;
        }
        ensure!(
            record.owner == session.owner
                && record.context == session.context
                && record.cluster == session.cluster,
            "interrupted secret operation belongs to a different owner/context/cluster"
        );
        verify_reference(&record.reference, session, Some(record)).context("interrupted secret revision is absent or unverifiable; recover from the declared private recovery source, do not regenerate an established credential")?;
        let current = session.env.pointer(&format!("/secrets/{}", record.logical));
        ensure!(
            current == Some(&record.reference) || current == record.previous.as_ref(),
            "secret reference changed during interrupted operation; manual recovery required"
        );
        if current != Some(&record.reference) {
            config::set_secret_reference(root, &record.logical, &record.reference)?;
        }
        history.revisions[index].pending = false;
        save_history(root, history)?;
        session.env = config::read_env(root)?;
    }
    Ok(())
}
fn validate_reference(reference: &Value, session: &Session) -> Result<()> {
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
        let path = Path::new(reference["file"].as_str().unwrap());
        let file = open_secure_file(path, libc::O_RDONLY, 0).context("recorded secret file is absent or unreadable; restore it from recovery source, never regenerate")?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.mode() & 0o007 == 0 && metadata.mode() & 0o022 == 0,
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
    ensure!(
        descriptor >= 0,
        "secure secret file access failed: {}",
        std::io::Error::last_os_error()
    );
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
fn validate_consumers(root: &Path, session: &Session) -> Result<()> {
    if session.backend != "compose" {
        return Ok(());
    }
    let project = nickel::evaluate(root, None)?;
    let rootless = docker_rootless(session)?;
    for (service_name, service) in project.services()? {
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
                .env
                .pointer(&format!("/secrets/{logical}"))
                .context("service secret grant has no reference")?;
            let metadata = fs::metadata(
                reference["file"]
                    .as_str()
                    .context("invalid file secret reference")?,
            )?;
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
