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
                "Swarm secret {name} must be an external immutable provisioned reference; provision or sync its current binding first"
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
    let (_, digest) = pin.split_once('@').context("registry digest has no separator")?;
    Ok(format!("{}@{digest}", named_image(image)))
}
fn resolve_images(
    project: &Project,
    names: &[String],
    docker: &Docker,
    output: &Output,
) -> Result<BTreeMap<String, String>> {
    let mut images = BTreeMap::new();
    for name in names {
        let image = image_name(project, name)?;
        let pin = if project.services()?[name].get("build").is_some() {
            ensure!(!image.contains('@'),
                "service {name}: a build target cannot contain a digest; declare a registry repository/tag");
            ensure!(registry(repository(&image)).is_some(),
                "built service {name} needs an explicit registry repository, not {image}");
            let tag = named_image(&image);
            operation_run(docker, output, "build", &build_args(project, name, &tag)?)?;
            operation_run(docker, output, "publish", &args(&["push", &tag]))?;
            digest(docker, &tag)?
        } else {
            operation_run(docker, output, "pull", &args(&["pull", &image]))?;
            if image.contains('@') { image } else { digest(docker, &image)? }
        };
        images.insert(name.clone(), pin);
    }
    Ok(images)
}

fn named_image(image: &str) -> String {
    let image = image.split('@').next().unwrap_or(image);
    if image.rfind(':').is_some_and(|index| !image[index..].contains('/')) {
        image.into()
    } else {
        format!("{image}:latest")
    }
}

fn operation_run(docker: &Docker, output: &Output, phase: &str, argv: &[String]) -> Result<()> {
    output.event(phase, &format!("docker {}", argv.join(" ")))?;
    docker.run(argv, None)
        .with_context(|| format!("{phase} failed: docker {}", argv.join(" ")))
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
fn check_resources(
    project: &Project,
    rendered: &Value,
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
            let labels = &service["Spec"]["Labels"];
            ensure!(
                labels[OWNER].as_str() == Some(owner)
                    && labels[PROJECT].as_str() == Some(project.name()?),
                "refusing to modify foreign service {native}; observed owner {}",
                labels[OWNER]
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
                    labels.and_then(|l| l.get(OWNER)).and_then(Value::as_str) == Some(owner)
                        && labels.and_then(|l| l.get(PROJECT)).and_then(Value::as_str) == Some(project.name()?),
                    "refusing to modify foreign {command} {name}; observed owner {}",
                    labels.and_then(|l| l.get(OWNER)).unwrap_or(&Value::Null)
                );
            }
        }
    }
    Ok(())
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

fn validate_task_linkage(service: &Value, tasks: Option<&[Value]>) -> Result<()> {
    let id = service["ID"].as_str().context("service has no immutable ID")?;
    if let Some(tasks) = tasks {
        for task in tasks {
            ensure!(task["ServiceID"].as_str() == Some(id) && task["ID"].as_str().is_some(),
                "Swarm task immutable service linkage could not be verified");
        }
    }
    Ok(())
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
            ensure!(service["Spec"]["Labels"][OWNER].as_str() == Some(project.owner()?)
                && service["Spec"]["Labels"][PROJECT].as_str() == Some(project.name()?),
                "service {native} ownership changed during convergence; observed owner {}",
                service["Spec"]["Labels"][OWNER]);
            let id = service["ID"].as_str().context("service has no immutable ID")?;
            let tasks = tasks(docker, id)?;
            validate_task_linkage(&service, tasks.as_deref())?;
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
                bail!("{native}: {reason}; completed prerequisites/migrations are not rolled back; inspect Docker's live service state");
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
) -> Result<()> {
    runtime::execute_action_sequence(project, "deploy", names, actions, docker, output)
}
fn recheck_environment(project: &Project) -> Result<()> {
    let current = crate::sources::snapshot(&project.root, None)?.values;
    let fields = if current.get("project").is_none() || current.get("backend").is_none() {
        crate::nickel::schema_values(&project.root, &current)?
    } else { Vec::new() };
    let effective = |path: &str| current.get(path).or_else(|| fields.iter()
        .find(|field| field.path == path).and_then(|field| field.default.as_ref()));
    ensure!(
        effective("project").and_then(Value::as_str) == Some(project.name()?)
            && effective("backend").and_then(Value::as_str).unwrap_or("compose")
                == project.backend()?,
        "environment project/backend changed before lifecycle execution; re-evaluate the project before applying resources"
    );
    Ok(())
}

