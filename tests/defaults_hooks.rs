use dockstride::{commands, defaults, output::Output};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn fixture(root: &Path) {
    fs::write(root.join("compose.ncl"), r#"
let contract = {
  project | String,
  backend | String | default = "compose",
  oauth | { enabled | Bool | default = false },
  requiredRuntimeInput | String,
} in
let env | contract = import "env.yaml" in
{
  dockstride | not_exported = {
    Config = contract,
    commands.defaults = { argv = ["python3", "hook.py"], timeoutSeconds = 5 },
    setup.defaults = { command = "defaults", fields = ["project", "oauth.enabled"], sources = true },
    readiness.api.http.url = env.requiredRuntimeInput,
    actions = [{name = "unavailable", argv = [env.requiredRuntimeInput]}],
  },
  name = env.project,
  services.api.image = env.requiredRuntimeInput,
}
"#).unwrap();
    fs::write(root.join("hook.py"), r#"import json,pathlib,sys
root = pathlib.Path('.')
count = root / 'calls'
count.write_text(str(int(count.read_text()) + 1) if count.exists() else '1')
context = json.load(sys.stdin)
(root / 'context.json').write_text(json.dumps(context))
sys.stdout.buffer.write((root / 'response.json').read_bytes())
"#).unwrap();
}

fn count(root: &Path) -> u64 {
    fs::read_to_string(root.join("calls")).map(|value| value.parse().unwrap()).unwrap_or(0)
}

fn response(root: &Path, value: Value) {
    fs::write(root.join("response.json"), serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn defaults_commands_are_isolated_and_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    fixture(directory.path());
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "defaults_subprocess_worker", "--nocapture"])
        .env("DKS_DEFAULTS_FIXTURE", directory.path())
        .env("DOCKER_HOST", "unix:///defaults-fixture-unused.sock")
        .env_remove("DOCKER_CONTEXT")
        .output().unwrap();
    assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
}

// Isolating the pinned Docker environment in a subprocess avoids modifying global
// environment variables while the rest of the integration suite runs in parallel.
#[test]
fn defaults_subprocess_worker() {
    let Some(root) = std::env::var_os("DKS_DEFAULTS_FIXTURE") else { return; };
    let root = Path::new(&root);
    let output = Output { quiet: true, json: false };
    let local = json!({"_dockstride":{"sources":[]},"secrets":{"api":{"file":"/private/do-not-forward"}}});
    let original = "# preserve this local document\n_dockstride:\n  sources: []\nsecrets:\n  api:\n    file: /private/do-not-forward\n";
    fs::write(root.join("env.yaml"), original).unwrap();
    response(root, json!({"schemaVersion":1,"values":{"project":"proposed"}}));
    let proposal = defaults::propose_for(root, &local, "up", 10, &output).unwrap();
    assert!(proposal.invoked);
    assert_eq!(proposal.values["project"], "proposed");
    assert_eq!(count(root), 1);
    let context_text = fs::read_to_string(root.join("context.json")).unwrap();
    let context: Value = serde_json::from_str(&context_text).unwrap();
    assert_eq!(context["purpose"], "up");
    assert_eq!(context["settings"]["backend"], "compose");
    assert_eq!(context["settings"]["oauth"]["enabled"], false);
    assert_eq!(context["missingFields"], json!(["project"]));
    assert_eq!(context["provenance"]["backend"]["origin"], "default");
    assert_eq!(context["sourcesDeclared"], true);
    assert!(!context_text.contains("do-not-forward"));
    assert!(context["settings"].get("_dockstride").is_none());
    assert!(context["settings"].get("secrets").is_none());
    assert!(context["provenance"].get("secrets.api.file").is_none());
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), original);

    let explicit = json!({"project":"chosen","_dockstride":{"sources":[]}});
    assert!(!defaults::propose(root, &explicit, 10, &output).unwrap().invoked);
    fs::write(root.join("shared.yaml"), "project: inherited\n").unwrap();
    let inherited = json!({"_dockstride":{"sources":[{"path":"shared.yaml"}]}});
    assert!(!defaults::propose(root, &inherited, 10, &output).unwrap().invoked);
    assert_eq!(count(root), 1);
    let context = commands::context_for(root, &inherited, "manual", &[]).unwrap();
    assert_eq!(context["settings"]["project"], "inherited");
    assert_eq!(context["settings"]["backend"], "compose");
    assert_eq!(context["provenance"]["project"]["file"], root.join("shared.yaml").to_str().unwrap());
    let preview = defaults::plan(root, &json!({})).unwrap();
    assert_eq!(preview["wouldRun"], true);
    assert_eq!(preview["discoverSources"], true);
    assert_eq!(preview["missingFields"], json!(["project"]));
    assert_eq!(count(root), 1);

    let proposed_source = root.join("not-created.yaml");
    let valid = json!({"schemaVersion":1,"values":{},"sources":[{"path":proposed_source,"createIfMissing":true}]});
    response(root, valid.clone());
    let proposal = defaults::propose(root, &json!({}), 10, &output).unwrap();
    assert!(proposal.sources.unwrap()[0].create_if_missing);
    assert!(!proposed_source.exists());
    assert_eq!(count(root), 2);

    let invalid = [
        json!({"schemaVersion":2,"values":{}}),
        json!({"schemaVersion":1,"values":{},"unexpected":true}),
        json!({"schemaVersion":1,"values":{"extra":"x"}}),
        json!({"schemaVersion":1,"values":{"secrets":{"api":"credential"}}}),
        json!({"schemaVersion":1,"values":{"_dockstride":{"sources":[]}}}),
        json!({"schemaVersion":1,"values":{"oauth":null}}),
        json!({"schemaVersion":1,"values":{"oauth.enabled":true}}),
        json!({"schemaVersion":1,"values":{"oauth":{}}}),
        json!({"schemaVersion":1,"values":[],"sources":[]}),
        json!({"schemaVersion":1}),
        json!({"schemaVersion":1,"values":{},"sources":null}),
        json!({"schemaVersion":1,"values":{},"sources":[{"path":proposed_source,"createIfMissing":true},{"path":"bad","extra":true}]}),
        json!({"schemaVersion":1,"values":{},"sources":[{"path":"x","createIfMissing":"yes"}]}),
        json!({"schemaVersion":1,"values":{},"sources":[{"path":"x"},{"path":"./x"}]}),
    ];
    for value in invalid {
        response(root, value);
        let before = count(root);
        assert!(defaults::propose(root, &json!({}), 10, &output).is_err());
        assert_eq!(count(root), before + 1, "Invalid output must not retry the generator");
        assert!(!proposed_source.exists());
        assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), original);
    }
    for bytes in [
        b"{\"schemaVersion\":1,\"values\":{\"project\":\"a\",\"project\":\"b\"}}".to_vec(),
        b"{\"schemaVersion\":1,\"values\":{}} {}".to_vec(),
        b"not json".to_vec(),
        vec![b' '; 1_048_577],
    ] {
        fs::write(root.join("response.json"), bytes).unwrap();
        let before = count(root);
        assert!(defaults::propose(root, &json!({}), 10, &output).is_err());
        assert_eq!(count(root), before + 1);
        assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), original);
    }
    response(root, valid);
    assert!(defaults::propose(root, &local, 10, &output).is_err(), "Explicit empty sources cannot be overwritten");
    assert!(!proposed_source.exists());

    response(root, json!({"schemaVersion":1,"manual":{"answer":42}}));
    let before = count(root);
    assert_eq!(commands::run(root, "defaults", 10, &output).unwrap()["manual"]["answer"], 42);
    assert_eq!(count(root), before + 1);
    let manual: Value = serde_json::from_str(&fs::read_to_string(root.join("context.json")).unwrap()).unwrap();
    assert_eq!(manual["purpose"], "manual");
    assert_eq!(manual["missingFields"], json!(["project"]));
    assert_eq!(manual["sourcesDeclared"], true);
    assert_eq!(fs::read_to_string(root.join("env.yaml")).unwrap(), original);
}

#[test]
fn named_commands_reject_invalid_execution_declarations() {
    for declaration in [
        json!({"argv":"echo unsafe"}),
        json!({"argv":[]}),
        json!({"argv":[""]}),
        json!({"argv":["echo",12]}),
        json!({"argv":["echo","\u{0}"]}),
        json!({"argv":["echo"],"timeoutSeconds":0}),
        json!({"argv":["echo"],"timeoutSeconds":301}),
        json!({"argv":["echo"],"timeoutSeconds":"30"}),
    ] {
        assert!(commands::declaration(&json!({"commands":{"bad":declaration}}), "bad").is_err());
    }
    assert_eq!(commands::declaration(&json!({"commands":{"ok":{"argv":["echo"]}}}), "ok").unwrap().1, 30);
}
