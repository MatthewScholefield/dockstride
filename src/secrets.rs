//! Credentials remain in external private files; YAML records current references
//! and immutable Swarm bindings, never credential bytes or publication history.
use crate::{config, nickel, output::Output, runtime::{self, Docker}, sources, state};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::{BTreeMap, BTreeSet}, fs::{self, File, OpenOptions},
    io::{IsTerminal, Read, Write}, os::unix::{ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt, PermissionsExt}, io::{AsRawFd, FromRawFd}},
    path::{Component, Path, PathBuf}};
const OWNER: &str = "io.dockstride.owner";

#[derive(Clone, Debug)]
pub enum SecretInput { File(PathBuf), Stdin }

#[derive(Debug)]
pub struct SyncFailed(pub Value);
impl std::fmt::Display for SyncFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("secret sync failed; inspect committed bindings and application state before retry")
    }
}
impl std::error::Error for SyncFailed {}

struct Session {
    root: PathBuf,
    snapshot: sources::EnvironmentSnapshot,
    metadata: Value,
    fields: Vec<crate::model::Field>,
    docker: Docker,
    owner: String,
    context: String,
    backend: String,
    project: String,
    bindings: BTreeMap<String, String>,
}

fn session(root: &Path, output: &Output) -> Result<Session> {
    let root = root.canonicalize().context("cannot resolve checkout root")?;
    let owner = root.to_str().context("checkout path is not UTF-8; use a UTF-8 checkout path")?.to_owned();
    let snapshot = sources::snapshot(&root, None)?;
    let metadata = nickel::setup_metadata_values(&root, &snapshot.values)?;
    let fields = nickel::schema_values(&root, &snapshot.values)?;
    let effective = config::effective(&snapshot.values, &fields)?;
    let backend = effective.get("backend").and_then(Value::as_str).unwrap_or("compose").to_owned();
    ensure!(matches!(backend.as_str(), "compose" | "swarm"), "unsupported secret backend");
    let project = effective.get("project").and_then(Value::as_str)
        .context("project name is required before provisioning")?.to_owned();
    crate::model::validate_project_name(&project)?;
    let bindings = sources::swarm_bindings(&snapshot.local)?;
    let docker = Docker::new(&root, Output { json: output.json, quiet: output.quiet });
    let context = docker.context()?;
    if backend == "swarm" {
        let info: Value = serde_json::from_str(&docker.capture(&["info".into(), "--format".into(), "{{json .Swarm}}".into()], None)?)?;
        ensure!(info["LocalNodeState"] == "active" && info["ControlAvailable"] == true,
            "Swarm secrets require an initialized manager; initialize explicitly");
    }
    Ok(Session { root, snapshot, metadata, fields, docker, owner, context, backend, project, bindings })
}

fn validate_namespace(session: &Session) -> Result<()> {
    let project = crate::model::Project {
        root: session.root.clone(), env: json!({"project":session.project,"backend":session.backend}),
        model: json!({"services":{}}), metadata: session.metadata.clone(), fields: session.fields.clone(),
        swarm_secrets: session.bindings.clone(),
    };
    runtime::validate_ownership(&project, &session.docker)
}

fn announce(session: &Session, output: &Output) -> Result<()> {
    output.event("secrets", &format!("{} / {}; checkout {}; Docker target {}",
        session.project, session.backend, session.owner, session.context))
}

fn refresh(session: &mut Session) -> Result<()> {
    session.snapshot = sources::snapshot(&session.root, None)?;
    session.bindings = sources::swarm_bindings(&session.snapshot.local)?;
    Ok(())
}

fn all_policies(session: &Session) -> Result<BTreeMap<String, Value>> {
    let mut policies = policies(&session.metadata)?;
    for field in session.fields.iter().filter(|field| field.kind == "secret") {
        let name = field.path.strip_prefix("secrets.").context("secret fields must be declared under Config.secrets")?;
        valid_name(name)?;
        policies.entry(name.to_owned()).or_insert_with(|| json!({"kind":"prompt"}));
    }
    Ok(policies)
}

