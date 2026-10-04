use crate::{model::Field, nickel, state};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

#[derive(Debug)]
pub struct MissingInputs {
    pub fields: Vec<Field>,
}
impl std::fmt::Display for MissingInputs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "missing required configuration inputs: {}",
            self.fields
                .iter()
                .map(|field| field.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
impl std::error::Error for MissingInputs {}

fn parse_document(text: &str) -> Result<Value> {
    let value: Value = serde_yaml::from_str(text).context("invalid env.yaml")?;
    if value.is_null() {
        return Ok(json!({}));
    }
    ensure!(
        value.is_object(),
        "env.yaml must contain a mapping of configuration fields"
    );
    check_secrets(&value)?;
    Ok(value)
}

fn document(root: &Path) -> Result<String> {
    match fs::read_to_string(root.join("env.yaml")) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn read_env(root: &Path) -> Result<Value> {
    parse_document(&document(root)?)
}

fn parts(path: &str) -> Result<Vec<&str>> {
    let parts: Vec<_> = path.split('.').collect();
    ensure!(
        !parts.is_empty()
            && parts
                .iter()
                .all(|p| !p.is_empty() && !p.contains(['\n', '\r', '\0'])),
        "invalid configuration path {path:?}"
    );
    Ok(parts)
}

fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |value, key| value.as_object()?.get(key))
}

fn put(value: &mut Value, path: &str, replacement: Option<Value>) -> Result<()> {
    let keys = parts(path)?;
    let mut object = value
        .as_object_mut()
        .context("configuration must be a mapping")?;
    for key in &keys[..keys.len() - 1] {
        if replacement.is_none() && !object.contains_key(*key) {
            return Ok(());
        }
        let child = object.entry((*key).to_owned()).or_insert_with(|| json!({}));
        object = child
            .as_object_mut()
            .with_context(|| format!("{key} is not a record"))?;
    }
    if let Some(replacement) = replacement {
        object.insert(keys[keys.len() - 1].to_owned(), replacement);
    } else {
        object.remove(keys[keys.len() - 1]);
    }
    Ok(())
}

fn effective(env: &Value, fields: &[Field]) -> Result<Value> {
    let mut value = env.clone();
    // Parents precede children so a nested default never hides an explicit child value.
    let mut defaults: Vec<_> = fields.iter().filter(|f| f.default.is_some()).collect();
    defaults.sort_by_key(|f| f.path.matches('.').count());
    for field in defaults {
        if at(&value, &field.path).is_none() {
            put(&mut value, &field.path, field.default.clone())?;
        }
    }
    Ok(value)
}

fn is_secret(path: &str) -> bool {
    path == "secrets" || path.starts_with("secrets.")
}

fn secret_reference(value: &Value) -> Result<()> {
    let reference = value
        .as_object()
        .context("secret contents are forbidden in env.yaml; use dks secrets replace")?;
    let file = reference.len() == 1
        && reference
            .get("file")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty());
    let external = reference.get("external") == Some(&json!(true))
        && reference
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        && reference
            .keys()
            .all(|key| key == "external" || key == "name");
    ensure!(
        file || external,
        "secret references must be {{file: path}} or {{external: true, name: object}}; use dks secrets replace for contents"
    );
    Ok(())
}

fn check_secrets(value: &Value) -> Result<()> {
    if let Some(secrets) = value.get("secrets") {
        for (name, reference) in secrets
            .as_object()
            .context("secrets must be a mapping of Docker secret references")?
        {
            secret_reference(reference).with_context(|| format!("secrets.{name}"))?;
        }
    }
    Ok(())
}

fn missing(env: &Value, fields: &[Field], include_secrets: bool) -> Result<Vec<Value>> {
    let values = effective(env, fields)?;
    Ok(fields.iter().filter(|field| field.required && (include_secrets || !is_secret(&field.path)) && at(&values, &field.path).is_none())
        .map(|field| json!({"path":field.path,"kind":field.kind,"doc":field.doc,"choices":field.choices,"command":if is_secret(&field.path) {"dks setup".to_owned()} else {format!("dks config set {} <value>",field.path)}})).collect())
}

