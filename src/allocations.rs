//! Generated endpoint ownership and explicit local override provenance.
use crate::{publication, runtime::Docker, sources, state};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, path::Path};

fn port(value: &Value) -> Result<u16> {
    let port = value.as_u64().context("allocation port must be an integer")?;
    ensure!(port > 0 && port <= 65535, "allocation port must be in 1..65535");
    Ok(port as u16)
}

fn normalize_global(value: Value) -> Result<Value> {
    if value.is_null() { return Ok(json!({"schemaVersion":1,"reservations":{}})); }
    let modern = value.get("schemaVersion").is_some();
    if modern { ensure!(value["schemaVersion"] == 1, "unsupported port reservation schema"); }
    let entries = if modern { &value["reservations"] } else { &value };
    let mut reservations = entries.as_object().context("port reservations must be a mapping")?.clone();
    for (key, entry) in &mut reservations {
        if let Some(root) = entry.as_str() { *entry = json!({"root":root,"legacy":true}); }
        ensure!(entry.is_object() && entry["root"].as_str().is_some_and(|root| Path::new(root).is_absolute()), "invalid reservation owner for {key}");
        if let Some(value) = entry.get("port") { port(value)?; }
        for field in ["ownerId", "daemonId", "connection", "field", "host", "protocol"] {
            if let Some(value) = entry.get(field) { ensure!(value.as_str().is_some_and(|text| !text.is_empty()), "invalid reservation {field} for {key}"); }
        }
        if let Some(value) = entry.get("legacy") { ensure!(value.is_boolean(), "invalid legacy reservation flag for {key}"); }
    }
    Ok(json!({"schemaVersion":1,"reservations":reservations}))
}

fn normalize_local(value: Value) -> Result<Value> {
    if value.is_null() { return Ok(json!({"schemaVersion":1,"allocations":{}})); }
    let modern = value.get("schemaVersion").is_some();
    if modern { ensure!(value["schemaVersion"] == 1, "unsupported local allocation schema"); }
    let entries = if modern { &value["allocations"] } else { &value };
    let mut allocations = entries.as_object().context("local allocations must be a mapping")?.clone();
    for (field, entry) in &mut allocations {
        if entry.is_u64() { *entry = json!({"port":port(entry)?,"generated":true,"legacy":true}); }
        ensure!(entry.is_object() && entry["generated"].is_boolean(), "invalid local allocation provenance for {field}");
        port(&entry["port"])?;
        for name in ["key", "ownerId", "daemonId", "connection"] {
            if let Some(value) = entry.get(name) { ensure!(value.as_str().is_some_and(|text| !text.is_empty()), "invalid local allocation {name} for {field}"); }
        }
        if let Some(value) = entry.get("legacy") { ensure!(value.is_boolean(), "invalid legacy allocation flag for {field}"); }
    }
    Ok(json!({"schemaVersion":1,"allocations":allocations}))
}

/// Secure same-byte reads, with normalization in memory only. No locks or writes.
pub fn read_global() -> Result<(Value, Option<String>)> {
    let (value, fingerprint) = state::read_fingerprinted(&state::global_root()?, "port-reservations")?;
    Ok((normalize_global(value)?, fingerprint))
}
pub fn read_local(root: &Path) -> Result<(Value, Option<String>)> {
    let (value, fingerprint) = state::read_fingerprinted(root, "ports")?;
    Ok((normalize_local(value)?, fingerprint))
}
fn at<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    field.split('.').try_fold(value, |value, key| value.get(key))
}

fn check_local(field: &str, entry: &Value, local: &Value) -> Result<()> {
    let current = at(local, field);
    if entry["generated"] == true {
        ensure!(current == Some(&entry["port"]), "Allocated port {field} conflicts with its saved generated value {}; reconcile or restore the local value before release", entry["port"]);
    } else {
        ensure!(entry.get("explicitValue").is_some(), "Explicit allocation {field} has no recorded override provenance");
        let expected = &entry["explicitValue"];
        ensure!(current.unwrap_or(&Value::Null) == expected, "Allocated port {field} changed outside Dockstride; reconcile its recorded explicit override before continuing");
    }
    Ok(())
}

/// Caller holds global -> local allocation -> config/source guards. An explicit
/// same-value write is an intentional override; untouched shared values are not.
pub(crate) fn prepare_local_edit(root: &Path, before: &Value, candidate: &Value, touched: &[String]) -> Result<Option<(publication::Change, Value)>> {
    let (mut saved, fingerprint) = read_local(root)?;
    let original = saved.clone();
    for (field, entry) in saved["allocations"].as_object_mut().unwrap() {
        check_local(field, entry, before)?;
        if at(before, field) != at(candidate, field) || touched.iter().any(|path| field == path || field.starts_with(&format!("{path}."))) {
            entry["generated"] = json!(false);
            entry["explicitValue"] = at(candidate, field).cloned().unwrap_or(Value::Null);
            // A legacy allocation retains its insufficient target evidence until
            // verified allocation reconciliation; explicit edits never invent it.
        }
    }
    if saved == original { return Ok(None); }
    let change = publication::Change::replace(&root.join(".dockstride/ports.json"), &serde_json::to_vec_pretty(&saved)?, 0o600)?;
    change.expect_before(&fingerprint)?;
    Ok(Some((change, saved["allocations"].clone())))
}

