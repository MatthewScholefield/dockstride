use crate::{
    model::Project,
    output::Output,
    runtime::{self, Docker},
    state,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    thread,
    time::{Duration, Instant},
};

const OWNER: &str = "io.dockstride.owner";
const PROJECT: &str = "io.dockstride.project";

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}
fn text(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => bail!("expected a scalar, got {value}"),
    }
}
fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .with_context(|| format!("{what} must be a record"))
}
fn flag(argv: &mut Vec<String>, name: &str, value: impl Into<String>) {
    argv.push(name.into());
    argv.push(value.into());
}
fn inspect(docker: &Docker, kind: &str, name: &str) -> Result<Value> {
    let output = docker.capture(&args(&[kind, "inspect", name]), None)?;
    let values: Value =
        serde_json::from_str(&output).context("Docker inspect returned invalid JSON")?;
    values
        .get(0)
        .cloned()
        .context("Docker inspect returned no object")
}
fn manager(docker: &Docker) -> Result<Value> {
    let info: Value =
        serde_json::from_str(&docker.capture(&args(&["info", "--format", "{{json .}}"]), None)?)?;
    ensure!(
        info.pointer("/Swarm/LocalNodeState")
            .and_then(Value::as_str)
            == Some("active")
            && info
                .pointer("/Swarm/ControlAvailable")
                .and_then(Value::as_bool)
                == Some(true),
        "deploy requires a Swarm manager in the selected Docker context; initialize Swarm explicitly, not as a deployment side effect"
    );
    Ok(info)
}
fn selection(project: &Project, selected: &[String]) -> Result<Vec<String>> {
    let services = project.services()?;
    let names: Vec<String> = if selected.is_empty() {
        services.keys().cloned().collect()
    } else {
        selected.to_vec()
    };
    let mut seen = BTreeSet::new();
    for name in &names {
        ensure!(services.contains_key(name), "unknown service {name}");
        ensure!(seen.insert(name), "service {name} was selected twice");
    }
    ensure!(!names.is_empty(), "deployment has no services");
    Ok(names)
}
fn validate_model(project: &Project) -> Result<()> {
    for (name, service) in project.services()? {
        let fields = object(service, &format!("service {name}"))?;
        for incompatible in [
            "privileged",
            "container_name",
            "network_mode",
            "links",
            "external_links",
            "devices",
            "pid",
            "ipc",
            "volumes_from",
            "restart",
        ] {
            ensure!(
                !fields.contains_key(incompatible),
                "service {name}: {incompatible} has no supported Swarm interpretation; declare a compatible Swarm service explicitly"
            );
        }
        for key in fields.keys() {
            ensure!(
                key.starts_with("x-")
                    || [
                        "image",
                        "build",
                        "develop",
                        "depends_on",
                        "profiles",
                        "environment",
                        "env_file",
                        "command",
                        "entrypoint",
                        "user",
                        "working_dir",
                        "hostname",
                        "read_only",
                        "init",
                        "tty",
                        "stop_grace_period",
                        "stop_signal",
                        "labels",
                        "networks",
                        "volumes",
                        "ports",
                        "secrets",
                        "configs",
                        "healthcheck",
                        "deploy",
                        "logging",
                        "dns",
                        "dns_search",
                        "extra_hosts",
                        "isolation",
                        "credential_spec",
                        "sysctls",
                        "ulimits",
                        "cap_add",
                        "cap_drop",
                        "group_add"
                    ]
                    .contains(&key.as_str()),
                "service {name}: unsupported Swarm field {key}; it will not be silently ignored"
            );
        }
        if let Some(mode) = service.pointer("/deploy/mode").and_then(Value::as_str) {
            ensure!(
                ["replicated", "global"].contains(&mode),
                "service {name}: job modes require an explicitly declared one-shot deploy prerequisite, not a long-running stack rollout"
            );
        }
        if let Some(build) = fields.get("build") {
            if let Some(build) = build.as_object() {
                for key in build.keys() {
                    ensure!(
                        [
                            "context",
                            "dockerfile",
                            "args",
                            "target",
                            "platform",
                            "labels",
                            "network",
                            "no_cache",
                            "pull"
                        ]
                        .contains(&key.as_str()),
                        "service {name}: unsupported build field {key}"
                    );
                }
            } else {
                ensure!(
                    build.is_string(),
                    "service {name}: build must be a path or record"
                );
            }
        }
    }
    let rendered = project.swarm()?;
    if let Some(secrets) = rendered.get("secrets").and_then(Value::as_object) {
        for (name, spec) in secrets {
            ensure!(
                spec.get("external").and_then(Value::as_bool) == Some(true),
                "Swarm secret {name} must be an external immutable provisioned reference; run setup with a durable recovery source first"
            );
        }
    }
    Ok(())
}
fn repository(image: &str) -> &str {
    let image = image.split('@').next().unwrap_or(image);
    match image.rfind(':') {
        Some(i) if !image[i..].contains('/') => &image[..i],
        _ => image,
    }
}
fn registry(repository: &str) -> Option<&str> {
    let (first, _) = repository.split_once('/')?;
    (first.contains('.') || first.contains(':') || first == "localhost").then_some(first)
}
fn image_name(project: &Project, name: &str) -> Result<String> {
    let service = &project.services()?[name];
    if let Some(image) = service.get("image").and_then(Value::as_str) {
        return Ok(image.into());
    }
    bail!(
        "service {name} requires an explicit image repository; Swarm publishing cannot infer registry ownership"
    )
}
fn build_args(project: &Project, name: &str, tag: &str) -> Result<Vec<String>> {
    let build = &project.services()?[name]["build"];
    let mut argv = args(&["build", "--tag", tag]);
    let context = if let Some(path) = build.as_str() {
        path
    } else {
        build.get("context").and_then(Value::as_str).unwrap_or(".")
    };
    if let Some(dockerfile) = build.get("dockerfile") {
        let dockerfile = dockerfile
            .as_str()
            .context("build.dockerfile must be a path")?;
        let path = std::path::Path::new(context).join(dockerfile);
        flag(&mut argv, "--file", path.to_string_lossy().into_owned());
    }
    for (key, option) in [
        ("target", "--target"),
        ("platform", "--platform"),
        ("network", "--network"),
    ] {
        if let Some(value) = build.get(key) {
            flag(&mut argv, option, text(value)?);
        }
    }
    if build.get("no_cache").and_then(Value::as_bool) == Some(true) {
        argv.push("--no-cache".into());
    }
    if build.get("pull").and_then(Value::as_bool) == Some(true) {
        argv.push("--pull".into());
    }
    for (key, option) in [("args", "--build-arg"), ("labels", "--label")] {
        if let Some(values) = build.get(key) {
            for (key, value) in key_values(values)? {
                flag(&mut argv, option, format!("{key}={value}"));
            }
        }
    }
    argv.push(context.into());
    Ok(argv)
}
fn key_values(value: &Value) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    if let Some(map) = value.as_object() {
        for (key, value) in map {
            values.insert(key.clone(), text(value).with_context(|| format!("{key} must have an explicit value; inherited host environment is not a deploy contract"))?);
        }
    } else if let Some(list) = value.as_array() {
        for item in list {
            let item = item.as_str().context("expected key=value string")?;
            let (key, value) = item
                .split_once('=')
                .context("deploy requires explicit key=value, not inherited host environment")?;
            values.insert(key.into(), value.into());
        }
    } else {
        bail!("expected record or key=value list");
    }
    Ok(values)
}
fn digest(docker: &Docker, image: &str) -> Result<String> {
    let values: Value = serde_json::from_str(&docker.capture(
        &args(&[
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            image,
        ]),
        None,
    )?)?;
    let repo = repository(image);
    let digests = values
        .as_array()
        .context("published image has no repository digests")?;
    let pin = digests
        .iter()
        .filter_map(Value::as_str)
        .find(|s| s.split('@').next() == Some(repo))
        .or_else(|| {
            if digests.len() == 1 {
                digests[0].as_str()
            } else {
                None
            }
        })
        .context("registry returned no unambiguous image digest")?;
    ensure!(
        pin.contains("@sha256:"),
        "registry did not return a sha256 digest for {image}"
    );
    Ok(pin.into())
}
fn resolve_images(
    project: &Project,
    names: &[String],
    previous: &Value,
    docker: &Docker,
    output: &Output,
    operation: &str,
) -> Result<Map<String, Value>> {
    let mut images = previous
        .get("images")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for name in names {
        let image = image_name(project, name)?;
        let old = images.get(name).cloned().unwrap_or(Value::Null);
        let result = if project.services()?[name].get("build").is_some() {
            let repo = repository(&image);
            ensure!(
                registry(repo).is_some(),
                "built service {name} needs an explicit registry repository, not {image}"
            );
            let revision = state::random_id()?;
            let tag = format!("{repo}:dks-{revision}");
            let argv = build_args(project, name, &tag)?;
            operation_run(project, docker, operation, output, "build", &argv)?;
            let id = docker
                .capture(
                    &args(&["image", "inspect", "--format", "{{.Id}}", &tag]),
                    None,
                )?
                .trim()
                .to_owned();
            if old.get("imageId").and_then(Value::as_str) == Some(id.as_str())
                && old.get("repository").and_then(Value::as_str) == Some(repo)
            {
                let pin = old
                    .get("digest")
                    .and_then(Value::as_str)
                    .context("previous build revision has no digest")?;
                operation_run(
                    project,
                    docker,
                    operation,
                    output,
                    "verify-image",
                    &args(&["pull", pin]),
                )?;
                output.event(
                    "publish",
                    &format!("{name}: reusing immutable revision {pin}"),
                )?;
                old
            } else {
                operation_run(
                    project,
                    docker,
                    operation,
                    output,
                    "publish",
                    &args(&["push", &tag]),
                )?;
                json!({"repository":repo,"tag":tag,"revision":revision,"imageId":id,"digest":digest(docker,&tag)?})
            }
        } else {
            operation_run(
                project,
                docker,
                operation,
                output,
                "pull",
                &args(&["pull", &image]),
            )?;
            json!({"repository":repository(&image),"digest":digest(docker,&image)?})
        };
        images.insert(name.clone(), result);
    }
    Ok(images)
}
fn journal(project: &Project, operation: &str, entry: Value) -> Result<()> {
    state::journal(&project.root, operation, &entry)
}
fn operation_run(
    project: &Project,
    docker: &Docker,
    operation: &str,
    output: &Output,
    phase: &str,
    argv: &[String],
) -> Result<()> {
    journal(
        project,
        operation,
        json!({"phase":phase,"state":"started","docker":argv}),
    )?;
    output.event(phase, &format!("docker {}", argv.join(" ")))?;
    docker
        .run(argv, None)
        .with_context(|| format!("{phase} failed: docker {}", argv.join(" ")))?;
    journal(
        project,
        operation,
        json!({"phase":phase,"state":"completed","docker":argv}),
    )
}
fn set_labels(value: &mut Value, owner: &str, project: &str) -> Result<()> {
    let map = value
        .as_object_mut()
        .context("resource definition must be a record")?;
    let mut labels = match map.get("labels") {
        Some(v) => key_values(v)?,
        None => BTreeMap::new(),
    };
    for (key, value) in [(OWNER, owner), (PROJECT, project)] {
        ensure!(
            labels.get(key).is_none_or(|existing| existing == value),
            "reserved ownership label {key} conflicts with deployment identity"
        );
        labels.insert(key.into(), value.into());
    }
    map.insert("labels".into(), serde_json::to_value(labels)?);
    Ok(())
}
fn render_owned(project: &Project, owner: &str) -> Result<Value> {
    let mut rendered = project.swarm()?;
    let name = project.name()?;
    let services = rendered
        .get_mut("services")
        .and_then(Value::as_object_mut)
        .context("Swarm document has no services")?;
    for (service_name, service) in services.iter_mut() {
        set_labels(service, owner, name)?;
        let map = service
            .as_object_mut()
            .context("service must be a record")?;
        let deploy = map.entry("deploy").or_insert_with(|| json!({}));
        set_labels(deploy, owner, name)?;
        let attachments = map
            .entry("networks")
            .or_insert_with(|| json!({"default":{}}));
        if let Some(list) = attachments.as_array() {
            let mut records = Map::new();
            for key in list {
                records.insert(
                    key.as_str()
                        .context("network references must be strings")?
                        .into(),
                    json!({}),
                );
            }
            *attachments = Value::Object(records);
        }
        for attachment in attachments
            .as_object_mut()
            .context("network attachments must be a list or record")?
            .values_mut()
        {
            if attachment.is_null() {
                *attachment = json!({});
            }
            let aliases = attachment
                .as_object_mut()
                .context("network attachment must be a record")?
                .entry("aliases")
                .or_insert_with(|| json!([]));
            let aliases = aliases
                .as_array_mut()
                .context("network aliases must be a list")?;
            if !aliases
                .iter()
                .any(|value| value.as_str() == Some(service_name.as_str()))
            {
                aliases.push(json!(service_name));
            }
        }
    }
    let map = rendered
        .as_object_mut()
        .context("Swarm document must be a record")?;
    let networks = map.entry("networks").or_insert_with(|| json!({}));
    if networks.is_null() {
        *networks = json!({});
    }
    let networks = networks
        .as_object_mut()
        .context("networks must be a record")?;
    networks.entry("default").or_insert_with(|| json!({}));
    for kind in ["networks", "volumes", "configs", "secrets"] {
        if let Some(resources) = map.get_mut(kind).and_then(Value::as_object_mut) {
            for resource in resources.values_mut() {
                if resource.is_null() {
                    *resource = json!({});
                }
                if resource.get("external").and_then(Value::as_bool) != Some(true) {
                    set_labels(resource, owner, name)?;
                }
            }
        }
    }
    Ok(rendered)
}
fn resource_name(project: &str, key: &str, spec: &Value) -> String {
    spec.get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if spec.get("external").and_then(Value::as_bool) == Some(true) {
                key.into()
            } else {
                format!("{project}_{key}")
            }
        })
}
fn shared_scope(rendered: &Value, previous: &Value, selected: bool) -> Result<()> {
    if !selected {
        return Ok(());
    }
    ensure!(
        previous.get("active").and_then(Value::as_bool) == Some(true),
        "selected-service deployment requires an active full deployment; deploy the full stack first to create shared resources"
    );
    let old = previous.get("rendered").context("selected-service deployment requires an existing full deployment snapshot; deploy the full stack first so shared resources are explicit")?;
    for kind in ["networks", "volumes", "configs"] {
        ensure!(
            rendered.get(kind) == old.get(kind),
            "selected deployment would change shared {kind}; apply an explicitly reviewed full deployment first (unrelated services will not be updated implicitly)"
        );
    }
    Ok(())
}
fn check_resources(
    project: &Project,
    rendered: &Value,
    previous: &Value,
    names: &[String],
    docker: &Docker,
    owner: &str,
) -> Result<()> {
    let service_names = docker.capture(&args(&["service", "ls", "--format", "{{.Name}}"]), None)?;
    let service_names: BTreeSet<&str> = service_names.lines().collect();
    for name in names {
        let native = format!("{}_{}", project.name()?, name);
        if service_names.contains(native.as_str()) {
            let service = inspect(docker, "service", &native)?;
            ensure!(
                service
                    .pointer("/Spec/Labels")
                    .and_then(|v| v.get(OWNER))
                    .and_then(Value::as_str)
                    == Some(owner),
                "refusing to adopt or modify unrelated service {native}"
            );
        }
    }
    for (kind, command) in [
        ("networks", "network"),
        ("volumes", "volume"),
        ("configs", "config"),
        ("secrets", "secret"),
    ] {
        let Some(resources) = rendered.get(kind).and_then(Value::as_object) else {
            continue;
        };
        let existing = docker.capture(&args(&[command, "ls", "--format", "{{.Name}}"]), None)?;
        let existing: BTreeSet<&str> = existing.lines().collect();
        for (key, spec) in resources {
            let name = resource_name(project.name()?, key, spec);
            let external = spec.get("external").and_then(Value::as_bool) == Some(true);
            if external {
                ensure!(
                    existing.contains(name.as_str()),
                    "external {command} {name} is missing in this Docker context; restore or explicitly provision it"
                );
                continue;
            }
            if existing.contains(name.as_str()) {
                let item = inspect(docker, command, &name)?;
                let labels = item.get("Labels").or_else(|| item.pointer("/Spec/Labels"));
                ensure!(
                    labels.and_then(|l| l.get(OWNER)).and_then(Value::as_str) == Some(owner),
                    "refusing to modify unrelated {command} {name}; its ownership label differs or is absent"
                );
                if let Some(old) = previous["rendered"][kind].get(key) {
                    ensure!(
                        old == spec || resource_name(project.name()?, key, old) != name,
                        "existing shared {command} {name} cannot be reconfigured safely; declare a new resource name and explicitly migrate consumers"
                    );
                }
            }
        }
    }
    Ok(())
}