pub fn deploy(
    project: &Project,
    plan_only: bool,
    timeout: u64,
    output: &Output,
) -> Result<Value> {
    ensure!(project.backend()? == "swarm", "deploy requires backend=swarm");
    validate_model(project)?;
    crate::diagnostics::validate(project)?;
    let names = selection(project, &[])?;
    let before = deployment_actions(project, &names, "before")?;
    let after = deployment_actions(project, &names, "after")?;
    // Validate every authored build target before any command can build/publish.
    for name in &names {
        let image = image_name(project, name)?;
        if project.services()?[name].get("build").is_some() {
            ensure!(!image.contains('@'),
                "service {name}: a build target cannot contain a digest; declare a registry repository/tag");
            ensure!(registry(repository(&image)).is_some(),
                "service {name}: built images require an explicit registry repository");
            build_args(project, name, &named_image(&image))?;
        }
    }
    let _lock = if plan_only { None } else {
        let lock = state::lock(&project.root, "lifecycle")?;
        recheck_environment(project)?;
        Some(lock)
    };
    let docker = Docker::new(&project.root, output.clone())
        .with_deadline(Instant::now() + Duration::from_secs(timeout.max(1)));
    let preflight = (|| -> Result<(String, Value)> {
        Ok((docker.context()?, manager(&docker)?))
    })();
    let (context, info) = preflight.map_err(|error| {
        if plan_only { error }
        else { runtime::startup_diagnostics(error, project, &names, timeout, &docker, output) }
    })?;
    let owner = project.owner()?;
    runtime::validate_ownership(project, &docker)?;
    let mut rendered = render_owned(project, owner)?;
    for name in &names {
        let image = image_name(project, name)?;
        if info.pointer("/Swarm/Nodes").and_then(Value::as_u64).unwrap_or(1) > 1
            && let Some(host) = registry(repository(&image)) {
            let loopback = host == "localhost" || host.starts_with("localhost:")
                || host == "127.0.0.1" || host.starts_with("127.0.0.1:")
                || host.starts_with("[::1]") || host.starts_with("0.0.0.0");
            ensure!(!loopback,
                "service {name}: a multi-node Swarm cannot distribute images through a loopback-only registry");
        }
    }
    check_resources(project, &rendered, &names, &docker, owner)?;
    let mut image_plan = Map::new();
    let mut operations = Vec::new();
    for name in &names {
        let image = image_name(project, name)?;
        let build = project.services()?[name].get("build");
        image_plan.insert(name.clone(), json!({"source":image,"build":build,
            "registry":registry(repository(&image))}));
        if build.is_some() {
            let tag = named_image(&image);
            operations.push(json!({"phase":"build","argv":build_args(project,name,&tag)?}));
            operations.push(json!({"phase":"publish","argv":["push",tag]}));
        } else {
            operations.push(json!({"phase":"pull","argv":["pull",image]}));
        }
    }
    for action in &before {
        operations.push(json!({"phase":"prerequisite","name":action["name"],"projectArgv":action["argv"]}));
    }
    operations.push(json!({"phase":"apply","argv":["stack","deploy","--detach=true",
        "--with-registry-auth","--compose-file","<temporary-full-manifest>",project.name()?]}));
    operations.push(json!({"phase":"convergence","services":names,"timeoutSeconds":timeout}));
    for action in &after {
        operations.push(json!({"phase":"after","name":action["name"],"projectArgv":action["argv"]}));
    }
    if plan_only {
        return Ok(json!({"backend":"swarm","context":context,"project":project.name()?,
            "owner":owner,"services":names,"scope":"full stack, no pruning","images":image_plan,
            "prerequisites":{"before":before,"after":after},"secrets":rendered.get("secrets"),
            "sharedResources":{"networks":rendered.get("networks"),"volumes":rendered.get("volumes"),
                "configs":rendered.get("configs")},"dockerOperations":operations,"planOnly":true}));
    }
    output.event("target", &format!("project={} backend=swarm checkout={owner} Docker target={context}", project.name()?))?;
    let result = (|| -> Result<Value> {
        let images = resolve_images(project, &names, &docker, output)?;
        for name in &names {
            rendered["services"][name]["image"] = json!(images[name]);
        }
        let mut temporary_project = project.clone();
        temporary_project.model = rendered.clone();
        let manifest = runtime::write_render(&temporary_project, "swarm")?;
        output.event("prerequisites",
            "running only explicitly declared deploy prerequisites; Swarm does not infer depends_on")?;
        run_actions(project, &names, &before, &docker, output)?;
        runtime::validate_ownership(project, &docker)?;
        check_resources(project, &rendered, &names, &docker, owner)?;
        operation_run(&docker, output, "apply", &args(&["stack","deploy","--detach=true",
            "--with-registry-auth","--compose-file",
            manifest.path().to_str().context("temporary manifest path is not UTF-8")?,project.name()?]))?;
        let convergence = wait_convergence(project, &names, &rendered, &docker, timeout, output)?;
        run_actions(project, &names, &after, &docker, output)?;
        Ok(json!({"deployed":names,"context":context,"services":images,"convergence":convergence}))
    })();
    result.map_err(|error| runtime::startup_diagnostics(error, project, &names, timeout, &docker, output))
}

