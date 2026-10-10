use crate::{model::Field, nickel, state, sources, output::Output};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::collections::BTreeMap;
use sha2::{Digest, Sha256};

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

pub(crate) fn parse_document(text: &str) -> Result<Value> {
    let value: Value = serde_yaml::from_str(text).context("invalid env.yaml")?;
    if value.is_null() {
        return Ok(json!({}));
    }
    ensure!(
        value.is_object(),
        "env.yaml must contain a mapping of configuration fields"
    );
    check_secrets(&value)?;
    sources::descriptors(&value)?;
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

pub(crate) fn put(value: &mut Value, path: &str, replacement: Option<Value>) -> Result<()> {
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

pub(crate) fn effective(env: &Value, fields: &[Field]) -> Result<Value> {
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

pub(crate) fn check_secrets(value: &Value) -> Result<()> {
    if let Some(secrets) = value.get("secrets") {
        for (name, reference) in secrets
            .as_object()
            .context("secrets must be a mapping of Docker secret references")?
        {
            crate::secrets::valid_name(name)?;
            secret_reference(reference).with_context(|| format!("secrets.{name}"))?;
            crate::secrets::validate_reference_shape(reference)?;
        }
    }
    Ok(())
}

fn missing(root: &Path, env: &Value, fields: &[Field], include_secrets: bool) -> Result<Vec<Value>> {
    let values = effective(env, fields)?;
    let nullable_ports = if fields.iter().any(|field| field.required && at(env, &field.path).is_some_and(Value::is_null)) {
        crate::allocations::eligible_fields(root, env, fields)?
    } else { Vec::new() };
    Ok(fields.iter().filter(|field| field.required && (include_secrets || !is_secret(&field.path))
        && (at(&values, &field.path).is_none()
            || (nullable_ports.contains(&field.path) && at(env, &field.path).is_some_and(Value::is_null))))
        .map(|field| json!({"path":field.path,"kind":field.kind,"doc":field.doc,"choices":field.choices,"command":if is_secret(&field.path) {"dks setup".to_owned()} else {format!("dks config set {} <value>",field.path)}})).collect())
}

pub fn list(root: &Path) -> Result<Value> {
    let snapshot = sources::snapshot(root, None)?;
    let env = &snapshot.values;
    let fields = nickel::schema_values(root, env)?;
    let values = effective(env, &fields)?;
    let entries: Vec<_> = fields.iter().map(|field| {
        let explicit = at(env, &field.path);
        let value = at(&values, &field.path);
        json!({"path":field.path,"kind":field.kind,"value":value,"default":field.default,"doc":field.doc,"required":field.required,"choices":field.choices,
            "origin":if explicit.is_some(){origin(&snapshot,&field.path)}else if value.is_some(){"default".to_owned()}else{"missing".to_owned()},"provenance":snapshot.provenance.get(&field.path),"secret":is_secret(&field.path)})
    }).collect();
    Ok(json!({"fields":entries,"missing":missing(root,env,&fields,true)?}))
}

pub fn get(root: &Path, path: &str) -> Result<Value> {
    parts(path)?;
    let snapshot = sources::snapshot(root, None)?;
    let env = &snapshot.values;
    let fields = nickel::schema_values(root, env)?;
    let values = effective(env, &fields)?;
    ensure!(
        fields
            .iter()
            .any(|f| f.path == path || f.path.starts_with(&format!("{path}.")))
            || at(env, path).is_some(),
        "unknown configuration field {path}"
    );
    Ok(
        json!({"path":path,"value":at(&values,path),"origin":if at(env,path).is_some(){origin(&snapshot,path)}else if at(&values,path).is_some(){"default".to_owned()}else{"missing".to_owned()},"provenance":snapshot.provenance.get(path)}),
    )
}

fn origin(snapshot: &sources::EnvironmentSnapshot, path: &str) -> String {
    snapshot.provenance.get(path).map(|origin| {
        if origin.file == snapshot.local_file {
            "env.yaml".to_owned()
        } else { origin.file.display().to_string() }
    }).unwrap_or_else(|| "missing".to_owned())
}

fn validate_candidate(root: &Path, before: &Value, candidate: &Value, managed: bool) -> Result<Vec<Field>> {
    check_secrets(candidate)?;
    let old = sources::snapshot(root, Some(before))?;
    let new = sources::snapshot(root, Some(candidate))?;
    crate::secrets::validate_config_transition(root, before, candidate, &new.values, managed)?;
    validate_values(root, &old.values, &new.values)
}

fn validate_values(root: &Path, before: &Value, candidate: &Value) -> Result<Vec<Field>> {
    // A null policy field is an unassigned port, not an explicit contract value.
    // Validate it as absent until preparation materializes its ordinary YAML value.
    if candidate["backend"].as_str().unwrap_or("compose") == "compose" {
        let ports = nickel::allocation_fields_values(root, candidate)?;
        let nulls = ports.into_iter().filter(|path| at(candidate, path).is_some_and(Value::is_null)).collect::<Vec<_>>();
        if !nulls.is_empty() {
            let mut without_nulls = candidate.clone();
            for path in nulls { put(&mut without_nulls, &path, None)?; }
            return validate_values(root, before, &without_nulls);
        }
    }
    let fields = nickel::schema_values(root, candidate)?;
    let previous_fields = nickel::schema_values(root, before)?;
    let previous_values = effective(before, &previous_fields)?;
    let candidate_values = effective(candidate, &fields)?;
    ensure!(!fields.iter().any(|field| field.required
        && at(&previous_values, &field.path).is_some()
        && at(&candidate_values, &field.path).is_none()),
        "candidate removes an existing required input without a default; configuration was not changed");
    ensure!(
        !missing(root, before, &previous_fields, true)?.is_empty()
            || missing(root, candidate, &fields, true)?.is_empty(),
        "candidate removes required inputs from a complete environment; configuration was not changed"
    );
    for field in &fields {
        if let Some(value) = at(candidate, &field.path) {
            nickel::validate_field_values(root, &field.path, value, candidate)
                .with_context(|| format!("invalid configuration field {}", field.path))?;
        }
    }
    if missing(root, candidate, &fields, true)?.is_empty() {
        let mut snapshot = sources::snapshot(root, None)?;
        snapshot.values = candidate.clone();
        nickel::evaluate_snapshot(root, &snapshot)
            .context("candidate environment evaluation failed; configuration was not changed")?;
    }
    Ok(fields)
}

fn create_source(path: &Path) -> Result<()> {
    let parent = path.parent().context("source path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(b"{}\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("shared source {} appeared concurrently; rerun without replacing it", path.display()))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Preserve original observations after our separately committed empty-source creations.
fn verify_after_creations(snapshot: &sources::EnvironmentSnapshot, created: &[PathBuf]) -> Result<()> {
    let empty = Some(hex::encode(Sha256::digest(b"{}\n")));
    for (path, expected) in &snapshot.fingerprints {
        let expected = if expected.is_none() && created.contains(path) { &empty } else { expected };
        ensure!(&sources::fingerprint(path)? == expected, "configuration or shared sources changed; rerun with current settings");
    }
    Ok(())
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
    let mut sequence_indent = None;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let indent = content.len() - content.trim_start_matches(' ').len();
        let body = &content[indent..];
        let significant = !body.is_empty() && !body.starts_with('#');
        if scalar_indent.is_some_and(|parent| !significant || indent > parent) {
            offset += line.len();
            continue;
        }
        if sequence_indent.is_some_and(|parent| !significant || indent > parent || (indent == parent && body.starts_with('-'))) {
            offset += line.len();
            continue;
        }
        sequence_indent = None;
        // YAML permits indentless block sequences at the mapping value's indent.
        // Their rows and nested fields belong to that value, not sibling mappings.
        if body.starts_with("- ") || body == "-" {
            sequence_indent = Some(indent);
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
        let mapping = raw.is_empty() || raw.split_whitespace().all(|token| token.starts_with('&') || token.starts_with('!'));
        let block = mapping || raw.starts_with('|') || raw.starts_with('>');
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
        if mapping {
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

fn document_end(text: &str) -> usize {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.strip_prefix("...").is_some_and(|suffix| {
            suffix.is_empty() || suffix.starts_with(char::is_whitespace)
        }) {
            return offset;
        }
        offset += line.len();
    }
    text.len()
}

fn internal_last(text: String, candidate: &Value) -> String {
    let text = section_last(text, candidate, "_dockstride");
    section_last(text, candidate, "secrets")
}

fn section_last(text: String, candidate: &Value, path: &str) -> String {
    if candidate.get(path).is_none() {
        return text;
    }
    let Ok(nodes) = entries(&text) else {
        return text;
    };
    let Some(indent) = nodes.iter().map(|node| node.indent).min() else {
        return text;
    };
    let Some(section) = nodes.iter().find(|node| node.indent == indent && node.path == path) else {
        return text;
    };
    let insertion = if path == "secrets" {
        nodes.iter().find(|node| node.indent == indent && node.path == "_dockstride")
            .map_or_else(|| document_end(&text), |node| node.start)
    } else {
        document_end(&text)
    };
    if !nodes.iter().any(|node| node.indent == indent && node.start > section.start && node.start < insertion) {
        return text;
    }
    let mut end = section.end.min(insertion);
    for line in text[section.line_end..end].split_inclusive('\n').rev() {
        let body = line.trim();
        let line_indent = line.len() - line.trim_start_matches(' ').len();
        if body.is_empty() || (body.starts_with('#') && line_indent <= indent) {
            end -= line.len();
        } else {
            break;
        }
    }
    let mut reordered = text[..section.start].to_owned();
    reordered.push_str(&text[end..insertion]);
    if !reordered.is_empty() && !reordered.ends_with('\n') {
        reordered.push('\n');
    }
    reordered.push_str(&text[section.start..end]);
    if !reordered.ends_with('\n') {
        reordered.push('\n');
    }
    reordered.push_str(&text[insertion..]);
    if parse_document(&reordered).is_ok_and(|value| value == *candidate) {
        reordered
    } else {
        text
    }
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
        let end = document_end(text);
        for line in text[..end].lines() {
            if let Some(comment) = delimiter(line, '#') {
                edited.push_str(&line[comment..]);
                edited.push('\n');
            }
        }
        edited.push_str(&serde_yaml::to_string(candidate)?);
        edited.push_str(&text[end..]);
        ensure!(
            parse_document(&edited)? == *candidate,
            "root mapping edit could not represent {path}; use dks config edit"
        );
        return Ok(internal_last(edited, candidate));
    }
    let normalized = internal_last(text.to_owned(), before);
    let text = normalized.as_str();
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
            let end = document_end(&edited);
            let mut fragment = mapping_fragment(&keys, value.clone(), 0)?;
            if end > 0 && !edited[..end].ends_with('\n') {
                fragment.insert(0, '\n');
            }
            edited.insert_str(end, &fragment);
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
    Ok(internal_last(edited, candidate))
}

fn mutate(root: &Path, path: &str, replacement: Option<Value>, secret: bool) -> Result<Value> {
    parts(path)?;
    let _lifecycle = state::lock(root, "lifecycle")?;
    let _config = state::lock(root, "config")?;
    let snapshot = sources::snapshot(root, None)?;
    let _sources = sources::lock_paths(snapshot.fingerprints.keys().cloned())?;
    snapshot.verify()?;
    mutate_locked(root, path, replacement, secret)
}

/// Caller holds checkout lifecycle, config, then sorted source guards.
/// This helper acquires no guards and verifies source fingerprints before writing.
fn mutate_locked(root: &Path, path: &str, replacement: Option<Value>, secret: bool) -> Result<Value> {
    parts(path)?;
    ensure!(secret || (path != "_dockstride" && !path.starts_with("_dockstride.")), "reserved configuration path");
    let text = document(root)?;
    let before = parse_document(&text)?;
    let snapshot = sources::snapshot(root, Some(&before))?;
    snapshot.verify_text(&snapshot.local_file, &text)?;
    let schema = nickel::schema(root, Some(&before))?;
    let mut candidate = before.clone();
    put(&mut candidate, path, replacement.clone())?;
    let candidate_schema = nickel::schema(root, Some(&candidate))?;
    ensure!(schema.iter().chain(&candidate_schema).any(|field| field.path == path
        || field.path.starts_with(&format!("{path}."))
        || (is_secret(path) && path.starts_with(&format!("{}.", field.path))))
        || (secret && path.starts_with("secrets."))
        || (is_secret(path) && candidate.get("secrets").is_some()), "unknown configuration field {path}");
    if replacement.is_none() {
        let values = effective(&sources::snapshot(root, Some(&candidate))?.values, &candidate_schema)?;
        ensure!(
            !candidate_schema.iter().any(|f| f.required
                && (f.path == path || f.path.starts_with(&format!("{path}.")))
                && at(&values, &f.path).is_none()),
            "unsetting {path} would remove a required input without a default"
        );
    }
    let fields = validate_candidate(root, &before, &candidate, secret)?;
    let edited = edit_document(&text, &before, &candidate, path, replacement.as_ref())?;
    snapshot.verify()?;
    state::atomic_write(&root.join("env.yaml"), edited.as_bytes(), 0o600)?;
    let snapshot = sources::snapshot(root, Some(&candidate))?;
    let values = effective(&snapshot.values, &fields)?;
    Ok(
        json!({"path":path,"value":at(&values,path),"origin":if at(&snapshot.values,path).is_some(){origin(&snapshot,path)}else if at(&values,path).is_some(){"default".to_owned()}else{"missing".to_owned()},"provenance":snapshot.provenance.get(path),"missing":missing(root,&snapshot.values,&fields,true)?,"applied":false}),
    )
}

pub fn set(root: &Path, path: &str, value: Value) -> Result<Value> {
    mutate(root, path, Some(value), false)
}
pub fn unset(root: &Path, path: &str) -> Result<Value> {
    mutate(root, path, None, false)
}

/// Prepare a single validated document transition for an allocation batch.
pub(crate) fn prepare_sets_locked(root: &Path, updates: &[(String, Value)]) -> Result<String> {
    let text = document(root)?;
    let before = parse_document(&text)?;
    let snapshot = sources::snapshot(root, Some(&before))?;
    snapshot.verify_text(&snapshot.local_file, &text)?;
    let mut candidate = before.clone();
    let mut edited = text;
    for (path, value) in updates {
        parts(path)?;
        ensure!(!is_secret(path), "secret configuration is reference-only; use dks secrets replace or dks setup");
        let previous = candidate.clone();
        put(&mut candidate, path, Some(value.clone()))?;
        edited = edit_document(&edited, &previous, &candidate, path, Some(value))?;
    }
    let fields = validate_candidate(root, &before, &candidate, false)?;
    for (path, _) in updates {
        ensure!(fields.iter().any(|field| field.path == *path || field.path.starts_with(&format!("{path}."))), "unknown configuration field {path}");
    }
    snapshot.verify()?;
    Ok(internal_last(edited, &candidate))
}

/// Publish a checkout-local secret reference under caller-owned lifecycle/config/source guards.
pub(crate) fn set_secret_reference_locked(root: &Path, name: &str, reference: &Value) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.contains('.'),
        "secret name must be a single configuration key"
    );
    secret_reference(reference)?;
    mutate_locked(
        root,
        &format!("secrets.{name}"),
        Some(reference.clone()),
        true,
    )?;
    Ok(())
}

/// Publish a current Swarm binding, optionally with its new local source, in one document.
pub(crate) fn set_swarm_secret_binding_locked(root: &Path, name: &str, docker_name: &str, reference: Option<&Value>) -> Result<()> {
    crate::secrets::valid_name(name)?;
    crate::secrets::valid_name(docker_name)?;
    let text = document(root)?;
    let before = parse_document(&text)?;
    let snapshot = sources::snapshot(root, Some(&before))?;
    snapshot.verify_text(&snapshot.local_file, &text)?;
    let mut candidate = before.clone();
    let mut edited = text;
    if let Some(reference) = reference {
        secret_reference(reference)?;
        let previous = candidate.clone();
        let path = format!("secrets.{name}");
        put(&mut candidate, &path, Some(reference.clone()))?;
        edited = edit_document(&edited, &previous, &candidate, &path, Some(reference))?;
    }
    let previous = candidate.clone();
    let path = format!("_dockstride.swarmSecrets.{name}");
    let binding = json!(docker_name);
    put(&mut candidate, &path, Some(binding.clone()))?;
    edited = edit_document(&edited, &previous, &candidate, &path, Some(&binding))?;
    validate_candidate(root, &before, &candidate, true)?;
    snapshot.verify()?;
    state::atomic_write(&root.join("env.yaml"), edited.as_bytes(), 0o600)
}

pub fn edit(root: &Path) -> Result<Value> {
    state::prepare(root)?;
    let text = document(root)?;
    let before = parse_document(&text)?;
    let snapshot = sources::snapshot(root, Some(&before))?;
    snapshot.verify_text(&snapshot.local_file, &text)?;
    let editor = std::env::var("EDITOR")
        .context("EDITOR is not set; set it to an executable and optional arguments")?;
    let argv = shell_words::split(&editor).context("invalid EDITOR argument quoting")?;
    ensure!(!argv.is_empty(), "EDITOR must name an executable");
    state::prepare(root)?;
    let mut temporary = tempfile::Builder::new().prefix("edit-").suffix(".yaml").tempfile_in(root.join(".dockstride"))?;
    temporary.write_all(text.as_bytes())?;
    let result = (|| -> Result<Value> {
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .arg(temporary.path())
            .status()
            .context("launch EDITOR executable")?;
        ensure!(
            status.success(),
            "editor exited with {status}; env.yaml was not changed"
        );
        let edited = fs::read_to_string(temporary.path())?;
        let candidate = parse_document(&edited)?;
        let edited = internal_last(edited, &candidate);
        let after = sources::snapshot(root, Some(&candidate))?;
        let _lifecycle = state::lock(root, "lifecycle")?;
        let _config = state::lock(root, "config")?;
        let _sources = sources::lock_paths(snapshot.fingerprints.keys().chain(after.fingerprints.keys()).cloned())?;
        snapshot.verify()?;
        after.verify()?;
        let fields = validate_candidate(root, &before, &candidate, false)?;
        snapshot.verify()?;
        after.verify()?;
        state::atomic_write(&root.join("env.yaml"), edited.as_bytes(), 0o600)?;
        Ok(json!({"edited":true,"missing":missing(root,&sources::snapshot(root,Some(&candidate))?.values,&fields,true)?,"applied":false}))
    })();
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
    Ok(slug)
}

pub fn setup(root: &Path, inputs: &[String], non_interactive: bool) -> Result<Value> {
    setup_with_options(root, inputs, non_interactive, 30, &Output::default())
}

pub fn setup_with_options(root: &Path, inputs: &[String], non_interactive: bool, timeout: u64, output: &Output) -> Result<Value> {
    setup_with_context(root, inputs, non_interactive, timeout, output, "setup")
}

pub fn setup_with_context(root: &Path, inputs: &[String], non_interactive: bool, timeout: u64, output: &Output, purpose: &str) -> Result<Value> {
    state::prepare(root)?;
    let hook_candidate = setup_plan_candidate(root, inputs)?;
    let explicit = parse_inputs(inputs)?;
    // Trusted child execution runs without configuration/source guards.
    let proposal = crate::defaults::propose_for(root, &hook_candidate, purpose, timeout, output)?;
    let _lifecycle = state::lock(root, "lifecycle")?;
    let _lock = state::lock(root, "config")?;
    let original = document(root)?;
    let before = parse_document(&original)?;
    let mut candidate = before.clone();
    let mut edited = original.clone();
    for (path, value) in &explicit {
        let previous = candidate.clone();
        put(&mut candidate, path, Some(value.clone()))?;
        edited = edit_document(&edited, &previous, &candidate, path, Some(value))?;
    }
    // Re-read all live layers after the hook; accepted concurrent settings beat proposals.
    let explicit_candidate = candidate.clone();
    let mut overrides = BTreeMap::new();
    let mut creations = Vec::new();
    if sources::descriptors(&candidate)?.is_none() {
        if let Some(descriptors) = &proposal.sources {
            let mut selected = Vec::new();
            let declaring_directory = fs::canonicalize(root)?;
            for descriptor in descriptors {
                let path = sources::expand_home(&descriptor.path)?;
                let path = if path.is_absolute() { path } else { declaring_directory.join(path) };
                let path = sources::identity(&path)?;
                if !path.exists() {
                    ensure!(descriptor.create_if_missing, "proposed source {} does not exist", path.display());
                    overrides.insert(path.clone(), json!({}));
                    creations.push(path.clone());
                }
                selected.push(json!({"path":sources::relative_path(&declaring_directory, &path)}));
            }
            let previous = candidate.clone();
            let selected = Value::Array(selected);
            put(&mut candidate, "_dockstride.sources", Some(selected.clone()))?;
            edited = edit_document(&edited, &previous, &candidate, "_dockstride.sources", Some(&selected))?;
        }
    }
    let (refreshed, _sources) = loop {
        // Source creators or shared editors can race lock acquisition. Refresh
        // their settings, not the command. Canonical graph changes reacquire the
        // whole sorted lock set instead of introducing an out-of-order lock.
        overrides.retain(|path, _| !path.exists());
        let refreshed = sources::snapshot(root, Some(&explicit_candidate))?;
        let layered = sources::snapshot_with_overrides(root, Some(&candidate), &overrides)?;
        let locks = sources::lock_paths(refreshed.fingerprints.keys().chain(layered.fingerprints.keys()).cloned())?;
        if sources::unchanged(&refreshed)? && sources::unchanged(&layered)?
            && overrides.keys().all(|path| layered.fingerprints.get(path) == Some(&None)) {
            break (refreshed, locks);
        }
        drop(locks);
    };
    refreshed.verify_text(&refreshed.local_file, &original)?;
    creations.retain(|path| overrides.contains_key(path));
    apply_proposals(root, &mut candidate, &mut edited, &proposal.values, &overrides)?;
    let candidate_snapshot = sources::snapshot_with_overrides(root, Some(&candidate), &overrides)?;
    // Validate the complete proposed publication before creating any source file.
    validate_values(root, &refreshed.values, &candidate_snapshot.values)?;
    let publication_snapshot = candidate_snapshot;
    let fields = nickel::schema_values(root, &publication_snapshot.values)?;
    let allocated_fields = if purpose != "deploy" {
        crate::allocations::eligible_fields(root, &publication_snapshot.values, &fields)?
    } else { Vec::new() };
    let interactive = !non_interactive && io::stdin().is_terminal();
    if interactive {
        for field in &fields {
            if !field.required
                || is_secret(&field.path)
                || allocated_fields.contains(&field.path)
                || at(&effective(&sources::snapshot_with_overrides(root,Some(&candidate),&overrides)?.values, &fields)?, &field.path).is_some()
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
                let proposed = sources::snapshot_with_overrides(root, Some(&candidate), &overrides)?;
                match validate_values(root, &refreshed.values, &proposed.values) {
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
    let final_snapshot = sources::snapshot_with_overrides(root, Some(&candidate), &overrides)?;
    let fields = validate_values(root, &refreshed.values, &final_snapshot.values)?;
    publication_snapshot.verify()?;
    final_snapshot.verify()?;
    refreshed.verify()?;
    // Source creation is a separate single-document operation. There is no
    // multi-file transaction: an unused empty source can remain after a failure.
    for path in &creations {
        create_source(path)?;
    }
    let edited = if edited != original { internal_last(edited, &candidate) } else { edited };
    if edited != original {
        verify_after_creations(&refreshed, &creations)?;
        verify_after_creations(&final_snapshot, &creations)?;
        state::atomic_write(&root.join("env.yaml"), edited.as_bytes(), 0o600)?;
    }
    let values = final_snapshot.values;
    let mut missing = missing(root, &values, &fields, false)?;
    let deferred_ports = missing.iter().filter(|entry| entry["path"].as_str().is_some_and(|path| allocated_fields.iter().any(|field| field == path))).cloned().collect::<Vec<_>>();
    missing.retain(|entry| !entry["path"].as_str().is_some_and(|path| allocated_fields.iter().any(|field| field == path)));
    let secret_missing = self::missing(root, &values, &fields, true)?
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
        json!({"status":if deferred_ports.is_empty(){"configured"}else{"allocation-required"},"complete":secret_missing.is_empty() && deferred_ports.is_empty(),"missing":missing,"deferredPorts":deferred_ports,"secret_missing":secret_missing,"project_proposal":project_proposal(root)?,"changed":edited!=original}),
    )
}

fn parse_inputs(inputs: &[String]) -> Result<Vec<(String,Value)>> {
    inputs.iter().map(|input| {
        let (path, source) = input.split_once('=').with_context(|| format!("input {input:?} must be path=value"))?;
        ensure!(!is_secret(path), "use secret provisioning rather than plaintext setup inputs");
        let value: Value = serde_yaml::from_str(source).with_context(|| format!("invalid value for {path}"))?;
        ensure!(path != "_dockstride.sources" || value == json!([]), "setup source input only supports _dockstride.sources=[]; use dks config sources add");
        ensure!(!(path == "_dockstride" || path.starts_with("_dockstride.")) || path == "_dockstride.sources", "reserved configuration path {path}");
        Ok((path.to_owned(),value))
    }).collect()
}

pub fn setup_plan_candidate(root: &Path, inputs: &[String]) -> Result<Value> {
    let before = read_env(root)?;
    let mut candidate = before.clone();
    let explicit = parse_inputs(inputs)?;
    for (path,value) in &explicit { put(&mut candidate,path,Some(value.clone()))?; }
    let fields = nickel::schema(root,Some(&candidate))?;
    for (path,_) in &explicit {
        ensure!(path == "_dockstride.sources" || fields.iter().any(|field| field.path == *path || field.path.starts_with(&format!("{path}."))), "unknown configuration field {path}");
    }
    validate_candidate(root,&before,&candidate,false)?;
    Ok(candidate)
}

fn proposal_entries(value: &Value, prefix: &str, entries: &mut Vec<(String, Value)>) {
    if let Some(mapping) = value.as_object().filter(|mapping| !mapping.is_empty()) {
        for (key, value) in mapping {
            proposal_entries(value, &if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") }, entries);
        }
    } else if !prefix.is_empty() { entries.push((prefix.to_owned(), value.clone())); }
}

fn apply_proposals(root: &Path, candidate: &mut Value, edited: &mut String, values: &Value, overrides: &BTreeMap<PathBuf,Value>) -> Result<()> {
    let snapshot = sources::snapshot_with_overrides(root, Some(candidate), overrides)?;
    let fields = nickel::schema_values(root, &snapshot.values)?;
    let mut effective_values = effective(&snapshot.values, &fields)?;
    let mut entries = Vec::new();
    proposal_entries(values, "", &mut entries);
    for (path, value) in entries {
        // Explicit null is a value. Inherited and contract defaults are satisfied.
        if at(&effective_values, &path).is_some() { continue; }
        // A non-record ancestor is a replacement boundary, not a missing child.
        let parts = parts(&path)?;
        if (1..parts.len()).any(|end| at(&effective_values,&parts[..end].join(".")).is_some_and(|v| !v.is_object())) { continue; }
        let previous = candidate.clone();
        put(candidate, &path, Some(value.clone()))?;
        put(&mut effective_values, &path, Some(value.clone()))?;
        *edited = edit_document(edited, &previous, candidate, &path, Some(&value))?;
    }
    Ok(())
}

pub fn sources_list(root: &Path) -> Result<Value> {
    let snapshot = sources::snapshot(root, None)?;
    let direct = sources::descriptors(&snapshot.local)?.unwrap_or_default();
    let directory = snapshot.local_file.parent().unwrap();
    let sources = direct.iter().map(|path| {
        let expanded = sources::expand_home(path)?;
        let resolved = sources::identity(&if expanded.is_absolute() { expanded } else { directory.join(expanded) }).ok();
        Ok(json!({"path":path,"resolved":resolved}))
    }).collect::<Result<Vec<_>>>()?;
    Ok(json!({"declared":sources::descriptors(&snapshot.local)?.is_some(),"sources":sources,"resolved":snapshot.sources}))
}

pub fn sources_add(root: &Path, path: &Path, create: bool) -> Result<Value> {
    change_sources(root, path, Some(create))
}

pub fn sources_remove(root: &Path, path: &Path) -> Result<Value> {
    change_sources(root, path, None)
}

fn change_sources(root: &Path, path: &Path, add: Option<bool>) -> Result<Value> {
    let _lifecycle = state::lock(root,"lifecycle")?;
    let _config = state::lock(root,"config")?;
    let text = document(root)?;
    let before = parse_document(&text)?;
    let previous = sources::snapshot(root,Some(&before))?;
    previous.verify_text(&previous.local_file, &text)?;
    let directory = previous.local_file.parent().unwrap();
    let path = sources::expand_home(path)?;
    let target = sources::identity(&if path.is_absolute(){path}else{directory.join(path)})?;
    let mut selected = sources::descriptors(&before)?.unwrap_or_default();
    let matches = |path: &PathBuf| {
        let path = sources::expand_home(path)?;
        sources::identity(&if path.is_absolute(){path}else{directory.join(path)}).map(|path| path == target)
    };
    let mut overrides = BTreeMap::new();
    let mut create = false;
    if let Some(allow_create) = add {
        ensure!(!selected.iter().map(matches).collect::<Result<Vec<_>>>()?.contains(&true), "shared source {} is already selected", target.display());
        if !target.exists() {
            ensure!(allow_create,"shared source {} does not exist; use --create",target.display());
            overrides.insert(target.clone(),json!({}));
            create = true;
        }
        selected.push(sources::relative_path(directory, &target));
    } else {
        let mut retained = Vec::new();
        let mut removed = false;
        for path in selected { if matches(&path)? { removed = true; } else { retained.push(path); } }
        ensure!(removed,"{} is not a directly selected shared source",target.display());
        selected = retained;
    }
    let selection = json!(selected.into_iter().map(|path| json!({"path":path})).collect::<Vec<_>>());
    let mut candidate = before.clone();
    put(&mut candidate,"_dockstride.sources",Some(selection.clone()))?;
    let (previous, next, _locks) = loop {
        // A concurrently created source is inherited and validated normally.
        overrides.retain(|path, _| !path.exists());
        let previous = sources::snapshot(root, Some(&before))?;
        let next = sources::snapshot_with_overrides(root, Some(&candidate), &overrides)?;
        let locks = sources::lock_paths(previous.fingerprints.keys().chain(next.fingerprints.keys()).cloned())?;
        if sources::unchanged(&previous)? && sources::unchanged(&next)?
            && overrides.keys().all(|path| next.fingerprints.get(path) == Some(&None)) {
            break (previous, next, locks);
        }
        drop(locks);
    };
    previous.verify_text(&previous.local_file, &text)?;
    validate_values(root,&previous.values,&next.values)?;
    let edited = edit_document(&text,&before,&candidate,"_dockstride.sources",Some(&selection))?;
    previous.verify()?;
    next.verify()?;
    let created = if create && overrides.contains_key(&target) {
        create_source(&target)?;
        vec![target.clone()]
    } else { Vec::new() };
    verify_after_creations(&previous, &created)?;
    verify_after_creations(&next, &created)?;
    state::atomic_write(&root.join("env.yaml"),edited.as_bytes(),0o600)?;
    sources_list(root)
}

fn shared_target(root: &Path, local: &Value, source: Option<&Path>) -> Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let direct = sources::descriptors(local)?.unwrap_or_default().into_iter().map(|path| {
        let path = sources::expand_home(&path)?;
        sources::identity(&if path.is_absolute(){path}else{root.join(path)})
    }).collect::<Result<Vec<_>>>()?;
    if let Some(source) = source {
        let source = sources::expand_home(source)?;
        let source = sources::identity(&if source.is_absolute(){source}else{root.join(source)})?;
        ensure!(direct.contains(&source),"{} is not a directly selected source; transitive sources are not edit targets",source.display());
        Ok(source)
    } else {
        ensure!(direct.len() == 1,"shared edits require exactly one direct source or --source FILE");
        Ok(direct[0].clone())
    }
}

pub fn set_shared(root: &Path, path: &str, value: Value, source: Option<&Path>) -> Result<Value> {
    mutate_shared(root,path,Some(value),source)
}

pub fn unset_shared(root: &Path, path: &str, source: Option<&Path>) -> Result<Value> {
    mutate_shared(root,path,None,source)
}

fn mutate_shared(root: &Path, path: &str, replacement: Option<Value>, source: Option<&Path>) -> Result<Value> {
    parts(path)?;
    ensure!(path != "_dockstride" && !path.starts_with("_dockstride."),"reserved shared configuration path");
    let _lifecycle = state::lock(root,"lifecycle")?;
    let _config = state::lock(root,"config")?;
    let before = sources::snapshot(root,None)?;
    let target = shared_target(root,&before.local,source)?;
    let text = fs::read_to_string(&target)?;
    before.verify_text(&target, &text)?;
    let raw = sources::parse(&text,&target)?;
    let mut candidate = raw.clone();
    put(&mut candidate,path,replacement.clone())?;
    let after = sources::snapshot_with_overrides(root,Some(&before.local),&BTreeMap::from([(target.clone(),candidate.clone())]))?;
    let mut validation = after.values.clone();
    let mut overlay = candidate.clone();
    overlay.as_object_mut().unwrap().remove("_dockstride");
    sources::merge(&mut validation, &overlay);
    let fields = nickel::schema_values(root, &validation)?;
    let previous_fields = nickel::schema_values(root, &before.values)?;
    ensure!(fields.iter().chain(&previous_fields).any(|field| field.path == path
        || field.path.starts_with(&format!("{path}."))
        || (is_secret(path) && path.starts_with(&format!("{}.",field.path))))
        || (is_secret(path) && validation.get("secrets").is_some()),
        "unknown configuration field {path}");
    crate::secrets::validate_config_transition(root, &raw, &candidate, &validation, false)?;
    let _locks = sources::lock_paths(before.fingerprints.keys().chain(after.fingerprints.keys()).cloned())?;
    before.verify()?;
    after.verify()?;
    validate_values(root,&before.values,&after.values).with_context(|| format!("invalid shared candidate {}",target.display()))?;
    // Validate the edited field even if a local override hides it in this checkout.
    if let Some(value) = &replacement {
        let mut validation = after.values.clone();
        put(&mut validation,path,Some(value.clone()))?;
        let fields = nickel::schema_values(root,&validation)?;
        for field in fields.iter().filter(|field| field.path == path || field.path.starts_with(&format!("{path}."))) {
            if let Some(value) = at(&validation,&field.path) { nickel::validate_field_values(root,&field.path,value,&validation)?; }
        }
    }
    let edited = edit_document(&text,&raw,&candidate,path,replacement.as_ref())?;
    before.verify()?;
    after.verify()?;
    state::atomic_write(&target,edited.as_bytes(),0o600)?;
    let fields = nickel::schema_values(root, &after.values)?;
    let values = effective(&after.values, &fields)?;
    Ok(json!({"path":path,"value":at(&values,path),
        "origin":if at(&after.values,path).is_some(){origin(&after,path)}else if at(&values,path).is_some(){"default".to_owned()}else{"missing".to_owned()},
        "provenance":after.provenance.get(path),
        "editedSource":target,"applied":false}))
}

pub fn edit_shared(root: &Path, source: Option<&Path>) -> Result<Value> {
    state::prepare(root)?;
    let before = sources::snapshot(root,None)?;
    let target = shared_target(root,&before.local,source)?;
    let text = fs::read_to_string(&target)?;
    before.verify_text(&target, &text)?;
    let editor = std::env::var("EDITOR").context("EDITOR is not set; set it to an executable and optional arguments")?;
    let argv = shell_words::split(&editor).context("invalid EDITOR argument quoting")?;
    ensure!(!argv.is_empty(),"EDITOR must name an executable");
    state::prepare(root)?;
    let mut temporary = tempfile::Builder::new().prefix("edit-").suffix(".yaml").tempfile_in(root.join(".dockstride"))?;
    temporary.write_all(text.as_bytes())?;
    let result = (|| -> Result<Value> {
        let status = Command::new(&argv[0]).args(&argv[1..]).arg(temporary.path()).status().context("launch EDITOR executable")?;
        ensure!(status.success(),"editor exited with {status}; shared source was not changed");
        let edited = fs::read_to_string(temporary.path())?;
        let candidate = sources::parse(&edited,&target)?;
        let edited = internal_last(edited, &candidate);
        let after = sources::snapshot_with_overrides(root,Some(&before.local),&BTreeMap::from([(target.clone(),candidate.clone())]))?;
        let _lifecycle = state::lock(root, "lifecycle")?;
        let _config = state::lock(root, "config")?;
        let _locks = sources::lock_paths(before.fingerprints.keys().chain(after.fingerprints.keys()).cloned())?;
        before.verify()?;
        after.verify()?;
        let fields = validate_values(root,&before.values,&after.values).with_context(|| format!("invalid shared candidate {}",target.display()))?;
        let mut validation = after.values.clone();
        let mut overlay = candidate.clone();
        overlay.as_object_mut().unwrap().remove("_dockstride");
        sources::merge(&mut validation, &overlay);
        crate::secrets::validate_config_transition(root, &sources::parse(&text,&target)?, &candidate, &validation, false)?;
        validate_shared_fields(root,&after.values,&candidate)?;
        before.verify()?;
        after.verify()?;
        state::atomic_write(&target,edited.as_bytes(),0o600)?;
        Ok(json!({"edited":true,"editedSource":target,"missing":missing(root,&after.values,&fields,true)?,"applied":false}))
    })();
    result
}

fn validate_shared_fields(root: &Path, effective: &Value, shared: &Value) -> Result<()> {
    let mut shared = shared.clone();
    shared.as_object_mut().unwrap().remove("_dockstride");
    let mut values = effective.clone();
    sources::merge(&mut values,&shared);
    for field in nickel::schema_values(root,&values)? {
        if let Some(value) = at(&values,&field.path) {
            nickel::validate_field_values(root,&field.path,value,&values)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod source_creation_tests {
    use super::create_source;

    #[test]
    fn late_source_creation_preserves_the_concurrent_document() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.yaml");
        let concurrent = b"# another writer\nproject: concurrent\n";
        std::fs::write(&path, concurrent).unwrap();
        assert!(create_source(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), concurrent);
    }
}
