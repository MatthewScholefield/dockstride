//! Trusted read-only project diagnostics. Suggestions remain data, never actions.
use crate::{commands, model::Project, output::Output, runtime::{self, Docker}, sources};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};

#[derive(Debug)]
pub struct DiagnosticReport(pub Value);
impl std::fmt::Display for DiagnosticReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Project diagnostics attached")
    }
}
impl std::error::Error for DiagnosticReport {}

#[derive(Debug)]
pub struct DiagnosticConfiguration;
impl std::fmt::Display for DiagnosticConfiguration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid project diagnostic declaration")
    }
}
impl std::error::Error for DiagnosticConfiguration {}

struct Hook {
    name: String,
    command: String,
    services: Vec<String>,
    triggers: Vec<String>,
}

fn strings(value: &Value, field: &str) -> Result<Vec<String>> {
    let values = value.as_array().with_context(|| format!("diagnostics {field} must be an array"))?;
    ensure!(!values.is_empty(), "diagnostics {field} cannot be empty");
    let mut unique = BTreeSet::new();
    values.iter().map(|value| {
        let name = value.as_str().with_context(|| format!("diagnostics {field} entries must be strings"))?;
        ensure!(!name.trim().is_empty(), "diagnostics {field} entries cannot be empty");
        ensure!(unique.insert(name), "diagnostics {field} contains duplicate {name}");
        Ok(name.to_owned())
    }).collect()
}

fn declarations(project: &Project) -> Result<Vec<Hook>> {
    let Some(value) = project.metadata.get("diagnostics") else { return Ok(Vec::new()) };
    let declarations = value.as_object().context("dockstride.diagnostics must be a record")?;
    let services = project.services()?;
    declarations.iter().map(|(name, value)| {
        ensure!(!name.trim().is_empty(), "Diagnostic name cannot be empty");
        let declaration = value.as_object().with_context(|| format!("Diagnostic {name} must be a record"))?;
        ensure!(declaration.keys().all(|key| matches!(key.as_str(), "command" | "services" | "on")),
            "Diagnostic {name} permits only command, services, and on");
        let command = value["command"].as_str().context("Diagnostic command must name a project command")?;
        commands::declaration(&project.metadata, command)?;
        let selected = strings(&value["services"], "services")?;
        for service in &selected {
            ensure!(services.contains_key(service), "Diagnostic {name} selects unknown service {service}");
        }
        let triggers = strings(&value["on"], "on")?;
        ensure!(triggers.iter().all(|trigger| matches!(trigger.as_str(), "unhealthy" | "readiness-failed" | "startup-failed")),
            "Diagnostic {name} has an unsupported trigger");
        Ok(Hook { name: name.clone(), command: command.to_owned(), services: selected, triggers })
    }).collect()
}

/// Pure declaration validation: no command, Docker call, or write is performed.
pub fn validate(project: &Project) -> Result<()> {
    declarations(project).map(|_| ()).map_err(|error| error.context(DiagnosticConfiguration))
}

fn findings(mut value: Value) -> Result<Vec<Value>> {
    let record = value.as_object().context("Diagnostic output must be an object")?;
    ensure!(record.keys().all(|key| matches!(key.as_str(), "schemaVersion" | "findings")),
        "Diagnostic output permits only schemaVersion and findings");
    ensure!(value["schemaVersion"] == 1, "Unsupported diagnostic schemaVersion");
    let findings = value["findings"].as_array().context("Diagnostic findings must be an array")?;
    for finding in findings {
        let record = finding.as_object().context("Each diagnostic finding must be an object")?;
        ensure!(record.keys().all(|key| matches!(key.as_str(), "code" | "severity" | "summary" | "evidence" | "suggestedCommand" | "suggestedAction")),
            "Unsupported diagnostic finding field");
        for field in ["code", "summary"] {
            ensure!(finding[field].as_str().is_some_and(|text| !text.trim().is_empty()),
                "Diagnostic finding {field} must be a nonempty string");
        }
        ensure!(finding["severity"].as_str().is_some_and(|severity| matches!(severity, "info" | "warning" | "error")),
            "Diagnostic finding severity must be info, warning, or error");
        ensure!(record.contains_key("evidence") && !finding["evidence"].is_null(),
            "Diagnostic finding must include evidence");
        for field in ["suggestedCommand", "suggestedAction"] {
            if let Some(suggestion) = record.get(field) {
                ensure!(suggestion.is_string() || suggestion.is_array() || suggestion.is_object(),
                    "Diagnostic suggestions must be strings, arrays, or objects");
            }
        }
    }
    match value.as_object_mut().unwrap().remove("findings").unwrap() {
        Value::Array(findings) => Ok(findings),
        _ => unreachable!("findings array validated above"),
    }
}

