//! Missing policy ports become ordinary YAML values; live probes are invocation-local.
use crate::{runtime::Docker, sources, state};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{net::{IpAddr, SocketAddr, TcpListener, UdpSocket, ToSocketAddrs}, path::Path};

fn port(value: &Value) -> Result<u16> {
    let port = value.as_u64().context("allocation port must be an integer")?;
    ensure!(port > 0 && port <= 65535, "allocation port must be in 1..65535");
    Ok(port as u16)
}
fn at<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    field.split('.').try_fold(value, |value, key| value.get(key))
}

/// Required native fields may be deferred until credentials make the model complete.
pub(crate) fn eligible_fields(root: &Path, values: &Value, fields: &[crate::model::Field]) -> Result<Vec<String>> {
    let effective = crate::config::effective(values, fields)?;
    if effective["backend"].as_str().unwrap_or("compose") != "compose" { return Ok(Vec::new()); }
    crate::nickel::allocation_fields_values(root, values)
}

struct Policy<'a> {
    field: &'a str,
    service: &'a str,
    host: IpAddr,
    protocol: &'a str,
    from: u16,
    to: u16,
    current: Option<u16>,
}
enum Probe { Tcp(TcpListener), Udp(UdpSocket) }
impl Probe {
    fn bind(address: SocketAddr, protocol: &str) -> std::io::Result<Self> {
        if protocol == "udp" { UdpSocket::bind(address).map(Self::Udp) }
        else { TcpListener::bind(address).map(Self::Tcp) }
    }
    fn address(&self) -> std::io::Result<SocketAddr> {
        match self { Self::Tcp(socket) => socket.local_addr(), Self::Udp(socket) => socket.local_addr() }
    }
}
fn overlaps(left: SocketAddr, right: SocketAddr) -> bool {
    left.port() == right.port() && (left.ip() == right.ip()
        || left.ip().is_unspecified() || right.ip().is_unspecified())
}

/// Caller owns the checkout lifecycle guard. No project hooks run here.
pub fn allocate(root: &Path, docker: &Docker) -> Result<bool> {
    let _config = state::lock(root, "config")?;
    let snapshot = sources::snapshot(root, None)?;
    let _sources = sources::lock_paths(snapshot.fingerprints.keys().cloned())?;
    snapshot.verify()?;
    let fields = crate::nickel::schema_values(root, &snapshot.values)?;
    let values = crate::config::effective(&snapshot.values, &fields)?;
    if values["backend"].as_str().unwrap_or("compose") != "compose" { return Ok(false); }
    let metadata = crate::nickel::setup_metadata_values(root, &snapshot.values)?;
    let Some(policies) = metadata["setup"]["ports"].as_object() else { return Ok(false); };
    let mut parsed = Vec::with_capacity(policies.len());
    for (field, policy) in policies {
        let service = policy["service"].as_str().filter(|service| !service.is_empty()).context("Port allocation requires service")?;
        port(&policy["target"]).with_context(|| format!("Port policy {field} requires a valid container target port"))?;
        let host = policy.get("host").map(|value| value.as_str().context("allocation host must be a string")).transpose()?.unwrap_or("127.0.0.1");
        let host = (host, 0).to_socket_addrs()?.next().context("allocation host has no address")?.ip();
        let protocol = policy.get("protocol").map(|value| value.as_str().context("allocation protocol must be a string")).transpose()?.unwrap_or("tcp");
        ensure!(matches!(protocol, "tcp" | "udp"), "Unsupported allocation protocol {protocol}");
        let from = policy.get("from").map(port).transpose()?.unwrap_or(49152);
        let to = policy.get("to").map(port).transpose()?.unwrap_or(65535);
        ensure!(to >= from, "Invalid allocation range for {field}");
        let current = at(&snapshot.values, field).filter(|value| !value.is_null()).map(port).transpose()?;
        parsed.push(Policy { field, service, host, protocol, from, to, current });
    }
    let mut endpoints = parsed.iter().filter_map(|policy| policy.current.map(|port| (SocketAddr::new(policy.host, port), policy.protocol))).collect::<Vec<_>>();
    let mut probes = Vec::new();
    let mut updates = Vec::new();
    if parsed.iter().any(|policy| policy.current.is_none()) {
        ensure!(crate::runtime::local_context(&docker.context()?), "Automatic port allocation requires a local Unix-socket Docker context; configure fixed ports for a remote daemon");
    }
    for policy in &parsed {
        if policy.current.is_some() { continue; }
        let mut selected = None;
        for port in policy.from..=policy.to {
            let address = SocketAddr::new(policy.host, port);
            if endpoints.iter().any(|(used, protocol)| *protocol == policy.protocol && overlaps(*used, address)) { continue; }
            if let Ok(probe) = Probe::bind(address, policy.protocol) {
                endpoints.push((probe.address()?, policy.protocol));
                probes.push(probe);
                selected = Some(port);
                break;
            }
        }
        let selected = selected.with_context(|| format!("No available port in declared range for {}; edit the ordinary configuration field", policy.field))?;
        updates.push((policy.field.to_owned(), json!(selected)));
    }
    let mut candidate = snapshot.local.clone();
    for (field, value) in &updates { crate::config::put(&mut candidate, field, Some(value.clone()))?; }
    let project = crate::nickel::evaluate(root, Some(&candidate))?;
    for policy in &parsed {
        ensure!(project.services()?.contains_key(policy.service), "Port policy {} names unknown service {}", policy.field, policy.service);
    }
    if updates.is_empty() { return Ok(false); }
    let edited = crate::config::prepare_sets_locked(root, &updates)?;
    snapshot.verify()?;
    state::atomic_write(&root.join("env.yaml"), edited.as_bytes(), 0o600)?;
    drop(probes);
    Ok(true)
}