pub fn list(root: &Path) -> Result<Value> {
    let env = read_env(root)?;
    let fields = nickel::schema(root, Some(&env))?;
    let values = effective(&env, &fields)?;
    let entries: Vec<_> = fields.iter().map(|field| {
        let explicit = at(&env, &field.path);
        let value = at(&values, &field.path);
        json!({"path":field.path,"kind":field.kind,"value":value,"default":field.default,"doc":field.doc,"required":field.required,"choices":field.choices,
            "origin":if explicit.is_some(){"env.yaml"}else if value.is_some(){"default"}else{"missing"},"secret":is_secret(&field.path)})
    }).collect();
    Ok(json!({"fields":entries,"missing":missing(&env,&fields,true)?}))
}

pub fn get(root: &Path, path: &str) -> Result<Value> {
    parts(path)?;
    let env = read_env(root)?;
    let fields = nickel::schema(root, Some(&env))?;
    let values = effective(&env, &fields)?;
    ensure!(
        fields
            .iter()
            .any(|f| f.path == path || f.path.starts_with(&format!("{path}.")))
            || at(&env, path).is_some(),
        "unknown configuration field {path}"
    );
    Ok(
        json!({"path":path,"value":at(&values,path),"origin":if at(&env,path).is_some(){"env.yaml"}else if at(&values,path).is_some(){"default"}else{"missing"}}),
    )
}