pub fn provision(root: &Path, non_interactive: bool, inputs: &BTreeMap<String, SecretInput>, output: &Output) -> Result<Value> {
    preflight_inputs(root, inputs, None)?;
    let _lifecycle = state::lock(root, "lifecycle")?;
    let _config = state::lock(root, "config")?;
    let mut session = session(root, output)?;
    let policies = all_policies(&session)?;
    for name in inputs.keys() {
        valid_name(name)?;
        ensure!(policies.contains_key(name) || session.snapshot.values.pointer(&format!("/secrets/{name}")).is_some(),
            "unknown initial secret input name {name}");
    }
    let _sources = sources::lock_paths(session.snapshot.fingerprints.keys().cloned())?;
    session.snapshot.verify()?;
    validate_namespace(&session)?;
    announce(&session, output)?;
    let missing: Vec<_> = policies.keys().filter(|name|
        session.snapshot.values.pointer(&format!("/secrets/{name}")).is_none_or(Value::is_null)).cloned().collect();
    let stdin_count = missing.iter().filter(|name| match inputs.get(*name) {
        Some(SecretInput::Stdin) => true, Some(SecretInput::File(_)) => false,
        None => policy_kind(&policies[*name]) == "stdin",
    }).count();
    ensure!(stdin_count <= 1, "only one missing secret may consume stdin per invocation");
    if non_interactive {
        let unresolved: Vec<_> = missing.iter().filter(|name| !inputs.contains_key(*name)
            && matches!(policy_kind(&policies[*name]), "prompt" | "reference")).collect();
        if !unresolved.is_empty() {
            return Err(config::MissingInputs { fields: unresolved.into_iter().map(|name|
                session.fields.iter().find(|field| field.path == format!("secrets.{name}")).cloned()
                    .unwrap_or_else(|| crate::model::Field { path: format!("secrets.{name}"), kind:"secret".into(),
                    doc:Some("Supply --secret-file NAME=PATH or an eligible stdin input.".into()),
                    default:None, required:true, choices:vec![] })).collect() }.into());
        }
    }
    // Existing refs are authoritative: missing bytes never cause regeneration.
    let existing: Vec<_> = references(&session.snapshot.values).filter(|(_, reference)| !reference.is_null())
        .map(|(name, reference)| (name.clone(), reference.clone())).collect();
    let mut results = Vec::new();
    for (name, reference) in existing {
        validate_source(&session, &reference)?;
        let mut status = "reused";
        if session.backend == "swarm" && reference.get("file").is_some() {
            if session.bindings.contains_key(&name) { verify_binding(&session, &name)?; }
            else {
                let (file, _) = reference_file(&session.root, &reference)?;
                let bytes = read_bounded(file)?;
                publish_binding(&mut session, &name, &bytes, None)?;
                status = "provisioned";
            }
        }
        results.push(json!({"name":name,"status":status,"reference":reference,"binding":binding(&session,&name)}));
    }
    let mut stdin_used = false;
    for name in missing {
        let reference = obtain_source(&session, &name, &policies[&name], inputs.get(&name), non_interactive, &mut stdin_used)?;
        // Persist initial generated/input bytes before Docker: retries reuse this exact file.
        session.snapshot.verify()?;
        config::set_secret_reference_locked(root, &name, &reference)?;
        refresh(&mut session)?;
        if session.backend == "swarm" {
            let (file, _) = reference_file(&session.root, &reference)?;
            publish_binding(&mut session, &name, &read_bounded(file)?, None)?;
        }
        results.push(json!({"name":name,"status":"provisioned","reference":reference,"binding":binding(&session,&name)}));
    }
    let eligible = crate::allocations::eligible_fields(root, &session.snapshot.values, &session.fields)?;
    let deferred = eligible.iter().any(|path|
        path.split('.').try_fold(&session.snapshot.values, |value, part| value.get(part)).is_none_or(Value::is_null));
    if !deferred { validate_consumers_with_session(root, &session)?; }
    Ok(json!({"secrets":results,"consumerValidation":if deferred {"deferred"} else {"validated"}}))
}

