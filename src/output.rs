use anyhow::Result;
use serde_json::{Value, json};
use std::io::{self, Write};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    Interactive,
    #[default]
    NonInteractive,
}

impl OutputMode {
    pub fn select(
        forced: bool,
        non_interactive: bool,
        json: bool,
        terminals: [bool; 3],
        capable: bool,
    ) -> Self {
        if !non_interactive
            && !json
            && (forced || (terminals.into_iter().all(|terminal| terminal) && capable))
        {
            Self::Interactive
        } else {
            Self::NonInteractive
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Output {
    pub json: bool,
    pub quiet: bool,
    pub mode: OutputMode,
    pub no_color: bool,
    pub verbose: bool,
    pub width: Option<u16>,
}

impl Output {
    fn style(&self) -> console::Style {
        console::Style::new().force_styling(self.mode == OutputMode::Interactive && !self.no_color)
    }

    fn heading(&self, text: &str) -> String {
        self.style()
            .cyan()
            .bold()
            .apply_to(console::strip_ansi_codes(text))
            .to_string()
    }

    pub fn verbose_event(&self, phase: &str, message: &str) -> Result<()> {
        if self.json || self.verbose {
            self.event(phase, message)?;
        }
        Ok(())
    }

    pub fn event(&self, phase: &str, message: &str) -> Result<()> {
        if self.quiet {
            return Ok(());
        }
        if self.json {
            let mut out = io::stdout().lock();
            serde_json::to_writer(
                &mut out,
                &json!({"schemaVersion":SCHEMA_VERSION,"type":"event","phase":phase,"message":message}),
            )?;
            writeln!(out)?;
            out.flush()?;
        } else {
            writeln!(
                io::stderr().lock(),
                "{} {}",
                self.style()
                    .dim()
                    .apply_to(console::strip_ansi_codes(phase)),
                console::strip_ansi_codes(message)
            )?;
        }
        Ok(())
    }

    pub fn step<T>(&self, message: &str, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let start = std::time::Instant::now();
        let result = action();
        let elapsed = start.elapsed();
        if !self.quiet {
            if self.json {
                let mut out = io::stdout().lock();
                serde_json::to_writer(
                    &mut out,
                    &json!({"schemaVersion":SCHEMA_VERSION,"type":"event",
                    "phase":"setup","message":message,"status":if result.is_ok() {"completed"} else {"failed"},
                    "elapsedMs":elapsed.as_secs_f64() * 1000.0}),
                )?;
                writeln!(out)?;
                out.flush()?;
            } else {
                let label = format!("{} {message}", self.step_label(result.is_ok()));
                let timing = if elapsed.as_secs_f64() < 1.0 {
                    format!("({:.0}ms)", elapsed.as_secs_f64() * 1000.0)
                } else {
                    format!("({:.2}s)", elapsed.as_secs_f64())
                };
                let padding = if self.mode == OutputMode::Interactive {
                    let width = self.width.unwrap_or_else(|| console::Term::stderr().size().1).min(100);
                    usize::from(width).saturating_sub(console::measure_text_width(&label) + timing.len()).max(1)
                } else {
                    1
                };
                writeln!(io::stderr().lock(), "{label}{:padding$}{}", "", self.style().dim().apply_to(timing))?;
            }
        }
        result
    }

    fn step_label(&self, success: bool) -> String {
        if self.mode == OutputMode::Interactive {
            if success {
                self.style().green().apply_to("✓").to_string()
            } else {
                self.style().red().apply_to("Failed:").to_string()
            }
        } else if success {
            "Completed:".into()
        } else {
            "Failed:".into()
        }
    }

    pub fn result(&self, value: &Value) -> Result<()> {
        self.write_result(&mut io::stdout().lock(), value)
    }

    fn write_result(&self, out: &mut impl Write, value: &Value) -> Result<()> {
        if self.json {
            serde_json::to_writer(
                &mut *out,
                &json!({"schemaVersion":SCHEMA_VERSION,"type":"result","ok":true,"result":value}),
            )?;
            writeln!(out)?;
        } else if let Some(text) = value.as_str() {
            write!(out, "{text}")?;
        } else {
            let mut rendered = Vec::new();
            if !render_human(self, &mut rendered, value)? {
                serde_json::to_writer_pretty(&mut rendered, value)?;
                writeln!(rendered)?;
            }
            if self.mode == OutputMode::NonInteractive || self.no_color {
                let text = String::from_utf8(rendered)?;
                write!(out, "{}", console::strip_ansi_codes(&text))?;
            } else {
                out.write_all(&rendered)?;
            }
        }
        Ok(())
    }

    fn table(&self, out: &mut impl Write, headers: &[&str], rows: Vec<Vec<String>>) -> Result<()> {
        let clean = |text: &str| console::strip_ansi_codes(text).into_owned();
        let width = self
            .width
            .unwrap_or_else(|| console::Term::stdout().size().1);
        let columns = rows.first().map_or(headers.len(), Vec::len);
        if self.mode == OutputMode::NonInteractive || usize::from(width) < columns * 4 + 1 {
            let cell = |text: &str| {
                clean(text)
                    .replace('\t', "\\t")
                    .replace('\r', "\\r")
                    .replace('\n', "\\n")
            };
            if !headers.is_empty() {
                writeln!(
                    out,
                    "{}",
                    headers
                        .iter()
                        .map(|text| cell(text))
                        .collect::<Vec<_>>()
                        .join("\t")
                )?;
            }
            for row in rows {
                writeln!(
                    out,
                    "{}",
                    row.iter()
                        .map(|text| cell(text))
                        .collect::<Vec<_>>()
                        .join("\t")
                )?;
            }
        } else {
            let mut table = comfy_table::Table::new();
            table
                .load_preset(comfy_table::presets::UTF8_BORDERS_ONLY)
                .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
                .set_content_arrangement(comfy_table::ContentArrangement::Dynamic)
                .set_width(width);
            if !headers.is_empty() {
                table.set_header(headers.iter().map(|text| clean(text)).collect::<Vec<_>>());
            }
            for row in rows {
                table.add_row(row.iter().map(|text| clean(text)).collect::<Vec<_>>());
            }
            writeln!(out, "{table}")?;
        }
        Ok(())
    }

    fn fields(
        &self,
        out: &mut impl Write,
        fields: &[Value],
        provisioning: &Value,
        compact: bool,
    ) -> Result<()> {
        let mut rows = Vec::new();
        for field in fields {
            let current = field
                .get("value")
                .or_else(|| field.get("default").filter(|value| !value.is_null()));
            let path = field["path"].as_str().unwrap_or("?");
            let secret = field["secret"] == true
                || field["kind"] == "secret"
                || path.starts_with("secrets.");
            let mut text = match current {
                None => "<missing>".into(),
                Some(_) if field["origin"] == "missing" => "<missing>".into(),
                Some(value) if secret && !value.is_null() => {
                    let name = path.strip_prefix("secrets.").unwrap_or(path);
                    let provision = provisioning["secrets"]
                        .as_array()
                        .and_then(|rows| rows.iter().find(|row| row["name"] == name));
                    let label = if value.get("name").is_some() {
                        "Docker secret"
                    } else if provision.is_some_and(|row| row["generated"] == true) {
                        "generated file"
                    } else if provision
                        .is_some_and(|row| row["origin"] == "existing" || row["status"] == "reused")
                    {
                        "existing file"
                    } else {
                        "file"
                    };
                    if self.verbose {
                        format!("*** ({label}) {}", display_value(value))
                    } else {
                        format!("*** ({label})")
                    }
                }
                Some(value) => display_value(value),
            };
            if !compact && !self.verbose && field["origin"] == "default" {
                text.push_str(" (default)");
            }
            let mut row = vec![path.to_owned(), text];
            if self.verbose && !compact {
                row.push(display_value(&field["origin"]));
                row.push(field["doc"].as_str().unwrap_or("").to_owned());
            }
            rows.push(row);
        }
        let headers = if self.verbose && !compact {
            vec!["Field", "Value", "Source", "Description"]
        } else {
            vec!["Field", "Value"]
        };
        if compact && self.mode == OutputMode::Interactive {
            self.table(out, &[], rows)
        } else {
            self.table(out, &headers, rows)
        }
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
            writeln!(
                io::stderr().lock(),
                "{} {}",
                self.style().red().bold().apply_to("Dockstride:"),
                console::strip_ansi_codes(message)
            )?;
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
            for key in ["status", "diagnostics", "secretSync"] {
                if let Some(report) = details.get(key) {
                    self.result(report)?;
                }
            }
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

fn render_human(output: &Output, out: &mut impl Write, value: &Value) -> Result<bool> {
    if value["scope"] == "invoking-repository" {
        writeln!(
            out,
            "{} · {}",
            output.heading("Worktrees"),
            display_value(&value["status"])
        )?;
        let rows = value["worktrees"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|worktree| {
                vec![
                    display_value(&worktree["root"]),
                    display_value(&worktree["configuration"]["status"]),
                ]
            })
            .collect();
        output.table(out, &["Checkout", "Configuration"], rows)?;
        if let Some(error) = value.get("error") {
            writeln!(out, "{}", display_value(error))?;
        }
        return Ok(true);
    }
    if let Some(sources) = value.get("sources").and_then(Value::as_array) {
        let rows = sources
            .iter()
            .map(|source| {
                let mut row = vec![display_value(&source["path"])];
                if output.verbose {
                    row.push(display_value(&source["resolved"]));
                }
                row
            })
            .collect();
        let headers = if output.verbose {
            vec!["Source", "Resolved"]
        } else {
            vec!["Source"]
        };
        output.table(out, &headers, rows)?;
        return Ok(true);
    }
    if value["operation"] == "secrets-sync" {
        let rows = value["secrets"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|secret| {
                let mut row = vec![
                    display_value(&secret["name"]),
                    secret["status"]
                        .as_str()
                        .unwrap_or("uncommitted")
                        .to_owned(),
                ];
                if output.verbose {
                    row.push(display_value(&secret["source"]));
                }
                row
            })
            .collect();
        let headers = if output.verbose {
            vec!["Secret", "Status", "Source"]
        } else {
            vec!["Secret", "Status"]
        };
        output.table(out, &headers, rows)?;
        for secret in value["secrets"].as_array().into_iter().flatten() {
            if secret["consumerRestartNeeded"] == true {
                writeln!(
                    out,
                    "{}: consumer restart/application rotation still required",
                    display_value(&secret["name"])
                )?;
            }
        }
        for field in ["committed", "uncommitted"] {
            if let Some(names) = value.get(field) {
                writeln!(out, "{field}: {}", display_value(names))?;
            }
        }
        return Ok(true);
    }
    if let Some(diagnostics) = value.get("diagnostics") {
        let checks = [
            ("Docker", "docker"),
            ("Configuration", "configuration"),
            ("Secrets", "secrets"),
        ];
        output.table(
            out,
            &["Check", "Status"],
            checks
                .iter()
                .map(|(name, key)| {
                    vec![
                        (*name).to_owned(),
                        if value[key]["ok"] == false {
                            "failed"
                        } else {
                            "checked"
                        }
                        .to_owned(),
                    ]
                })
                .collect(),
        )?;
        for (_, key) in checks {
            if value[key]["ok"] == false {
                serde_json::to_writer_pretty(&mut *out, &value[key])?;
                writeln!(out)?;
            }
        }
        if value["configuration"]["runtime"]["ok"].as_bool() == Some(false) {
            writeln!(out, "Runtime        failed")?;
            serde_json::to_writer_pretty(&mut *out, &value["configuration"]["runtime"])?;
            writeln!(out)?;
        }
        return render_human(output, out, diagnostics);
    }
    if let Some(hooks) = value.get("plannedHooks") {
        writeln!(out, "Diagnostics planned · hooks not run")?;
        serde_json::to_writer_pretty(&mut *out, hooks)?;
        writeln!(out)?;
        return Ok(true);
    }
    if let Some(findings) = value.get("findings").and_then(Value::as_array) {
        writeln!(out, "{}", output.heading("Diagnostics"))?;
        for finding in findings {
            writeln!(
                out,
                "  {} · {} · {}",
                finding["severity"].as_str().unwrap_or("?"),
                finding["code"].as_str().unwrap_or("?"),
                finding["summary"].as_str().unwrap_or("?")
            )?;
            writeln!(out, "    Evidence: {}", display_value(&finding["evidence"]))?;
            for key in ["suggestedCommand", "suggestedAction"] {
                if let Some(suggestion) = finding.get(key) {
                    writeln!(
                        out,
                        "    Suggested (not run): {}",
                        display_value(suggestion)
                    )?;
                }
            }
        }
        let failures = value.get("failures").and_then(Value::as_array);
        for failure in failures.into_iter().flatten() {
            writeln!(out, "  Hook failed: {}", display_value(failure))?;
        }
        if findings.is_empty() && failures.is_none_or(Vec::is_empty) {
            writeln!(out, "  No findings")?;
        }
        return Ok(true);
    }
    if let Some(fields) = value.get("fields").and_then(Value::as_array) {
        output.fields(out, fields, &Value::Null, false)?;
        return Ok(true);
    }
    if let Some(created) = value.get("created") {
        writeln!(
            out,
            "{} · Nickel {} · library {}",
            output.heading("Initialized"),
            display_value(&value["evaluator"]),
            display_value(&value["library"])
        )?;
        output.table(
            out,
            &["Field", "Value"],
            vec![
                vec!["Created".into(), display_value(created)],
                vec!["Next".into(), display_value(&value["next"])],
            ],
        )?;
        return Ok(true);
    }
    if let Some(path) = value
        .get("path")
        .and_then(Value::as_str)
        .filter(|_| value.get("value").is_some())
    {
        let mut field = value.clone();
        field["secret"] = json!(path.starts_with("secrets"));
        output.fields(out, &[field], &Value::Null, false)?;
        if output.verbose {
            if let Some(provenance) = value.get("provenance") {
                writeln!(out, "Source: {}", display_value(provenance))?;
            }
        }
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
        if output.mode == OutputMode::Interactive {
            writeln!(out)?;
        }
        writeln!(out, "{}", output.heading("Environment ready"))?;
        if output.mode == OutputMode::Interactive {
            writeln!(out, "Configuration:")?;
        }
        if let Some(fields) = value["configuration"]["fields"].as_array() {
            let mut fields = fields.clone();
            for path in ["project", "backend"] {
                if !fields.iter().any(|field| field["path"] == path) {
                    if let Some(identity) = value["secretProvisioning"].get(path) {
                        fields.insert(0, json!({"path":path,"value":identity}));
                    }
                }
            }
            output.fields(out, &fields, &value["secretProvisioning"], true)?;
        }
        if output.mode == OutputMode::Interactive {
            writeln!(out)?;
        }
        writeln!(out, "Edit env.yaml to override configuration.")?;
        writeln!(out, "Next: dks up")?;
        writeln!(
            out,
            "No containers started. Existing secrets are preserved on rerun."
        )?;
        return Ok(true);
    }
    if let Some(deployed) = value.get("deployed") {
        writeln!(out, "{}", output.heading("Deployment complete"))?;
        output.table(
            out,
            &["Field", "Value"],
            vec![
                vec!["Deployed".into(), display_value(deployed)],
                vec!["Context".into(), display_value(&value["context"])],
                vec!["Services".into(), display_value(&value["services"])],
                vec!["Convergence".into(), display_value(&value["convergence"])],
                vec!["Secrets".into(), "preserved".into()],
            ],
        )?;
        return Ok(true);
    }
    if value.get("removed").is_some() || value.get("removedServices").is_some() {
        let resources = value
            .get("removed")
            .or_else(|| value.get("removedServices"))
            .unwrap();
        let preserved = value
            .get("volumesPreserved")
            .or_else(|| value.get("dataRetained"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        output.table(
            out,
            &["Field", "Value"],
            vec![
                vec!["Removed".into(), display_value(resources)],
                vec![
                    "Data".into(),
                    if preserved {
                        "preserved"
                    } else {
                        "owned volumes deleted"
                    }
                    .into(),
                ],
                vec!["Secrets".into(), "preserved".into()],
            ],
        )?;
        return Ok(true);
    }
    if let Some(name) = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|_| value.get("reference").is_some())
    {
        output.fields(
            out,
            &[json!({"path":format!("secrets.{name}"),"value":value["reference"],"secret":true})],
            &Value::Null,
            true,
        )?;
        if let Some(consumers) = value.get("consumers") {
            writeln!(out, "Consumers: {}", display_value(consumers))?;
        }
        if value["applied"] == false {
            writeln!(out, "Saved · containers unchanged")?;
        }
        return Ok(true);
    }
    if let Some(secrets) = value.get("secrets").and_then(Value::as_array) {
        let rows = secrets
            .iter()
            .map(|secret| {
                let status = secret["status"]
                    .as_str()
                    .unwrap_or(if secret["present"] == true {
                        "present"
                    } else {
                        "missing"
                    });
                let mut row = vec![
                    display_value(&secret["name"]),
                    secret["backend"]
                        .as_str()
                        .unwrap_or("configured")
                        .to_owned(),
                    status.to_owned(),
                    secret
                        .get("consumers")
                        .map(display_value)
                        .unwrap_or_default(),
                ];
                if output.verbose {
                    row.push(display_value(&secret["reference"]));
                }
                row
            })
            .collect();
        let mut headers = vec!["Secret", "Backend", "Status", "Consumers"];
        if output.verbose {
            headers.push("Reference");
        }
        output.table(out, &headers, rows)?;
        return Ok(true);
    }
    if value.get("actions").is_some() && value.get("workflow").is_some() {
        writeln!(
            out,
            "{} · {} · {}",
            output.heading("Plan"),
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
            writeln!(
                out,
                "{}: {}",
                output.heading("Project"),
                console::strip_ansi_codes(project)
            )?;
        }
        let rows = services
            .iter()
            .map(|service| {
                let readiness = if service["required"] == false {
                    "excluded"
                } else if service["applicationReady"] == false {
                    "application failed"
                } else if service["applicationReady"] == true {
                    "application verified"
                } else if service["containerReady"] == true {
                    "container ready"
                } else {
                    "not verified"
                };
                vec![
                    service["name"].as_str().unwrap_or("?").to_owned(),
                    service["status"].as_str().unwrap_or("unknown").to_owned(),
                    readiness.to_owned(),
                ]
            })
            .collect();
        output.table(out, &["Service", "Status", "Readiness"], rows)?;
        if let Some(endpoints) = value.get("endpoints").and_then(Value::as_object) {
            writeln!(out)?;
            output.table(
                out,
                &["Endpoint", "Value"],
                endpoints
                    .iter()
                    .map(|(name, endpoint)| vec![name.clone(), display_value(endpoint)])
                    .collect(),
            )?;
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
        Value::String(text) if text.is_empty() => "(empty)".into(),
        Value::String(text) => console::strip_ansi_codes(text).into_owned(),
        Value::Array(items) => items
            .iter()
            .map(display_value)
            .collect::<Vec<_>>()
            .join(", "),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(output: &Output, value: Value) -> String {
        let mut bytes = Vec::new();
        output.write_result(&mut bytes, &value).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn interactive(no_color: bool, width: u16) -> Output {
        Output {
            mode: OutputMode::Interactive,
            no_color,
            width: Some(width),
            ..Output::default()
        }
    }

    #[test]
    fn mode_selection_requires_all_terminals_and_capability_unless_forced() {
        assert_eq!(Output::default().mode, OutputMode::NonInteractive);
        assert_eq!(
            OutputMode::select(false, false, false, [true; 3], true),
            OutputMode::Interactive
        );
        for terminals in [
            [false, true, true],
            [true, false, true],
            [true, true, false],
        ] {
            assert_eq!(
                OutputMode::select(false, false, false, terminals, true),
                OutputMode::NonInteractive
            );
        }
        assert_eq!(
            OutputMode::select(false, false, false, [true; 3], false),
            OutputMode::NonInteractive
        );
        assert_eq!(
            OutputMode::select(true, false, false, [false; 3], false),
            OutputMode::Interactive
        );
        for (non_interactive, json) in [(true, false), (false, true)] {
            assert_eq!(
                OutputMode::select(true, non_interactive, json, [true; 3], true),
                OutputMode::NonInteractive
            );
        }
    }

    #[test]
    fn structured_tables_share_plain_and_rounded_presentations() {
        for value in [
            json!({"fields":[{"path":"backend","value":"compose"}]}),
            json!({"services":[{"name":"api","status":"running","containerReady":true}]}),
            json!({"secrets":[{"name":"token","present":true}]}),
            json!({"sources":[{"path":"shared.yaml","resolved":"/private/shared.yaml"}]}),
            json!({"scope":"invoking-repository","status":"available","worktrees":[{"root":"checkout","configuration":{"status":"present"}}]}),
            json!({"operation":"secrets-sync","secrets":[{"name":"token","status":"committed"}]}),
            json!({"deployed":"project","context":"local","services":["api"]}),
            json!({"removed":["api"],"volumesPreserved":true}),
        ] {
            let plain = render(&Output::default(), value.clone());
            assert!(!plain.contains('╭') && !plain.contains('\x1b'));
            let pretty = render(&interactive(true, 80), value);
            assert!(pretty.contains('╭') && pretty.contains('╯'), "{pretty}");
            assert!(!pretty.contains('\x1b'));
        }
    }

    #[test]
    fn rounded_tables_fit_narrow_widths_and_unicode() {
        let value = json!({"fields":[
            {"path":"deep.configuration.field","value":"averylongunbrokentextvalue"},
            {"path":"unicode","value":"界界界界界界界界界界界界界界界界界"}
        ]});
        for width in [12, 20, 32, 48] {
            let text = render(&interactive(true, width), value.clone());
            for line in text.lines() {
                assert!(
                    console::measure_text_width(line) <= usize::from(width),
                    "width {width}: {line}"
                );
            }
        }
    }

    #[test]
    fn setup_masks_truthful_secret_origins_and_keeps_details_verbose_only() {
        let fields = json!([
            {"path":"project","value":"demo","doc":"long description"},
            {"path":"backend","value":"compose"},
            {"path":"secrets.generated","value":{"file":"/private/generated"},"secret":true},
            {"path":"secrets.existing","value":{"file":"/private/existing"},"secret":true},
            {"path":"secrets.input","value":{"file":"/private/input"},"secret":true},
            {"path":"secrets.docker","value":{"name":"private-docker-name"},"secret":true}
        ]);
        let provisioning = json!({"secrets":[
            {"name":"generated","generated":true},
            {"name":"existing","status":"reused","generated":false},
            {"name":"input","status":"provisioned","generated":false}
        ]});
        let value = json!({"configured":true,"configuration":{"fields":fields},"secretProvisioning":provisioning});
        for output in [Output::default(), interactive(true, 80)] {
            let text = render(&output, value.clone());
            for label in [
                "*** (generated file)",
                "*** (existing file)",
                "*** (file)",
                "*** (Docker secret)",
            ] {
                assert!(text.contains(label), "{text}");
            }
            assert!(
                !text.contains("/private/")
                    && !text.contains("private-docker-name")
                    && !text.contains("long description")
            );
            assert!(text.contains("Environment ready") && text.contains("Next: dks up"));
            if output.mode == OutputMode::Interactive {
                assert!(text.contains("Configuration:\n╭") && !text.contains('╞'));
            }
            let verbose = render(
                &Output {
                    verbose: true,
                    ..output
                },
                value.clone(),
            );
            assert!(verbose.contains("/private/generated"));
        }
        let replacement =
            json!({"name":"input","reference":{"file":"/private/input"},"applied":false});
        let text = render(&Output::default(), replacement);
        assert!(text.contains("*** (file)") && !text.contains("/private/input"));
        let get =
            json!({"path":"secrets.input","value":{"file":"/private/input"},"origin":"env.yaml"});
        assert!(!render(&Output::default(), get.clone()).contains("/private/input"));
        assert!(
            render(
                &Output {
                    verbose: true,
                    ..Output::default()
                },
                get
            )
            .contains("/private/input")
        );
    }

    #[test]
    fn empty_null_and_missing_remain_distinct() {
        let value = json!({"fields":[
            {"path":"empty","value":""},
            {"path":"null","value":null,"origin":"env.yaml"},
            {"path":"missing","value":null,"origin":"missing"},
            {"path":"schemaMissing","default":null},
            {"path":"absent"}
        ]});
        let text = render(&Output::default(), value);
        for line in [
            "empty\t(empty)",
            "null\tnull",
            "missing\t<missing>",
            "schemaMissing\t<missing>",
            "absent\t<missing>",
        ] {
            assert!(text.contains(line), "{text}");
        }
    }

    #[test]
    fn color_changes_only_styles_and_steps_are_not_events() {
        let value = json!({"configured":true,"configuration":{"fields":[{"path":"backend","value":"compose"}]}});
        let colored = render(&interactive(false, 80), value.clone());
        let uncolored = render(&interactive(true, 80), value);
        assert!(colored.contains('\x1b'));
        assert_eq!(console::strip_ansi_codes(&colored), uncolored);
        assert_eq!(interactive(true, 80).step_label(true), "✓");
        assert!(interactive(false, 80).step_label(true).contains("\x1b[32m"));
        assert_eq!(Output::default().step_label(true), "Completed:");
        assert_eq!(interactive(true, 80).step_label(false), "Failed:");
    }

    #[test]
    fn raw_strings_and_json_payloads_preserve_every_byte() {
        for text in ["", "no newline", "yaml: value\n", "\x1b[32mraw\x1b[0m\n\n"] {
            for output in [Output::default(), interactive(false, 80)] {
                assert_eq!(render(&output, json!(text)), text);
            }
            let json = render(
                &Output {
                    json: true,
                    ..Output::default()
                },
                json!(text),
            );
            let record: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(record["result"], text);
            assert_eq!(record["schemaVersion"], SCHEMA_VERSION);
            assert_eq!(record["type"], "result");
            assert_eq!(record["ok"], true);
        }
    }

    #[test]
    fn plain_tables_escape_control_characters() {
        let text = render(
            &Output::default(),
            json!({"fields":[{"path":"field","value":"one\ttwo\nthree\r\x1b[31mred\x1b[0m"}]}),
        );
        assert_eq!(text, "Field\tValue\nfield\tone\\ttwo\\nthree\\rred\n");
    }
}
