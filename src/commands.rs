//! Trusted, argv-only project processes. Capture is bounded and never invokes a shell.
use crate::{config, model::Field, nickel, output::Output, runtime::{self, Docker}, sources};
use anyhow::{Context, Result, ensure};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::{fmt, io::{Read, Write}, os::unix::process::CommandExt, path::Path, process::{Command, Stdio}, thread};

const OUTPUT_LIMIT: usize = 1_048_576;

#[derive(Debug)]
pub(crate) struct CapturedCommandFailed;
impl fmt::Display for CapturedCommandFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Project command failed")
    }
}
impl std::error::Error for CapturedCommandFailed {}

#[derive(Debug)]
pub(crate) struct InvalidCommandOutput;
impl fmt::Display for InvalidCommandOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid project command output")
    }
}
impl std::error::Error for InvalidCommandOutput {}

pub fn argv(value: &Value) -> Result<Vec<String>> {
    let args: Vec<String> = value.as_array().context("argv must be an array, never a shell string")?
        .iter().map(|v| v.as_str().map(str::to_owned).context("argv entries must be strings")).collect::<Result<_>>()?;
    ensure!(!args.is_empty() && !args[0].is_empty(), "Project command argv cannot be empty");
    ensure!(args.iter().all(|arg| !arg.contains('\0')), "Project command argv cannot contain NUL");
    Ok(args)
}

fn process(root: &Path, args: &[String], docker: &Docker) -> Result<Command> {
    ensure!(!args.is_empty(), "Project command argv cannot be empty");
    docker.remaining(std::time::Duration::from_secs(300))?;
    let mut command = Command::new(&args[0]);
    command.args(&args[1..]).current_dir(root).process_group(0);
    docker.inherit_connection(&mut command)?;
    docker.remaining(std::time::Duration::from_secs(300))?;
    Ok(command)
}