fn binding<'a>(session: &'a Session, name: &str) -> Option<&'a String> {
    if session.backend == "swarm" && session.snapshot.values.pointer(&format!("/secrets/{name}/file")).is_some() {
        session.bindings.get(name)
    } else { None }
}

pub fn list(root: &Path) -> Result<Value> {
    let session = session(root, &Output { json:false, quiet:true })?;
    let consumers = consumers(root)?;
    let mut rows = Vec::new();
    for (name, reference) in references(&session.snapshot.values) {
        let present = validate_source(&session, reference).and_then(|_| {
            if session.backend == "swarm" && reference.get("file").is_some() { verify_binding(&session,name) } else { Ok(()) }
        }).is_ok();
        rows.push(json!({"name":name,"backend":session.backend,"reference":reference,
            "binding":binding(&session,name),"consumers":consumers.get(name).cloned().unwrap_or_default(),"present":present}));
    }
    Ok(json!({"secrets":rows}))
}

pub fn doctor(root: &Path) -> Result<Value> {
    let session = session(root, &Output { json:false, quiet:true })?;
    let mut issues = Vec::new();
    for (name, reference) in references(&session.snapshot.values) {
        let result = validate_source(&session,reference).and_then(|_| {
            if session.backend == "swarm" && reference.get("file").is_some() { verify_binding(&session,name) } else { Ok(()) }
        });
        if let Err(error) = result { issues.push(json!({"secret":name,"error":error.to_string()})); }
    }
    if let Err(error) = validate_consumers_with_session(root,&session) { issues.push(json!({"error":error.to_string()})); }
    Ok(json!({"ok":issues.is_empty(),"issues":issues,"context":session.context,"backend":session.backend}))
}

pub fn replace(root: &Path, name: &str, input: Option<&SecretInput>, apply: bool, non_interactive: bool, output: &Output) -> Result<Value> {
    valid_name(name)?;
    if let Some(input) = input { preflight_inputs(root, &BTreeMap::from([(name.to_owned(),input.clone())]), None)?; }
    let _lifecycle = state::lock(root,"lifecycle")?;
    let config_guard = state::lock(root,"config")?;
    let mut session = session(root,output)?;
    let source_guards = sources::lock_paths(session.snapshot.fingerprints.keys().cloned())?;
    session.snapshot.verify()?;
    ensure!(session.snapshot.values.pointer(&format!("/secrets/{name}")).is_some(), "secret has no existing reference; run setup first");
    validate_namespace(&session)?;
    let project = nickel::evaluate_snapshot(root,&session.snapshot)?;
    runtime::validate_ownership(&project,&session.docker)?;
    let rotation = if apply { Some(rotation_scope(&project,&session.metadata,name)?) } else { None };
    let policy = all_policies(&session)?.remove(name).unwrap_or_else(|| json!({"kind":"prompt"}));
    announce(&session,output)?;
    let reference = obtain_source(&session,name,&policy,input,non_interactive,&mut false)?;
    let mut candidate = session.snapshot.clone();
    candidate.values.as_object_mut().context("settings must be a mapping")?.entry("secrets")
        .or_insert_with(|| json!({})).as_object_mut().context("secrets must be a mapping")?.insert(name.to_owned(),reference.clone());
    validate_candidate_consumers(root,&session,&candidate)?;
    if session.backend == "swarm" {
        let (file,_) = reference_file(&session.root,&reference)?;
        publish_binding(&mut session,name,&read_bounded(file)?,Some(&reference))?;
    } else {
        session.snapshot.verify()?;
        config::set_secret_reference_locked(root,name,&reference)?;
        refresh(&mut session)?;
    }
    let affected = consumers(root)?.remove(name).unwrap_or_default();
    drop(source_guards);
    drop(config_guard);
    if let Some((workflow,services)) = rotation {
        let project = nickel::evaluate_snapshot(root,&session.snapshot)?;
        runtime::execute_actions(&project,&workflow,&services,&session.docker,output)
            .context("source/binding replacement committed; old material untouched; declared application rotation failed")?;
    }
    Ok(json!({"name":name,"reference":reference,"binding":binding(&session,name),"consumers":affected,"applied":apply}))
}