fn status_scope(project: &Project, selected: &[String]) -> Result<(Value, Vec<String>)> {
    Ok((project.swarm()?, selection(project, selected)?))
}

fn normalized_image(image: &str) -> String {
    let named = named_image(image);
    let repo = repository(&named);
    let tag = &named[repo.len()..];
    let hub_path = repo.strip_prefix("docker.io/")
        .or_else(|| repo.strip_prefix("index.docker.io/"))
        .or_else(|| repo.strip_prefix("registry-1.docker.io/"));
    let repo = if let Some(path) = hub_path {
        if path.contains('/') { format!("docker.io/{path}") }
        else { format!("docker.io/library/{path}") }
    } else if registry(repo).is_none() {
        if repo.contains('/') { format!("docker.io/{repo}") }
        else { format!("docker.io/library/{repo}") }
    } else { repo.to_owned() };
    format!("{repo}{tag}")
}

fn image_matches(authored: &str, live: &str) -> bool {
    if authored.contains('@') { authored == live }
    else { normalized_image(authored) == normalized_image(live) }
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
    ensure!(owner == project.owner()?, "Diagnostic checkout ownership differs from the project");
    let (rendered, names) = status_scope(project, selected)?;
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
                            if container["Id"].as_str() == Some(container_id)
                                && labels["com.docker.swarm.service.id"].as_str() == Some(service_id)
                                && labels["com.docker.swarm.task.id"].as_str() == Some(task_id)
                                && direct_owner {
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
        let (rendered, names) = status_scope(project, selected).context(crate::status::StatusConfiguration)?;
        let required: BTreeSet<_> = names.iter().cloned().collect();
        let mut visible: BTreeSet<_> = project.services()?.keys().cloned().collect();
        visible.extend(rendered["services"].as_object().context("missing services")?.keys().cloned());
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
        let owner = project.owner().context(crate::status::StatusConfiguration)?;
        if Instant::now() >= deadline { return Ok(()); }
        runtime::validate_ownership(project, &docker).map_err(status_configuration_error)?;
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
                    ensure!(service.pointer("/Spec/Labels").and_then(|v| v.get(OWNER)).and_then(Value::as_str) == Some(owner)
                        && service.pointer("/Spec/Labels").and_then(|v| v.get(PROJECT)).and_then(Value::as_str) == Some(project.name()?),
                        "ownership changed for service {native}");
                    Ok(())
                })().context(crate::status::StatusConfiguration)?;
                report["services"][index]["serviceId"] = service.get("ID").cloned().unwrap_or(Value::Null);
                let service_id = service["ID"].as_str().context("service has no immutable ID")?;
                let task_values = tasks(&docker, service_id)?;
                validate_task_linkage(&service, task_values.as_deref())?;
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
                let configured_mode = rendered["services"][name].pointer("/deploy/mode")
                    .and_then(Value::as_str).unwrap_or("replicated");
                let live_mode = if service.pointer("/Spec/Mode/Global").is_some() { "global" } else { "replicated" };
                let configured = if configured_mode == "replicated" {
                    Some(rendered["services"][name].pointer("/deploy/replicas").and_then(Value::as_u64).unwrap_or(1))
                } else { None };
                let expected = configured.unwrap_or(desired);
                // Swarm never infers completion from Compose depends_on.
                let oneshot = runtime::one_shot(project, name)?;
                let (mut container_ready, successful, mut failure) = swarm_container_status(&service, task_values.as_deref(), expected, oneshot);
                if desired != expected {
                    container_ready = false;
                    failure = Some(format!("desired replicas {desired} differ from configured replicas {expected}"));
                }
                if configured_mode != live_mode {
                    container_ready = false;
                    failure = Some("live service mode differs from current configuration".into());
                }
                if let Some(expected_image) = rendered["services"][name].get("image").and_then(Value::as_str)
                    && !service.pointer("/Spec/TaskTemplate/ContainerSpec/Image").and_then(Value::as_str)
                        .is_some_and(|live| image_matches(expected_image, live)) {
                    container_ready = false;
                    failure = Some("live service image differs from the authored reference".into());
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
        report["ready"] = json!(report["services"].as_array().context("missing rows")?.iter()
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
    if argv[0] != "volume" {
        ensure!(resource.get("ID").or_else(|| resource.get("Id")).and_then(Value::as_str) == Some(argv[2].as_str()),
            "resource immutable ID changed before deletion; refusing to remove {}", argv[2]);
    }
    Ok(true)
}

fn remove_teardown_resource(
    project: &Project,
    docker: &Docker,
    owner: &str,
    argv: &[String],
    volume_fingerprint: Option<&Value>,
    output: &Output,
) -> Result<()> {
    if !verify_teardown_resource(project, docker, owner, argv, volume_fingerprint)? {
        return Ok(());
    }
    if argv[0] == "service" {
        return operation_run(docker, output, "remove-service", argv);
    }
    // Service removal is asynchronous; wait before removing referenced resources.
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
    let owner = project.owner()?;
    runtime::validate_ownership(project, &docker)?;
    let services = owned_services(project, &docker, owner)?;
    let mut operations = Vec::<Vec<String>>::new();
    let mut volume_fingerprints = BTreeMap::new();
    let mut external = BTreeSet::new();
    for (kind, plural) in [("network", "networks"), ("volume", "volumes"), ("config", "configs")] {
        if let Some(resources) = project.model.get(plural).and_then(Value::as_object) {
            for (key, spec) in resources {
                if spec.get("external").and_then(Value::as_bool) == Some(true) {
                    external.insert((kind.to_owned(), resource_name(project.name()?, key, spec)));
                }
            }
        }
    }
    for name in &services {
        let service = inspect(&docker, "service", name)?;
        ensure!(
            service
                .pointer("/Spec/Labels")
                .and_then(|v| v.get(OWNER))
                .and_then(Value::as_str)
                == Some(owner)
                && service["Spec"]["Labels"][PROJECT].as_str() == Some(project.name()?),
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
            if external.contains(&(kind.to_owned(), name.to_owned())) { continue; }
            let item = inspect(&docker, kind, name)?;
            let labels = item.get("Labels").or_else(|| item.pointer("/Spec/Labels"));
            ensure!(
                labels.and_then(|v| v.get(OWNER)).and_then(Value::as_str) == Some(owner)
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
    output.event("target", &format!("project={} backend=swarm checkout={owner} Docker target={}", project.name()?, docker.context()?))?;
    for argv in operations {
        remove_teardown_resource(
            project,
            &docker,
            &owner,
            &argv,
            volume_fingerprints.get(&argv[2]),
            output,
        )?;
    }
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
            root: root.canonicalize().unwrap(),
            env: json!({"project":"race","backend":"swarm"}),
            model: json!({}),
            metadata: json!({}),
            fields: Vec::new(),
            swarm_secrets: BTreeMap::new(),
        };
        let resource = json!({"ID":"captured-id","Labels":{OWNER:project.owner().unwrap(),PROJECT:"race"},
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
            remove_teardown_resource(&project, &docker, project.owner().unwrap(), argv, fingerprint, &output)
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
            root: root.canonicalize().unwrap(),
            env: json!({"project":"race","backend":"swarm"}),
            model: json!({"services":{"api":{"image":"image@sha256:abc"}}}),
            metadata: json!({}),
            fields: Vec::new(),
            swarm_secrets: BTreeMap::new(),
        };
        let mut service = json!({"ID":"service-api","Spec":{"Labels":{OWNER:project.owner().unwrap(),PROJECT:"race"},"Mode":{"Replicated":{"Replicas":1}},
            "TaskTemplate":{"ContainerSpec":{"Image":"image@sha256:abc"}}}});
        let current = json!({"ID":"current","ServiceID":"service-api","Slot":1,"CreatedAt":"2026-10-05T01:00:00Z",
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
    fn old_failed_task_of_same_image_does_not_poison_a_later_rollout() {
        let service = json!({"UpdatedAt":"2026-10-04T08:00:00Z"});
        let task = json!({"UpdatedAt":"2026-10-04T07:00:00Z","Spec":{"ContainerSpec":{"Image":"same@sha256:abc"}},"Status":{"State":"failed"}});
        assert!(rollout_failure(&service, &[task], "same@sha256:abc").is_none());
    }
}
