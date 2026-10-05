use dockstride::defaults;
use serde_json::json;
use std::fs;

#[test]
fn invalid_defaults_declarations_fail_before_command_execution() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for declaration in [
        r#"{ command = "missing", fields = ["project"] }"#,
        r#"{ command = "hook", fields = "project" }"#,
        r#"{ command = "hook", fields = ["secrets.api"] }"#,
        r#"{ command = "hook", fields = ["_dockstride.sources"] }"#,
        r#"{ command = "hook", fields = ["project", "project"] }"#,
        r#"{ command = "hook", fields = ["oauth", "oauth.enabled"] }"#,
        r#"{ command = "hook", fields = ["oauth..enabled"] }"#,
        r#"{ command = "hook", fields = ["project"], sources = "yes" }"#,
        r#"{ command = "hook", fields = ["project"], extra = true }"#,
    ] {
        fs::write(root.join("compose.ncl"), format!(r#"
{{
  dockstride | not_exported = {{
    Config = {{project | String}},
    commands.hook = {{argv = ["touch", "unexpected-execution"]}},
    setup.defaults = {declaration},
  }},
  services.api.image = (import "env.yaml").project,
}}
"#)).unwrap();
        assert!(defaults::plan(root, &json!({})).is_err(), "Accepted invalid declaration: {declaration}");
        assert!(!root.join("unexpected-execution").exists());
        assert!(!root.join("env.yaml").exists());
    }
}