struct SyncSelection { name:String, reference:Value, bytes:Option<Vec<u8>>, rotation:Option<(String,Vec<String>)> }

fn sync_report(rows: &[Value], plan: bool, committed: &[String], applied: &[String], names: &[String], swarm:bool) -> Value {
    let uncommitted: Vec<_> = if swarm { names.iter().filter(|name| !committed.contains(name)).collect() } else { vec![] };
    json!({"operation":"secrets-sync","sideEffects":!plan,"secrets":rows,"committed":committed,"uncommitted":uncommitted,"applied":applied})
}

pub fn sync(root:&Path, names:&[String], plan_only:bool, confirmed:bool, apply:bool, output:&Output) -> Result<Value> {
    ensure!(!names.is_empty(),"secret sync requires explicit names");
    ensure!(plan_only || confirmed,"secret sync requires explicit confirmation");
    let mut unique = BTreeSet::new();
    for name in names { valid_name(name)?; ensure!(unique.insert(name),"duplicate secret sync selection {name}"); }
    let _lifecycle = if plan_only { None } else { Some(state::lock(root,"lifecycle")?) };
    let config_guard = if plan_only { None } else { Some(state::lock(root,"config")?) };
    let mut session = session(root,output)?;
    let source_guards = if plan_only { None } else { Some(sources::lock_paths(session.snapshot.fingerprints.keys().cloned())?) };
    session.snapshot.verify()?;
    validate_namespace(&session)?;
    let swarm = session.backend == "swarm";
    let project = nickel::evaluate_snapshot(root,&session.snapshot)?;
    validate_consumers_with_session(root,&session)?;
    let consumer_map = consumers(root)?;
    let mut selections = Vec::new();
    let mut rows = Vec::new();
    // Preflight all metadata, permissions and procedures before bytes/publication.
    for name in names {
        let reference = session.snapshot.values.pointer(&format!("/secrets/{name}")).context("secret has no existing reference; run setup first")?.clone();
        ensure!(reference.get("file").is_some(), "external named secret {name} has no readable file source to sync");
        let (_,path) = reference_file(root,&reference)?;
        if swarm && session.bindings.contains_key(name) {
            // Explicit sync permits an absent object, but never a foreign existing binding.
            verify_binding_if_present(&session,name)?;
        }
        let origin = session.snapshot.provenance.get(&format!("secrets.{name}"))
            .map(|origin| origin.file.clone()).unwrap_or_else(|| session.snapshot.local_file.clone());
        let rotation = if apply { Some(rotation_scope(&project,&session.metadata,name)?) } else { None };
        rows.push(json!({"name":name,"status":if plan_only {"planned"} else {"uncommitted"},
            "source":{"canonicalPath":path,"origin":origin},"reference":reference,
            "binding":binding(&session,name),"consumers":consumer_map.get(name).cloned().unwrap_or_default(),
            "applied":false,"consumerRestartNeeded":!plan_only}));
        selections.push(SyncSelection { name:name.clone(), reference, bytes:None, rotation });
    }
    if plan_only { return Ok(sync_report(&rows,true,&[],&[],names,swarm)); }
    announce(&session,output)?;
    // Compose only validates original files; Swarm reads every selected source before publishing.
    if swarm {
        for selection in &mut selections {
            let (file,_) = reference_file(root,&selection.reference)?;
            selection.bytes = Some(read_bounded(file)?);
        }
    }
    let mut committed = Vec::new();
    let mut applied = Vec::new();
    for (index,selection) in selections.iter().enumerate() {
        if swarm {
            let old_binding = session.bindings.get(&selection.name).cloned();
            if let Err(error) = publish_binding(&mut session,&selection.name,selection.bytes.as_deref().unwrap(),None) {
                if session.bindings.get(&selection.name) != old_binding.as_ref() {
                    committed.push(selection.name.clone());
                    rows[index]["status"] = json!("published");
                    rows[index]["binding"] = json!(binding(&session,&selection.name));
                }
                return Err(error.context(SyncFailed(sync_report(&rows,false,&committed,&applied,names,swarm))));
            }
            committed.push(selection.name.clone());
            rows[index]["status"] = json!("published");
            rows[index]["binding"] = json!(binding(&session,&selection.name));
        } else { rows[index]["status"] = json!("validated"); }
    }
    for selection in &mut selections { selection.bytes = None; }
    drop(source_guards);
    drop(config_guard);
    if apply {
        let project = nickel::evaluate_snapshot(root,&session.snapshot).map_err(|error|
            error.context(SyncFailed(sync_report(&rows,false,&committed,&applied,names,swarm))))?;
        for (index,selection) in selections.iter().enumerate() {
            let (workflow,services) = selection.rotation.as_ref().unwrap();
            runtime::execute_actions(&project,workflow,services,&session.docker,output).map_err(|error|
                error.context(SyncFailed(sync_report(&rows,false,&committed,&applied,names,swarm))))?;
            applied.push(selection.name.clone());
            rows[index]["applied"] = json!(true);
            rows[index]["consumerRestartNeeded"] = json!(false);
        }
    }
    Ok(sync_report(&rows,false,&committed,&applied,names,swarm))
}