fn live_owned_resources(root: &Path, identity: &Value) -> Result<bool> {
    let Some(owner) = identity["id"].as_str() else {
        return Ok(false);
    };
    let mut namespaces = vec![
        vec!["ps", "--all"],
        vec!["volume", "ls"],
        vec!["network", "ls"],
    ];
    if identity["backend"].as_str() == Some("swarm") {
        namespaces.extend([
            vec!["service", "ls"],
            vec!["secret", "ls"],
            vec!["config", "ls"],
        ]);
    }
    for namespace in namespaces {
        let recorded = identity["context"].as_str().context(
            "recorded Docker connection is missing; restore ownership state before transitioning",
        )?;
        let mut args = namespace.into_iter().map(str::to_owned).collect::<Vec<_>>();
        args.extend([
            "--filter".to_owned(),
            format!("label=io.dockstride.owner={owner}"),
            "--quiet".to_owned(),
        ]);
        let result = crate::runtime::capture_pinned(root,recorded,&args)
            .context("cannot verify previous Docker resources before identity/backend transition; restore Docker access or explicitly tear down the previous environment")?;
        if !result.trim().is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn protect_identity(root: &Path, before: &Value, after: &Value, fields: &[Field]) -> Result<()> {
    let identity = state::read(root, "identity")?;
    let old = effective(before, fields)?;
    let new = effective(after, fields)?;
    let changed = ["project", "backend"]
        .into_iter()
        .filter(|field| old.get(*field) != new.get(*field))
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return Ok(());
    }
    let deployment = state::read(root, "deployment")?;
    let occupied = identity["resources"].as_bool() == Some(true)
        || (deployment.is_object()
            && deployment["removed"].as_bool() != Some(true)
            && deployment["active"].as_bool() != Some(false))
        || live_owned_resources(root, &identity)?;
    ensure!(
        !occupied,
        "cannot change {} while owned resources exist; explicitly tear down the previous environment before an identity/backend transition",
        changed.join(", ")
    );
    Ok(())
}

fn validate_candidate(root: &Path, before: &Value, candidate: &Value) -> Result<Vec<Field>> {
    check_secrets(candidate)?;
    let fields = nickel::schema(root, Some(candidate))?;
    let previous_fields = nickel::schema(root, Some(before))?;
    ensure!(
        !missing(before, &previous_fields, true)?.is_empty()
            || missing(candidate, &fields, true)?.is_empty(),
        "candidate removes required inputs from a complete environment; env.yaml was not changed"
    );
    protect_identity(root, before, candidate, &fields)?;
    // Nickel remains authoritative, even while unrelated required inputs are absent.
    for field in &fields {
        if let Some(value) = at(candidate, &field.path) {
            nickel::validate_field(root, &field.path, value, candidate)
                .with_context(|| format!("invalid configuration field {}", field.path))?;
        }
    }
    if missing(candidate, &fields, true)?.is_empty() {
        nickel::evaluate(root, Some(candidate))
            .context("candidate environment evaluation failed; env.yaml was not changed")?;
    }
    Ok(fields)
}

/// Block-mapping document coordinates, derived lexically rather than searching source text.
#[derive(Debug)]
struct Entry {
    path: String,
    start: usize,
    line_end: usize,
    end: usize,
    value_start: usize,
    value_end: usize,
    indent: usize,
    block: bool,
}

fn delimiter(text: &str, needle: char) -> Option<usize> {
    let mut quote = None;
    let mut escape = false;
    let mut depth = 0usize;
    for (offset, ch) in text.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if quote == Some('"') && ch == '\\' {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch == '[' || ch == '{' {
            depth += 1;
            continue;
        }
        if ch == ']' || ch == '}' {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth == 0
            && ch == needle
            && needle != '#'
            && (needle != ':'
                || text[offset + 1..]
                    .chars()
                    .next()
                    .is_none_or(|c| c.is_whitespace()))
        {
            return Some(offset);
        }
        if depth == 0
            && ch == '#'
            && (offset == 0
                || text[..offset]
                    .chars()
                    .last()
                    .is_some_and(char::is_whitespace))
        {
            return if needle == '#' { Some(offset) } else { None };
        }
    }
    None
}

fn entries(text: &str) -> Result<Vec<Entry>> {
    let mut result: Vec<Entry> = Vec::new();
    let mut parents: Vec<(usize, String)> = Vec::new();
    let mut offset = 0;
    let mut scalar_indent = None;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let indent = content.len() - content.trim_start_matches(' ').len();
        let body = &content[indent..];
        let significant = !body.is_empty() && !body.starts_with('#');
        if scalar_indent.is_some_and(|parent| !significant || indent > parent) {
            offset += line.len();
            continue;
        }
        scalar_indent = None;
        if !significant {
            offset += line.len();
            continue;
        }
        for entry in result.iter_mut().rev() {
            if entry.end == text.len() && offset > entry.start && indent <= entry.indent {
                entry.end = offset;
            }
        }
        while parents.last().is_some_and(|(parent, _)| *parent >= indent) {
            parents.pop();
        }
        if body.starts_with('-') || body == "---" || body == "..." {
            offset += line.len();
            continue;
        }
        let Some(colon) = delimiter(body, ':') else {
            offset += line.len();
            continue;
        };
        let key: Value = serde_yaml::from_str(body[..colon].trim())?;
        let Some(key) = key.as_str() else {
            offset += line.len();
            continue;
        };
        let path = if let Some((_, parent)) = parents.last() {
            format!("{parent}.{key}")
        } else {
            key.to_owned()
        };
        let suffix = &body[colon + 1..];
        let leading = suffix.len() - suffix.trim_start().len();
        let value_start = offset + indent + colon + 1 + leading;
        let value_part = &text[value_start..offset + content.len()];
        let comment = delimiter(value_part, '#').unwrap_or(value_part.len());
        let value_end = value_start + value_part[..comment].trim_end().len();
        let raw = &text[value_start..value_end];
        let block = raw.is_empty() || raw.starts_with('|') || raw.starts_with('>');
        if raw.starts_with('|') || raw.starts_with('>') {
            scalar_indent = Some(indent);
        }
        result.push(Entry {
            path: path.clone(),
            start: offset,
            line_end: offset + line.len(),
            end: text.len(),
            value_start,
            value_end,
            indent,
            block,
        });
        if raw.is_empty() {
            parents.push((indent, path));
        }
        offset += line.len();
    }
    Ok(result)
}

fn yaml_value(value: &Value) -> Result<String> {
    if value.is_array() || value.is_object() {
        Ok(serde_yaml::to_string(value)?.trim_end().to_owned())
    } else {
        Ok(serde_json::to_string(value)?)
    }
}

fn mapping_fragment(keys: &[&str], value: Value, indent: usize) -> Result<String> {
    let mut nested = value;
    for key in keys.iter().rev() {
        let mut map = Map::new();
        map.insert((*key).to_owned(), nested);
        nested = Value::Object(map);
    }
    let yaml = serde_yaml::to_string(&nested)?;
    Ok(yaml
        .lines()
        .map(|line| format!("{}{line}\n", " ".repeat(indent)))
        .collect())
}

fn edit_document(
    text: &str,
    before: &Value,
    candidate: &Value,
    path: &str,
    replacement: Option<&Value>,
) -> Result<String> {
    // A flow mapping represents the entire root, not a block into which keys
    // can be appended. Regenerate that affected record while retaining comments.
    if text
        .lines()
        .map(str::trim_start)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with('#')
                && !line.starts_with('%')
                && *line != "---"
                && *line != "..."
        })
        .is_some_and(|line| line.starts_with('{'))
    {
        let mut edited = String::new();
        for line in text.lines() {
            if let Some(comment) = delimiter(line, '#') {
                edited.push_str(&line[comment..]);
                edited.push('\n');
            }
        }
        edited.push_str(&serde_yaml::to_string(candidate)?);
        ensure!(
            parse_document(&edited)? == *candidate,
            "root mapping edit could not represent {path}; use dks config edit"
        );
        return Ok(edited);
    }
    let nodes = entries(text)?;
    let keys = parts(path)?;
    let mut edited = text.to_owned();
    if let Some(node) = nodes.iter().find(|entry| entry.path == path) {
        match replacement {
            None => {
                let end = if node.block { node.end } else { node.line_end };
                edited.replace_range(node.start..end, "");
            }
            Some(value)
                if (!value.is_array() && !value.is_object())
                    || value.as_array().is_some_and(Vec::is_empty)
                    || value.as_object().is_some_and(Map::is_empty) =>
            {
                if node.block {
                    edited.replace_range(node.line_end..node.end, "");
                }
                let mut scalar = yaml_value(value)?;
                if text[..node.value_start].ends_with(':') {
                    scalar.insert(0, ' ');
                }
                if text[node.value_end..node.line_end].starts_with('#') {
                    scalar.push(' ');
                }
                edited.replace_range(node.value_start..node.value_end, &scalar);
            }
            Some(value) => {
                let suffix = &text[node.value_end..node.line_end];
                let comment = suffix.trim_end_matches(['\r', '\n']);
                let nested = yaml_value(value)?
                    .lines()
                    .map(|line| format!("{}{line}\n", " ".repeat(node.indent + 2)))
                    .collect::<String>();
                let header = format!(
                    "{}{}\n{}",
                    &text[node.start..node.value_start],
                    comment,
                    nested
                );
                let end = if node.block { node.end } else { node.line_end };
                edited.replace_range(node.start..end, &header);
            }
        }
    } else if let Some(value) = replacement {
        let parent = nodes
            .iter()
            .filter(|entry| {
                path.starts_with(&format!("{}.", entry.path))
                    && entry.block
                    && at(before, &entry.path).is_some_and(Value::is_object)
            })
            .max_by_key(|entry| entry.path.len());
        if let Some(parent) = parent {
            let remainder: Vec<_> = path[parent.path.len() + 1..].split('.').collect();
            let mut fragment = mapping_fragment(&remainder, value.clone(), parent.indent + 2)?;
            if parent.end > 0 && !text[..parent.end].ends_with('\n') {
                fragment.insert(0, '\n');
            }
            edited.insert_str(parent.end, &fragment);
        } else if keys.len() == 1 || at(before, keys[0]).is_none() {
            if !edited.is_empty() && !edited.ends_with('\n') {
                edited.push('\n');
            }
            edited.push_str(&mapping_fragment(&keys, value.clone(), 0)?);
        } else {
            // Flow collections, aliases, and advanced YAML remain valid; replace only their nearest root record.
            let root = keys[0];
            if let Some(node) = nodes.iter().find(|node| node.path == root) {
                edited.replace_range(
                    node.start..if node.block { node.end } else { node.line_end },
                    &mapping_fragment(&[root], candidate[root].clone(), 0)?,
                );
            } else {
                bail!("unsupported YAML layout at {path}; use dks config edit");
            }
        }
    } else if at(before, path).is_some() {
        let root = keys[0];
        if let Some(node) = nodes.iter().find(|node| node.path == root) {
            edited.replace_range(
                node.start..if node.block { node.end } else { node.line_end },
                &mapping_fragment(&[root], candidate[root].clone(), 0)?,
            );
        } else {
            bail!("unsupported YAML layout at {path}; use dks config edit");
        }
    }
    // YAML's bare `parent:` means null, not the empty record left by removing its last child.
    for node in entries(&edited)?.into_iter().rev() {
        if node.value_start == node.value_end
            && at(candidate, &node.path)
                .and_then(Value::as_object)
                .is_some_and(Map::is_empty)
        {
            let mut empty = "{}".to_owned();
            if edited[..node.value_start].ends_with(':') {
                empty.insert(0, ' ');
            }
            if edited[node.value_end..node.line_end].starts_with('#') {
                empty.push(' ');
            }
            edited.insert_str(node.value_start, &empty);
        }
    }
    ensure!(
        parse_document(&edited)? == *candidate,
        "document-preserving edit could not represent {path}; use dks config edit"
    );
    Ok(edited)
}