fn error_kind(error: &anyhow::Error) -> &'static str {
    if error.is::<runtime::Cancelled>() { "cancelled" }
    else if error.is::<runtime::DeadlineExceeded>() { "deadline-exceeded" }
    else if error.is::<runtime::DockerError>() { "docker" }
    else if error.is::<runtime::PrerequisiteFailed>() { "prerequisite" }
    else if error.is::<DiagnosticConfiguration>() { "configuration" }
    else { "operation" }
}

fn failure(report: &mut Value, stage: &str, hook: Option<&Hook>, kind: &str, message: &str) {
    let mut value = json!({"stage":stage,"kind":kind,"message":message});
    if let Some(hook) = hook {
        value["hook"] = json!(hook.name);
        value["command"] = json!(hook.command);
    }
    report["failures"].as_array_mut().unwrap().push(value);
}

fn secret_references(project: &Project, selected: &[String]) -> Value {
    let mut references = serde_json::Map::new();
    for service in selected {
        let mut files = Vec::new();
        for mount in project.model["services"][service]["secrets"].as_array().into_iter().flatten() {
            let Some(source) = mount.as_str().or_else(|| mount["source"].as_str()) else { continue };
            let target = mount.get("target").and_then(Value::as_str).unwrap_or(source);
            let definition = &project.model["secrets"][source];
            let mut reference = json!({"source":source,"target":target});
            if let Some(file) = definition["file"].as_str() {
                reference["file"] = json!(file);
            } else if definition["external"] == true {
                reference["external"] = json!(true);
                reference["name"] = json!(definition["name"].as_str().unwrap_or(source));
            } else { continue }
            files.push(reference);
        }
        if !files.is_empty() { references.insert(service.clone(), json!(files)); }
    }
    Value::Object(references)
}

fn observed_services(observations: &Value, primary: Option<&anyhow::Error>) -> Vec<Value> {
    let mut rows = observations["services"].as_array().cloned().unwrap_or_default();
    if let Some(report) = primary.and_then(|error| error.downcast_ref::<crate::status::StatusReport>()) {
        for previous in report.0["services"].as_array().into_iter().flatten() {
            let Some(name) = previous["name"].as_str().or_else(|| previous["service"].as_str()) else { continue };
            if previous["applicationReady"] == false || previous["unhealthy"] == true {
                if !rows.iter().any(|row| row["service"].as_str() == Some(name)) {
                    rows.push(json!({"service":name,"observed":false,"verifiedContainerIds":[],"applicationReady":null}));
                }
                let row = rows.iter_mut().find(|row| row["service"].as_str() == Some(name)).unwrap();
                if previous["applicationReady"] == false { row["applicationReady"] = json!(false); }
                if previous["unhealthy"] == true { row["unhealthy"] = json!(true); }
            }
        }
    }
    rows
}

fn applicable(hook: &Hook, selected: &[String], trigger: &str, rows: &[Value]) -> Vec<String> {
    if !hook.services.iter().any(|service| selected.contains(service)) { return Vec::new() }
    if trigger == "doctor" { return vec!["doctor".into()] }
    hook.triggers.iter().filter(|event| match event.as_str() {
        "startup-failed" => trigger == "startup-failed",
        "unhealthy" => rows.iter().any(|row| hook.services.iter().any(|name| row["service"].as_str() == Some(name.as_str())) && row["unhealthy"] == true),
        "readiness-failed" => rows.iter().any(|row| hook.services.iter().any(|name| row["service"].as_str() == Some(name.as_str())) && row["applicationReady"] == false),
        _ => false,
    }).cloned().collect()
}