// The selected-service adapter intentionally supports a finite, validated subset.
// Anything outside it is rejected before build/apply rather than silently dropped.
#[derive(Clone, Default)]
struct ServiceOptions {
    scalar: BTreeMap<String, String>,
    lists: BTreeMap<String, BTreeMap<String, String>>,
    command: Option<Vec<String>>,
    image: String,
}
fn list_insert(options: &mut ServiceOptions, kind: &str, key: String, value: String) {
    options
        .lists
        .entry(kind.into())
        .or_default()
        .insert(key, value);
}
fn strings(value: &Value, what: &str) -> Result<Vec<String>> {
    value
        .as_array()
        .with_context(|| {
            format!("{what} must use argv/list syntax for selected service deployment")
        })?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .context("argv entries must be strings")
        })
        .collect()
}
fn adapter(project: &str, service: &Value, rendered: &Value) -> Result<ServiceOptions> {
    let mut options = ServiceOptions::default();
    let fields = object(service, "service")?;
    for key in fields.keys() {
        ensure!(
            [
                "image",
                "environment",
                "command",
                "entrypoint",
                "user",
                "working_dir",
                "hostname",
                "read_only",
                "init",
                "tty",
                "stop_grace_period",
                "stop_signal",
                "labels",
                "networks",
                "volumes",
                "ports",
                "secrets",
                "configs",
                "healthcheck",
                "deploy",
                "logging"
            ]
            .contains(&key.as_str()),
            "selected-service adapter does not support field {key}; use an explicitly reviewed full deployment or simplify that service"
        );
    }
    options.image = service
        .get("image")
        .and_then(Value::as_str)
        .context("Swarm service needs image")?
        .into();
    for (field, option) in [
        ("user", "user"),
        ("working_dir", "workdir"),
        ("hostname", "hostname"),
        ("stop_grace_period", "stop-grace-period"),
        ("stop_signal", "stop-signal"),
        ("read_only", "read-only"),
        ("init", "init"),
        ("tty", "tty"),
    ] {
        if let Some(value) = fields.get(field) {
            options.scalar.insert(option.into(), text(value)?);
        }
    }
    if let Some(command) = fields.get("command") {
        options.command = Some(strings(command, "command")?);
    }
    if let Some(entrypoint) = fields.get("entrypoint") {
        options.scalar.insert(
            "entrypoint".into(),
            strings(entrypoint, "entrypoint")?
                .iter()
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    for (field, kind) in [("environment", "env"), ("labels", "container-label")] {
        if let Some(value) = fields.get(field) {
            for (key, value) in key_values(value)? {
                list_insert(&mut options, kind, key.clone(), format!("{key}={value}"));
            }
        }
    }
    if let Some(deploy) = fields.get("deploy") {
        let deploy = object(deploy, "deploy")?;
        for key in deploy.keys() {
            ensure!(
                [
                    "mode",
                    "replicas",
                    "labels",
                    "endpoint_mode",
                    "placement",
                    "restart_policy",
                    "update_config",
                    "rollback_config",
                    "resources"
                ]
                .contains(&key.as_str()),
                "selected-service adapter does not support deploy.{key}"
            );
        }
        for (field, option) in [
            ("mode", "mode"),
            ("replicas", "replicas"),
            ("endpoint_mode", "endpoint-mode"),
        ] {
            if let Some(value) = deploy.get(field) {
                options.scalar.insert(option.into(), text(value)?);
            }
        }
        if let Some(labels) = deploy.get("labels") {
            for (key, value) in key_values(labels)? {
                list_insert(&mut options, "label", key.clone(), format!("{key}={value}"));
            }
        }
        for (field, prefix, allowed) in [
            (
                "update_config",
                "update",
                &[
                    "parallelism",
                    "delay",
                    "failure_action",
                    "monitor",
                    "max_failure_ratio",
                    "order",
                ][..],
            ),
            (
                "rollback_config",
                "rollback",
                &[
                    "parallelism",
                    "delay",
                    "failure_action",
                    "monitor",
                    "max_failure_ratio",
                    "order",
                ][..],
            ),
            (
                "restart_policy",
                "restart",
                &["condition", "delay", "max_attempts", "window"][..],
            ),
        ] {
            if let Some(value) = deploy.get(field) {
                for (key, value) in object(value, field)? {
                    ensure!(
                        allowed.contains(&key.as_str()),
                        "unsupported deploy.{field}.{key}"
                    );
                    options
                        .scalar
                        .insert(format!("{prefix}-{}", key.replace('_', "-")), text(value)?);
                }
            }
        }
        if let Some(placement) = deploy.get("placement") {
            for key in object(placement, "placement")?.keys() {
                ensure!(
                    ["constraints", "max_replicas_per_node"].contains(&key.as_str()),
                    "unsupported placement.{key}"
                );
            }
            if let Some(values) = placement.get("constraints") {
                for value in strings(values, "constraints")? {
                    list_insert(&mut options, "constraint", value.clone(), value);
                }
            }
            if let Some(value) = placement.get("max_replicas_per_node") {
                options
                    .scalar
                    .insert("replicas-max-per-node".into(), text(value)?);
            }
        }
        if let Some(resources) = deploy.get("resources") {
            for (kind, values) in object(resources, "resources")? {
                ensure!(
                    ["limits", "reservations"].contains(&kind.as_str()),
                    "unsupported resources.{kind}"
                );
                let prefix = if kind == "limits" { "limit" } else { "reserve" };
                for (key, value) in object(values, "resource values")? {
                    ensure!(
                        ["cpus", "memory"].contains(&key.as_str()),
                        "unsupported resources.{kind}.{key}"
                    );
                    options.scalar.insert(
                        format!("{prefix}-{}", if key == "cpus" { "cpu" } else { "memory" }),
                        text(value)?,
                    );
                }
            }
        }
    }
    options
        .scalar
        .entry("mode".into())
        .or_insert_with(|| "replicated".into());
    if options.scalar["mode"] == "replicated" {
        options
            .scalar
            .entry("replicas".into())
            .or_insert_with(|| "1".into());
    }
    options
        .scalar
        .entry("endpoint-mode".into())
        .or_insert_with(|| "vip".into());
    list_insert(
        &mut options,
        "label",
        "com.docker.stack.namespace".into(),
        format!("com.docker.stack.namespace={project}"),
    );
    let stack_image = options.image.clone();
    list_insert(
        &mut options,
        "label",
        "com.docker.stack.image".into(),
        format!("com.docker.stack.image={stack_image}"),
    );
    let default_network = json!(["default"]);
    let networks = fields.get("networks").unwrap_or(&default_network);
    let entries: Vec<(String, Value)> = if let Some(map) = networks.as_object() {
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    } else {
        strings(networks, "networks")?
            .into_iter()
            .map(|s| (s, Value::Null))
            .collect()
    };
    for (key, spec) in entries {
        let definitions = rendered
            .get("networks")
            .and_then(|v| v.get(&key))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let name = resource_name(project, &key, &definitions);
        let mut value = format!("name={name}");
        if let Some(map) = spec.as_object() {
            for field in map.keys() {
                ensure!(
                    field == "aliases",
                    "unsupported selected network attachment field {field}"
                );
            }
            if let Some(aliases) = spec.get("aliases") {
                for alias in strings(aliases, "network aliases")?
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                {
                    ensure!(
                        !alias.contains(','),
                        "network aliases containing commas are unsupported"
                    );
                    value.push_str(&format!(",alias={alias}"));
                }
            }
        }
        list_insert(&mut options, "network", name, value);
    }
    if let Some(volumes) = fields.get("volumes") {
        for volume in volumes.as_array().context("volumes must be a list")? {
            let (kind, source, target, read_only) = if let Some(s) = volume.as_str() {
                let parts: Vec<_> = s.split(':').collect();
                ensure!(
                    parts.len() >= 2 && parts.len() <= 3,
                    "selected deployment requires explicit source:target mounts"
                );
                ensure!(
                    parts.len() != 3 || ["ro", "rw"].contains(&parts[2]),
                    "unsupported mount mode"
                );
                (
                    if parts[0].starts_with('/') || parts[0].starts_with('.') {
                        "bind"
                    } else {
                        "volume"
                    },
                    parts[0].to_owned(),
                    parts[1].to_owned(),
                    parts.get(2) == Some(&"ro"),
                )
            } else {
                for key in object(volume, "mount")?.keys() {
                    ensure!(
                        ["type", "source", "target", "read_only"].contains(&key.as_str()),
                        "unsupported selected mount field {key}"
                    );
                }
                (
                    volume
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("volume"),
                    volume
                        .get("source")
                        .and_then(Value::as_str)
                        .context("mount requires source")?
                        .into(),
                    volume
                        .get("target")
                        .and_then(Value::as_str)
                        .context("mount requires target")?
                        .into(),
                    volume
                        .get("read_only")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
            };
            ensure!(
                ["bind", "volume"].contains(&kind),
                "unsupported mount type {kind}"
            );
            ensure!(
                kind != "bind" || source.starts_with('/'),
                "Swarm bind source must be an absolute host path, not {source}"
            );
            let source = if kind == "volume" {
                resource_name(project, &source, &rendered["volumes"][&source])
            } else {
                source
            };
            ensure!(
                !source.contains(',') && !target.contains(','),
                "mount paths containing commas need a full deployment"
            );
            list_insert(
                &mut options,
                "mount",
                target.clone(),
                format!("type={kind},source={source},target={target},readonly={read_only}"),
            );
        }
    }
    if let Some(ports) = fields.get("ports") {
        for port in ports.as_array().context("ports must be a list")? {
            let (target, published, protocol, mode) = if let Some(s) = port.as_str() {
                let (s, protocol) = s.split_once('/').unwrap_or((s, "tcp"));
                let parts: Vec<_> = s.split(':').collect();
                ensure!(
                    parts.len() == 2,
                    "selected Swarm ports require published:target without a host IP or range"
                );
                (
                    parts[1].into(),
                    parts[0].into(),
                    protocol.into(),
                    "ingress".into(),
                )
            } else {
                for key in object(port, "port")?.keys() {
                    ensure!(
                        ["target", "published", "protocol", "mode"].contains(&key.as_str()),
                        "unsupported selected port field {key}"
                    );
                }
                (
                    text(&port["target"])?,
                    text(&port["published"])?,
                    port.get("protocol")
                        .and_then(Value::as_str)
                        .unwrap_or("tcp")
                        .into(),
                    port.get("mode")
                        .and_then(Value::as_str)
                        .unwrap_or("ingress")
                        .into(),
                )
            };
            let (target, published, protocol, mode): (String, String, String, String) =
                (target, published, protocol, mode);
            ensure!(
                target.parse::<u16>().is_ok() && published.parse::<u16>().is_ok(),
                "ports must be single numeric ports"
            );
            list_insert(
                &mut options,
                "publish",
                format!("{target}/{protocol}"),
                format!("target={target},published={published},protocol={protocol},mode={mode}"),
            );
        }
    }
    for kind in ["secrets", "configs"] {
        if let Some(values) = fields.get(kind) {
            for value in values
                .as_array()
                .context("secret/config references must be a list")?
            {
                let logical = value
                    .as_str()
                    .or_else(|| value.get("source").and_then(Value::as_str))
                    .context("secret/config reference requires source")?;
                if let Some(map) = value.as_object() {
                    for field in map.keys() {
                        ensure!(
                            ["source", "target", "uid", "gid", "mode"].contains(&field.as_str()),
                            "unsupported {kind} attachment {field}"
                        );
                    }
                }
                let source = resource_name(project, logical, &rendered[kind][logical]);
                let mut entry = format!(
                    "source={source},target={}",
                    value
                        .get("target")
                        .and_then(Value::as_str)
                        .unwrap_or(logical)
                );
                for field in ["uid", "gid", "mode"] {
                    let encoded = if let Some(v) = value.get(field) {
                        if field == "mode" && v.is_number() {
                            format!("{:04o}", v.as_u64().context("mode must be nonnegative")?)
                        } else {
                            text(v)?
                        }
                    } else {
                        if field == "mode" {
                            "0444".into()
                        } else {
                            "0".into()
                        }
                    };
                    entry.push_str(&format!(",{field}={encoded}"));
                }
                list_insert(
                    &mut options,
                    if kind == "secrets" {
                        "secret"
                    } else {
                        "config"
                    },
                    source,
                    entry,
                );
            }
        }
    }
    if let Some(health) = fields.get("healthcheck") {
        for key in object(health, "healthcheck")?.keys() {
            ensure!(
                [
                    "disable",
                    "test",
                    "interval",
                    "timeout",
                    "retries",
                    "start_period",
                    "start_interval"
                ]
                .contains(&key.as_str()),
                "unsupported healthcheck.{key}"
            );
        }
        if health.get("disable").and_then(Value::as_bool) == Some(true) {
            options
                .scalar
                .insert("no-healthcheck".into(), "true".into());
        }
        if let Some(test) = health.get("test") {
            let test = strings(test, "healthcheck test")?;
            ensure!(
                test.first().map(String::as_str) == Some("CMD-SHELL") && test.len() == 2,
                "selected healthcheck requires [CMD-SHELL, command] syntax"
            );
            options.scalar.insert("health-cmd".into(), test[1].clone());
        }
        for field in [
            "interval",
            "timeout",
            "retries",
            "start_period",
            "start_interval",
        ] {
            if let Some(value) = health.get(field) {
                options
                    .scalar
                    .insert(format!("health-{}", field.replace('_', "-")), text(value)?);
            }
        }
    }
    if let Some(logging) = fields.get("logging") {
        for field in object(logging, "logging")?.keys() {
            ensure!(
                ["driver", "options"].contains(&field.as_str()),
                "unsupported logging.{field}"
            );
        }
        if let Some(driver) = logging.get("driver") {
            options.scalar.insert("log-driver".into(), text(driver)?);
        }
        if let Some(values) = logging.get("options") {
            for (key, value) in key_values(values)? {
                list_insert(
                    &mut options,
                    "log-opt",
                    key.clone(),
                    format!("{key}={value}"),
                );
            }
        }
    }
    Ok(options)
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn live_options(
    service: &Value,
    networks: &BTreeMap<String, String>,
    recorded: &ServiceOptions,
    intent: Option<&ServiceOptions>,
) -> Result<ServiceOptions> {
    let spec = service
        .get("Spec")
        .context("live service is missing Spec")?;
    let container = spec
        .pointer("/TaskTemplate/ContainerSpec")
        .context("live service has no container specification")?;
    let mut old = recorded.clone();
    old.lists.clear();
    if let Some(intent) = intent {
        for (key, value) in &intent.scalar {
            old.scalar.insert(key.clone(), value.clone());
        }
    }
    old.image = container
        .get("Image")
        .and_then(Value::as_str)
        .context("live service has no image")?
        .into();
    old.scalar.insert(
        "mode".into(),
        if spec.pointer("/Mode/Global").is_some() {
            "global"
        } else {
            "replicated"
        }
        .into(),
    );
    old.command = container
        .get("Args")
        .and_then(Value::as_array)
        .filter(|v| {
            !v.is_empty()
                || recorded.command.is_some()
                || intent.is_some_and(|i| i.command.is_some())
        })
        .map(|v| strings(&json!(v), "live args"))
        .transpose()?;
    for (field, option) in [
        ("User", "user"),
        ("Dir", "workdir"),
        ("Hostname", "hostname"),
    ] {
        if let Some(value) = container
            .get(field)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            old.scalar.insert(option.into(), value.into());
        }
    }
    if let Some(command) = container
        .get("Command")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
    {
        old.scalar.insert(
            "entrypoint".into(),
            strings(&json!(command), "live entrypoint")?
                .iter()
                .map(|v| shell_quote(v))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    for (field, option) in [("ReadOnly", "read-only"), ("Init", "init"), ("TTY", "tty")] {
        if container.get(field).and_then(Value::as_bool) == Some(true) {
            old.scalar.insert(option.into(), "true".into());
        }
    }
    for field in ["CapabilityAdd", "CapabilityDrop", "Groups", "Hosts"] {
        ensure!(
            container
                .get(field)
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty),
            "live selected service contains unsupported {field}; inspect the recorded intent and explicitly reconcile with a reviewed full deployment"
        );
    }
    if let Some(env) = container.get("Env") {
        for (key, value) in key_values(env)? {
            list_insert(&mut old, "env", key.clone(), format!("{key}={value}"));
        }
    }
    for (value, kind) in [
        (container.get("Labels"), "container-label"),
        (spec.get("Labels"), "label"),
    ] {
        if let Some(value) = value {
            for (key, value) in key_values(value)? {
                list_insert(&mut old, kind, key.clone(), format!("{key}={value}"));
            }
        }
    }
    if let Some(attachments) = spec
        .pointer("/TaskTemplate/Networks")
        .and_then(Value::as_array)
    {
        for attachment in attachments {
            let target = attachment
                .get("Target")
                .and_then(Value::as_str)
                .context("live network has no target")?;
            let name = networks
                .get(target)
                .context("live network target has no ownership-checked name resolution")?;
            let mut value = format!("name={name}");
            if let Some(aliases) = attachment.get("Aliases") {
                for alias in strings(aliases, "live aliases")?
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                {
                    ensure!(
                        !alias.contains(','),
                        "live network alias cannot be safely represented"
                    );
                    value.push_str(&format!(",alias={alias}"));
                }
            }
            list_insert(&mut old, "network", name.clone(), value);
        }
    }
    if let Some(mounts) = container.get("Mounts").and_then(Value::as_array) {
        for mount in mounts {
            let kind = mount
                .get("Type")
                .and_then(Value::as_str)
                .context("live mount has no type")?;
            ensure!(
                ["bind", "volume"].contains(&kind),
                "live mount type {kind} cannot be reconciled through selected apply; explicitly reconcile the full deployment"
            );
            let source = mount
                .get("Source")
                .and_then(Value::as_str)
                .context("live mount has no source")?;
            let target = mount
                .get("Target")
                .and_then(Value::as_str)
                .context("live mount has no target")?;
            ensure!(
                !source.contains(',') && !target.contains(','),
                "live mount paths cannot be represented safely"
            );
            let readonly = mount
                .get("ReadOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            list_insert(
                &mut old,
                "mount",
                target.into(),
                format!("type={kind},source={source},target={target},readonly={readonly}"),
            );
        }
    }
    if let Some(ports) = spec
        .pointer("/EndpointSpec/Ports")
        .and_then(Value::as_array)
    {
        for port in ports {
            let target = port
                .get("TargetPort")
                .and_then(Value::as_u64)
                .context("live published port has no target")?;
            let published = port
                .get("PublishedPort")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let protocol = port
                .get("Protocol")
                .and_then(Value::as_str)
                .unwrap_or("tcp");
            let mode = port
                .get("PublishMode")
                .and_then(Value::as_str)
                .unwrap_or("ingress");
            list_insert(
                &mut old,
                "publish",
                format!("{target}/{protocol}"),
                format!("target={target},published={published},protocol={protocol},mode={mode}"),
            );
        }
    }
    for (field, kind, name_field) in [
        ("Secrets", "secret", "SecretName"),
        ("Configs", "config", "ConfigName"),
    ] {
        if let Some(references) = container.get(field).and_then(Value::as_array) {
            for reference in references {
                let name = reference
                    .get(name_field)
                    .and_then(Value::as_str)
                    .context("live immutable reference has no native name")?;
                let file = reference
                    .get("File")
                    .context("live immutable reference is not a supported file attachment")?;
                let target = file
                    .get("Name")
                    .and_then(Value::as_str)
                    .context("live immutable reference has no file target")?;
                let uid = file.get("UID").and_then(Value::as_str).unwrap_or("0");
                let gid = file.get("GID").and_then(Value::as_str).unwrap_or("0");
                let mode = file.get("Mode").and_then(Value::as_u64).unwrap_or(0o444);
                list_insert(
                    &mut old,
                    kind,
                    name.into(),
                    format!("source={name},target={target},uid={uid},gid={gid},mode={mode:04o}"),
                );
            }
        }
    }
    if let Some(constraints) = spec.pointer("/TaskTemplate/Placement/Constraints") {
        for constraint in strings(constraints, "live constraints")? {
            list_insert(&mut old, "constraint", constraint.clone(), constraint);
        }
    }
    if let Some(options) = spec.pointer("/TaskTemplate/LogDriver/Options") {
        for (key, value) in key_values(options)? {
            list_insert(&mut old, "log-opt", key.clone(), format!("{key}={value}"));
        }
    }
    Ok(old)
}
fn current_options(
    project: &Project,
    docker: &Docker,
    native: &str,
    owner: &str,
    recorded: &ServiceOptions,
) -> Result<(String, ServiceOptions)> {
    let service = inspect(docker, "service", native)?;
    ensure!(
        service
            .pointer("/Spec/Labels")
            .and_then(|v| v.get(OWNER))
            .and_then(Value::as_str)
            == Some(owner)
            && service
                .pointer("/Spec/Labels")
                .and_then(|v| v.get(PROJECT))
                .and_then(Value::as_str)
                == Some(project.name()?),
        "live service {native} is not owned; refusing to adopt its state"
    );
    let id = service
        .get("ID")
        .and_then(Value::as_str)
        .context("live service has no immutable ID")?
        .to_owned();
    let mut networks = BTreeMap::new();
    if let Some(attachments) = service
        .pointer("/Spec/TaskTemplate/Networks")
        .and_then(Value::as_array)
    {
        for attachment in attachments {
            let target = attachment
                .get("Target")
                .and_then(Value::as_str)
                .context("live network has no target")?;
            let network = inspect(docker, "network", target)?;
            networks.insert(
                target.into(),
                network
                    .get("Name")
                    .and_then(Value::as_str)
                    .context("live network has no name")?
                    .into(),
            );
        }
    }
    let intents = state::read(&project.root, "deployment-intents")?;
    let intent = if let Some(intent) = intents["services"].get(native) {
        ensure!(
            intents["owner"].as_str() == Some(owner)
                && intents["context"].as_str() == Some(docker.context()?.as_str()),
            "interrupted deployment intent belongs to another context/owner"
        );
        Some(adapter(
            project.name()?,
            &intent["definition"],
            &intent["rendered"],
        )?)
    } else {
        None
    };
    let options = live_options(&service, &networks, recorded, intent.as_ref())?;
    Ok((id, options))
}

fn selected_operation(
    name: &str,
    desired: &ServiceOptions,
    previous: Option<&ServiceOptions>,
) -> Result<Vec<String>> {
    let update = previous.is_some();
    let mut argv = args(&[
        "service",
        if update { "update" } else { "create" },
        "--detach",
        "--with-registry-auth",
    ]);
    if !update {
        flag(&mut argv, "--name", name);
    }
    if let Some(old) = previous {
        ensure!(
            old.scalar.get("mode") == desired.scalar.get("mode"),
            "selected update cannot change service mode; recreate that service explicitly"
        );
        for key in old.scalar.keys() {
            ensure!(
                desired.scalar.contains_key(key),
                "selected update cannot safely reset removed option {key}; declare an explicit value or use a full deployment"
            );
        }
        ensure!(
            old.command.is_none() || desired.command.is_some(),
            "selected update cannot reset command to unknown image defaults; declare explicit argv or use full deployment"
        );
    }
    for (key, value) in &desired.scalar {
        if update && key == "mode" {
            continue;
        }
        if key == "no-healthcheck" {
            if value == "true" {
                argv.push("--no-healthcheck".into());
            }
            continue;
        }
        if ["read-only", "init", "tty"].contains(&key.as_str()) {
            argv.push(format!("--{key}={value}"));
        } else {
            flag(&mut argv, &format!("--{key}"), value.clone());
        }
    }
    for kind in [
        "env",
        "container-label",
        "label",
        "network",
        "mount",
        "publish",
        "secret",
        "config",
        "constraint",
        "log-opt",
    ] {
        let new = desired.lists.get(kind);
        let old = previous.and_then(|p| p.lists.get(kind));
        if update && kind == "log-opt" {
            ensure!(
                old.is_none_or(|o| o.keys().all(|k| new.is_some_and(|n| n.contains_key(k)))),
                "selected update cannot remove logging options safely; use a full deployment"
            );
        }
        if let Some(old) = old {
            for (key, value) in old {
                if new.and_then(|n| n.get(key)) != Some(value) && kind != "log-opt" {
                    flag(&mut argv, &format!("--{kind}-rm"), key.clone());
                }
            }
        }
        if let Some(new) = new {
            for (key, value) in new {
                if !update || old.and_then(|o| o.get(key)) != Some(value) {
                    flag(
                        &mut argv,
                        &format!(
                            "--{kind}{}",
                            if update && kind != "log-opt" {
                                "-add"
                            } else {
                                ""
                            }
                        ),
                        value.clone(),
                    );
                }
            }
        }
    }
    if update {
        flag(&mut argv, "--image", desired.image.clone());
        if let Some(command) = &desired.command {
            flag(
                &mut argv,
                "--args",
                command
                    .iter()
                    .map(|s| shell_quote(s))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        argv.push(name.into());
    } else {
        argv.push(desired.image.clone());
        if let Some(command) = &desired.command {
            argv.extend(command.clone());
        }
    }
    Ok(argv)
}
fn owned_services(project: &Project, docker: &Docker, owner: &str) -> Result<Vec<String>> {
    let names = docker.capture(
        &args(&[
            "service",
            "ls",
            "--filter",
            &format!("label={OWNER}={owner}"),
            "--filter",
            &format!("label={PROJECT}={}", project.name()?),
            "--format",
            "{{.Name}}",
        ]),
        None,
    )?;
    Ok(names
        .lines()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}
fn rollout_failure(service: &Value, tasks: &[Value], expected_image: &str) -> Option<String> {
    if let Some(status) = service.get("UpdateStatus")
        && let Some(state) = status.get("State").and_then(Value::as_str)
        && (state == "paused" || state.starts_with("rollback"))
    {
        return Some(format!(
            "rollout {state}: {}",
            status
                .get("Message")
                .and_then(Value::as_str)
                .unwrap_or("Swarm rejected the desired revision")
        ));
    }
    let version = service
        .pointer("/Spec/TaskTemplate/ForceUpdate")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    for task in tasks {
        let started = service
            .pointer("/UpdateStatus/StartedAt")
            .or_else(|| service.get("UpdatedAt"))
            .and_then(Value::as_str);
        if let (Some(started), Some(updated)) =
            (started, task.get("UpdatedAt").and_then(Value::as_str))
            && updated < started
        {
            continue;
        }
        if task
            .pointer("/Spec/ContainerSpec/Image")
            .and_then(Value::as_str)
            != Some(expected_image)
        {
            continue;
        }
        if task
            .pointer("/Spec/ForceUpdate")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            != version
        {
            continue;
        }
        let state = task
            .pointer("/Status/State")
            .and_then(Value::as_str)
            .unwrap_or("");
        if ["failed", "rejected", "orphaned"].contains(&state) {
            return Some(format!(
                "task {} {state}: {}",
                task.get("ID").and_then(Value::as_str).unwrap_or("unknown"),
                task.pointer("/Status/Err")
                    .and_then(Value::as_str)
                    .unwrap_or("inspect service tasks for details")
            ));
        }
    }
    None
}
fn tasks(docker: &Docker, name: &str) -> Result<Option<Vec<Value>>> {
    let ids = docker.capture(
        &args(&["service", "ps", "--no-trunc", "--quiet", name]),
        None,
    )?;
    let ids: Vec<_> = ids.lines().filter(|s| !s.is_empty()).collect();
    if ids.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let mut argv = args(&["inspect", "--type", "task"]);
    argv.extend(ids.iter().map(|id| (*id).to_owned()));
    match docker.capture(&argv, None) {
        Ok(output) => Ok(Some(serde_json::from_str::<Vec<Value>>(&output)?)),
        Err(error) if disappeared_tasks(&error, &ids) => {
            // Swarm can remove tasks after service ps. Discard the entire
            // observation: partial survivors can expose an obsolete slot as
            // ready. Convergence re-observes under its existing deadline;
            // status reports this single incomplete observation as not ready.
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn disappeared_tasks(error: &anyhow::Error, ids: &[&str]) -> bool {
    let Some(error) = error.downcast_ref::<runtime::DockerError>() else {
        return false;
    };
    error.status == 1
        && !error.stderr.is_empty()
        && error.stderr.lines().all(|line| {
            line.strip_prefix("Error response from daemon: task ")
                .and_then(|line| line.strip_suffix(" not found"))
                .is_some_and(|id| ids.contains(&id))
        })
}
fn wait_convergence(
    project: &Project,
    names: &[String],
    rendered: &Value,
    docker: &Docker,
    timeout: u64,
    output: &Output,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        let mut all = true;
        let mut summaries = Vec::new();
        for name in names {
            let native = format!("{}_{}", project.name()?, name);
            let service = inspect(docker, "service", &native)?;
            let tasks = tasks(docker, &native)?;
            let pin = rendered["services"][name]["image"]
                .as_str()
                .context("missing pinned image")?;
            let desired = service
                .pointer("/ServiceStatus/DesiredTasks")
                .and_then(Value::as_u64)
                .or_else(|| {
                    service
                        .pointer("/Spec/Mode/Replicated/Replicas")
                        .and_then(Value::as_u64)
                });
            let desired = match desired {
                Some(n) => n,
                None => {
                    let status = docker.capture(
                        &args(&[
                            "service",
                            "ls",
                            "--filter",
                            &format!("name={native}"),
                            "--format",
                            "{{.Replicas}}",
                        ]),
                        None,
                    )?;
                    status
                        .trim()
                        .split('/')
                        .nth(1)
                        .and_then(|s| s.split_whitespace().next())
                        .and_then(|s| s.parse().ok())
                        .context("cannot determine desired global-service tasks")?
                }
            };
            let oneshot = runtime::one_shot(project, name)?;
            let (ready, successful, failure) = swarm_container_status(&service, tasks.as_deref(), desired, oneshot);
            if let Some(reason) = failure {
                bail!("{native}: {reason}; completed prerequisites/migrations are not rolled back, previous snapshots and secret revisions are retained");
            }
            all &= ready && service.pointer("/Spec/TaskTemplate/ContainerSpec/Image")
                .and_then(Value::as_str) == Some(pin);
            summaries.push(json!({"service":name,"running":if oneshot {0} else {successful},
                "completed":if oneshot {successful} else {0},"oneShot":oneshot,"desired":desired,"image":pin,
                "tasksObserved":tasks.is_some()}));
        }
        if all {
            return Ok(json!(summaries));
        }
        ensure!(
            Instant::now() < deadline,
            "Swarm rollout timed out after {timeout}s; last observed state: {}; inspect docker service ps --no-trunc; migrations are not undone",
            json!(summaries)
        );
        output.event("convergence", &json!(summaries).to_string())?;
        thread::sleep(Duration::from_millis(500));
    }
}

// Swarm prerequisites are explicit commands; Compose dependencies/profiles do not define their scope.
fn deployment_actions<'a>(project: &'a Project, names: &[String], stage: &str) -> Result<Vec<&'a Value>> {
    let Some(actions) = project.metadata.get("actions") else {
        return Ok(Vec::new());
    };
    let mut applicable = Vec::new();
    for action in actions
        .as_array()
        .context("actions must be an ordered array")?
    {
        let workflows = action.get("workflows").and_then(Value::as_array);
        if !workflows.is_some_and(|list| list.iter().any(|v| v.as_str() == Some("deploy"))) {
            continue;
        }
        let services = action.get("services").and_then(Value::as_array);
        if services.is_some_and(|list| {
            !list.is_empty()
                && !list.iter().any(|s| {
                    s.as_str()
                        .is_some_and(|s| names.iter().any(|name| name == s))
                })
        }) {
            continue;
        }
        ensure!(
            action.get("kind").and_then(Value::as_str) == Some("command"),
            "deploy prerequisite {} must use an explicit argv command; Compose up/run/exec/stop/prerequisite cannot implement Swarm migration semantics",
            action
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unnamed>")
        );
        ensure!(
            action.get("name").and_then(Value::as_str).is_some(),
            "each deploy prerequisite requires a name"
        );
        ensure!(action["argv"].as_array().is_some_and(|args| !args.is_empty() && args.iter().all(Value::is_string)),
            "deploy prerequisite argv must be a nonempty string list");
        let action_stage = action
            .get("stage")
            .and_then(Value::as_str)
            .unwrap_or("before");
        ensure!(
            ["before", "after"].contains(&action_stage),
            "invalid deploy prerequisite stage {action_stage}"
        );
        if action_stage == stage {
            applicable.push(action);
        }
    }
    Ok(applicable)
}
fn run_actions(
    project: &Project,
    names: &[String],
    actions: &[&Value],
    docker: &Docker,
    output: &Output,
    operation: &str,
) -> Result<()> {
    journal(
        project,
        operation,
        json!({"phase":"prerequisites","state":"started","actions":actions}),
    )?;
    runtime::execute_action_sequence(project, "deploy", names, actions, docker, output)?;
    journal(
        project,
        operation,
        json!({"phase":"prerequisites","state":"completed","actions":actions}),
    )
}
fn read_owner(project: &Project, context: &str, required: bool) -> Result<String> {
    let identity = state::read(&project.root, "identity")?;
    let Some(id) = identity.get("id").and_then(Value::as_str) else {
        ensure!(
            !required,
            "no Dockstride ownership identity; refusing to operate by stack name alone"
        );
        return Ok("<new-owner>".into());
    };
    let root = std::fs::canonicalize(&project.root)?
        .to_string_lossy()
        .into_owned();
    ensure!(
        identity["root"].as_str() == Some(root.as_str()),
        "environment identity belongs to another checkout"
    );
    ensure!(
        identity["project"].as_str() == Some(project.name()?)
            && identity["backend"].as_str() == Some(project.backend()?),
        "project/backend differs from recorded ownership identity"
    );
    if let Some(recorded) = identity
        .get("context")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        ensure!(
            recorded == context,
            "Docker context differs from environment ownership identity"
        );
    }
    Ok(id.into())
}
fn merge_selected_snapshot(
    previous: &Value,
    rendered: &Value,
    names: &[String],
    project: &str,
) -> Result<Value> {
    let mut committed = previous.clone();
    if !committed["secrets"].is_object() {
        committed["secrets"] = json!({});
    }
    if let Some(old_secrets) = previous.get("secrets").and_then(Value::as_object) {
        for (logical, old) in old_secrets {
            if rendered["secrets"].get(logical) == Some(old) {
                continue;
            }
            let retained = resource_name(project, logical, old);
            let alias = format!("dks-retained-{retained}");
            if let Some(existing) = committed["secrets"].get(&alias) {
                ensure!(
                    existing == old,
                    "retained secret alias conflicts with project declaration"
                );
            }
            committed["secrets"][&alias] = old.clone();
            for (name, service) in committed["services"]
                .as_object_mut()
                .context("snapshot services must be a record")?
            {
                if names.contains(name) {
                    continue;
                }
                if let Some(references) = service.get_mut("secrets").and_then(Value::as_array_mut) {
                    for reference in references {
                        if reference.as_str() == Some(logical.as_str()) {
                            *reference = json!({"source":alias,"target":logical});
                        } else if reference.get("source").and_then(Value::as_str)
                            == Some(logical.as_str())
                        {
                            reference["source"] = json!(alias);
                            if reference.get("target").is_none() {
                                reference["target"] = json!(logical);
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(secrets) = rendered.get("secrets").and_then(Value::as_object) {
        for (key, value) in secrets {
            committed["secrets"][key] = value.clone();
        }
    }
    for name in names {
        committed["services"][name] = rendered["services"][name].clone();
    }
    Ok(committed)
}

fn deployment_state(root: &std::path::Path) -> Result<Value> {
    let committed = state::read(root, "deployment")?;
    let applied = state::read(root, "deployment-applied")?;
    if applied.get("active").and_then(Value::as_bool) == Some(true)
        && applied.get("revision") != committed.get("revision")
    {
        Ok(applied)
    } else {
        Ok(committed)
    }
}

fn recheck_environment(project: &Project) -> Result<()> {
    runtime::recover_publication(&project.root)?;
    let current = crate::sources::snapshot(&project.root, None)?.values;
    ensure!(
        current.get("project").and_then(Value::as_str) == Some(project.name()?)
            && current
                .get("backend")
                .and_then(Value::as_str)
                .unwrap_or("compose")
                == project.backend()?,
        "environment project/backend changed before lifecycle execution; re-evaluate the project before applying resources"
    );
    Ok(())
}

pub fn deploy(
    project: &Project,
    selected: &[String],
    plan_only: bool,
    timeout: u64,
    output: &Output,
) -> Result<Value> {
    ensure!(
        project.backend()? == "swarm",
        "deploy requires backend=swarm"
    );
    validate_model(project)?;
    crate::diagnostics::validate(project)?;
    let names = selection(project, selected)?;
    let before = deployment_actions(project, &names, "before")?;
    let after = deployment_actions(project, &names, "after")?;
    let _lock = if plan_only {
        None
    } else {
        let lock = state::lock(&project.root, "lifecycle")?;
        recheck_environment(project)?;
        Some(lock)
    };
    let docker = Docker::new(&project.root, output.clone())
        .with_deadline(Instant::now() + Duration::from_secs(timeout.max(1)));
    let preflight = (|| -> Result<(String, Value)> {
        let context = docker.context()?;
        let info = manager(&docker)?;
        Ok((context, info))
    })();
    let (context, info) = preflight.map_err(|error| {
        if plan_only { error }
        else { runtime::startup_diagnostics(error, project, &names, timeout, &docker, output) }
    })?;
    let previous = deployment_state(&project.root)?;
    if previous.get("active").and_then(Value::as_bool) == Some(true) {
        ensure!(
            previous.get("context").and_then(Value::as_str) == Some(context.as_str()),
            "deployment snapshot belongs to a different Docker context; switch back or explicitly create a separate environment"
        );
    }
    let owner = if plan_only {
        read_owner(project, &context, false)?
    } else {
        runtime::validate_ownership(project, &docker, true)
            .map_err(|error| runtime::startup_diagnostics(error, project, &names, timeout, &docker, output))?
    };
    let mut rendered = render_owned(project, &owner)?;
    shared_scope(&rendered, &previous, !selected.is_empty())?;
    for name in &names {
        let image = image_name(project, name)?;
        if project.services()?[name].get("build").is_some() {
            ensure!(
                registry(repository(&image)).is_some(),
                "service {name}: built images require an explicit registry repository"
            );
        }
        if info
            .pointer("/Swarm/Nodes")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            > 1
            && let Some(host) = registry(repository(&image))
        {
            let loopback = host == "localhost"
                || host.starts_with("localhost:")
                || host == "127.0.0.1"
                || host.starts_with("127.0.0.1:")
                || host.starts_with("[::1]")
                || host.starts_with("0.0.0.0");
            ensure!(
                !loopback,
                "service {name}: a multi-node Swarm cannot distribute images through a loopback-only registry"
            );
        }
        if !selected.is_empty() {
            adapter(project.name()?, &rendered["services"][name], &rendered)?;
            if let Some(old) = previous.pointer(&format!("/rendered/services/{name}")) {
                let old = adapter(project.name()?, old, &previous["rendered"])?;
                let new = adapter(project.name()?, &rendered["services"][name], &rendered)?;
                selected_operation(&format!("{}_{}", project.name()?, name), &new, Some(&old))?;
            }
        }
    }
    let mut image_plan = Map::new();
    for name in &names {
        let image = image_name(project, name)?;
        image_plan.insert(name.clone(),json!({"source":image,"build":project.services()?[name].get("build"),"revision":if project.services()?[name].get("build").is_some(){"new immutable revision or reusable identical image"}else{"resolve registry digest"},"previous":previous["images"][name],"registry":registry(repository(&image))}));
    }
    let mut operations = Vec::new();
    for name in &names {
        let image = image_name(project, name)?;
        if project.services()?[name].get("build").is_some() {
            let tag = format!("{}:dks-<new-revision>", repository(&image));
            operations.push(json!({"phase":"build","argv":build_args(project,name,&tag)?}));
            operations.push(json!({"phase":"publish","argv":["push",tag],"condition":"image ID differs from the recorded immutable revision"}));
        } else {
            operations.push(json!({"phase":"pull","argv":["pull",image]}));
        }
    }
    for action in &before {
        operations.push(
            json!({"phase":"prerequisite","name":action["name"],"projectArgv":action["argv"]}),
        );
    }
    if selected.is_empty() {
        operations.push(json!({"phase":"apply","argv":["stack","deploy","--detach=true","--with-registry-auth","--compose-file","<pinned-snapshot>",project.name()?]}));
    } else {
        for name in &names {
            let mut desired = rendered["services"][name].clone();
            desired["image"] = json!("<registry-digest>");
            let new = adapter(project.name()?, &desired, &rendered)?;
            let native = format!("{}_{}", project.name()?, name);
            let existing = owned_services(project, &docker, &owner)?;
            let (native_id, old) = if existing.contains(&native) {
                let recorded = adapter(
                    project.name()?,
                    previous["rendered"]["services"]
                        .get(name)
                        .context("owned selected service has no recorded specification")?,
                    &previous["rendered"],
                )?;
                let (id, current) = current_options(project, &docker, &native, &owner, &recorded)?;
                (id, Some(current))
            } else {
                (native, None)
            };
            operations.push(
                json!({"phase":"apply","argv":selected_operation(&native_id,&new,old.as_ref())?}),
            );
        }
    }
    operations.push(json!({"phase":"convergence","argv":["service","inspect","<selected-service>"],"timeoutSeconds":timeout}));
    for action in &after {
        operations
            .push(json!({"phase":"after","name":action["name"],"projectArgv":action["argv"]}));
    }
    let plan = json!({"backend":"swarm","context":context,"project":project.name()?,"selectedServices":names,"scope":if selected.is_empty(){"full stack, no pruning"}else{"selected services only; no implicit dependencies"},"images":image_plan,"prerequisites":{"before":before,"after":after},"secrets":rendered.get("secrets"),"sharedResources":{"networks":rendered.get("networks"),"volumes":rendered.get("volumes"),"configs":rendered.get("configs")},"dockerOperations":operations,"planOnly":plan_only});
    if plan_only {
        return Ok(plan);
    }
    // Keep the baseline check next to application even while holding the lifecycle lock.
    ensure!(
        deployment_state(&project.root)? == previous,
        "deployment changed while planning; rerun against the new snapshot"
    );
    check_resources(project, &rendered, &previous, &names, &docker, &owner).map_err(|error| {
        if error.is::<runtime::DockerError>() || error.is::<runtime::DeadlineExceeded>() {
            runtime::startup_diagnostics(error, project, &names, timeout, &docker, output)
        } else { error }
    })?;
    let operation = format!("deploy-{}", state::random_id()?);
    journal(
        project,
        &operation,
        json!({"state":"started","plan":plan,"previous":previous}),
    )?;
    let result = (|| -> Result<Value> {
        let images = resolve_images(project, &names, &previous, &docker, output, &operation)?;
        for name in &names {
            rendered["services"][name]["image"] = images[name]["digest"].clone();
        }
        let snapshot_rendered = if selected.is_empty() {
            rendered.clone()
        } else {
            merge_selected_snapshot(&previous["rendered"], &rendered, &names, project.name()?)?
        };
        let snapshot_path = project
            .root
            .join(".dockstride")
            .join(format!("{operation}.yaml"));
        state::atomic_write(
            &snapshot_path,
            serde_yaml::to_string(&snapshot_rendered)?.as_bytes(),
            0o600,
        )?;
        state::save(
            &project.root,
            &format!("snapshot-{operation}"),
            &json!({"context":context,"owner":owner,"selectedServices":names,"rendered":snapshot_rendered,"images":images,"previous":previous}),
        )?;
        output.event("prerequisites","running only explicitly declared deploy prerequisites; Swarm does not infer depends_on")?;
        run_actions(project, &names, &before, &docker, output, &operation)?;
        state::mark_resources(&project.root, true)?;
        let mut applied = previous.clone();
        if !applied.is_object() {
            applied = json!({});
        }
        applied["active"] = json!(true);
        applied["context"] = json!(context);
        applied["owner"] = json!(owner);
        applied["project"] = json!(project.name()?);
        applied["revision"] = json!(operation);
        applied["images"] = json!(images);
        applied["snapshot"] = json!(snapshot_path);
        if selected.is_empty() {
            operation_run(
                project,
                &docker,
                &operation,
                output,
                "apply",
                &args(&[
                    "stack",
                    "deploy",
                    "--detach=true",
                    "--with-registry-auth",
                    "--compose-file",
                    snapshot_path
                        .to_str()
                        .context("snapshot path is not UTF-8")?,
                    project.name()?,
                ]),
            )?;
            applied["rendered"] = rendered.clone();
            state::save(&project.root, "deployment-applied", &applied)?;
        } else {
            let existing = owned_services(project, &docker, &owner)?;
            for name in &names {
                let native = format!("{}_{}", project.name()?, name);
                let new = adapter(project.name()?, &rendered["services"][name], &rendered)?;
                let (native_id, old) = if existing.contains(&native) {
                    let recorded=adapter(project.name()?,previous["rendered"]["services"].get(name).context("existing selected service has no recorded specification; refuse unsafe adoption")?,&previous["rendered"])?;
                    let (id, current) =
                        current_options(project, &docker, &native, &owner, &recorded)?;
                    (id, Some(current))
                } else {
                    (native.clone(), None)
                };
                let mut argv = selected_operation(&native_id, &new, old.as_ref())?;
                // Stack namespace label keeps native stack tooling transparent for scoped creates.
                if old.is_none() {
                    argv.splice(
                        2..2,
                        args(&[
                            "--label",
                            &format!("com.docker.stack.namespace={}", project.name()?),
                        ]),
                    );
                }
                let mut intents = state::read(&project.root, "deployment-intents")?;
                if !intents.is_object() {
                    intents = json!({"services":{}});
                }
                intents["owner"] = json!(owner);
                intents["context"] = json!(context);
                intents["services"][&native] = json!({"operation":operation,"definition":rendered["services"][name],"rendered":rendered,"docker":argv});
                state::save(&project.root, "deployment-intents", &intents)?;
                journal(
                    project,
                    &operation,
                    json!({"phase":"apply-intent","service":native,"nativeId":native_id,"docker":argv}),
                )?;
                operation_run(project, &docker, &operation, output, "apply", &argv)?;
                applied["rendered"] = merge_selected_snapshot(
                    &applied["rendered"],
                    &rendered,
                    std::slice::from_ref(name),
                    project.name()?,
                )?;
                state::save(&project.root, "deployment-applied", &applied)?;
                intents["services"]
                    .as_object_mut()
                    .context("intent services must be a record")?
                    .remove(&native);
                state::save(&project.root, "deployment-intents", &intents)?;
            }
        }
        let convergence = wait_convergence(project, &names, &rendered, &docker, timeout, output)?;
        run_actions(project, &names, &after, &docker, output, &operation)?;
        let committed = snapshot_rendered;
        let record = json!({"active":true,"context":context,"owner":owner,"project":project.name()?,"revision":operation,"snapshot":snapshot_path,"rendered":committed,"images":images,"selectedServices":names,"convergence":convergence});
        state::save(&project.root, "deployment", &record)?;
        state::save(&project.root, "deployment-applied", &record)?;
        journal(
            project,
            &operation,
            json!({"state":"completed","deployment":record}),
        )?;
        Ok(
            json!({"deployed":names,"context":context,"snapshot":snapshot_path,"revision":operation,"convergence":convergence,"secretsRetained":true}),
        )
    })();
    match result {
        Ok(result) => Ok(result),
        Err(error) => {
            // Failure journaling is best effort and must never obscure an
            // already-applied deployment or the actual Docker/operation error.
            let _ = journal(
                project,
                &operation,
                json!({"state":if error.is::<runtime::Cancelled>() {"cancelled"} else {"failed"},
                    "error":format!("{error:#}"),"rollback":"not attempted; migration/application state is not transactional","secretsRetained":true}),
            );
            Err(runtime::startup_diagnostics(error, project, &names, timeout, &docker, output))
        }
    }
}

fn status_scope(project: &Project, applied: &Value, selected: &[String]) -> Result<(Value, Vec<String>)> {
    let rendered = if applied.get("active").and_then(Value::as_bool) == Some(true) {
        applied.get("rendered").cloned().context("active deployment has no rendered service scope")?
    } else {
        project.swarm()?
    };
    let services = rendered.get("services").and_then(Value::as_object)
        .context("deployment services must be a record")?;
    // selectedServices describes the last operation, not the accumulated applied
    // scope. Selected updates retain all previously applied services in rendered.
    let names = if selected.is_empty() {
        services.keys().cloned().collect()
    } else {
        let mut seen = BTreeSet::new();
        for name in selected {
            ensure!(services.contains_key(name), "unknown applied service {name}");
            ensure!(seen.insert(name), "service {name} was selected twice");
        }
        selected.to_vec()
    };
    ensure!(!names.is_empty(), "status has no required services");
    Ok((rendered, names))
}

fn current_status_tasks<'a>(service: &Value, tasks: &'a [Value]) -> BTreeMap<(u64, &'a str), &'a Value> {
    let image = service.pointer("/Spec/TaskTemplate/ContainerSpec/Image");
    let version = service.pointer("/Spec/TaskTemplate/ForceUpdate").and_then(Value::as_u64).unwrap_or(0);
    let mut latest = BTreeMap::<(u64, &str), &Value>::new();
    for task in tasks {
        if task.pointer("/Spec/ContainerSpec/Image") != image
            || task.pointer("/Spec/ForceUpdate").and_then(Value::as_u64).unwrap_or(0) != version {
            continue;
        }
        // Replicated services identify slots numerically; global services use
        // node IDs. Keep one newest task per slot without counting old attempts.
        let slot = task.get("Slot").and_then(Value::as_u64).filter(|n| *n > 0)
            .map(|slot| (slot, ""))
            .unwrap_or_else(|| (0, task.get("NodeID").or_else(|| task.get("ID"))
                .and_then(Value::as_str).unwrap_or("unknown")));
        fn timestamp(value: &Value) -> &str {
            value.get("CreatedAt").or_else(|| value.get("UpdatedAt"))
                .and_then(Value::as_str).unwrap_or("")
        }
        if latest.get(&slot).is_none_or(|old| timestamp(task) >= timestamp(old)) {
            latest.insert(slot, task);
        }
    }
    latest
}

fn status_task_summary(task: &Value) -> Value {
    json!({"id":task.get("ID"),"nodeId":task.get("NodeID"),"slot":task.get("Slot"),
        "state":task.pointer("/Status/State"),"desiredState":task.get("DesiredState"),
        "containerId":task.pointer("/Status/ContainerStatus/ContainerID"),
        "exitCode":task.pointer("/Status/ContainerStatus/ExitCode"),
        "oomKilled":task.pointer("/Status/ContainerStatus/OOMKilled")})
}

fn swarm_container_status(service: &Value, tasks: Option<&[Value]>, expected: u64, oneshot: bool) -> (bool, u64, Option<String>) {
    let image = service.pointer("/Spec/TaskTemplate/ContainerSpec/Image").and_then(Value::as_str).unwrap_or("");
    let Some(tasks) = tasks else {
        return (false, 0, rollout_failure(service, &[], image));
    };
    let current = current_status_tasks(service, tasks);
    let failure = rollout_failure(service, tasks, image).or_else(|| current.values().find_map(|task| {
        let state = task.pointer("/Status/State").and_then(Value::as_str).unwrap_or("");
        if task.pointer("/Status/ContainerStatus/OOMKilled").and_then(Value::as_bool) == Some(true) {
            Some("task was OOM-killed".into())
        } else if ["failed", "rejected", "orphaned", "paused", "rollback"].contains(&state) {
            Some(format!("task {state}"))
        } else if oneshot && state == "complete"
            && task.pointer("/Status/ContainerStatus/ExitCode").and_then(Value::as_i64) != Some(0) {
            Some("one-shot task did not exit successfully".into())
        } else {
            None
        }
    }));
    let successful = current.values().filter(|task| {
        if oneshot {
            task.pointer("/Status/State").and_then(Value::as_str) == Some("complete")
                && task.pointer("/Status/ContainerStatus/ExitCode").and_then(Value::as_i64) == Some(0)
                && task.pointer("/Status/ContainerStatus/OOMKilled").and_then(Value::as_bool) != Some(true)
        } else {
            task.pointer("/Status/State").and_then(Value::as_str) == Some("running")
                && task.get("DesiredState").and_then(Value::as_str) == Some("running")
        }
    }).count() as u64;
    let updating = service.pointer("/UpdateStatus/State").and_then(Value::as_str) == Some("updating");
    (failure.is_none() && successful == expected && (!oneshot || expected > 0) && !updating, successful, failure)
}

pub(crate) fn diagnostic_observations(
    project: &Project,
    selected: &[String],
    docker: &Docker,
    owner: &str,
    context: &str,
) -> Result<Value> {
    manager(docker)?;
    let applied = deployment_state(&project.root)?;
    if applied["active"] == true {
        ensure!(applied["context"].as_str() == Some(context)
            && applied["owner"].as_str() == Some(owner),
            "Diagnostic deployment ownership or connection differs from the environment");
    }
    // Startup may be adding services outside the previous applied scope.
    // Explicit selection is desired scope; doctor without selection uses the
    // accumulated applied scope, with no Compose dependency/profile expansion.
    let (rendered, names) = if selected.is_empty() {
        status_scope(project, &applied, selected)?
    } else {
        (project.swarm()?, selection(project, selected)?)
    };
    let existing: BTreeSet<_> = owned_services(project, docker, owner)?.into_iter().collect();
    let mut services = Vec::new();
    for name in names {
        docker.remaining(Duration::from_secs(10))?;
        let native = format!("{}_{}", project.name()?, name);
        let mut row = json!({"service":name,"observed":true,"present":false,
            "containerReady":false,"unhealthy":false,"applicationReady":null,
            "oneShot":runtime::one_shot(project, &name)?,"verifiedContainerIds":[],
            "containers":[],"tasks":[],"failures":[]});
        if !existing.contains(&native) {
            services.push(row);
            continue;
        }
        let observation = (|| -> Result<()> {
            let service = inspect(docker, "service", &native)?;
            ensure!(service["Spec"]["Name"].as_str() == Some(native.as_str())
                && service["Spec"]["Labels"][OWNER].as_str() == Some(owner)
                && service["Spec"]["Labels"][PROJECT].as_str() == Some(project.name()?),
                "Diagnostic service ownership could not be verified");
            let service_id = runtime::diagnostic_resource_id(&service["ID"])
                .context("Diagnostic service has no immutable identity")?;
            let tasks = tasks(docker, service_id)?
                .context("Swarm tasks disappeared during diagnostic observation")?;
            // Verify the complete task set before publishing any task IDs.
            for task in &tasks {
                ensure!(task["ServiceID"].as_str() == Some(service_id)
                    && runtime::diagnostic_resource_id(&task["ID"]).is_some(),
                    "Diagnostic task linkage could not be verified");
            }
            row["present"] = json!(true);
            row["verifiedServiceId"] = json!(service_id);
            let desired = service.pointer("/Spec/Mode/Replicated/Replicas").and_then(Value::as_u64)
                .or_else(|| service.pointer("/ServiceStatus/DesiredTasks").and_then(Value::as_u64));
            let configured = rendered["services"][&name].pointer("/deploy/replicas").and_then(Value::as_u64)
                .or_else(|| if service.pointer("/Spec/Mode/Replicated").is_some() { Some(1) } else { None });
            row["desiredReplicas"] = json!(desired);
            if let Some(expected) = configured.or(desired) {
                let (ready, successful, _) = swarm_container_status(&service, Some(&tasks),
                    expected, runtime::one_shot(project, &name)?);
                row["containerReady"] = json!(ready && desired == Some(expected));
                row["successfulTasks"] = json!(successful);
                row["expectedReplicas"] = json!(expected);
            }
            for task in current_status_tasks(&service, &tasks).into_values() {
                let task_id = runtime::diagnostic_resource_id(&task["ID"]).unwrap();
                let state = task["Status"]["State"].as_str().filter(|state|
                    ["new", "allocated", "pending", "assigned", "accepted", "preparing", "ready",
                        "starting", "running", "complete", "shutdown", "failed", "rejected",
                        "remove", "orphaned"].contains(state));
                let mut summary = json!({"id":task_id,"serviceId":service_id,"state":state,
                    "exitCode":task.pointer("/Status/ContainerStatus/ExitCode").and_then(Value::as_i64),
                    "oomKilled":task.pointer("/Status/ContainerStatus/OOMKilled").and_then(Value::as_bool)});
                if let Some(container_id) = task.pointer("/Status/ContainerStatus/ContainerID")
                    .and_then(runtime::diagnostic_resource_id) {
                    let container = inspect(docker, "container", container_id);
                    match container {
                        Ok(container) => {
                            let labels = &container["Config"]["Labels"];
                            let direct_owner = labels[OWNER].as_str() == Some(owner)
                                && labels[PROJECT].as_str() == Some(project.name()?);
                            let legacy_owner = labels.get(OWNER).is_none()
                                && labels.get(PROJECT).is_none_or(|project_label|
                                    project_label.as_str() == Some(project.name().unwrap_or("")));
                            if container["Id"].as_str() == Some(container_id)
                                && labels["com.docker.swarm.service.id"].as_str() == Some(service_id)
                                && labels["com.docker.swarm.task.id"].as_str() == Some(task_id)
                                && (direct_owner || legacy_owner) {
                                let mut state = runtime::diagnostic_container_state(&container);
                                state["id"] = json!(container_id);
                                state["taskId"] = json!(task_id);
                                if state["health"] == "unhealthy" {
                                    row["unhealthy"] = json!(true);
                                    row["containerReady"] = json!(false);
                                }
                                summary["verifiedContainerId"] = json!(container_id);
                                row["verifiedContainerIds"].as_array_mut().unwrap().push(json!(container_id));
                                row["containers"].as_array_mut().unwrap().push(state);
                            } else {
                                row["containerReady"] = json!(false);
                                row["failures"].as_array_mut().unwrap().push(json!("Task container ownership or immutable linkage could not be verified"));
                            }
                        }
                        Err(_) => {
                            // Historical/remote task containers may be absent
                            // on this manager. Absence is never ownership proof.
                            row["failures"].as_array_mut().unwrap().push(json!("Task container could not be observed on the pinned Docker connection"));
                        }
                    }
                }
                row["tasks"].as_array_mut().unwrap().push(summary);
            }
            Ok(())
        })();
        if observation.is_err() {
            row["observed"] = json!(false);
            row["containerReady"] = json!(false);
            row["verifiedContainerIds"] = json!([]);
            row.as_object_mut().unwrap().remove("verifiedServiceId");
            row["containers"] = json!([]);
            row["tasks"] = json!([]);
            row["failures"].as_array_mut().unwrap().push(json!("Service or task observation/ownership verification failed"));
        }
        services.push(row);
    }
    Ok(json!({"backend":"swarm","project":project.name()?,"context":context,"services":services}))
}

fn status_configuration_error(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<runtime::DockerError>().is_some() {
        error
    } else {
        error.context(crate::status::StatusConfiguration)
    }
}

pub fn status(
    project: &Project,
    selected: &[String],
    inspect_only: bool,
    timeout: u64,
    output: &Output,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(timeout.min(10));
    let docker = Docker::new(&project.root, output.clone()).with_deadline(deadline);
    let mut report = json!({"backend":"swarm","project":project.env.get("project"),"context":null,
        "inspectOnly":inspect_only,"ready":false,"deadlineExceeded":false,
        "requiredServices":[],"excludedServices":[],"services":[],"endpoints":project.endpoints()});
    let result = (|| -> Result<()> {
        let applied = deployment_state(&project.root).context(crate::status::StatusConfiguration)?;
        let recorded = applied.get("active").and_then(Value::as_bool) == Some(true);
        let (rendered, names) = status_scope(project, &applied, selected).context(crate::status::StatusConfiguration)?;
        let required: BTreeSet<_> = names.iter().cloned().collect();
        let mut visible: BTreeSet<_> = project.services()?.keys().cloned().collect();
        visible.extend(rendered["services"].as_object().context("missing services")?.keys().cloned());
        report["deploymentRecorded"] = json!(recorded);
        if !recorded { report["error"] = json!("No recorded deployment; deploy this environment first"); }
        report["requiredServices"] = json!(names);
        report["excludedServices"] = json!(visible.difference(&required).collect::<Vec<_>>());
        report["services"] = json!(visible.iter().map(|name| json!({
            "name":name,"required":required.contains(name),"observed":false,"containerReady":false,
            "applicationReady":null,"ready":false,"status":if required.contains(name) {"unobserved"} else {"excluded"}
        })).collect::<Vec<_>>());
        // Validate configured checks even when inspection skips execution or a
        // missing service cannot be probed.
        for name in &names {
            runtime::validate_readiness(project, name).context(crate::status::StatusConfiguration)?;
        }
        if Instant::now() >= deadline { return Ok(()); }
        let context = docker.context()?;
        report["context"] = json!(context);
        let info = manager(&docker).map_err(status_configuration_error)?;
        report["daemonId"] = info.get("ID").cloned().unwrap_or(Value::Null);
        let identity = state::read(&project.root, "identity")?;
        let owner = read_owner(project, &context, false).context(crate::status::StatusConfiguration)?;
        if recorded {
            (|| -> Result<()> {
                ensure!(identity.get("id").is_some(), "applied deployment has no ownership identity");
                ensure!(applied.get("context").and_then(Value::as_str) == Some(context.as_str()),
                    "Docker context differs from applied deployment");
                ensure!(applied.get("owner").and_then(Value::as_str) == Some(owner.as_str()),
                    "applied deployment ownership differs from environment identity");
                Ok(())
            })().context(crate::status::StatusConfiguration)?;
        }
        if Instant::now() >= deadline { return Ok(()); }
        check_resources(project, &rendered, &applied, &names, &docker, &owner).map_err(status_configuration_error)?;
        if Instant::now() >= deadline { return Ok(()); }
        let existing: BTreeSet<_> = owned_services(project, &docker, &owner)?.into_iter().collect();
        report["ownedServices"] = json!(existing);
        for name in &names {
            if Instant::now() >= deadline { break; }
            let index = report["services"].as_array().context("missing status rows")?.iter()
                .position(|row| row["name"].as_str() == Some(name)).context("missing service row")?;
            let native = format!("{}_{}", project.name()?, name);
            report["services"][index]["nativeName"] = json!(native);
            if !existing.contains(&native) {
                report["services"][index]["observed"] = json!(true);
                report["services"][index]["status"] = json!("missing");
                continue;
            }
            let observation = (|| -> Result<()> {
                let service = inspect(&docker, "service", &native)?;
                (|| -> Result<()> {
                    ensure!(service.pointer("/Spec/Labels").and_then(|v| v.get(OWNER)).and_then(Value::as_str) == Some(owner.as_str())
                        && service.pointer("/Spec/Labels").and_then(|v| v.get(PROJECT)).and_then(Value::as_str) == Some(project.name()?),
                        "ownership changed for service {native}");
                    Ok(())
                })().context(crate::status::StatusConfiguration)?;
                report["services"][index]["serviceId"] = service.get("ID").cloned().unwrap_or(Value::Null);
                let task_values = tasks(&docker, &native)?;
                let desired = service.pointer("/Spec/Mode/Replicated/Replicas").and_then(Value::as_u64)
                    .or_else(|| service.pointer("/ServiceStatus/DesiredTasks").and_then(Value::as_u64));
                let desired = match desired {
                    Some(desired) => desired,
                    None => {
                        let replicas = docker.capture(&args(&["service","ls","--filter",&format!("name={native}"),"--format","{{.Replicas}}"]), None)?;
                        replicas.trim().split('/').nth(1).and_then(|s| s.split_whitespace().next())
                            .and_then(|n| n.parse().ok()).context("cannot determine desired global-service tasks")?
                    }
                };
                let configured = rendered["services"][name].pointer("/deploy/replicas").and_then(Value::as_u64)
                    .or_else(|| if service.pointer("/Spec/Mode/Replicated").is_some() { Some(1) } else { None });
                let expected = configured.unwrap_or(desired);
                // Swarm never infers completion from Compose depends_on.
                let oneshot = runtime::one_shot(project, name)?;
                let (mut container_ready, successful, mut failure) = swarm_container_status(&service, task_values.as_deref(), expected, oneshot);
                if desired != expected {
                    container_ready = false;
                    failure = Some(format!("desired replicas {desired} differ from applied replicas {expected}"));
                }
                if let Some(expected_image) = rendered["services"][name].get("image").and_then(Value::as_str)
                    && service.pointer("/Spec/TaskTemplate/ContainerSpec/Image").and_then(Value::as_str) != Some(expected_image) {
                    container_ready = false;
                    failure = Some("live service image differs from applied revision".into());
                }
                let row = &mut report["services"][index];
                row["observed"] = json!(true);
                row["desiredReplicas"] = json!(desired);
                row["expectedReplicas"] = json!(expected);
                row["successfulTasks"] = json!(successful);
                row["oneShot"] = json!(oneshot);
                row["tasksObserved"] = json!(task_values.is_some());
                row["tasks"] = json!(task_values.as_deref().unwrap_or(&[]).iter().map(status_task_summary).collect::<Vec<_>>());
                row["containerReady"] = json!(container_ready);
                row["rolloutState"] = service.pointer("/UpdateStatus/State").cloned().unwrap_or(Value::Null);
                if let Some(failure) = failure { row["error"] = json!(failure); }
                let configured_check = project.metadata["readiness"].get(name).is_some();
                let application = if !inspect_only && container_ready && successful > 0 && configured_check {
                    if Instant::now() >= deadline {
                        report["services"][index]["status"] = json!("application-unobserved");
                        return Ok(());
                    }
                    Some(runtime::readiness_check(project, name, &docker, output,
                        deadline.saturating_duration_since(Instant::now()).as_secs().max(1))?)
                } else { None };
                let ready = container_ready && (!configured_check || application == Some(true));
                report["services"][index]["applicationReady"] = json!(application);
                report["services"][index]["ready"] = json!(ready);
                report["services"][index]["status"] = json!(if ready { if oneshot { "completed" } else { "ready" } }
                    else if container_ready && inspect_only { "inspected" } else { "not-ready" });
                Ok(())
            })();
            if let Err(error) = observation {
                if error.is::<runtime::DeadlineExceeded>() && Instant::now() >= deadline {
                    report["services"][index]["status"] = json!("unobserved");
                } else {
                    report["services"][index]["status"] = json!("error");
                    report["services"][index]["error"] = json!(error.to_string());
                }
                return Err(error);
            }
        }
        report["ready"] = json!(recorded && report["services"].as_array().context("missing rows")?.iter()
            .filter(|row| row["required"] == true).all(|row| row["ready"] == true));
        Ok(())
    })();
    report["deadlineExceeded"] = json!(Instant::now() >= deadline);
    if report["deadlineExceeded"] == true { report["ready"] = json!(false); }
    if let Err(error) = result {
        if !(error.is::<runtime::DeadlineExceeded>() && report["deadlineExceeded"] == true) {
            return Err(error.context(crate::status::StatusReport(report)));
        }
    }
    crate::status::finish(report)
}

fn disappeared_teardown_network(error: &anyhow::Error, kind: &str, id: &str) -> bool {
    let Some(error) = error.downcast_ref::<runtime::DockerError>() else {
        return false;
    };
    // Only captured immutable network IDs qualify. Volume names can refer to
    // recreated data, and a name from resource enumeration is not ownership proof.
    kind == "network"
        && error.status == 1
        && !error.stderr.is_empty()
        && error.stderr.lines().all(|line| {
            line.strip_prefix("Error response from daemon: network ")
                .and_then(|line| line.strip_suffix(" not found"))
                == Some(id)
        })
}

fn verify_teardown_resource(
    project: &Project,
    docker: &Docker,
    owner: &str,
    argv: &[String],
    volume_fingerprint: Option<&Value>,
) -> Result<bool> {
    let resource = match inspect(docker, &argv[0], &argv[2]) {
        Ok(resource) => resource,
        Err(error) if disappeared_teardown_network(&error, &argv[0], &argv[2]) => return Ok(false),
        Err(error) => return Err(error),
    };
    let labels = resource
        .get("Labels")
        .or_else(|| resource.pointer("/Spec/Labels"));
    ensure!(
        labels.and_then(|v| v.get(OWNER)).and_then(Value::as_str) == Some(owner)
            && labels.and_then(|v| v.get(PROJECT)).and_then(Value::as_str)
                == Some(project.name()?),
        "resource ownership changed before deletion; refusing to remove {}",
        argv[2]
    );
    if argv[0] == "volume" {
        ensure!(
            volume_fingerprint == Some(
                &json!({"created":resource.get("CreatedAt"),"driver":resource.get("Driver"),"mountpoint":resource.get("Mountpoint"),"labels":labels})
            ),
            "owned volume name was recreated before deletion; refusing to delete replacement data"
        );
    }
    Ok(true)
}

fn remove_teardown_resource(
    project: &Project,
    docker: &Docker,
    owner: &str,
    operation: &str,
    argv: &[String],
    volume_fingerprint: Option<&Value>,
    output: &Output,
) -> Result<()> {
    if !verify_teardown_resource(project, docker, owner, argv, volume_fingerprint)? {
        return Ok(());
    }
    if argv[0] == "service" {
        return operation_run(project, docker, operation, output, "remove-service", argv);
    }
    // Service removal is asynchronous; wait before removing referenced resources.
    journal(
        project,
        operation,
        json!({"phase":"remove-resource","state":"started","docker":argv}),
    )?;
    output.event("remove-resource", &format!("docker {}", argv.join(" ")))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut reported_absence = None;
    loop {
        if !verify_teardown_resource(project, docker, owner, argv, volume_fingerprint)? {
            break;
        }
        let error = match reported_absence.take() {
            Some(error) => error,
            None => match docker.run(argv, None) {
                Ok(()) => break,
                Err(error) => error,
            },
        };
        if Instant::now() >= deadline {
            journal(
                project,
                operation,
                json!({"state":"failed","error":format!("{error:#}"),"docker":argv}),
            )?;
            return Err(error).context(
                "teardown resource remains in use; nothing unrelated was removed",
            );
        }
        if disappeared_teardown_network(&error, &argv[0], &argv[2]) {
            // Swarm's removal endpoint can report absence while local overlay
            // inspection still sees the owned object. Stop issuing rm, but
            // require inspect to prove absence under this same deadline.
            reported_absence = Some(error);
        }
        thread::sleep(Duration::from_millis(500));
    }
    journal(
        project,
        operation,
        json!({"phase":"remove-resource","state":"completed","docker":argv}),
    )?;
    Ok(())
}

pub fn teardown(
    project: &Project,
    destroy: bool,
    plan_only: bool,
    confirmed: bool,
    output: &Output,
) -> Result<Value> {
    let _lock = if plan_only {
        None
    } else {
        ensure!(
            !destroy || confirmed,
            "destroying data requires explicit confirmation"
        );
        let lock = state::lock(&project.root, "lifecycle")?;
        recheck_environment(project)?;
        Some(lock)
    };
    let docker = Docker::new(&project.root, output.clone());
    let info = manager(&docker)?;
    let owner = if plan_only {
        read_owner(project, &docker.context()?, true)?
    } else {
        runtime::validate_ownership(project, &docker, false)?
    };
    let services = owned_services(project, &docker, &owner)?;
    let mut operations = Vec::<Vec<String>>::new();
    let mut volume_fingerprints = BTreeMap::new();
    for name in &services {
        let service = inspect(&docker, "service", name)?;
        ensure!(
            service
                .pointer("/Spec/Labels")
                .and_then(|v| v.get(OWNER))
                .and_then(Value::as_str)
                == Some(owner.as_str()),
            "ownership changed for service {name}"
        );
        operations.push(args(&[
            "service",
            "rm",
            service
                .get("ID")
                .and_then(Value::as_str)
                .context("service has no immutable ID")?,
        ]));
    }
    for (kind, remove) in [("network", true), ("config", true), ("volume", destroy)] {
        if !remove {
            continue;
        }
        if kind == "volume"
            && info
                .pointer("/Swarm/Nodes")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                > 1
        {
            bail!(
                "destructive volume teardown cannot safely enumerate worker-local volumes in a multi-node Swarm; remove explicitly owned data on each node, or use non-destructive down"
            );
        }
        let resources = docker.capture(
            &args(&[
                kind,
                "ls",
                "--filter",
                &format!("label={OWNER}={owner}"),
                "--filter",
                &format!("label={PROJECT}={}", project.name()?),
                "--format",
                "{{.Name}}",
            ]),
            None,
        )?;
        for name in resources.lines().filter(|s| !s.is_empty()) {
            let item = inspect(&docker, kind, name)?;
            let labels = item.get("Labels").or_else(|| item.pointer("/Spec/Labels"));
            ensure!(
                labels.and_then(|v| v.get(OWNER)).and_then(Value::as_str) == Some(owner.as_str())
                    && labels.and_then(|v| v.get(PROJECT)).and_then(Value::as_str)
                        == Some(project.name()?),
                "ownership changed for {kind} {name}"
            );
            if kind == "volume" {
                volume_fingerprints.insert(name.to_owned(),json!({"created":item.get("CreatedAt"),"driver":item.get("Driver"),"mountpoint":item.get("Mountpoint"),"labels":labels}));
            }
            let id = if kind == "volume" {
                name
            } else {
                item.get("ID")
                    .or_else(|| item.get("Id"))
                    .and_then(Value::as_str)
                    .context("resource has no immutable ID")?
            };
            operations.push(args(&[kind, "rm", id]));
        }
    }
    let plan = json!({"backend":"swarm","context":docker.context()?,"project":project.name()?,"dockerOperations":operations,"destroyData":destroy,"secretsRetained":true,"planOnly":plan_only});
    if plan_only {
        return Ok(plan);
    }
    let operation = format!("down-{}", state::random_id()?);
    journal(project, &operation, json!({"state":"started","plan":plan}))?;
    for argv in operations {
        remove_teardown_resource(
            project,
            &docker,
            &owner,
            &operation,
            &argv,
            volume_fingerprints.get(&argv[2]),
            output,
        )?;
    }
    let mut deployment = state::read(&project.root, "deployment")?;
    if !deployment.is_object() {
        deployment = json!({});
    }
    deployment["active"] = json!(false);
    state::save(&project.root, "deployment", &deployment)?;
    state::save(&project.root, "deployment-applied", &deployment)?;
    // Preserved volumes/secrets still bind this environment to its identity/context.
    state::mark_resources(&project.root, true)?;
    journal(
        project,
        &operation,
        json!({"state":"completed","secretsRetained":true,"dataRetained":!destroy}),
    )?;
    Ok(
        json!({"removedServices":services,"dataRetained":!destroy,"secretsRetained":true,"context":docker.context()?}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_network_disappearance_completes_teardown_without_hiding_failures() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("docker");
        std::fs::write(&executable, r#"#!/bin/sh
root="$DKS_NETWORK_RACE_ROOT"
mode=$(cat "$root/mode")
fail() {
  case "$mode" in
    *mixed) printf 'Error response from daemon: network captured-id not found\npermission denied\n' >&2 ;;
    *foreign) printf 'Error response from daemon: network unrequested-id not found\n' >&2 ;;
    *kind) printf 'Error response from daemon: config captured-id not found\n' >&2 ;;
    *permission) printf 'permission denied\n' >&2 ;;
    *transport) printf 'Cannot connect to the Docker daemon\n' >&2 ;;
    *blank) : ;;
    *) printf 'Error response from daemon: network captured-id not found\n' >&2 ;;
  esac
  case "$mode" in *status) exit 2 ;; *) exit 1 ;; esac
}
case "$1 $2" in
  "network inspect"|"volume inspect")
    count=$(cat "$root/inspections")
    count=$((count + 1))
    printf '%s\n' "$count" > "$root/inspections"
    case "$mode" in
      inspect-*|before-delete|volume-missing) fail ;;
      during-wait) if [ "$count" -gt 1 ]; then fail; fi ;;
      at-delete|after-busy|remove-*) if [ "$count" -gt 2 ]; then fail; fi ;;
      stale-delete) if [ "$count" -gt 3 ]; then fail; fi ;;
    esac
    case "$mode" in
      replaced-volume|replaced-network) if [ "$count" -gt 1 ]; then cat "$root/replacement.json"; else cat "$root/resource.json"; fi ;;
      *) cat "$root/resource.json" ;;
    esac ;;
  "network rm"|"volume rm")
    count=$(cat "$root/removals")
    printf '%s\n' "$((count + 1))" > "$root/removals"
    case "$mode" in
      after-busy) printf 'network has active endpoints\n' >&2; exit 1 ;;
      at-delete|stale-delete|remove-*) fail ;;
      *) exit 0 ;;
    esac ;;
  *) exit 42 ;;