fn mutate(root: &Path, path: &str, replacement: Option<Value>, secret: bool) -> Result<Value> {
    parts(path)?;
    ensure!(
        secret || !is_secret(path),
        "secret configuration is reference-only; use dks secrets replace or dks setup"
    );
    let _lifecycle = if path == "project" || path == "backend" {
        Some(state::lock(root, "lifecycle")?)
    } else {
        None
    };
    let _lock = state::lock(root, "config")?;
    let text = document(root)?;
    let before = parse_document(&text)?;
    let schema = nickel::schema(root, Some(&before))?;
    ensure!(
        schema
            .iter()
            .any(|f| f.path == path || f.path.starts_with(&format!("{path}.")))
            || (secret && path.starts_with("secrets.")),
        "unknown configuration field {path}"
    );
    let mut candidate = before.clone();
    put(&mut candidate, path, replacement.clone())?;
    if replacement.is_none() {
        let values = effective(&candidate, &schema)?;
        ensure!(
            !schema.iter().any(|f| f.required
                && (f.path == path || f.path.starts_with(&format!("{path}.")))
                && at(&values, &f.path).is_none()),
            "unsetting {path} would remove a required input without a default"
        );
    }
    let fields = validate_candidate(root, &before, &candidate)?;
    let edited = edit_document(&text, &before, &candidate, path, replacement.as_ref())?;
    state::atomic_write(&root.join("env.yaml"), edited.as_bytes(), 0o600)?;
    let values = effective(&candidate, &fields)?;
    Ok(
        json!({"path":path,"value":at(&values,path),"origin":if at(&candidate,path).is_some(){"env.yaml"}else if at(&values,path).is_some(){"default"}else{"missing"},"missing":missing(&candidate,&fields,true)?,"applied":false}),
    )
}