fn inspect_secret(session:&Session, docker_name:&str) -> Result<Value> {
    let info: Value = serde_json::from_str(&session.docker.capture(&["secret".into(),"inspect".into(),docker_name.into()],None)?)?;
    let spec = info.get(0).and_then(|value| value.get("Spec")).context("invalid Docker secret inspection response")?;
    ensure!(spec["Name"].as_str() == Some(docker_name),"Swarm secret name mismatch");
    Ok(spec.clone())
}

fn verify_secret_owner(session:&Session, name:&str, docker_name:&str, spec:&Value) -> Result<()> {
    ensure!(spec["Labels"][OWNER].as_str() == Some(&session.owner)
        && spec["Labels"]["io.dockstride.project"].as_str() == Some(&session.project)
        && spec["Labels"]["io.dockstride.secret"].as_str() == Some(name),
        "Swarm secret {docker_name} has foreign owner {:?}; refusing adoption",spec["Labels"][OWNER]);
    Ok(())
}

fn verify_binding(session:&Session, name:&str) -> Result<()> {
    let docker_name = session.bindings.get(name).with_context(|| format!("secret {name} has no current Swarm binding; run setup or secrets sync"))?;
    let spec = inspect_secret(session,docker_name).with_context(|| format!("bound Swarm secret {docker_name} is missing; explicitly sync from its valid source file"))?;
    verify_secret_owner(session,name,docker_name,&spec)
}

fn verify_binding_if_present(session:&Session, name:&str) -> Result<()> {
    let docker_name = &session.bindings[name];
    // Enumerate names to distinguish absence from permission/daemon inspection failures.
    let names = session.docker.capture(&["secret".into(),"ls".into(),"--format".into(),"{{.Name}}".into()],None)?;
    if names.lines().any(|entry| entry == docker_name) { verify_binding(session,name)?; }
    Ok(())
}