pub fn streamed(root: &Path, args: &[String], docker: &Docker, output: &Output, timeout: u64) -> Result<()> {
    let mut child = process(root, args, docker)?.stdin(Stdio::inherit()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().with_context(|| format!("Cannot start project command {}", args[0]))?;
    let out = runtime::streaming(child.stdout.take().unwrap(), output.clone(), "hook");
    let err = runtime::streaming(child.stderr.take().unwrap(), output.clone(), "hook");
    let result = docker.wait(&mut child, timeout);
    let _ = out.join();
    let _ = err.join();
    ensure!(result?.success(), "Project command {} failed", args[0]);
    Ok(())
}

pub fn captured(root: &Path, args: &[String], docker: &Docker, output: &Output, context: &Value, timeout: u64) -> Result<Value> {
    let input = serde_json::to_vec(context)?;
    let mut child = process(root, args, docker)?.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().with_context(|| format!("Cannot start project command {}", args[0]))?;
    let mut stdout = child.stdout.take().unwrap();
    let out = thread::spawn(move || -> std::io::Result<(Vec<u8>, bool)> {
        let mut bytes = Vec::new();
        let mut exceeded = false;
        let mut buffer = [0u8; 8192];
        loop {
            let count = stdout.read(&mut buffer)?;
            if count == 0 { break; }
            let accepted = count.min(OUTPUT_LIMIT.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&buffer[..accepted]);
            exceeded |= accepted < count;
        }
        Ok((bytes, exceeded))
    });
    let err = runtime::tail_bytes(child.stderr.take().unwrap());
    thread::scope(|scope| {
        let mut stdin = child.stdin.take().unwrap();
        let writer = scope.spawn(move || stdin.write_all(&input));
        let result = docker.wait(&mut child, timeout);
        let (bytes, exceeded) = out.join().map_err(|_| anyhow::anyhow!("Project output reader failed"))??;
        let stderr = err.join().map_err(|_| anyhow::anyhow!("Project stderr reader failed"))?;
        if !stderr.is_empty() { output.event("hook", &String::from_utf8_lossy(&stderr))?; }
        if !result?.success() { return Err(CapturedCommandFailed.into()); }
        writer.join().map_err(|_| anyhow::anyhow!("Project input writer failed"))??;
        (|| -> Result<Value> {
            ensure!(!exceeded, "Project command output exceeds 1 MiB");
            let mut parser = serde_json::Deserializer::from_slice(&bytes);
            let result = UniqueJson::deserialize(&mut parser).context("Project command must return one JSON object without duplicate keys")?.0;
            parser.end().context("Project command must return one JSON object")?;
            ensure!(result.is_object(), "Project command must return one JSON object");
            ensure!(result["schemaVersion"] == 1, "Unsupported project command schemaVersion");
            Ok(result)
        })().map_err(|error| error.context(InvalidCommandOutput))
    })
}

/// Validate before execution, including when a configured defaults hook is not needed.
pub fn declaration(metadata: &Value, name: &str) -> Result<(Vec<String>, u64)> {
    ensure!(!name.is_empty(), "Project command name cannot be empty");
    let declaration = metadata.get("commands").and_then(|v| v.get(name)).with_context(|| format!("Unknown project command {name}"))?;
    ensure!(declaration.is_object(), "Project command {name} must be a record");
    let args = argv(&declaration["argv"])?;
    let declared = match declaration.get("timeoutSeconds") {
        Some(value) => value.as_u64().context("timeoutSeconds must be an integer")?,
        None => 30,
    };
    ensure!((1..=300).contains(&declared), "Project command timeoutSeconds must be 1–300");
    Ok((args, declared))
}

pub fn named(root: &Path, metadata: &Value, name: &str, docker: &Docker, output: &Output, context: &Value, timeout: u64) -> Result<Value> {
    let (args, declared) = declaration(metadata, name)?;
    captured(root, &args, docker, output, context, if timeout == 0 { declared } else { declared.min(timeout) })
}

pub(crate) fn context_snapshot(root: &Path, purpose: &str, snapshot: &sources::EnvironmentSnapshot, fields: &[Field], missing: &[String]) -> Result<Value> {
    let mut settings = config::effective(&snapshot.values, fields)?;
    settings.as_object_mut().context("configuration must be a mapping")?.remove("secrets");
    let mut provenance = serde_json::Map::new();
    for (path, origin) in &snapshot.provenance {
        if path != "secrets" && !path.starts_with("secrets.") && path != "_dockstride" && !path.starts_with("_dockstride.") {
            provenance.insert(path.clone(), serde_json::to_value(origin)?);
        }
    }
    for field in fields {
        if field.path != "secrets" && !field.path.starts_with("secrets.") && !provenance.contains_key(&field.path) {
            provenance.insert(field.path.clone(), json!({"path":field.path,"origin":if field.default.is_some() {"default"} else {"missing"}}));
        }
    }
    let sources_declared = snapshot.local.get("_dockstride").and_then(|metadata| metadata.get("sources")).is_some();
    Ok(json!({"schemaVersion":1,"purpose":purpose,"checkout":root.canonicalize()?,"settings":settings,
        "provenance":provenance,"sources":snapshot.sources,"sourcesDeclared":sources_declared,"missingFields":missing,"projectProposal":config::project_proposal(root)?}))
}

pub fn context_for(root: &Path, local: &Value, purpose: &str, missing: &[String]) -> Result<Value> {
    let snapshot = sources::snapshot(root, Some(local))?;
    let fields = nickel::schema(root, Some(local))?;
    context_snapshot(root, purpose, &snapshot, &fields, missing)
}

pub fn context(root: &Path, purpose: &str) -> Result<Value> {
    crate::defaults::command_context(root, &config::read_env(root)?, purpose)
}

pub fn run(root: &Path, name: &str, timeout: u64, output: &Output) -> Result<Value> {
    let metadata = nickel::bootstrap_metadata(root, None)?;
    named(root, &metadata, name, &Docker::new(root, output.clone()), output, &context(root, "manual")?, timeout)
}

// serde_json::Value accepts duplicate keys by replacing earlier values. A command
// response is a proposal, so ambiguously conflicting writes must fail closed.
struct UniqueJson(Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = UniqueJson;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("JSON with unique object keys")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> std::result::Result<Self::Value, E> { Ok(UniqueJson(value.into())) }
            fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Self::Value, E> { Ok(UniqueJson(value.into())) }
            fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Self::Value, E> { Ok(UniqueJson(value.into())) }
            fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(value).map(|value| UniqueJson(value.into())).ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> { Ok(UniqueJson(value.into())) }
            fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<Self::Value, E> { Ok(UniqueJson(value.into())) }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> { Ok(UniqueJson(Value::Null)) }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> { Ok(UniqueJson(Value::Null)) }
            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<UniqueJson>()? { values.push(value.0); }
                Ok(UniqueJson(values.into()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, UniqueJson>()? {
                    match values.entry(key) {
                        serde_json::map::Entry::Vacant(entry) => { entry.insert(value.0); },
                        serde_json::map::Entry::Occupied(entry) => return Err(de::Error::custom(format!("duplicate JSON key {}", entry.key()))),
                    }
                }
                Ok(UniqueJson(values.into()))
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}