esac
"#).unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "deploy::tests::network_disappearance_child", "--ignored"])
            .env("PATH", format!("{}:{}", directory.path().display(), std::env::var("PATH").unwrap_or_default()))
            .env("DKS_NETWORK_RACE_ROOT", directory.path())
            .env_remove("DOCKER_CONTEXT")
            .env("DOCKER_HOST", "unix:///network-race-fixture.sock")
            .output().unwrap();
        assert!(child.status.success(), "{}\n{}", String::from_utf8_lossy(&child.stdout), String::from_utf8_lossy(&child.stderr));
    }

    #[test]
    #[ignore = "isolated helper invoked by captured_network_disappearance_completes_teardown_without_hiding_failures"]
    fn network_disappearance_child() {
        let root = std::path::PathBuf::from(std::env::var_os("DKS_NETWORK_RACE_ROOT").unwrap());
        let output = Output { json: true, quiet: true };
        let docker = Docker::new(&root, output.clone());
        let project = Project {
            root: root.clone(),
            env: json!({"project":"race","backend":"swarm"}),
            model: json!({}),
            metadata: json!({}),
            fields: Vec::new(),
        };
        let resource = json!({"ID":"captured-id","Labels":{OWNER:"owner",PROJECT:"race"},
            "CreatedAt":"original","Driver":"local","Mountpoint":"/original"});
        let fingerprint = json!({"created":resource["CreatedAt"],"driver":resource["Driver"],
            "mountpoint":resource["Mountpoint"],"labels":resource["Labels"]});
        let prepare = |mode: &str, value: &Value| {
            std::fs::write(root.join("mode"), mode).unwrap();
            std::fs::write(root.join("inspections"), "0").unwrap();
            std::fs::write(root.join("removals"), "0").unwrap();
            std::fs::write(root.join("resource.json"), serde_json::to_vec(&json!([value])).unwrap()).unwrap();
        };
        let removals = || std::fs::read_to_string(root.join("removals")).unwrap().trim().parse::<u32>().unwrap();
        let network = args(&["network", "rm", "captured-id"]);
        let remove = |argv: &[String], fingerprint: Option<&Value>| {
            remove_teardown_resource(&project, &docker, "owner", "down-race", argv, fingerprint, &output)
        };
        for (mode, expected_removals) in [
            ("before-delete", 0), ("during-wait", 0), ("at-delete", 1),
            ("stale-delete", 1), ("after-busy", 1), ("present", 1),
        ] {
            prepare(mode, &resource);
            remove(&network, None).unwrap();
            assert_eq!(removals(), expected_removals, "{mode}");
            if mode == "stale-delete" {
                assert_eq!(std::fs::read_to_string(root.join("inspections")).unwrap().trim(), "4");
            }
        }
        // Missing-object text mixed with other failures or referring to a
        // different kind/ID must not turn either inspect or rm into success.
        for phase in ["inspect", "remove"] {
            for (failure, status, evidence) in [
                ("mixed", 1, "permission denied"),
                ("foreign", 1, "network unrequested-id not found"),
                ("kind", 1, "config captured-id not found"),
                ("permission", 1, "permission denied"),
                ("transport", 1, "Cannot connect to the Docker daemon"),
                ("status", 2, "network captured-id not found"),
                ("blank", 1, ""),
            ] {
                prepare(&format!("{phase}-{failure}"), &resource);
                let error = remove(&network, None).unwrap_err();
                let original = error.downcast_ref::<runtime::DockerError>().unwrap();
                assert_eq!(original.status, status);
                if evidence.is_empty() {
                    assert!(original.stderr.is_empty());
                } else {
                    assert!(original.stderr.contains(evidence));
                }
                assert_eq!(removals(), u32::from(phase == "remove"));
            }
        }
        for (label, value) in [(OWNER, "foreign-owner"), (PROJECT, "foreign-project")] {
            let mut foreign = resource.clone();
            foreign["Labels"][label] = json!(value);
            prepare("present", &foreign);
            assert!(remove(&network, None).is_err());
            assert_eq!(removals(), 0);
        }
        let volume = args(&["volume", "rm", "captured-id"]);
        prepare("volume-missing", &resource);
        assert!(remove(&volume, Some(&fingerprint)).unwrap_err().is::<runtime::DockerError>());
        assert_eq!(removals(), 0);
        let mut replacement = resource.clone();
        replacement["CreatedAt"] = json!("replacement");
        std::fs::write(root.join("replacement.json"), serde_json::to_vec(&json!([replacement.clone()])).unwrap()).unwrap();
        for (mode, value) in [("present", &replacement), ("replaced-volume", &resource)] {
            prepare(mode, value);
            assert!(remove(&volume, Some(&fingerprint)).is_err());
            assert_eq!(removals(), 0);
        }
        replacement = resource.clone();
        replacement["Labels"][OWNER] = json!("foreign-owner");
        std::fs::write(root.join("replacement.json"), serde_json::to_vec(&json!([replacement])).unwrap()).unwrap();
        prepare("replaced-network", &resource);
        assert!(remove(&network, None).is_err());
        assert_eq!(removals(), 0);
    }

    #[test]
    fn disappearing_tasks_are_reobserved_without_stale_or_zero_replica_success() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("docker");
        std::fs::write(&executable, r#"#!/bin/sh
root="$DKS_TASK_RACE_ROOT"
mode=$(cat "$root/mode")
case "$1 $2" in
  "service inspect") cat "$root/service.json" ;;
  "service ps")
    case "$mode" in
      stable) printf 'current\n' ;;
      empty) : ;;
      *) printf 'obsolete\ncurrent\n' ;;
    esac ;;
  "inspect --type")
    case "$mode" in
      stable) cat "$root/current.json" ;;
      malformed) printf 'invalid JSON\n' ;;
      race|missing)
        cat "$root/obsolete.json"
        printf 'Error response from daemon: task current not found\n' >&2
        if [ "$mode" = race ]; then printf 'stable\n' > "$root/mode"; fi
        exit 1 ;;
      mixed)
        printf 'Error response from daemon: task current not found\npermission denied\n' >&2
        exit 1 ;;
      foreign)
        printf 'Error response from daemon: task unrequested not found\n' >&2
        exit 1 ;;
      status)
        printf 'Error response from daemon: task current not found\n' >&2
        exit 2 ;;
      blank) exit 1 ;;
      permission) printf 'permission denied\n' >&2; exit 1 ;;
      transport) printf 'Cannot connect to the Docker daemon\n' >&2; exit 1 ;;
      *) exit 42 ;;
    esac ;;
  *) exit 42 ;;