/// Required native fields may be deferred until credentials make the model complete.
pub(crate) fn eligible_fields(root: &Path, values: &Value, fields: &[crate::model::Field]) -> Result<Vec<String>> {
    let effective = crate::config::effective(values, fields)?;
    if effective["backend"].as_str().unwrap_or("compose") != "compose" { return Ok(Vec::new()); }
    let mut fields = crate::nickel::allocation_fields_values(root, values)?;
    let (saved, _) = read_local(root)?;
    fields.retain(|field| saved["allocations"][field]["generated"] != false);
    Ok(fields)
}
/// Caller owns the invoking lifecycle guard. No project hooks run here.
pub fn allocate(root: &Path, docker: &Docker) -> Result<bool> {
    let global = state::global_root()?;
    let _global = state::global_lock()?;
    let _local = state::lock(root, "port-allocation")?;
    let _config = state::lock(root, "config")?;
    publication::recover_locked(root)?;
    let snapshot = sources::snapshot(root, None)?;
    let _sources = sources::lock_paths(snapshot.fingerprints.keys().cloned())?;
    snapshot.verify()?;
    let fields = crate::nickel::schema_values(root, &snapshot.values)?;
    let values = crate::config::effective(&snapshot.values, &fields)?;
    if values["backend"].as_str().unwrap_or("compose") != "compose" { return Ok(false); }
    let metadata = crate::nickel::setup_metadata(root, Some(&snapshot.local))?;
    let Some(policies) = metadata["setup"]["ports"].as_object() else { return Ok(false); };
    if policies.is_empty() { return Ok(false); }
    let (mut saved, saved_before) = read_local(root)?;
    let (mut reservations, reservations_before) = read_global()?;
    let original_saved = saved.clone();
    let original_reservations = reservations.clone();
    let canonical = fs::canonicalize(root)?.to_str().context("checkout path must be UTF-8")?.to_owned();
    let registry = crate::registry::read()?;
    let entry = registry["environments"].get(&canonical);
    let identity = state::read(root, "identity")?;
    ensure!(identity["root"].as_str() == Some(&canonical), "allocation ownership identity belongs to another checkout");
    let owner = identity["id"].as_str().context("initialize checkout ownership before allocating endpoints")?;
    let connection = docker.context()?;
    ensure!(identity["context"].as_str() == Some(&connection), "Docker connection differs from allocation ownership target");
    if let Some(entry) = entry {
        ensure!(entry["ownerId"].as_str() == Some(owner) && entry["connection"].as_str() == Some(&connection), "registered allocation ownership or target differs");
    }
    let daemon = docker.capture_timeout(&["info".into(), "--format".into(), "{{.ID}}".into()], None, 30)?;
    let daemon = daemon.trim();
    ensure!(!daemon.is_empty() && daemon != "<no value>" && !daemon.chars().any(char::is_whitespace), "Docker daemon has no verifiable identity");
    if let Some(entry) = entry {
        ensure!(entry["daemonId"].as_str() == Some(daemon), "Docker daemon differs from registered allocation target");
    }
    let pending = publication::pending_global()?;
    let mut pending_ports = std::collections::BTreeSet::new();
    for operation in pending["operations"].as_array().unwrap() {
        if let Some(reservations) = operation["claims"]["reservations"].as_object() {
            pending_ports.extend(reservations.keys().map(String::as_str));
        } else if let Some(reservations) = operation["claims"]["reservations"].as_array() {
            pending_ports.extend(reservations.iter().filter_map(|row| row.as_str().or_else(|| row["key"].as_str())));
        }
    }
    for (field, allocation) in saved["allocations"].as_object().unwrap() {
        check_local(field, allocation, &snapshot.local)?;
    }
    let mut updates = Vec::new();
    for (field, policy) in policies {
        let current = at(&snapshot.values, field).filter(|value| !value.is_null());
        let existing = saved["allocations"].get(field).cloned();
        if let Some(allocation) = &existing {
            check_local(field, allocation, &snapshot.local)?;
            for (name, expected) in [("ownerId", owner), ("daemonId", daemon), ("connection", connection.as_str())] {
                if let Some(recorded) = allocation.get(name) { ensure!(recorded.as_str() == Some(expected), "Allocation {field} {name} belongs to another environment"); }
            }
        } else if current.is_some() { continue; }
        let service = policy["service"].as_str().context("Port allocation requires service")?;
        ensure!(!service.is_empty(), "Port allocation requires a nonempty service");
        ensure!(policy["target"].as_u64().is_some_and(|port| port > 0 && port <= 65535), "Port policy {field} requires a valid container target port");
        if existing.is_none() {
            ensure!(crate::runtime::local_context(&connection), "Automatic port allocation requires a local Unix-socket Docker context; configure fixed ports for a remote daemon");
        }
        let host = policy["host"].as_str().unwrap_or("127.0.0.1");
        let protocol = policy["protocol"].as_str().unwrap_or("tcp");
        ensure!(matches!(protocol, "tcp" | "udp"), "Unsupported allocation protocol {protocol}");
        let allocated = if let Some(existing) = &existing { port(&existing["port"])? } else {
            let start = port(&json!(policy["from"].as_u64().unwrap_or(49152)))?;
            let end = port(&json!(policy["to"].as_u64().unwrap_or(65535)))?;
            ensure!(end >= start, "Invalid allocation range for {field}");
            (start..=end).find(|port| {
                let suffix = format!(":{port}/{protocol}");
                !reservations["reservations"].as_object().unwrap().keys().any(|key| key.ends_with(&suffix))
                    && !pending_ports.iter().any(|key| key.ends_with(&suffix))
                    && crate::runtime::bindable(host, *port, protocol)
            }).context("No unreserved available port in declared range")?
        };
        let key = existing.as_ref().and_then(|allocation| allocation["key"].as_str()).map(str::to_owned).unwrap_or_else(|| format!("{host}:{allocated}/{protocol}"));
        if existing.as_ref().is_some_and(|allocation| allocation["generated"] == true) {
            ensure!(key == format!("{host}:{allocated}/{protocol}"), "Port policy {field} changed its reserved endpoint; release the allocation explicitly before changing host/protocol");
        }
        if let Some(reservation) = reservations["reservations"].get(&key) {
            ensure!(reservation["root"].as_str() == Some(&canonical), "Port reservation {key} belongs to another checkout");
            if let Some(recorded) = reservation.get("port") { ensure!(port(recorded)? == allocated, "Port reservation {key} disagrees with saved allocation {field}"); }
            for (name, expected) in [("ownerId", owner), ("daemonId", daemon), ("connection", connection.as_str()), ("field", field.as_str())] {
                if let Some(recorded) = reservation.get(name) { ensure!(recorded.as_str() == Some(expected), "Port reservation {key} {name} conflicts with invoking allocation"); }
            }
        } else {
            ensure!(existing.is_none(), "Saved allocation {field} has no matching global reservation {key}; reconcile before continuing");
        }
        if existing.as_ref().is_some_and(|allocation| allocation["generated"] == false) { continue; }
        reservations["reservations"][&key] = json!({"root":canonical,"ownerId":owner,"daemonId":daemon,"connection":connection,"field":field,"port":allocated,"host":host,"protocol":protocol});
        saved["allocations"][field] = json!({"port":allocated,"key":key,"generated":true,"ownerId":owner,"daemonId":daemon,"connection":connection});
        if current.is_none() { updates.push((field.clone(), json!(allocated))); }
    }
    let mut local = snapshot.local.clone();
    for (field, value) in &updates { crate::config::put(&mut local, field, Some(value.clone()))?; }
    let allocated = crate::nickel::evaluate(root, Some(&local))?;
    for field in saved["allocations"].as_object().unwrap().keys() {
        if let Some(service) = policies.get(field).and_then(|policy| policy["service"].as_str()) {
            ensure!(allocated.services()?.contains_key(service), "Port policy {field} names unknown service {service}");
        }
    }
    let registration = crate::registry::prepare_with_docker(&allocated, &snapshot.sources, docker, Some(&saved["allocations"]))?;
    ensure!(registration.owner_id == owner, "allocation registration changed checkout ownership");
    if saved == original_saved && reservations == original_reservations && updates.is_empty() && registration.changes.is_empty() { return Ok(false); }
    let mut changes = vec![
        publication::Change::replace(&global.join(".dockstride/port-reservations.json"), &serde_json::to_vec_pretty(&reservations)?, 0o600)?,
        publication::Change::replace(&root.join(".dockstride/ports.json"), &serde_json::to_vec_pretty(&saved)?, 0o600)?,
    ];
    changes[0].expect_before(&reservations_before)?;
    changes[1].expect_before(&saved_before)?;
    if !updates.is_empty() { changes.push(crate::config::prepare_sets_locked(root, &updates)?); }
    changes.extend(registration.changes);
    snapshot.verify()?;
    publication::publish(root, "allocate-ports", changes, json!({"root":canonical,"ownerId":owner,"daemonId":daemon,"connection":connection,"reservations":reservations["reservations"]}))?;
    Ok(!updates.is_empty())
}
