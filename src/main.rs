use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use dockstride::{config, deploy, model::Project, nickel, output::Output, runtime, secrets, state};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

#[derive(Parser, Debug)]
#[command(
    name = "dks",
    version,
    about = "A transparent path from Docker checkout to development and Swarm deployment"
)]
struct Cli {
    #[arg(short = 'C', long = "directory", global = true, default_value = ".")]
    directory: PathBuf,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    non_interactive: bool,
    #[arg(long, global = true)]
    no_color: bool,
    #[arg(short, long, global = true)]
    quiet: bool,
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Inspect operations without generating secrets, editing files, building, or deploying.
    #[arg(long, global = true)]
    plan: bool,
    /// Supply an initial secret from a private file; existing references are reused.
    #[arg(long = "secret-file", global = true, value_name = "NAME=PATH")]
    secret_files: Vec<String>,
    /// Supply one initial secret from piped stdin, never from an argument.
    #[arg(long = "secret-stdin", global = true, value_name = "NAME")]
    secret_stdin: Option<String>,
    #[arg(long, global = true, default_value_t = 180)]
    timeout: u64,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a starter configuration and checked-in pinned Nickel library.
    Init,
    /// Execute a named project command without applying its returned settings.
    Run { name: String },
    /// List configured environments or explicitly forget a resource-free registration.
    Env {
        #[command(subcommand)]
        command: EnvironmentCommand,
    },
    /// Explicitly release generated endpoints or collect proven-stale reservations.
    Ports {
        #[command(subcommand)]
        command: PortCommand,
    },
    /// Fill missing environment values and provision declared secrets, without starting containers.
    Setup {
        #[arg(long = "set", value_name = "PATH=VALUE")]
        inputs: Vec<String>,
        /// Disable automatic shared-source discovery for this checkout.
        #[arg(long)]
        no_shared_sources: bool,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Set up, build, start, and verify a Compose development stack.
    Up {
        #[arg(long = "profile", value_name = "NAME")]
        profiles: Vec<String>,
        services: Vec<String>,
    },
    /// Start and verify, then enter the declared development loop.
    Dev {
        #[arg(long = "profile", value_name = "NAME")]
        profiles: Vec<String>,
        services: Vec<String>,
    },
    /// Verify the required backend scope and application readiness within one bounded observation.
    Status {
        #[arg(long = "profile", value_name = "NAME")]
        profiles: Vec<String>,
        #[arg(long)]
        inspect_only: bool,
        services: Vec<String>,
    },
    /// Stream Docker logs for the configured backend.
    Logs {
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(long, default_value_t = 100)]
        tail: u32,
        services: Vec<String>,
    },
    /// Execute an argv command in a Compose service (Swarm use explicit stack/service operations).
    Exec {
        service: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Stop owned resources, preserving volumes and secrets.
    Down,
    /// Remove owned application data, never secrets. Requires --yes or confirmation.
    Destroy {
        #[arg(long)]
        yes: bool,
    },
    /// Publish and deploy all or selected Swarm services.
    Deploy { services: Vec<String> },
    Secrets {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Export the exact Docker document without executing project hooks.
    Render {
        #[arg(long, value_enum)]
        target: Option<Target>,
    },
    /// Diagnose configuration, Docker capabilities, context, secrets, and conflicts.
    Doctor,
    /// Explicit native Compose passthrough; unmanaged Docker semantics.
    Compose {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Explicit native stack passthrough; unmanaged Docker semantics.
    Stack {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Generate shell completion definitions.
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand, Debug)]
enum EnvironmentCommand {
    List {
        #[arg(long)]
        worktrees: bool,
    },
    Forget {
        path: PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PortCommand {
    Release {
        #[arg(long)]
        yes: bool,
    },
    Gc {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    List,
    Sources {
        #[command(subcommand)]
        command: SourceCommand,
    },
    Get {
        path: String,
    },
    Set {
        path: String,
        value: Option<String>,
        #[arg(long, conflicts_with = "value")]
        file: Option<PathBuf>,
        #[arg(long)]
        shared: bool,
        #[arg(long, requires = "shared")]
        source: Option<PathBuf>,
    },
    Unset {
        path: String,
        #[arg(long)]
        shared: bool,
        #[arg(long, requires = "shared")]
        source: Option<PathBuf>,
    },
    Edit {
        #[arg(long)]
        shared: bool,
        #[arg(long, requires = "shared")]
        source: Option<PathBuf>,
    },
    Schema,
}

#[derive(Subcommand, Debug)]
enum SourceCommand {
    List,
    Add {
        path: PathBuf,
        #[arg(long)]
        create: bool,
    },
    Remove { path: PathBuf },
}

#[derive(Subcommand, Debug)]
enum SecretCommand {
    List,
    Replace {
        name: String,
        #[arg(long, conflicts_with = "stdin")]
        file: Option<PathBuf>,
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        apply: bool,
    },
    /// Explicitly compare imported private files and publish changed revisions.
    Sync {
        #[arg(required = true)]
        names: Vec<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        apply: bool,
    },
    Gc {
        names: Vec<String>,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(ValueEnum, Clone, Debug)]
enum Target {
    Compose,
    Swarm,
    Build,
}

fn main() {
    let no_color = std::env::var_os("NO_COLOR").is_some()
        || std::env::args_os().any(|arg| arg == "--no-color" || arg == "--json");
    let command = if no_color {
        Cli::command().color(clap::ColorChoice::Never)
    } else {
        Cli::command()
    };
    let cli = Cli::from_arg_matches(&command.get_matches()).unwrap_or_else(|error| error.exit());
    let output = Output {
        json: cli.json,
        quiet: cli.quiet,
    };
    match execute(&cli, &output) {
        Ok(Some(result)) => {
            if let Err(error) = output.result(&result) {
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
                {
                    return;
                }
                eprintln!("Dockstride output: {error:#}");
                std::process::exit(1);
            }
        }
        Ok(None) => {}
        Err(error) => {
            let (category, code) = classify(&error);
            let message = if cli.verbose {
                format!("{error:?}")
            } else {
                format!("{error:#}")
            };
            let mut details = json!({});
            if let Some(missing) = error.downcast_ref::<config::MissingInputs>() {
                details["missingInputs"] = json!(missing.fields);
            }
            if let Some(docker) = error.downcast_ref::<runtime::DockerError>() {
                details["underlyingDockerStatus"] = json!(docker.status);
                details["operation"] = json!(docker.args);
            }
            if let Some(prerequisite) = error.downcast_ref::<runtime::PrerequisiteFailed>() {
                details["prerequisite"] = prerequisite.0.clone();
            }
            if let Some(blocked) = error.downcast_ref::<dockstride::environment::ForgetBlocked>() {
                details["environment"] = blocked.0.clone();
            }
            if let Some(blocked) = error.downcast_ref::<dockstride::ports::PortsBlocked>() {
                details["ports"] = blocked.0.clone();
            }
            if let Some(failed) = error.downcast_ref::<dockstride::status::StatusFailed>() {
                details["status"] = failed.0.clone();
            }
            if let Some(report) = error.downcast_ref::<dockstride::status::StatusReport>() {
                details["status"] = report.0.clone();
            }
            if let Some(report) = error.downcast_ref::<dockstride::diagnostics::DiagnosticReport>() {
                details["diagnostics"] = report.0.clone();
            }
            if let Some(report) = error.downcast_ref::<secrets::SyncFailed>() {
                details["secretSync"] = report.0.clone();
            }
            if let Ok(pending) = dockstride::publication::pending(&cli.directory)
                && !pending.is_null()
            {
                details["pendingPublication"] = pending;
            }
            let _ = output.diagnostic(category, code, &message, details);
            std::process::exit(code);
        }
    }
}

fn classify(error: &anyhow::Error) -> (&'static str, i32) {
    if error.is::<runtime::Cancelled>() {
        return ("cancelled", 130);
    }
    if error.is::<config::MissingInputs>() {
        return ("configuration", 2);
    }
    if error.is::<nickel::NickelError>() {
        return ("configuration", 2);
    }
    if let Some(error) = error.downcast_ref::<runtime::DockerError>() {
        return if error.status == 130 {
            ("cancelled", 130)
        } else {
            ("docker", 4)
        };
    }
    if error.is::<runtime::PrerequisiteFailed>() {
        return ("operation", 1);
    }
    if error.is::<dockstride::status::StatusConfiguration>() {
        return ("configuration", 2);
    }
    if error.is::<dockstride::status::StatusFailed>() {
        return ("operation", 1);
    }
    if error.is::<dockstride::diagnostics::DiagnosticConfiguration>() {
        return ("configuration", 2);
    }
    let mut message = format!("{error:#}");
    message.make_ascii_lowercase();
    if message.contains("cancel") || message.contains("interrupted") {
        ("cancelled", 130)
    } else if message.contains("confirm") {
        ("consent", 5)
    } else if message.contains("env.yaml")
        || message.contains("nickel")
        || message.contains("configuration")
        || message.contains("missing")
        || message.contains("required")
    {
        ("configuration", 2)
    } else if message.contains("ownership")
        || message.contains("context")
        || message.contains("docker")
        || message.contains("port conflict")
    {
        ("prerequisite", 3)
    } else {
        ("operation", 1)
    }
}

fn execute(cli: &Cli, out: &Output) -> Result<Option<Value>> {
    if let Commands::Completions { shell } = cli.command {
        let mut command = Cli::command();
        if cli.json {
            let mut buffer = Vec::new();
            clap_complete::generate(shell, &mut command, "dks", &mut buffer);
            return Ok(Some(json!(String::from_utf8(buffer)?)));
        }
        clap_complete::generate(shell, &mut command, "dks", &mut io::stdout());
        return Ok(None);
    }
    let root = cli.directory.canonicalize().with_context(|| {
        format!(
            "project directory {} does not exist",
            cli.directory.display()
        )
    })?;
    runtime::pin_invocation();
    let pending = dockstride::publication::pending(&root)?;
    if !pending.is_null() {
        out.event("pending", &format!("Interrupted publication: {}", serde_json::to_string(&pending)?))?;
    }
    let non_interactive = cli.non_interactive || cli.json || !io::stdin().is_terminal();
    let secret_inputs = initial_secret_inputs(cli)?;
    let result = match &cli.command {
        Commands::Init => {
            prohibit_plan_mutation(cli)?;
            nickel::init(&root)?
        }
        Commands::Run { name } => {
            prohibit_plan_mutation(cli)?;
            dockstride::commands::run(&root, name, cli.timeout, out)?
        }
        Commands::Env { command } => match command {
            EnvironmentCommand::List { worktrees } => dockstride::environment::list(&root, *worktrees)?,
            EnvironmentCommand::Forget { path, yes } => {
                let confirmed = cli.plan || *yes || confirm(cli, non_interactive, "Forget only this resource-free environment registration?")?;
                dockstride::environment::forget(&root, path, cli.plan, confirmed, out)?
            }
        },
        Commands::Ports { command } => {
            let (yes, gc) = match command {
                PortCommand::Release { yes } => (*yes, false),
                PortCommand::Gc { yes } => (*yes, true),
            };
            let confirmed = cli.plan || yes || confirm(cli, non_interactive,
                if gc { "Collect only reservations with verified stale ownership and no Docker resources?" }
                else { "Release owned generated endpoints after proving Docker resources are absent?" })?;
            if gc { dockstride::ports::gc(&root, cli.plan, confirmed, out)? }
            else { dockstride::ports::release(&root, cli.plan, confirmed, out)? }
        }
        Commands::Setup { inputs, no_shared_sources } => {
            let mut inputs = inputs.clone();
            if *no_shared_sources { inputs.push("_dockstride.sources=[]".to_owned()); }
            if cli.plan {
                let candidate = config::setup_plan_candidate(&root, &inputs)?;
                configuration_plan(&root, "setup", &secret_inputs, Some(&candidate))?
            } else {
                config::setup_with_context(&root, &inputs, non_interactive, cli.timeout, out, "setup")?;
                let provisioned = secrets::provision(&root, non_interactive, &secret_inputs, out)?;
                {
                    let _lifecycle = state::lock(&root, "lifecycle")?;
                    if runtime::allocate_ports(&root)? {
                        out.event("Ports", "declared checkout-local allocations persisted")?;
                    }
                }
                if provisioned["consumerValidation"] == "deferred" { secrets::validate_consumers(&root, out)?; }
                json!({"configured":true,"started":false,"configuration":config::list(&root)?})
            }
        }
        Commands::Config { command } => match command {
            ConfigCommand::List => config::list(&root)?,
            ConfigCommand::Sources { command } => match command {
                SourceCommand::List => config::sources_list(&root)?,
                SourceCommand::Add { path, create } => {
                    prohibit_plan_mutation(cli)?;
                    config::sources_add(&root, path, *create)?
                }
                SourceCommand::Remove { path } => {
                    prohibit_plan_mutation(cli)?;
                    config::sources_remove(&root, path)?
                }
            },
            ConfigCommand::Get { path } => config::get(&root, path)?,
            ConfigCommand::Schema => json!({"fields":nickel::schema(&root,None)?}),
            ConfigCommand::Set { path, value, file, shared, source } => {
                prohibit_plan_mutation(cli)?;
                let value = if let Some(file) = file {
                    parse_value(&std::fs::read_to_string(file)?)?
                } else {
                    parse_value(
                        value
                            .as_deref()
                            .context("config set requires VALUE or --file")?,
                    )?
                };
                if *shared { config::set_shared(&root, path, value, source.as_deref())? }
                else { config::set(&root, path, value)? }
            }
            ConfigCommand::Unset { path, shared, source } => {
                prohibit_plan_mutation(cli)?;
                if *shared { config::unset_shared(&root, path, source.as_deref())? }
                else { config::unset(&root, path)? }
            }
            ConfigCommand::Edit { shared, source } => {
                prohibit_plan_mutation(cli)?;
                if non_interactive {
                    bail!(
                        "config edit requires an interactive editor; use config set --file for automation"
                    );
                }
                if *shared { config::edit_shared(&root, source.as_deref())? }
                else { config::edit(&root)? }
            }
        },
        Commands::Render { target } => {
            let project = nickel::evaluate(&root, None)?;
            let target = match target {
                Some(Target::Compose) | Some(Target::Build) => "compose",
                Some(Target::Swarm) => "swarm",
                None => project.backend()?,
            };
            let model = runtime::render_document(&project, target)?;
            if cli.json {
                model
            } else {
                json!(serde_yaml::to_string(&model)?)
            }
        }
        Commands::Doctor => doctor(&root, cli.timeout, cli.plan, out)?,
        Commands::Secrets { command } => match command {
            SecretCommand::List => secrets::list(&root)?,
            SecretCommand::Replace {
                name,
                file,
                stdin,
                apply,
            } => {
                prohibit_plan_mutation(cli)?;
                let input = if let Some(file) = file {
                    let path = if file.is_absolute() { file.clone() }
                        else { std::env::current_dir()?.join(file) };
                    Some(secrets::SecretInput::File(path))
                } else if *stdin {
                    Some(secrets::SecretInput::Stdin)
                } else { None };
                secrets::replace(&root, name, input.as_ref(), *apply, non_interactive, out)?
            }
            SecretCommand::Sync { names, yes, apply } => {
                let confirmed = cli.plan || *yes || confirm(
                    cli, non_interactive, "Synchronize the selected imported files into immutable secret revisions?",
                )?;
                secrets::sync(&root, names, cli.plan, confirmed, *apply, out)?
            }
            SecretCommand::Gc { names, yes } => {
                let confirmed = cli.plan
                    || *yes
                    || confirm(
                        cli,
                        non_interactive,
                        "Delete only the explicitly selected, unreferenced owned secret revisions?",
                    )?;
                secrets::gc(&root, names, cli.plan, confirmed, out)?
            }
        },
        Commands::Up { services, .. } | Commands::Dev { services, .. } | Commands::Deploy { services } => {
            let workflow = match cli.command {
                Commands::Dev { .. } => "dev",
                Commands::Deploy { .. } => "deploy",
                _ => "up",
            };
            let profiles = match &cli.command {
                Commands::Up { profiles, .. } | Commands::Dev { profiles, .. } => profiles.as_slice(),
                _ => &[],
            };
            let project = if cli.plan {
                match nickel::evaluate(&root, None) {
                    Ok(project) => project,
                    Err(error) => {
                        return Ok(Some(
                            json!({"workflow":workflow,"sideEffects":false,"unresolved":configuration_plan(&root,workflow,&secret_inputs,None)?,"diagnostic":format!("{error:#}")}),
                        ));
                    }
                }
            } else {
                config::setup_with_context(&root, &[], non_interactive, cli.timeout, out, workflow)?;
                let provisioned = secrets::provision(&root, non_interactive, &secret_inputs, out)?;
                if workflow != "deploy" {
                    let _lifecycle = state::lock(&root, "lifecycle")?;
                    runtime::allocate_ports(&root)?;
                }
                let project = nickel::evaluate(&root, None)?;
                if provisioned["consumerValidation"] == "deferred" { secrets::validate_consumers(&root, out)?; }
                header(&project, out)?;
                let result = if workflow == "deploy" {
                    deploy::deploy(&project, services, false, cli.timeout, out)?
                } else {
                    runtime::lifecycle(
                        &project,
                        workflow,
                        services,
                        profiles,
                        false,
                        false,
                        cli.timeout,
                        out,
                    )?
                };
                return Ok(Some(result));
            };
            if workflow == "deploy" {
                deploy::deploy(&project, services, true, cli.timeout, out)?
            } else {
                runtime::lifecycle(&project, workflow, services, profiles, true, false, cli.timeout, out)?
            }
        }
        Commands::Status { services, profiles, inspect_only } => {
            let project = nickel::evaluate(&root, None).map_err(|error| {
                error.context(dockstride::status::StatusReport(json!({
                    "backend":null,"project":null,"context":null,"inspectOnly":inspect_only,
                    "ready":false,"deadlineExceeded":false,"configurationObserved":false,
                    "requiredServices":[],"excludedServices":[],"services":[],"endpoints":{}
                })))
            })?;
            if project.backend()? == "swarm" {
                if !profiles.is_empty() {
                    let required: Vec<_> = if services.is_empty() {
                        project.services()?.keys().map(String::as_str).collect()
                    } else {
                        services.iter().map(String::as_str).collect()
                    };
                    let rows: Vec<_> = required.iter().map(|name| json!({
                        "name":name,"required":true,"observed":false,"containerReady":false,
                        "applicationReady":null,"ready":false,"status":"unobserved"
                    })).collect();
                    return Err(anyhow::anyhow!("Compose profiles do not apply to Swarm status")
                        .context(dockstride::status::StatusConfiguration)
                        .context(dockstride::status::StatusReport(json!({
                            "backend":"swarm","project":project.name()?,"context":null,
                            "inspectOnly":inspect_only,"ready":false,"deadlineExceeded":false,
                            "requiredServices":required,"excludedServices":[],"services":rows,
                            "endpoints":project.endpoints()
                        }))));
                }
                deploy::status(&project, services, *inspect_only, cli.timeout, out)?
            } else {
                runtime::status(&project, services, profiles, *inspect_only, cli.timeout, out)?
            }
        }
        Commands::Down | Commands::Destroy { .. } => {
            let project = nickel::evaluate(&root, None)?;
            let destroy = matches!(cli.command, Commands::Destroy { .. });
            let yes = matches!(cli.command, Commands::Destroy { yes: true });
            let confirmed = !destroy
                || cli.plan
                || yes
                || confirm(
                    cli,
                    non_interactive,
                    "Permanently delete this environment's owned volumes and application data? Secrets will be preserved.",
                )?;
            if !confirmed {
                bail!("destruction confirmation declined; no resources changed");
            }
            if project.backend()? == "swarm" {
                deploy::teardown(&project, destroy, cli.plan, confirmed, out)?
            } else {
                runtime::lifecycle(
                    &project,
                    if destroy { "destroy" } else { "down" },
                    &[],
                    &[],
                    cli.plan,
                    confirmed,
                    cli.timeout,
                    out,
                )?
            }
        }
        Commands::Logs {
            follow,
            tail,
            services,
        } => {
            let project = nickel::evaluate(&root, None)?;
            let mut args = vec!["logs".into(), "--tail".into(), tail.to_string()];
            if *follow {
                args.push("--follow".into());
            }
            if cli.plan {
                return Ok(Some(
                    json!({"sideEffects":false,"backend":project.backend()?,"operation":"logs","services":services,"follow":follow,"tail":tail}),
                ));
            }
            if project.backend()? == "compose" {
                args.extend(services.clone());
                runtime::passthrough(&project, "compose", &args, out)?
            } else {
                if services.len() != 1 {
                    bail!("Swarm logs requires exactly one service; use dks logs SERVICE");
                }
                if !project.services()?.contains_key(&services[0]) {
                    bail!("unknown service {}", services[0]);
                }
                args.push(format!("{}_{}", project.name()?, services[0]));
                args.insert(0, "service".into());
                runtime::Docker::new(&root, out.clone()).native(&args, None)?;
                json!({"completed":true})
            }
        }
        Commands::Exec { service, args } => {
            let project = nickel::evaluate(&root, None)?;
            if project.backend()? != "compose" {
                bail!(
                    "exec requires a Compose environment; Swarm tasks are node-local, use Docker explicitly"
                );
            }
            if !project.services()?.contains_key(service) {
                bail!("unknown service {service}");
            }
            let mut argv = vec!["exec".into()];
            if non_interactive {
                argv.push("-T".into());
            }
            argv.push(service.clone());
            argv.extend(args.clone());
            if cli.plan {
                json!({"sideEffects":false,"backend":"compose","project":project.name()?,"argv":argv})
            } else {
                runtime::passthrough(&project, "compose", &argv, out)?
            }
        }
        Commands::Compose { args } | Commands::Stack { args } => {
            if cli.plan {
                bail!(
                    "--plan is not supported for native passthrough; use managed commands for side-effect-free planning"
                );
            }
            let project = nickel::evaluate(&root, None)?;
            let namespace = if matches!(cli.command, Commands::Compose { .. }) {
                "compose"
            } else {
                "stack"
            };
            out.event("Passthrough", "unmanaged native Docker semantics")?;
            runtime::passthrough(&project, namespace, args, out)?
        }
        Commands::Completions { .. } => unreachable!(),
    };
    if !cli.json
        && !cli.plan
        && matches!(
            cli.command,
            Commands::Logs { .. }
                | Commands::Exec { .. }
                | Commands::Compose { .. }
                | Commands::Stack { .. }
        )
    {
        return Ok(None);
    }
    Ok(Some(result))
}

fn prohibit_plan_mutation(cli: &Cli) -> Result<()> {
    if cli.plan {
        bail!("this mutation does not support --plan; no files changed");
    }
    Ok(())
}
fn parse_value(text: &str) -> Result<Value> {
    let value: Value = serde_yaml::from_str(text).context(
        "value must be valid YAML/JSON; quote strings that resemble booleans or numbers",
    )?;
    Ok(value)
}
fn confirm(cli: &Cli, non_interactive: bool, message: &str) -> Result<bool> {
    if non_interactive || cli.json {
        bail!(
            "confirmation required: {message} Supply the explicit --yes flag in noninteractive mode"
        );
    }
    eprint!("{message} [y/N] ");
    io::stderr().flush()?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
fn header(project: &Project, out: &Output) -> Result<()> {
    out.event(
        "Dockstride",
        &format!("{} · {}", project.name()?, project.backend()?),
    )
}
fn configuration_plan(
    root: &Path,
    workflow: &str,
    inputs: &BTreeMap<String, secrets::SecretInput>,
    candidate: Option<&Value>,
) -> Result<Value> {
    let snapshot = dockstride::sources::snapshot(root, candidate)?;
    let env = snapshot.values;
    let fields = nickel::schema(root, candidate)?;
    let missing: Vec<_> = fields
        .iter()
        .filter(|f| f.required && f.default.is_none() && lookup(&env, &f.path).is_none())
        .map(|f| json!({"path":f.path,"kind":f.kind,"description":f.doc}))
        .collect();
    let setup = match nickel::setup_metadata(root, candidate) {
        Ok(metadata) => metadata,
        Err(error) => json!({"unresolved":true,"diagnostic":format!("{error:#}")}),
    };
    let provided: serde_json::Map<String, Value> = inputs
        .iter()
        .map(|(name, input)| {
            (
                name.clone(),
                match input {
                    secrets::SecretInput::File(path) => json!({"source":"file","path":path}),
                    secrets::SecretInput::Stdin => json!({"source":"stdin"}),
                },
            )
        })
        .collect();
    let defaults = dockstride::defaults::plan(root, &snapshot.local)?;
    Ok(
        json!({"workflow":workflow,"sideEffects":false,"missingInputs":missing,"setup":setup,"defaults":defaults,"providedSecretInputs":provided}),
    )
}
fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |v, key| v.get(key))
}
fn doctor(root: &Path, timeout: u64, plan_only: bool, out: &Output) -> Result<Value> {
    let docker = runtime::Docker::new(root, out.clone());
    let prerequisites = match docker.check() {
        Ok(value) => json!({"ok":true,"details":value}),
        Err(error) => json!({"ok":false,"diagnostic":format!("{error:#}")}),
    };
    let mut diagnostics = json!({"schemaVersion":1,"findings":[],"failures":[]});
    let configuration = match nickel::evaluate(root, None) {
        Ok(project) => {
            let view = if project.backend()? == "swarm" {
                project.swarm()
            } else {
                project.compose()
            };
            let runtime = match runtime::doctor(&project, out) {
                Ok(value) => value,
                Err(error) => json!({"ok":false,"diagnostic":format!("{error:#}")}),
            };
            diagnostics = if plan_only {
                dockstride::diagnostics::validate(&project)?;
                json!({"schemaVersion":1,"sideEffects":false,"findings":[],"failures":[],
                    "plannedHooks":project.metadata.get("diagnostics").cloned().unwrap_or_else(|| json!({}))})
            } else {
                dockstride::diagnostics::run(
                    &project, &[], "doctor", timeout, &docker, out, None,
                )
            };
            match view {
                Ok(_) => {
                    json!({"ok":true,"project":project.name()?,"backend":project.backend()?,"services":project.services()?.keys().collect::<Vec<_>>(),"runtime":runtime})
                }
                Err(error) => json!({"ok":false,"diagnostic":format!("{error:#}")}),
            }
        }
        Err(error) => json!({"ok":false,"diagnostic":format!("{error:#}")}),
    };
    let secrets = match secrets::doctor(root) {
        Ok(value) => value,
        Err(error) => json!({"ok":false,"diagnostic":format!("{error:#}")}),
    };
    Ok(json!({"docker":prerequisites,"configuration":configuration,"secrets":secrets,"diagnostics":diagnostics}))
}

fn initial_secret_inputs(cli: &Cli) -> Result<BTreeMap<String, secrets::SecretInput>> {
    if (!cli.secret_files.is_empty() || cli.secret_stdin.is_some())
        && !matches!(
            cli.command,
            Commands::Setup { .. }
                | Commands::Up { .. }
                | Commands::Dev { .. }
                | Commands::Deploy { .. }
        )
    {
        bail!(
            "initial --secret-file/--secret-stdin inputs apply only to setup/up/dev/deploy; use secrets replace --file/--stdin for replacement"
        );
    }
    let mut inputs = BTreeMap::new();
    for specification in &cli.secret_files {
        let (name, file) = specification
            .split_once('=')
            .context("--secret-file requires NAME=PATH, never a secret value")?;
        if name.is_empty() || file.is_empty() {
            bail!("--secret-file requires a nonempty NAME=PATH");
        }
        let path = PathBuf::from(file);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        if inputs
            .insert(name.to_owned(), secrets::SecretInput::File(path))
            .is_some()
        {
            bail!("duplicate initial secret input {name}");
        }
    }
    if let Some(name) = &cli.secret_stdin {
        if name.is_empty() {
            bail!("--secret-stdin requires a logical secret name");
        }
        if inputs
            .insert(name.clone(), secrets::SecretInput::Stdin)
            .is_some()
        {
            bail!("duplicate initial secret input {name}");
        }
    }
    Ok(inputs)
}