esac
"#).unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "deploy::tests::task_disappearance_child", "--ignored"])
            .env("PATH", format!("{}:{}", directory.path().display(), std::env::var("PATH").unwrap_or_default()))
            .env("DKS_TASK_RACE_ROOT", directory.path())
            .env_remove("DOCKER_CONTEXT")
            .env("DOCKER_HOST", "unix:///task-race-fixture.sock")
            .output().unwrap();
        assert!(child.status.success(), "{}\n{}", String::from_utf8_lossy(&child.stdout), String::from_utf8_lossy(&child.stderr));
    }

    #[test]
    #[ignore = "isolated helper invoked by disappearing_tasks_are_reobserved_without_stale_or_zero_replica_success"]
    fn task_disappearance_child() {
        let root = std::path::PathBuf::from(std::env::var_os("DKS_TASK_RACE_ROOT").unwrap());
        let output = Output { json: true, quiet: true };
        let docker = Docker::new(&root, output.clone());
        let project = Project {
            root: root.clone(),
            env: json!({"project":"race","backend":"swarm"}),
            model: json!({"services":{"api":{"image":"image@sha256:abc"}}}),
            metadata: json!({}),
            fields: Vec::new(),
        };
        let mut service = json!({"Spec":{"Mode":{"Replicated":{"Replicas":1}},
            "TaskTemplate":{"ContainerSpec":{"Image":"image@sha256:abc"}}}});
        let current = json!({"ID":"current","Slot":1,"CreatedAt":"2026-10-05T01:00:00Z",
            "DesiredState":"running","Spec":{"ContainerSpec":{"Image":"image@sha256:abc"}},
            "Status":{"State":"running"}});
        let mut obsolete = current.clone();
        obsolete["ID"] = json!("obsolete");
        obsolete["CreatedAt"] = json!("2026-10-05T00:00:00Z");
        for (file, value) in [("service.json", json!([service.clone()])),
            ("current.json", json!([current])), ("obsolete.json", json!([obsolete]))] {
            std::fs::write(root.join(file), serde_json::to_vec(&value).unwrap()).unwrap();
        }
        let mode = |value: &str| std::fs::write(root.join("mode"), value).unwrap();
        mode("race");
        let incomplete = tasks(&docker, "race_api").unwrap();
        assert!(incomplete.is_none());
        // Both strict status and convergence use this checker. Unknown is
        // never equivalent to a verified empty collection, even at zero scale.
        for expected in [0, 1] {
            for oneshot in [false, true] {
                assert_eq!(swarm_container_status(&service, incomplete.as_deref(), expected, oneshot), (false, 0, None));
            }
        }
        let observed = tasks(&docker, "race_api").unwrap();
        assert_eq!(swarm_container_status(&service, observed.as_deref(), 1, false), (true, 1, None));
        mode("race");
        let rendered = json!({"services":{"api":{"image":"image@sha256:abc"}}});
        let converged = wait_convergence(&project, &["api".into()], &rendered, &docker, 5, &output).unwrap();
        assert_eq!(converged[0]["running"], 1);
        assert_eq!(converged[0]["tasksObserved"], true);

        mode("missing");
        assert!(wait_convergence(&project, &["api".into()], &rendered, &docker, 0, &output)
            .unwrap_err().to_string().contains("Swarm rollout timed out"));
        service["Spec"]["Mode"]["Replicated"]["Replicas"] = json!(0);
        std::fs::write(root.join("service.json"), serde_json::to_vec(&json!([service.clone()])).unwrap()).unwrap();
        assert!(wait_convergence(&project, &["api".into()], &rendered, &docker, 0, &output)
            .unwrap_err().to_string().contains("Swarm rollout timed out"));
        mode("empty");
        let empty = tasks(&docker, "race_api").unwrap();
        assert_eq!(swarm_container_status(&service, empty.as_deref(), 0, false), (true, 0, None));
        assert_eq!(swarm_container_status(&service, empty.as_deref(), 0, true), (false, 0, None));

        for (value, status, message) in [
            ("mixed", 1, "permission denied"),
            ("foreign", 1, "task unrequested not found"),
            ("status", 2, "task current not found"),
            ("blank", 1, ""),
            ("permission", 1, "permission denied"),
            ("transport", 1, "Cannot connect to the Docker daemon"),
        ] {
            mode(value);
            let error = tasks(&docker, "race_api").unwrap_err();
            let original = error.downcast_ref::<runtime::DockerError>().unwrap();
            assert_eq!(original.status, status);
            if message.is_empty() {
                assert!(original.stderr.is_empty());
            } else {
                assert!(original.stderr.contains(message));
            }
        }
        mode("malformed");
        let error = tasks(&docker, "race_api").unwrap_err();
        assert!(error.downcast_ref::<serde_json::Error>().is_some());

        let mut oneshot_project = project;
        oneshot_project.metadata = json!({"oneshots":["api"]});
        service["Spec"]["Mode"]["Replicated"]["Replicas"] = json!(1);
        std::fs::write(root.join("service.json"), serde_json::to_vec(&json!([service])).unwrap()).unwrap();
        for file in ["current.json", "obsolete.json"] {
            let mut completed: Value = serde_json::from_slice(&std::fs::read(root.join(file)).unwrap()).unwrap();
            completed[0]["DesiredState"] = json!("shutdown");
            completed[0]["Status"] = json!({"State":"complete","ContainerStatus":{"ExitCode":0}});
            std::fs::write(root.join(file), serde_json::to_vec(&completed).unwrap()).unwrap();
        }
        mode("race");
        let completed = wait_convergence(&oneshot_project, &["api".into()], &rendered, &docker, 5, &output).unwrap();
        assert_eq!(completed[0]["completed"], 1);
        assert_eq!(completed[0]["tasksObserved"], true);
    }

    #[test]
    fn selected_update_does_not_touch_dependencies_or_force_restarts() {
        let document = json!({"networks":{"default":{}},"services":{"api":{"image":"registry.test/api@sha256:abc","environment":{"MODE":"new"},"deploy":{"labels":{OWNER:"owner",PROJECT:"app"}}},"db":{"image":"postgres:17"}}});
        let old=adapter("app",&json!({"image":"registry.test/api@sha256:old","environment":{"MODE":"old","REMOVED":"x"},"deploy":{"labels":{OWNER:"owner",PROJECT:"app"}}}),&document).unwrap();
        let new = adapter("app", &document["services"]["api"], &document).unwrap();
        let argv = selected_operation("app_api", &new, Some(&old)).unwrap();
        assert_eq!(argv.last().unwrap(), "app_api");
        assert!(
            !argv
                .iter()
                .any(|s| s == "app_db" || s == "--force" || s == "--prune")
        );
        assert!(argv.windows(2).any(|s| s == ["--env-rm", "REMOVED"]));
        assert!(argv.windows(2).any(|s| s == ["--env-add", "MODE=new"]));
    }
    #[test]
    fn selected_scope_rejects_shared_network_change() {
        let previous = json!({"active":true,"rendered":{"networks":{"default":{"driver":"overlay"}},"volumes":{}}});
        let changed = json!({"networks":{"default":{"driver":"bridge"}},"volumes":{}});
        assert!(
            shared_scope(&changed, &previous, true)
                .unwrap_err()
                .to_string()
                .contains("shared networks")
        );
        assert!(shared_scope(&previous["rendered"], &previous, true).is_ok());
    }
    #[test]
    fn failed_desired_revision_is_not_success_even_if_old_tasks_are_running() {
        let service = json!({"Spec":{"TaskTemplate":{}},"UpdateStatus":{"State":"rollback_completed","Message":"tasks failed"}});
        assert!(
            rollout_failure(&service, &[], "new@sha256:abc")
                .unwrap()
                .contains("rollback_completed")
        );
        let task = json!({"ID":"task","Spec":{"ContainerSpec":{"Image":"new@sha256:abc"}},"Status":{"State":"rejected","Err":"no suitable node"}});
        assert!(
            rollout_failure(&json!({}), &[task], "new@sha256:abc")
                .unwrap()
                .contains("no suitable node")
        );
    }
    #[test]
    fn obsolete_failed_revision_does_not_poison_new_rollout() {
        let task = json!({"Spec":{"ContainerSpec":{"Image":"old@sha256:abc"}},"Status":{"State":"failed"}});
        assert!(rollout_failure(&json!({}), &[task], "new@sha256:abc").is_none());
    }
    #[test]
    fn unsupported_selected_options_are_not_ignored() {
        let result = adapter(
            "app",
            &json!({"image":"api:tag","privileged":true}),
            &json!({}),
        );
        assert!(result.err().unwrap().to_string().contains("privileged"));
    }
    #[test]
    fn removed_scalar_rejects_unsafe_partial_reset() {
        let old = adapter("app", &json!({"image":"api:old","user":"1000"}), &json!({})).unwrap();
        let new = adapter("app", &json!({"image":"api:new"}), &json!({})).unwrap();
        assert!(
            selected_operation("app_api", &new, Some(&old))
                .unwrap_err()
                .to_string()
                .contains("removed option user")
        );
    }
    #[test]
    fn selected_secret_revision_preserves_other_consumers_and_old_reference() {
        let old = json!({"services":{"api":{"image":"api:old","secrets":["key"]},"worker":{"image":"worker:old","secrets":["key"]}},"secrets":{"key":{"external":true,"name":"key-old"}}});
        let new = json!({"services":{"api":{"image":"api:new","secrets":["key"]},"worker":{"image":"worker:unapplied","secrets":["key"]}},"secrets":{"key":{"external":true,"name":"key-new"}}});
        let merged = merge_selected_snapshot(&old, &new, &["api".into()], "app").unwrap();
        let api = adapter("app", &merged["services"]["api"], &merged).unwrap();
        let worker = adapter("app", &merged["services"]["worker"], &merged).unwrap();
        assert!(api.lists["secret"].contains_key("key-new"));
        assert!(worker.lists["secret"].contains_key("key-old"));
        assert_eq!(worker.image, "worker:old");
        assert_eq!(merged["secrets"]["dks-retained-key-old"]["name"], "key-old");
    }
    #[test]
    fn old_failed_task_of_same_image_does_not_poison_a_later_rollout() {
        let service = json!({"UpdatedAt":"2026-10-04T08:00:00Z"});
        let task = json!({"UpdatedAt":"2026-10-04T07:00:00Z","Spec":{"ContainerSpec":{"Image":"same@sha256:abc"}},"Status":{"State":"failed"}});
        assert!(rollout_failure(&service, &[task], "same@sha256:abc").is_none());
    }
    #[test]
    fn interrupted_selected_apply_reconciles_unrecorded_grants_from_live_spec() {
        let recorded = adapter(
            "app",
            &json!({"image":"api:old","environment":{"KEEP":"old"}}),
            &json!({}),
        )
        .unwrap();
        let desired = adapter(
            "app",
            &json!({"image":"api:new","environment":{"KEEP":"new"}}),
            &json!({}),
        )
        .unwrap();
        let live = json!({"Spec":{"Mode":{"Replicated":{"Replicas":1}},"TaskTemplate":{"ContainerSpec":{"Image":"api:interrupted","Env":["KEEP=interrupted","UNRECORDED=password"],"Secrets":[{"SecretName":"secret-interrupted","File":{"Name":"credential","UID":"0","GID":"0","Mode":292}}],"Configs":[{"ConfigName":"config-interrupted","File":{"Name":"/config","UID":"0","GID":"0","Mode":292}}],"Mounts":[{"Type":"bind","Source":"/private","Target":"/leaked","ReadOnly":true}]}}}});
        let old = live_options(&live, &BTreeMap::new(), &recorded, None).unwrap();
        let operations = selected_operation("immutable-service-id", &desired, Some(&old)).unwrap();
        for (flag, value) in [
            ("--env-rm", "UNRECORDED"),
            ("--secret-rm", "secret-interrupted"),
            ("--config-rm", "config-interrupted"),
            ("--mount-rm", "/leaked"),
        ] {
            assert!(
                operations
                    .windows(2)
                    .any(|pair| pair[0] == flag && pair[1] == value),
                "missing removal for {flag} {value}"
            );
        }
        assert_eq!(operations.last().unwrap(), "immutable-service-id");
    }
}
