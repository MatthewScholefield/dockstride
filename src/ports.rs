//! Explicit endpoint reset and conservative stale-reservation collection.
//! Plans inspect ordinary metadata only and never acquire publication guards.
use crate::{allocations, config, output::Output, publication, registry, runtime, sources, state};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::{BTreeMap,BTreeSet}, fmt, fs, path::Path};

#[derive(Debug)]
pub struct PortsBlocked(pub Value);
impl fmt::Display for PortsBlocked {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Port operation blocked; recorded reservations and settings were retained")
    }
}
impl std::error::Error for PortsBlocked {}

fn strings(args: &[&str]) -> Vec<String> { args.iter().map(|arg| (*arg).to_owned()).collect() }

/// Query the recorded connection, verifying its daemon before resource discovery.
/// `endpoint_only` deliberately ignores retained volumes, networks and credentials.
/// A failed query is an observation error, never evidence of resource absence.
pub fn resource_observations(root: &Path, record: &Value, endpoint_only: bool) -> Value {
    let mut report = json!({"daemonVerified":false,"resources":[],"errors":[],"queries":[]});
    let Some(connection) = record["connection"].as_str().filter(|value| !value.is_empty()) else {
        report["errors"].as_array_mut().unwrap().push(json!({"reason":"missing recorded Docker connection"}));
        return report;
    };
    let Some(daemon) = record["daemonId"].as_str().filter(|value| !value.is_empty()) else {
        report["errors"].as_array_mut().unwrap().push(json!({"reason":"missing verified Docker daemon ID"}));
        return report;
    };
    let Some(owner) = record["ownerId"].as_str().filter(|value| !value.is_empty()) else {
        report["errors"].as_array_mut().unwrap().push(json!({"reason":"missing Docker owner ID"}));
        return report;
    };
    let query = |args: Vec<String>, report: &mut Value| -> Option<String> {
        report["queries"].as_array_mut().unwrap().push(json!(args));
        match runtime::capture_pinned(root, connection, &args) {
            Ok(value) => Some(value),
            Err(error) => {
                report["errors"].as_array_mut().unwrap().push(json!({"args":args,"reason":format!("{error:#}")}));
                None
            }
        }
    };
    let Some(actual) = query(strings(&["info", "--format", "{{.ID}}"]), &mut report) else { return report; };
    report["observedDaemonId"] = json!(actual.trim());
    if actual.trim() != daemon {
        report["errors"].as_array_mut().unwrap().push(json!({"reason":"recorded Docker target now identifies a different daemon","expectedDaemonId":daemon,"observedDaemonId":actual.trim()}));
        return report;
    }
    report["daemonVerified"] = json!(true);
    let mut kinds = vec![("container", vec!["ps", "-aq"])];
    if !endpoint_only {
        kinds.extend([("volume",vec!["volume","ls","-q"]),("network",vec!["network","ls","-q"])]);
    }
    // Swarm objects can outlive containers, including on a Compose checkout's daemon.
    if let Some(swarm) = query(strings(&["info","--format","{{.Swarm.LocalNodeState}}"]), &mut report) {
        if swarm.trim() == "active" {
            kinds.push(("service",vec!["service","ls","-q"]));
            if !endpoint_only { kinds.extend([("secret",vec!["secret","ls","-q"]),("config",vec!["config","ls","-q"])]); }
        } else if swarm.trim() != "inactive" {
            report["errors"].as_array_mut().unwrap().push(json!({"reason":"unable to establish Docker Swarm state","observed":swarm.trim()}));
        }
    }
    for (kind, listing) in kinds {
        let mut args = strings(&listing);
        args.extend(["--filter".to_owned(),format!("label=io.dockstride.owner={owner}")]);
        let Some(ids) = query(args, &mut report) else { continue; };
        for id in ids.split_whitespace().collect::<BTreeSet<_>>() {
            // Record each ID before inspection so partial transport failures retain evidence.
            let mut row = json!({"kind":kind,"id":id,"ownerId":owner,"verified":false});
            let format = match kind {
                "container" => "{{json .Config.Labels}}",
                "service" => "{{json .Spec.Labels}}",
                _ => "{{json .Labels}}",
            };
            if let Some(text) = query(strings(&[kind,"inspect",id,"--format",format]), &mut report) {
                match serde_json::from_str::<Value>(&text) {
                    Ok(labels) if labels["io.dockstride.owner"].as_str() == Some(owner) => {
                        row["verified"] = json!(true);
                        row["labels"] = labels;
                    }
                    Ok(labels) => report["errors"].as_array_mut().unwrap().push(json!({"kind":kind,"id":id,"reason":"resource ownership changed or label filter returned a foreign resource","labels":labels})),
                    Err(error) => report["errors"].as_array_mut().unwrap().push(json!({"kind":kind,"id":id,"reason":format!("invalid resource labels: {error}")})),
                }
            }
            report["resources"].as_array_mut().unwrap().push(row);
        }
    }
    report
}
fn resources_absent(report: &Value) -> bool {
    report["daemonVerified"] == true && report["errors"].as_array().is_some_and(Vec::is_empty)
        && report["resources"].as_array().is_some_and(Vec::is_empty)
}
fn at<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    field.split('.').try_fold(value, |value, part| value.get(part))
}
fn blocker(report: &mut Value, value: Value) { report["blockers"].as_array_mut().unwrap().push(value); }
fn pending_blockers(report: &mut Value) -> Result<()> {
    let pending = publication::pending_global()?;
    for operation in pending["operations"].as_array().unwrap() {
        blocker(report,json!({"reason":"pending publication retains conservative claims","operation":operation}));
    }
    Ok(())
}
fn record_matches(record: &Value, entry: &Value) -> bool {
    ["ownerId","daemonId","connection"].iter().all(|field|
        record.get(*field).is_none_or(|value| value == &entry[*field]))
}
fn target_evidence(record: &Value) -> bool {
    ["ownerId","daemonId","connection"].iter().all(|field| record[*field].as_str().is_some_and(|value| !value.is_empty()))
}
fn reservation_port(key: &str) -> Option<u64> {
    let (address,protocol) = key.rsplit_once('/')?;
    if !matches!(protocol,"tcp"|"udp") { return None; }
    address.rsplit_once(':')?.1.parse().ok()
}
fn endpoint_port(endpoint: &Value) -> Option<u64> {
    let text = endpoint.as_str()?;
    let authority = text.split_once("://").map_or(text, |(_,rest)| rest).split(['/', '?', '#']).next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}
