use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Field {
    pub path: String,
    pub kind: String,
    pub doc: Option<String>,
    pub default: Option<Value>,
    pub required: bool,
    pub choices: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    pub env: Value,
    pub model: Value,
    pub metadata: Value,
    pub fields: Vec<Field>,
    pub swarm_secrets: BTreeMap<String, String>,
}

impl Project {
    /// Evaluation establishes the canonical absolute root once per project.
    pub fn owner(&self) -> Result<&str> {
        self.root.to_str().context("checkout path is not UTF-8; move the checkout to a UTF-8 path")
    }

    pub fn name(&self) -> Result<&str> {
        let name = self
            .env
            .get("project")
            .and_then(Value::as_str)
            .context("env.yaml: project must be a nonempty string")?;
        validate_project_name(name)?;
        Ok(name)
    }

    pub fn backend(&self) -> Result<&str> {
        match self
            .env
            .get("backend")
            .and_then(Value::as_str)
            .unwrap_or("compose")
        {
            "compose" => Ok("compose"),
            "swarm" => Ok("swarm"),
            value => bail!("env.yaml: unsupported backend {value:?}; expected compose or swarm"),
        }
    }

    pub fn services(&self) -> Result<&Map<String, Value>> {
        self.model
            .get("services")
            .and_then(Value::as_object)
            .context("compose.ncl: services must be a record")
    }

    pub fn compose(&self) -> Result<Value> {
        let mut model = self.model.clone();
        let record = model
            .as_object_mut()
            .context("compose.ncl must evaluate to a record")?;
        record.remove("dockstride");
        normalize_resources(record)?;
        for (name, service) in record
            .get("services")
            .and_then(Value::as_object)
            .context("services must be a record")?
        {
            if !service.is_object() {
                bail!("services.{name} must be a record");
            }
        }
        Ok(model)
    }

    pub fn swarm(&self) -> Result<Value> {
        let mut model = self.compose()?;
        let record = model.as_object_mut().unwrap();
        // Stack's legacy Compose loader requires a version; modern Compose does not.
        record.insert("version".into(), json!("3.8"));
        if record.contains_key("include") {
            bail!(
                "Compose include is not supported by the Swarm adapter; express included services in Nickel"
            );
        }
        record.remove("name");
        if record.contains_key("profiles") {
            bail!("top-level profiles cannot be rendered for Swarm");
        }
        for (name, service) in record
            .get_mut("services")
            .and_then(Value::as_object_mut)
            .unwrap()
        {
            let service = service.as_object_mut().unwrap();
            // These are development-only fields with an explicit, documented adapter.
            for field in [
                "build",
                "develop",
                "depends_on",
                "profiles",
                "container_name",
                "restart",
            ] {
                service.remove(field);
            }
            for field in [
                "links",
                "network_mode",
                "privileged",
                "devices",
                "pid",
                "ipc",
                "uts",
                "pull_policy",
            ] {
                if service.contains_key(field) {
                    bail!(
                        "services.{name}.{field} is not supported by the Swarm adapter; use Docker-native deploy fields or a Compose environment"
                    );
                }
            }
            if service.get("image").and_then(Value::as_str).is_none() {
                bail!("services.{name}.image is required for Swarm; Swarm does not build images");
            }
        }
        if let Some(secrets) = record.get_mut("secrets").and_then(Value::as_object_mut) {
            for (name, reference) in secrets {
                if reference.get("file").is_some() {
                    let binding = self.swarm_secrets.get(name).with_context(|| format!(
                        "Swarm secret {name} has no local binding; run dks setup or dks secrets sync {name} --yes"
                    ))?;
                    *reference = json!({"external":true,"name":binding});
                }
            }
        }
        Ok(model)
    }

    pub fn endpoints(&self) -> Value {
        self.metadata
            .get("endpoints")
            .cloned()
            .unwrap_or_else(|| json!({}))
    }
}

pub fn validate_project_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        bail!(
            "env.yaml: project must start with a lowercase letter/digit and contain only lowercase letters, digits, '-' or '_'"
        );
    }
    Ok(())
}

fn normalize_resources(record: &mut Map<String, Value>) -> Result<()> {
    for key in ["volumes", "networks"] {
        if let Some(Value::Array(names)) = record.get(key) {
            let mut resources = Map::new();
            for name in names {
                let name = name
                    .as_str()
                    .with_context(|| format!("{key} entries must be resource names"))?;
                if resources.insert(name.into(), json!({})).is_some() {
                    bail!("duplicate {key} resource {name}");
                }
            }
            record.insert(key.into(), Value::Object(resources));
        }
    }
    Ok(())
}