pub fn set(root: &Path, path: &str, value: Value) -> Result<Value> {
    mutate(root, path, Some(value), false)
}
pub fn unset(root: &Path, path: &str) -> Result<Value> {
    mutate(root, path, None, false)
}
pub fn set_secret_reference(root: &Path, name: &str, reference: &Value) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.contains('.'),
        "secret name must be a single configuration key"
    );
    secret_reference(reference)?;
    mutate(
        root,
        &format!("secrets.{name}"),
        Some(reference.clone()),
        true,
    )?;
    Ok(())
}

pub fn edit(root: &Path) -> Result<Value> {
    let _lifecycle = state::lock(root, "lifecycle")?;
    let _lock = state::lock(root, "config")?;
    let text = document(root)?;
    let before = parse_document(&text)?;
    let editor = std::env::var("EDITOR")
        .context("EDITOR is not set; set it to an executable and optional arguments")?;
    let argv = shell_words::split(&editor).context("invalid EDITOR argument quoting")?;
    ensure!(!argv.is_empty(), "EDITOR must name an executable");
    let temporary = root
        .join(".dockstride")
        .join(format!("edit-{}.yaml", state::random_id()?));
    state::atomic_write(&temporary, text.as_bytes(), 0o600)?;
    let result = (|| -> Result<Value> {
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .arg(&temporary)
            .status()
            .context("launch EDITOR executable")?;
        ensure!(
            status.success(),
            "editor exited with {status}; env.yaml was not changed"
        );
        let edited = fs::read_to_string(&temporary)?;
        let candidate = parse_document(&edited)?;
        let fields = validate_candidate(root, &before, &candidate)?;
        state::atomic_write(&root.join("env.yaml"), edited.as_bytes(), 0o600)?;
        Ok(json!({"edited":true,"missing":missing(&candidate,&fields,true)?,"applied":false}))
    })();
    let _ = fs::remove_file(temporary);
    result
}