fn publish_binding(session:&mut Session, name:&str, bytes:&[u8], reference:Option<&Value>) -> Result<String> {
    session.snapshot.verify()?;
    let docker_name = format!("dks-{}",state::random_id()?);
    let args = vec!["secret".into(),"create".into(),"--label".into(),format!("{OWNER}={}",session.owner),
        "--label".into(),format!("io.dockstride.project={}",session.project),
        "--label".into(),format!("io.dockstride.secret={name}"),docker_name.clone(),"-".into()];
    session.docker.capture(&args,Some(bytes)).map_err(|error| creation_error(error,bytes))?;
    let publication = (|| -> Result<()> {
        let spec = inspect_secret(session,&docker_name)?;
        verify_secret_owner(session,name,&docker_name,&spec)?;
        session.snapshot.verify()?;
        config::set_swarm_secret_binding_locked(&session.root,name,&docker_name,reference)?;
        Ok(())
    })();
    if let Err(error) = publication {
        // An atomic replace can succeed before its durability acknowledgement fails.
        // Observe the exact random binding rather than reporting a committed object as unbound.
        let committed = config::read_env(&session.root).is_ok_and(|local|
            local.pointer(&format!("/_dockstride/swarmSecrets/{name}")).and_then(Value::as_str)
                == Some(docker_name.as_str()));
        if committed {
            session.bindings.insert(name.to_owned(),docker_name.clone());
            return Err(error.context(format!("Swarm binding {docker_name} committed; publication acknowledgement failed; old material untouched")));
        }
        return Err(error.context(format!("created unbound Docker secret {docker_name}; reconcile manually; no object was adopted or deleted")));
    }
    // Track the authoritative commit before attempting any fallible refresh.
    session.bindings.insert(name.to_owned(),docker_name.clone());
    if let Some(reference) = reference {
        session.snapshot.values.as_object_mut().unwrap().entry("secrets").or_insert_with(||json!({}))
            .as_object_mut().unwrap().insert(name.to_owned(),reference.clone());
    }
    // Refresh source fingerprints before any later publication or trusted action.
    refresh(session).context("Swarm binding committed; unable to refresh source snapshot")?;
    Ok(docker_name)
}

fn validate_source(session:&Session, reference:&Value) -> Result<()> {
    validate_reference_shape(reference)?;
    if reference.get("file").is_some() {
        reference_file(&session.root,reference).context("referenced secret is missing or unreadable; restore this file or explicitly replace")?;
    } else {
        ensure!(session.backend == "swarm","external named secrets require Swarm");
        inspect_secret(session,reference["name"].as_str().unwrap())?;
    }
    Ok(())
}

fn obtain_source(session:&Session, name:&str, policy:&Value, input:Option<&SecretInput>, non_interactive:bool, stdin_used:&mut bool) -> Result<Value> {
    if matches!(input,Some(SecretInput::File(_))) || policy_kind(policy) == "reference" {
        return obtain_reference(&session.root,input,non_interactive);
    }
    let bytes = match input { Some(SecretInput::Stdin) => read_stdin(stdin_used)?,
        Some(SecretInput::File(_)) => unreachable!(), None => obtain(policy,non_interactive,stdin_used)? };
    if session.backend == "compose" { local_scope(session)?; }
    let directory = private_directory(session,true)?;
    let reference = json!({"file":directory.join(format!("{name}--{}",state::random_id()?))});
    write_private(&reference,&bytes,session,name)?;
    Ok(reference)
}

fn validate_candidate_consumers(root:&Path, session:&Session, snapshot:&sources::EnvironmentSnapshot) -> Result<()> {
    let candidate = Session { root:session.root.clone(), snapshot:snapshot.clone(), metadata:session.metadata.clone(),
        fields:session.fields.clone(), docker:Docker::new(root,Output {json:false,quiet:true}), owner:session.owner.clone(),
        context:session.context.clone(), backend:session.backend.clone(), project:session.project.clone(),bindings:session.bindings.clone() };
    validate_consumers_with_session(root,&candidate)
}

