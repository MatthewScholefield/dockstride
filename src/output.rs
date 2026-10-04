use anyhow::Result;
use serde_json::{Value, json};
use std::io::{self, Write};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Default)]
pub struct Output {
    pub json: bool,
    pub quiet: bool,
}

impl Output {
    pub fn event(&self, phase: &str, message: &str) -> Result<()> {
        if self.json {
            let mut out = io::stdout().lock();
            serde_json::to_writer(
                &mut out,
                &json!({"schemaVersion":SCHEMA_VERSION,"type":"event","phase":phase,"message":message}),
            )?;
            writeln!(out)?;
            out.flush()?;
        } else if !self.quiet {
            writeln!(io::stderr().lock(), "{phase:<14} {message}")?;
        }
        Ok(())
    }

    pub fn result(&self, value: &Value) -> Result<()> {
        let mut out = io::stdout().lock();
        if self.json {
            serde_json::to_writer(
                &mut out,
                &json!({"schemaVersion":SCHEMA_VERSION,"type":"result","ok":true,"result":value}),
            )?;
        } else if let Some(text) = value.as_str() {
            write!(out, "{text}")?;
            if text.ends_with('\n') {
                return Ok(());
            }
        } else if render_human(&mut out, value)? {
            return Ok(());
        } else {
            serde_json::to_writer_pretty(&mut out, value)?;
        }
        writeln!(out)?;
        Ok(())
    }

    pub fn error(&self, category: &str, code: i32, message: &str) -> Result<()> {
        if self.json {
            let mut out = io::stdout().lock();
            serde_json::to_writer(
                &mut out,
                &json!({"schemaVersion":SCHEMA_VERSION,"type":"error","ok":false,"category":category,"exitCode":code,"message":message}),
            )?;
            writeln!(out)?;
        } else {
            writeln!(io::stderr().lock(), "Dockstride: {message}")?;
        }
        Ok(())
    }

    pub fn diagnostic(
        &self,
        category: &str,
        code: i32,
        message: &str,
        details: Value,
    ) -> Result<()> {
        if !self.json {
            return self.error(category, code, message);
        }
        let mut out = io::stdout().lock();
        serde_json::to_writer(
            &mut out,
            &json!({"schemaVersion":SCHEMA_VERSION,"type":"error","ok":false,"category":category,"exitCode":code,"message":message,"details":details}),
        )?;
        writeln!(out)?;
        Ok(())
    }
}