/// Run one bounded diagnostic phase. Its failures are data, never a replacement
/// for the original operation error. No hook means no context or Docker work.
pub fn run(project: &Project, selected: &[String], trigger: &str, timeout: u64,
    docker: &Docker, output: &Output, primary: Option<&anyhow::Error>) -> Value {
    let mut report = json!({"schemaVersion":1,"findings":[],"failures":[]});
    let hooks = match declarations(project) {
        Ok(hooks) => hooks,
        Err(_) => {
            failure(&mut report, "configuration", None, "configuration", "Invalid project diagnostic declaration");
            return report;
        }
    };
    if hooks.is_empty() { return report }
    if !matches!(trigger, "doctor" | "startup-failed") {
        failure(&mut report, "configuration", None, "configuration", "Unsupported diagnostic invocation trigger");
        return report;
    }
    let docker = docker.for_diagnostics(timeout);
    let budget = Duration::from_secs(10);
    let context = (|| -> Result<Value> {
        docker.remaining(budget)?;
        let snapshot = sources::snapshot(&project.root, None)?;
        let context = commands::context_snapshot(&project.root, trigger, &snapshot, &project.fields, &[])?;
        docker.remaining(budget)?;
        Ok(context)
    })();
    let mut context = match context {
        Ok(context) => context,
        Err(error) => {
            failure(&mut report, "context", None, error_kind(&error), "Could not construct non-secret diagnostic context");
            return report;
        }
    };
    let observations = match runtime::diagnostic_observations(project, selected, &docker) {
        Ok(value) => value,
        Err(error) => {
            failure(&mut report, "observations", None, error_kind(&error), "Could not obtain ownership-verified diagnostic observations");
            json!({"services":[]})
        }
    };
    let scope: Vec<String> = observations["services"].as_array().map(|rows| rows.iter()
        .filter_map(|row| row["service"].as_str().map(str::to_owned)).collect()).filter(|scope: &Vec<String>| !scope.is_empty())
        .unwrap_or_else(|| if selected.is_empty() { project.services().map(|services| services.keys().cloned().collect()).unwrap_or_default() } else { selected.to_vec() });
    let rows = observed_services(&observations, primary);
    let mut failed: Vec<_> = rows.iter().filter(|row| row["unhealthy"] == true || row["applicationReady"] == false
        || (row["observed"] == true && row["containerReady"] == false)).cloned().collect();
    if let Some(prerequisite) = primary.and_then(|error| error.downcast_ref::<runtime::PrerequisiteFailed>()) {
        if let Some(service) = prerequisite.0["service"].as_str() {
            if !failed.iter().any(|row| row["service"].as_str() == Some(service)) {
                failed.push(json!({"service":service,"prerequisiteFailed":true,"exitCode":prerequisite.0["exitCode"]}));
            }
        }
    }
    report["trigger"] = json!(trigger);
    report["observations"] = observations.clone();
    context["diagnostics"] = json!({"trigger":trigger,"services":scope,"observations":observations,
        "failedServices":failed,"secretReferences":secret_references(project, &scope),
        "primaryFailure":primary.map(|error| json!({"kind":error_kind(error)}))});
    for hook in hooks {
        let triggers = applicable(&hook, &scope, trigger, &rows);
        if triggers.is_empty() { continue }
        context["diagnostics"]["triggers"] = json!(triggers);
        context["diagnostics"]["hook"] = json!(hook.name);
        let result = docker.remaining(budget).and_then(|_| commands::named(&project.root, &project.metadata,
            &hook.command, &docker, output, &context, 10));
        match result {
            Ok(value) => match findings(value) {
                Ok(values) => report["findings"].as_array_mut().unwrap().extend(values),
                Err(_) => failure(&mut report, "hook", Some(&hook), "invalid-output", "Hook returned malformed diagnostic findings"),
            },
            Err(error) => {
                let kind = if error.is::<commands::CapturedCommandFailed>() { "command-failed" }
                    else if error.is::<commands::InvalidCommandOutput>() { "invalid-output" }
                    else if error.is::<runtime::DeadlineExceeded>() || error.is::<runtime::Cancelled>() { error_kind(&error) }
                    else { "execution-failed" };
                failure(&mut report, "hook", Some(&hook), kind, "Diagnostic hook did not complete with valid output");
            }
        }
    }
    report
}