pub(crate) fn endpoints_without(endpoints: &Value, ports: &BTreeSet<u64>) -> Value {
    let mut endpoints = endpoints.clone();
    if let Some(mapping) = endpoints.as_object_mut() {
        mapping.retain(|_,value| endpoint_port(value).is_none_or(|port| !ports.contains(&port)));
    }
    endpoints
}

struct ReleasePlan {
    report: Value,
    global: Value,
    global_before: Option<String>,
    local: Value,
    local_before: Option<String>,
    remove_fields: Vec<String>,
    endpoints: Value,
    snapshot: sources::EnvironmentSnapshot,
}
fn prepare_release(root: &Path) -> Result<ReleasePlan> {
    let canonical = root.to_str().context("checkout path must be UTF-8")?;
    let (mut global,global_before) = allocations::read_global()?;
    let (mut local,local_before) = allocations::read_local(root)?;
    let snapshot = sources::snapshot(root,None)?;
    let entry = registry::read()?["environments"][canonical].clone();
    let mut report = json!({"operation":"ports-release","root":canonical,"plan":true,"reservations":[],"fields":[],"preservedFields":[],"protectedReservations":[],"blockers":[]});
    pending_blockers(&mut report)?;
    let identity = state::read(root,"identity")?;
    if entry.is_null() || identity["id"] != entry["ownerId"] || identity["root"].as_str() != Some(canonical)
        || identity["context"] != entry["connection"] {
        blocker(&mut report,json!({"reason":"checkout is not registered with its saved ownership identity"}));
    }
    let mut selected = BTreeSet::new();
    let mut fields = Vec::new();
    let mut removed_ports = BTreeSet::new();
    for (field,allocation) in local["allocations"].as_object().unwrap() {
        let port = allocation["port"].as_u64().context("invalid local allocation port")?;
        if !record_matches(allocation,&entry) || (allocation["legacy"] != true && !target_evidence(allocation)) {
            blocker(&mut report,json!({"field":field,"reason":"allocation belongs to another owner or Docker target","allocation":allocation}));
            continue;
        }
        let keys = if let Some(key) = allocation["key"].as_str() { vec![key.to_owned()] }
            else { global["reservations"].as_object().unwrap().iter().filter(|(key,value)| value["root"].as_str() == Some(canonical) && reservation_port(key) == Some(port)).map(|(key,_)| key.clone()).collect() };
        if keys.len() != 1 {
            blocker(&mut report,json!({"field":field,"port":port,"reason":"allocation has no unique owned reservation","keys":keys}));
            continue;
        }
        let key = &keys[0];
        let reservation = &global["reservations"][key];
        if reservation["root"].as_str() != Some(canonical) || !record_matches(reservation,&entry)
            || reservation_port(key) != Some(port)
            || reservation.get("port").is_some_and(|value| value.as_u64() != Some(port)) {
            blocker(&mut report,json!({"field":field,"key":key,"reason":"reservation is missing or belongs to another owner or target","reservation":reservation}));
            continue;
        }
        let current = at(&snapshot.local,field);
        if allocation["generated"] == true {
            if current.and_then(Value::as_u64) != Some(port) {
                blocker(&mut report,json!({"field":field,"port":port,"localValue":current,"reason":"generated allocation no longer matches local settings; reconcile explicitly"}));
                continue;
            }
            fields.push(field.clone());
            removed_ports.insert(port);
            report["fields"].as_array_mut().unwrap().push(json!({"field":field,"port":port,"key":key}));
        } else {
            let Some(explicit) = allocation.get("explicitValue") else {
                blocker(&mut report,json!({"field":field,"reason":"explicit allocation has no recorded override provenance"}));
                continue;
            };
            if current.unwrap_or(&Value::Null) != explicit {
                blocker(&mut report,json!({"field":field,"localValue":current,"reason":"explicit allocation provenance no longer matches local settings"}));
                continue;
            }
            report["preservedFields"].as_array_mut().unwrap().push(json!({"field":field,"value":current,"reason":"explicit local override"}));
        }
        selected.insert(key.clone());
    }
    // Modern owner-bound orphan records can be released. Unlinked historical strings cannot.
    for (key,reservation) in global["reservations"].as_object().unwrap() {
        if reservation["root"].as_str() != Some(canonical) { continue; }
        if selected.contains(key) { continue; }
        if reservation["legacy"] == true || !target_evidence(reservation) {
            report["protectedReservations"].as_array_mut().unwrap().push(json!({"key":key,"reason":"insufficient ownership evidence and no matching local allocation"}));
        } else if record_matches(reservation,&entry) {
            selected.insert(key.clone());
        } else {
            blocker(&mut report,json!({"key":key,"reason":"reservation belongs to another owner or target","reservation":reservation}));
        }
    }
    for key in &selected {
        report["reservations"].as_array_mut().unwrap().push(json!({"key":key,"record":global["reservations"][key]}));
        global["reservations"].as_object_mut().unwrap().remove(key);
    }
    local["allocations"].as_object_mut().unwrap().retain(|_,allocation| {
        allocation["key"].as_str().is_none_or(|key| !selected.contains(key))
            && !(allocation["legacy"] == true && selected.iter().any(|key| reservation_port(key) == allocation["port"].as_u64()))
    });
    let resources = resource_observations(root,&entry,true);
    if !resources_absent(&resources) { blocker(&mut report,json!({"reason":"endpoint resource absence was not proved","observations":resources})); }
    report["docker"] = resources;
    let endpoints = endpoints_without(&entry["allocatedEndpoints"],&removed_ports);
    Ok(ReleasePlan {report,global,global_before,local,local_before,remove_fields:fields,endpoints,snapshot})
}