pub fn ensure_external_path(root:&Path, path:&Path) -> Result<()> {
    ensure!(path.is_absolute(),"secret path must be absolute");
    reject_symlinks(path)?;
    let root = root.canonicalize().context("cannot resolve checkout boundary")?;
    let mut ancestor = path.to_owned();
    loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor.parent().context("secret path has no existing ancestor")?.to_owned();
            }
            Err(error) => return Err(error).context("cannot inspect secret path ancestry"),
        }
    }
    let canonical = ancestor.canonicalize().context("cannot resolve secret path ancestor")?;
    ensure!(!canonical.starts_with(&root) && !path.starts_with(&root),"secret files/storage must be outside the canonical checkout");
    let directory = if fs::metadata(&canonical)?.is_dir() { canonical } else { canonical.parent().unwrap().to_owned() };
    for parent in directory.ancestors() {
        match fs::symlink_metadata(parent.join(".git")) {
            Ok(_) => bail!("secret files/storage must be outside every Git repository or linked worktree: {}",parent.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error).context("cannot verify Git-free secret ancestry"),
        }
        let metadata = fs::metadata(parent)?;
        let uid = unsafe { libc::geteuid() };
        ensure!(metadata.uid() == uid || metadata.uid() == 0,"secret ancestor must be owned by current user or root");
        ensure!(metadata.mode() & 0o022 == 0 || metadata.mode() & libc::S_ISVTX != 0,
            "secret ancestor is writable by other users: {}",parent.display());
    }
    Ok(())
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

fn policies(metadata:&Value) -> Result<BTreeMap<String,Value>> {
    let Some(value) = metadata.pointer("/setup/secrets") else { return Ok(BTreeMap::new()); };
    let record = value.as_object().context("setup.secrets must be a record")?;
    for (name,policy) in record {
        valid_name(name)?;
        ensure!(matches!(policy_kind(policy),"generate"|"reference"|"prompt"|"stdin"),"removed or unknown secret policy kind for {name}");
        for field in ["durable","recoveryFile","recoveryDirectory"] {
            ensure!(policy.get(field).is_none(),"removed secret policy field {field} for {name}");
        }
    }
    Ok(record.iter().map(|(name,value)|(name.clone(),value.clone())).collect())
}
fn policy_kind(policy:&Value) -> &str {
    policy.get("kind").and_then(Value::as_str).unwrap_or(if policy.get("bytes").is_some() {"generate"} else {"prompt"})
}

fn reference_file(root:&Path, reference:&Value) -> Result<(File,PathBuf)> {
    validate_reference_shape(reference)?;
    let path = Path::new(reference["file"].as_str().context("file reference required")?);
    ensure_external_path(root,path)?;
    let file = open_secure_file(path,libc::O_RDONLY|libc::O_NOATIME,0)
        .context("referenced secret is absent or unreadable; restore this file or explicitly replace")?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.uid() == unsafe {libc::geteuid()}
        && metadata.mode() & 0o007 == 0 && metadata.mode() & 0o022 == 0
        && metadata.len() > 0 && metadata.len() <= 1_048_576,
        "referenced secret file must be nonempty, private, regular, and owned by the current user");
    Ok((file,path.canonicalize()?))
}

pub fn preflight_inputs(root:&Path, inputs:&BTreeMap<String,SecretInput>, candidate:Option<&Value>) -> Result<()> {
    if inputs.is_empty() { return Ok(()); }
    let snapshot = sources::snapshot(root,candidate)?;
    let policies = policies(&nickel::setup_metadata_values(root,&snapshot.values)?)?;
    ensure!(inputs.values().filter(|input| matches!(input,SecretInput::Stdin)).count() <= 1,
        "only one explicit secret input may consume stdin");
    for (name,input) in inputs {
        valid_name(name)?;
        ensure!(!(matches!(input,SecretInput::Stdin) && policies.get(name).is_some_and(|policy| policy_kind(policy) == "reference")),
            "reference secret {name} requires a private file path; stdin cannot supply a reference");
        // Reused references ignore replacement inputs, including unreadable files.
        if snapshot.values.pointer(&format!("/secrets/{name}")).is_none_or(Value::is_null) {
            if let SecretInput::File(path) = input {
                let path = if path.is_absolute() {path.clone()} else {std::env::current_dir()?.join(path)};
                reference_file(root,&json!({"file":path}))?;
            }
        }
    }
    Ok(())
}