fn render_human(out: &mut impl Write, value: &Value) -> Result<bool> {
    if let Some(fields) = value.get("fields").and_then(Value::as_array) {
        writeln!(
            out,
            "{:<24} {:<22} {:<10} DESCRIPTION",
            "FIELD", "VALUE", "SOURCE"
        )?;
        for field in fields {
            let current = field
                .get("value")
                .or_else(|| field.get("default"))
                .unwrap_or(&Value::Null);
            let current = if current.is_null() {
                "<missing>".into()
            } else {
                display_value(current)
            };
            writeln!(
                out,
                "{:<24} {:<22} {:<10} {}",
                field["path"].as_str().unwrap_or("?"),
                current,
                field["origin"].as_str().unwrap_or("contract"),
                field["doc"].as_str().unwrap_or("")
            )?;
        }
        return Ok(true);
    }
    if let Some(created) = value.get("created") {
        writeln!(
            out,
            "Initialized · Nickel {} · library {}",
            display_value(&value["evaluator"]),
            display_value(&value["library"])
        )?;
        writeln!(out, "Created        {}", display_value(created))?;
        writeln!(out, "Next           {}", display_value(&value["next"]))?;
        return Ok(true);
    }
    if let Some(path) = value
        .get("path")
        .and_then(Value::as_str)
        .filter(|_| value.get("value").is_some())
    {
        writeln!(
            out,
            "{path:<24} {} · {}",
            display_value(&value["value"]),
            display_value(&value["origin"])
        )?;
        if value.get("applied").and_then(Value::as_bool) == Some(false) {
            writeln!(out, "Saved · containers unchanged")?;
        }
        if let Some(missing) = value
            .get("missing")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
        {
            writeln!(
                out,
                "Setup required: {}",
                missing
                    .iter()
                    .map(|field| field["path"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
            for field in missing {
                if let Some(command) = field["command"].as_str() {
                    writeln!(out, "  {command}")?;
                }
            }
        }
        return Ok(true);
    }
    if value.get("configured").and_then(Value::as_bool) == Some(true) {
        writeln!(out, "Configuration ready · no containers started")?;
        return render_human(out, &value["configuration"]);
    }
    if let Some(deployed) = value.get("deployed") {
        writeln!(out, "Deployed       {}", display_value(deployed))?;
        writeln!(out, "Context        {}", display_value(&value["context"]))?;
        writeln!(out, "Revision       {}", display_value(&value["revision"]))?;
        writeln!(out, "Snapshot       {}", display_value(&value["snapshot"]))?;
        writeln!(out, "Secrets        preserved")?;
        return Ok(true);
    }
    if value.get("removed").is_some() || value.get("removedServices").is_some() {
        let resources = value
            .get("removed")
            .or_else(|| value.get("removedServices"))
            .unwrap();
        writeln!(out, "Removed        {}", display_value(resources))?;
        let preserved = value
            .get("volumesPreserved")
            .or_else(|| value.get("dataRetained"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        writeln!(
            out,
            "Data           {}",
            if preserved {
                "preserved"
            } else {
                "owned volumes deleted"
            }
        )?;
        writeln!(out, "Secrets        preserved")?;
        return Ok(true);
    }
    if let Some(secrets) = value.get("secrets").and_then(Value::as_array) {
        writeln!(
            out,
            "{:<22} {:<12} {:<14} CONSUMERS / REVISION",
            "SECRET", "BACKEND", "STATUS"
        )?;
        for secret in secrets {
            let status =
                secret["status"]
                    .as_str()
                    .unwrap_or(if secret["present"].as_bool() == Some(true) {
                        "present"
                    } else {
                        "missing"
                    });
            writeln!(
                out,
                "{:<22} {:<12} {:<14} {} / {}",
                secret["name"].as_str().unwrap_or("?"),
                secret["backend"].as_str().unwrap_or("configured"),
                status,
                secret
                    .get("consumers")
                    .map(display_value)
                    .unwrap_or_default(),
                secret["revision"].as_str().unwrap_or("-")
            )?;
            if let Some(retained) = secret.get("retained").and_then(Value::as_array) {
                for revision in retained {
                    writeln!(
                        out,
                        "  retained     {}",
                        revision["revision"].as_str().unwrap_or("?")
                    )?;
                }
            }
        }
        return Ok(true);
    }
    if value.get("actions").is_some() && value.get("workflow").is_some() {
        writeln!(
            out,
            "Plan · {} · {}",
            value["project"].as_str().unwrap_or("?"),
            value["workflow"].as_str().unwrap_or("?")
        )?;
        serde_json::to_writer_pretty(&mut *out, value)?;
        writeln!(out)?;
        return Ok(true);
    }
    if let Some(services) = value
        .get("services")
        .and_then(Value::as_array)
        .filter(|items| items.iter().all(Value::is_object))
    {
        if let Some(project) = value.get("project").and_then(Value::as_str) {
            writeln!(out, "Project        {project}")?;
        }
        writeln!(out, "{:<22} {:<22} READINESS", "SERVICE", "STATUS")?;
        for service in services {
            let readiness = if service["applicationReady"].as_bool() == Some(true) {
                "application verified"
            } else if service["containerReady"].as_bool() == Some(true) {
                "container ready"
            } else {
                "not verified"
            };
            writeln!(
                out,
                "{:<22} {:<22} {readiness}",
                service["name"].as_str().unwrap_or("?"),
                service["status"].as_str().unwrap_or("unknown")
            )?;
        }
        if let Some(endpoints) = value.get("endpoints").and_then(Value::as_object) {
            writeln!(out)?;
            for (name, endpoint) in endpoints {
                writeln!(out, "{name:<14} {}", display_value(endpoint))?;
            }
        }
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            writeln!(out, "{message}")?;
        }
        return Ok(true);
    }
    Ok(false)
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(display_value)
            .collect::<Vec<_>>()
            .join(", "),
        _ => value.to_string(),
    }
}