/// Release is an explicit reset, not part of ordinary down/destroy.
pub fn release(root: &Path, plan: bool, confirmed: bool, _output: &Output) -> Result<Value> {
    ensure!(plan || confirmed,"port release requires --yes (or --plan)");
    let root = fs::canonicalize(root)?;
    if plan { return Ok(prepare_release(&root)?.report); }
    let _lifecycle = state::lock(&root,"lifecycle")?;
    let _global = state::global_lock()?;
    let _local = state::lock(&root,"port-allocation")?;
    let _config = state::lock(&root,"config")?;
    publication::recover_locked(&root)?;
    let mut prepared = prepare_release(&root)?;
    if !prepared.report["blockers"].as_array().unwrap().is_empty() { return Err(PortsBlocked(prepared.report).into()); }
    let _sources = sources::lock_paths(prepared.snapshot.fingerprints.keys().cloned())?;
    prepared.snapshot.verify()?;
    let count = prepared.report["reservations"].as_array().unwrap().len();
    if count > 0 {
        let global = publication::Change::replace(&state::global_root()?.join(".dockstride/port-reservations.json"),&serde_json::to_vec_pretty(&prepared.global)?,0o600)?;
        global.expect_before(&prepared.global_before)?;
        let local = publication::Change::replace(&root.join(".dockstride/ports.json"),&serde_json::to_vec_pretty(&prepared.local)?,0o600)?;
        local.expect_before(&prepared.local_before)?;
        let mut changes = vec![global,local];
        if !prepared.remove_fields.is_empty() { changes.push(config::prepare_removals_locked(&root,&prepared.remove_fields)?); }
        changes.push(registry::endpoint_change(&root,&prepared.endpoints,&prepared.local["allocations"])?);
        prepared.snapshot.verify()?;
        let entry = registry::read()?["environments"][root.to_str().unwrap()].clone();
        publication::publish(&root,"release-ports",changes,json!({"root":root,"ownerId":entry["ownerId"],"daemonId":entry["daemonId"],"connection":entry["connection"],"reservations":prepared.report["reservations"]}))?;
    }
    prepared.report["plan"] = json!(false);
    prepared.report["released"] = json!(count);
    Ok(prepared.report)
}

