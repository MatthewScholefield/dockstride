//! Compatibility-sensitive Nickel 0.19 adapter. Sources are never rewritten.
use crate::model::{Field, Project};
use anyhow::{Context, Result, anyhow, bail};
use nickel_lang_core::{
    cache::InputFormat,
    error::{
        Error,
        report::{ColorOpt, report_as_str},
    },
    eval::{
        cache::CacheImpl,
        value::{Container, NickelValue, ValueContentRef},
    },
    program::ProgramBuilder,
    term::{
        Term,
        record::{Field as NickelField, RecordData},
    },
    typ::TypeF,
};
use serde_json::{Value, json};
use std::{fs, io::Write, path::Path};

type Program = nickel_lang_core::program::Program<CacheImpl>;
pub const EVALUATOR_VERSION: &str = "0.19.0";
pub const LIBRARY_VERSION: &str = "0.2.0";

#[derive(Debug)]
pub struct NickelError {
    pub report: String,
}
impl std::fmt::Display for NickelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.report)
    }
}
impl std::error::Error for NickelError {}

fn diagnostic(program: &Program, error: Error) -> anyhow::Error {
    NickelError {
        report: report_as_str(&mut program.files(), error, ColorOpt::Never),
    }
    .into()
}

fn env_data(root: &Path, candidate: Option<&Value>) -> Result<Value> {
    Ok(crate::sources::snapshot(root, candidate)?.values)
}


fn program_values(root: &Path, env: &Value, expression: Option<&str>) -> Result<Program> {
    let root = fs::canonicalize(root).context("opening project directory")?;
    if !env.is_object() {
        bail!("env.yaml must contain a YAML mapping");
    }
    let mut selector = "__dockstride_result".to_owned();
    while env.get(&selector).is_some() {
        selector.push('_');
    }
    let expression = expression.unwrap_or("import \"compose.ncl\"");
    let wrapper = format!("{{ {selector} = ({expression}) }}");
    // ProgramBuilder registers *both* sources before resolving imports. The YAML
    // source's normalized absolute name matches the actual env.yaml import, and
    // SourceKind::Memory bypasses filesystem timestamps even when the file is absent.
    // The shallow merge is private to this wrapper; selecting its result field keeps
    // environment keys out of output and never forces unrelated service fields.
    let mut program: Program = ProgramBuilder::new()
        .add_source_string(wrapper, root.join(".dockstride-query.ncl"))
        .add_source_with_format(
            std::io::Cursor::new(serde_yaml::to_string(&env)?),
            root.join("env.yaml"),
            InputFormat::Yaml,
        )
        .build()?;
    program.field = program
        .parse_field_path(selector)
        .map_err(|error| diagnostic(&program, error.into()))?;
    Ok(program)
}

fn export(root: &Path, candidate: Option<&Value>, expression: &str) -> Result<Value> {
    let snapshot = crate::sources::snapshot(root, candidate)?;
    export_values(root, &snapshot.values, expression).with_context(|| format!("evaluating environment with shared sources {:?}", snapshot.sources))
}

fn export_values(root: &Path, values: &Value, expression: &str) -> Result<Value> {
    let mut program = program_values(root, values, Some(expression))?;
    let value = program
        .eval_full_for_export()
        .map_err(|error| diagnostic(&program, error))?;
    serde_json::to_value(value).context("serializing Nickel result")
}

fn field_expr(path: &str) -> Result<String> {
    if path.is_empty() || path.split('.').any(|part| part.is_empty()) {
        bail!("invalid field path {path:?}");
    }
    Ok(path
        .split('.')
        .map(|part| serde_json::to_string(part).unwrap())
        .collect::<Vec<_>>()
        .join("."))
}

fn record(value: &NickelValue) -> Option<&RecordData> {
    match value.content_ref() {
        ValueContentRef::Record(Container::Alloc(record)) => Some(record),
        _ => None,
    }
}