fn obtain_reference(root:&Path,input:Option<&SecretInput>, non_interactive:bool) -> Result<Value> {
    let path = match input {
        Some(SecretInput::File(path)) => path.clone(),
        Some(SecretInput::Stdin) => bail!("reference secrets require a private file path, not stdin"),
        None => {
            ensure!(!non_interactive && std::io::stdin().is_terminal(),"reference secret requires --secret-file NAME=PATH");
            PathBuf::from(rpassword::prompt_password("Private secret FILE PATH (hidden): ")?)
        }
    };
    let path = if path.is_absolute() {path} else {std::env::current_dir()?.join(path)};
    let (_,path) = reference_file(root,&json!({"file":path}))?;
    Ok(json!({"file":path}))
}

fn obtain(policy:&Value,non_interactive:bool,stdin_used:&mut bool) -> Result<Vec<u8>> {
    let bytes = match policy_kind(policy) {
        "generate" => {
            let size = policy.get("bytes").and_then(Value::as_u64).unwrap_or(32);
            ensure!((16..=65536).contains(&size),"generated secret bytes must be 16..65536");
            let mut bytes = vec![0;size as usize];
            getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("cryptographic random generation failed"))?;
            match policy.get("encoding").and_then(Value::as_str).unwrap_or("hex") {
                "hex" => hex::encode(bytes).into_bytes(), "base64" => base64(&bytes).into_bytes(),
                _ => bail!("secret encoding must be hex or base64"),
            }
        }
        "stdin" => read_stdin(stdin_used)?,
        "prompt" => {
            ensure!(!non_interactive && std::io::stdin().is_terminal(),"secret input required; supply an original file/stdin input or a hidden prompt");
            rpassword::prompt_password("Secret (hidden): ")?.into_bytes()
        }
        _ => bail!("unknown secret provisioning policy"),
    };
    ensure!(!bytes.is_empty() && bytes.len() <= 1_048_576,"secret must contain 1..1048576 bytes");
    Ok(bytes)
}

fn private_directory(session:&Session,create:bool) -> Result<PathBuf> {
    let uid = unsafe {libc::geteuid()};
    let parent = match session.metadata.pointer("/setup/secretDirectory").and_then(Value::as_str) {
        Some(path) => PathBuf::from(path),
        None => {
            let data = match std::env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty()) {
                Some(path) => PathBuf::from(path),
                None => PathBuf::from(std::env::var_os("HOME").context("HOME missing; declare setup.secretDirectory")?).join(".local/share"),
            };
            data.join("dockstride/secrets")
        }
    };
    ensure_external_path(&session.root,&parent)?;
    use std::os::unix::fs::DirBuilderExt;
    if !parent.exists() {
        ensure!(create,"secret storage is absent; restore files or explicitly replace");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&parent)?;
    }
    let metadata = fs::symlink_metadata(&parent)?;
    ensure!(metadata.is_dir() && (metadata.uid() == uid || metadata.uid() == 0)
        && metadata.mode() & 0o022 == 0,"secret parent has incompatible ownership/permissions");
    let directory = parent.join(format!("u{uid}"));
    ensure_external_path(&session.root,&directory)?;
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error).context("provision the per-user secret directory explicitly; no privilege elevation"),
        }
    }
    let metadata = fs::symlink_metadata(&directory)?;
    ensure!(metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0
        && metadata.mode() & 0o300 == 0o300,"per-user secret directory must be private, current-user owned and writable/traversable");
    Ok(directory)
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
            if !managed { reference_file(root, reference)?; }
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

fn read_bounded(mut file: File) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(!bytes.is_empty() && bytes.len() <= 1_048_576, "secret input size invalid");
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
    error.context("Docker secret creation failed; current source reference retained; retry explicitly")
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
        // Existing private storage may be non-listable. Flush through the file
        // descriptor without changing directory permissions.
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
    let project = nickel::evaluate_snapshot(root, &session.snapshot)?;
    runtime::validate_ownership(&project,&session.docker)?;
    if session.backend != "compose" {
        return Ok(());
    }
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
            let (file, _) = reference_file(root, reference)?;
            let metadata = file.metadata()?;
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