struct GcPlan { report: Value, global: Value, before: Option<String>, removed: Vec<(String,String)> }
fn prepare_gc(root: &Path) -> Result<GcPlan> {
    let (mut global,before) = allocations::read_global()?;
    let registry = registry::read()?;
    let mut report = json!({"operation":"ports-gc","plan":true,"candidates":[],"protected":[],"blockers":[]});
    pending_blockers(&mut report)?;
    let mut removed = Vec::new();
    let mut observed_targets = BTreeMap::new();
    for (key,record) in global["reservations"].as_object().unwrap() {
        let checkout = record["root"].as_str().context("reservation root missing")?;
        let saved = &registry["environments"][checkout];
        let absent = match fs::symlink_metadata(checkout) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                report["protected"].as_array_mut().unwrap().push(json!({"key":key,"record":record,"reason":format!("cannot establish checkout absence: {error}")}));
                continue;
            }
            Ok(_) => false,
        };
        let reason = if record["legacy"] == true { Some("legacy reservation lacks authoritative target/owner evidence") }
            else if !target_evidence(record) { Some("reservation lacks authoritative target/owner evidence") }
            else if !absent && !saved.is_null() { Some("checkout exists and registration is not retired") }
            else if !saved.is_null() && !record_matches(record,saved) { Some("reservation conflicts with saved registration") }
            else { None };
        if let Some(reason) = reason {
            report["protected"].as_array_mut().unwrap().push(json!({"key":key,"record":record,"reason":reason}));
            continue;
        }
        let target = (record["ownerId"].as_str().unwrap(),record["daemonId"].as_str().unwrap(),record["connection"].as_str().unwrap());
        let observations = observed_targets.entry(target).or_insert_with(|| resource_observations(root,record,true));
        if !resources_absent(&observations) {
            report["protected"].as_array_mut().unwrap().push(json!({"key":key,"record":record,"reason":"endpoint resource absence was not proved","observations":observations}));
            continue;
        }
        report["candidates"].as_array_mut().unwrap().push(json!({"key":key,"record":record,"checkoutAbsent":absent,"registrationRetired":saved.is_null(),"observations":observations}));
        removed.push((checkout.to_owned(),key.clone()));
    }
    drop(observed_targets);
    for (_,key) in &removed { global["reservations"].as_object_mut().unwrap().remove(key); }
    Ok(GcPlan {report,global,before,removed})
}

pub fn gc(root: &Path, plan: bool, confirmed: bool, _output: &Output) -> Result<Value> {
    ensure!(plan || confirmed,"port GC requires --yes (or --plan)");
    let root = fs::canonicalize(root)?;
    if plan { return Ok(prepare_gc(&root)?.report); }
    let _lifecycle = state::lock(&root,"lifecycle")?;
    let _global = state::global_lock()?;
    let _local = state::lock(&root,"port-allocation")?;
    let _config = state::lock(&root,"config")?;
    publication::recover_locked(&root)?;
    let mut prepared = prepare_gc(&root)?;
    if !prepared.report["blockers"].as_array().unwrap().is_empty() { return Err(PortsBlocked(prepared.report).into()); }
    if !prepared.removed.is_empty() {
        let change = publication::Change::replace(&state::global_root()?.join(".dockstride/port-reservations.json"),&serde_json::to_vec_pretty(&prepared.global)?,0o600)?;
        change.expect_before(&prepared.before)?;
        let mut changes = vec![change];
        let saved_registry = registry::read()?;
        if prepared.removed.iter().any(|(root,_)| saved_registry["environments"].get(root).is_some()) {
            changes.push(registry::prune_allocations_change(&prepared.removed)?);
        }
        // No other checkout's lifecycle, source files, or local metadata is touched.
        publication::publish(&root,"gc-ports",changes,json!({"reservations":prepared.report["candidates"]}))?;
    }
    prepared.report["plan"] = json!(false);
    prepared.report["collected"] = json!(prepared.removed.len());
    Ok(prepared.report)
}