fn annotation_kind(field: &NickelField) -> (String, Vec<Value>) {
    for annotation in field.metadata.iter_annots() {
        match &annotation.typ.typ {
            TypeF::String => return ("string".into(), vec![]),
            TypeF::Number => return ("number".into(), vec![]),
            TypeF::Bool => return ("boolean".into(), vec![]),
            TypeF::Array(_) => return ("array".into(), vec![]),
            TypeF::Record(_) | TypeF::Dict { .. } => return ("record".into(), vec![]),
            TypeF::Contract(contract) => {
                // Inspect the parsed annotation, not text from compose.ncl. The named
                // library contracts are deliberately the supported reflection boundary.
                let name = contract.to_string();
                if name.ends_with(".Port") || name == "Port" {
                    return ("port".into(), vec![]);
                }
                if name.ends_with(".Backend") || name == "Backend" {
                    return ("choice".into(), vec![json!("compose"), json!("swarm")]);
                }
                if name.ends_with(".SecretSource") || name == "SecretSource" {
                    return ("secret".into(), vec![]);
                }
                if let ValueContentRef::Term(Term::App(app)) = contract.content_ref() {
                    let function = app.head.to_string();
                    if (function.ends_with(".Choice") || function == "Choice")
                        && let Ok(Value::Array(choices)) = serde_json::to_value(&app.arg)
                    {
                        return ("choice".into(), choices);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(value) = &field.value {
        match value.content_ref() {
            ValueContentRef::String(_) => return ("string".into(), vec![]),
            ValueContentRef::Number(_) => return ("number".into(), vec![]),
            ValueContentRef::Bool(_) => return ("boolean".into(), vec![]),
            ValueContentRef::Array(_) => return ("array".into(), vec![]),
            ValueContentRef::Record(_) => return ("record".into(), vec![]),
            _ => {}
        }
    }
    ("advanced".into(), vec![])
}

fn discover(data: &RecordData, prefix: &str, fields: &mut Vec<Field>) {
    for (name, field) in &data.fields {
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}.{name}")
        };
        let (kind, choices) = annotation_kind(field);
        if kind != "secret" {
            let contracts = field
                .pending_contracts
                .iter()
                .filter_map(|contract| record(&contract.contract));
            let value = field.value.as_ref().and_then(record);
            let mut nested = false;
            // A partial record value does not replace its record contracts. Discover
            // every contract first, then overlay the value, preserving absent required
            // leaves and annotations/docs on the leaves that the value supplies.
            for contract in contracts {
                discover(contract, &path, fields);
                nested = true;
            }
            if let Some(value) = value {
                discover(value, &path, fields);
                nested = true;
            }
            if nested {
                continue;
            }
        }
        let default = field
            .value
            .as_ref()
            .and_then(|value| serde_json::to_value(value).ok());
        let doc = field.metadata.doc().map(str::to_owned);
        if let Some(existing) = fields.iter_mut().find(|existing| existing.path == path) {
            if existing.kind == "advanced" {
                existing.kind = kind;
            }
            if !choices.is_empty() {
                existing.choices = choices;
            }
            if doc.is_some() {
                existing.doc = doc;
            }
            if field.value.is_some() {
                existing.required = false;
                if default.is_some() {
                    existing.default = default;
                }
            } else if existing.default.is_none() {
                existing.required |= !field.metadata.opt();
            }
        } else {
            fields.push(Field {
                path,
                kind,
                doc,
                required: field.value.is_none() && !field.metadata.opt(),
                default,
                choices,
            });
        }
    }
}

pub fn schema(root: &Path, candidate: Option<&Value>) -> Result<Vec<Field>> {
    let snapshot = crate::sources::snapshot(root, candidate)?;
    schema_values(root, &snapshot.values).with_context(|| format!("discovering configuration with shared sources {:?}", snapshot.sources))
}

pub(crate) fn schema_values(root: &Path, values: &Value) -> Result<Vec<Field>> {
    let mut program = program_values(root, values, None)?;
    program.field = program
        .parse_field_path(format!("{}.dockstride.Config", program.field))
        .map_err(|error| diagnostic(&program, error.into()))?;
    let spine = program
        .eval_record_spine()
        .map_err(|error| diagnostic(&program, error))?;
    let data =
        record(&spine).ok_or_else(|| anyhow!("dockstride.Config must be a record contract"))?;
    if data.fields.keys().any(|name| name.label() == "_dockstride") {
        bail!("dockstride.Config cannot define reserved _dockstride metadata");
    }
    let mut fields = Vec::new();
    discover(data, "", &mut fields);
    if fields.iter().any(|field| field.path == "_dockstride" || field.path.starts_with("_dockstride.")) {
        bail!("dockstride.Config cannot define reserved _dockstride metadata");
    }
    fields.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(fields)
}

pub fn setup_metadata(root: &Path, candidate: Option<&Value>) -> Result<Value> {
    export(
        root,
        candidate,
        "let p = import \"compose.ncl\" in {setup = if std.record.has_field \"setup\" p.dockstride then p.dockstride.setup else {}}",
    )
}

pub(crate) fn setup_metadata_values(root: &Path, values: &Value) -> Result<Value> {
    export_values(root, values,
        "let p = import \"compose.ncl\" in {setup = if std.record.has_field \"setup\" p.dockstride then p.dockstride.setup else {}}")
}

/// Discover allocation names without forcing policy values or unrelated setup.
pub(crate) fn allocation_fields_values(root: &Path, values: &Value) -> Result<Vec<String>> {
    let fields = export_values(root, values,
        "let p = import \"compose.ncl\" in if std.record.has_field \"setup\" p.dockstride && std.record.has_field \"ports\" p.dockstride.setup then std.record.fields p.dockstride.setup.ports else []")?;
    serde_json::from_value(fields).context("setup port fields must be strings")
}

/// Discover commands/defaults without forcing operational metadata or service inputs.
pub fn bootstrap_metadata(root: &Path, candidate: Option<&Value>) -> Result<Value> {
    export(root, candidate,
        "let p = import \"compose.ncl\" in let m = p.dockstride in {commands = if std.record.has_field \"commands\" m then m.commands else {}, setup = {defaults = if std.record.has_field \"setup\" m && std.record.has_field \"defaults\" m.setup then m.setup.defaults else null}}")
}

pub fn metadata(root: &Path, candidate: Option<&Value>) -> Result<Value> {
    // Operational metadata is evaluated only after complete input validation. Setup
    // callers use setup_metadata so missing action/readiness inputs stay unforced.
    let expression = "let p = import \"compose.ncl\" in let m = p.dockstride in let remove = fun k r => if std.record.has_field k r then std.record.remove k r else r in m |> remove \"Config\" |> remove \"canonical\"";
    export(root, candidate, expression)
}

pub fn validate_field(root: &Path, path: &str, value: &Value, candidate: &Value) -> Result<()> {
    let values = env_data(root, Some(candidate))?;
    validate_field_values(root, path, value, &values)
}

pub(crate) fn validate_field_values(root: &Path, path: &str, value: &Value, values: &Value) -> Result<()> {
    let selector = field_expr(path)?;
    let fields = schema_values(root, values)?;
    if !fields.iter().any(|field| field.path == path) {
        bail!("unknown configuration field {path}; inspect dks config schema");
    }
    // Applying the *whole* contract then selecting one leaf leaves unrelated required
    // inputs unevaluated, while all contracts on the selected value still run in Nickel.
    let expression = format!(
        "let p = import \"compose.ncl\" in let env | p.dockstride.Config = import \"env.yaml\" in env.{selector}"
    );
    let checked = export_values(root, values, &expression).with_context(|| {
        format!("invalid env.yaml field {path}; correct with dks config set {path} <value>")
    })?;
    // The explicit value protects callers accidentally validating a different candidate.
    if &checked != value {
        bail!("candidate field {path} differs from the value being validated");
    }
    Ok(())
}

pub fn evaluate(root: &Path, candidate: Option<&Value>) -> Result<Project> {
    let snapshot = crate::sources::snapshot(root, candidate)?;
    evaluate_values(root, &snapshot.values).with_context(|| format!("evaluating environment with shared sources {:?}", snapshot.sources))
}

pub(crate) fn evaluate_values(root: &Path, values: &Value) -> Result<Project> {
    let env = export_values(root, values, "let p = import \"compose.ncl\" in let env | p.dockstride.Config = import \"env.yaml\" in env")
        .context("validating complete env.yaml; run dks setup for missing inputs")?;
    // The contract-expanded environment is already effective: never resolve it as a
    // local document, which would lose inherited provenance and source declarations.
    let metadata = export_values(root, &env, "let p = import \"compose.ncl\" in let m = p.dockstride in let remove = fun k r => if std.record.has_field k r then std.record.remove k r else r in m |> remove \"Config\" |> remove \"canonical\"")?;
    if !metadata.is_object() {
        bail!("dockstride metadata must be a record");
    }
    let model = export_values(root, &env,
        "let p = import \"compose.ncl\" in if std.record.has_field \"canonical\" p.dockstride then p.dockstride.canonical else p",
    )?;
    let fields = schema_values(root, &env)?;
    Ok(Project {
        root: fs::canonicalize(root)?,
        env,
        model,
        metadata,
        fields,
    })
}

pub fn init(root: &Path) -> Result<Value> {
    fs::create_dir_all(root)?;
    let files = [
        ("compose.ncl", include_str!("../assets/compose.ncl")),
        (
            "libs/dockstride.ncl",
            include_str!("../assets/dockstride.ncl"),
        ),
    ];
    for (name, _) in &files {
        if root.join(name).exists() {
            bail!(
                "refusing to overwrite {}; use an empty project directory",
                root.join(name).display()
            );
        }
    }
    let mut created = Vec::new();
    for (name, content) in files {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap())?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        created.push(name);
    }
    let env = root.join("env.yaml");
    if !env.exists() {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(env)?;
        file.write_all(b"{}\n")?;
        file.sync_all()?;
        created.push("env.yaml");
    }
    let ignore = root.join(".gitignore");
    let ignore_exists = ignore.exists();
    let existing = if ignore_exists {
        fs::read_to_string(&ignore)?
    } else {
        String::new()
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ignore)?;
    let missing: Vec<_> = ["/env.yaml", "/.dockstride/"]
        .into_iter()
        .filter(|pattern| {
            !existing
                .lines()
                .any(|line| line == *pattern || line == &pattern[1..])
        })
        .collect();
    if !missing.is_empty() && !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    for pattern in missing {
        writeln!(file, "{pattern}")?;
    }
    file.sync_all()?;
    if !ignore_exists {
        created.push(".gitignore");
    }
    Ok(
        json!({"created":created,"evaluator":EVALUATOR_VERSION,"library":LIBRARY_VERSION,"next":"dks setup"}),
    )
}