pub fn project_proposal(root: &Path) -> Result<String> {
    let root = fs::canonicalize(root)?;
    let base = root
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("project")
        .to_lowercase();
    let mut slug = base
        .chars()
        .map(|ch| {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        slug = "project".to_owned();
    }
    slug.truncate(40);
    let digest = hex::encode(Sha256::digest(root.as_os_str().as_encoded_bytes()));
    Ok(format!("{slug}-{}", &digest[..8]))
}

pub fn setup(root: &Path, inputs: &[String], non_interactive: bool) -> Result<Value> {
    let _lifecycle = state::lock(root, "lifecycle")?;
    let _lock = state::lock(root, "config")?;
    let original = document(root)?;
    let before = parse_document(&original)?;
    let mut candidate = before.clone();
    let mut edited = original.clone();
    let fields = nickel::schema(root, Some(&before))?;
    for input in inputs {
        let (path, source) = input
            .split_once('=')
            .with_context(|| format!("input {input:?} must be path=value"))?;
        ensure!(
            !is_secret(path),
            "use secret provisioning rather than plaintext setup inputs"
        );
        ensure!(
            fields
                .iter()
                .any(|f| f.path == path || f.path.starts_with(&format!("{path}."))),
            "unknown configuration field {path}"
        );
        let value: Value =
            serde_yaml::from_str(source).with_context(|| format!("invalid value for {path}"))?;
        let previous = candidate.clone();
        put(&mut candidate, path, Some(value.clone()))?;
        validate_candidate(root, &before, &candidate)?;
        edited = edit_document(&edited, &previous, &candidate, path, Some(&value))?;
    }
    let interactive = !non_interactive && io::stdin().is_terminal();
    if interactive {
        for field in &fields {
            if !field.required
                || is_secret(&field.path)
                || at(&effective(&candidate, &fields)?, &field.path).is_some()
            {
                continue;
            }
            let proposal = if field.path == "project" {
                Some(project_proposal(root)?)
            } else {
                None
            };
            loop {
                eprint!(
                    "{} ({}){}{}: ",
                    field.path,
                    field.kind,
                    field
                        .doc
                        .as_ref()
                        .map(|d| format!(" — {d}"))
                        .unwrap_or_default(),
                    proposal
                        .as_ref()
                        .map(|p| format!(" [{p}]"))
                        .unwrap_or_default()
                );
                io::stderr().flush()?;
                let mut response = String::new();
                ensure!(
                    io::stdin().read_line(&mut response)? > 0,
                    "input ended before {} was supplied",
                    field.path
                );
                let response = response.trim();
                let value = if response.is_empty() {
                    if let Some(p) = &proposal {
                        json!(p)
                    } else {
                        eprintln!("A value is required.");
                        continue;
                    }
                } else if field.kind.eq_ignore_ascii_case("string") || field.path == "project" {
                    json!(response)
                } else {
                    match serde_yaml::from_str::<Value>(response) {
                        Ok(value) => value,
                        Err(error) => {
                            eprintln!("{error}");
                            continue;
                        }
                    }
                };
                let previous = candidate.clone();
                put(&mut candidate, &field.path, Some(value.clone()))?;
                match validate_candidate(root, &before, &candidate) {
                    Ok(_) => {
                        edited = edit_document(
                            &edited,
                            &previous,
                            &candidate,
                            &field.path,
                            Some(&value),
                        )?;
                        break;
                    }
                    Err(error) => {
                        candidate = previous;
                        eprintln!("{error:#}");
                    }
                }
            }
        }
    }
    let fields = validate_candidate(root, &before, &candidate)?;
    if edited != original {
        state::atomic_write(&root.join("env.yaml"), edited.as_bytes(), 0o600)?;
    }
    let missing = missing(&candidate, &fields, false)?;
    let secret_missing = self::missing(&candidate, &fields, true)?
        .into_iter()
        .filter(|entry| entry["path"].as_str().is_some_and(is_secret))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        let paths = missing
            .iter()
            .filter_map(|entry| entry["path"].as_str())
            .collect::<Vec<_>>();
        return Err(MissingInputs {
            fields: fields
                .iter()
                .filter(|field| paths.contains(&field.path.as_str()))
                .cloned()
                .collect(),
        }
        .into());
    }
    Ok(
        json!({"status":if missing.is_empty(){"configured"}else{"missing"},"complete":missing.is_empty() && secret_missing.is_empty(),"missing":missing,"secret_missing":secret_missing,"project_proposal":project_proposal(root)?,"changed":edited!=original}),
    )
}
