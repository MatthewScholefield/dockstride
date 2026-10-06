use crate::{nickel, registry, sources, state};
use crate::secret_protection::observe;
use serde_json::{Value, json};
use std::{fs, os::unix::fs::{PermissionsExt, symlink}, path::{Path, PathBuf}, process::Command};
use tempfile::TempDir;

fn isolated(name: &str, run: impl FnOnce()) {
    if std::env::var("DKS_PROTECTION_CHILD").as_deref() == Ok(name) { run(); return; }
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap()).args(["--exact", &format!("secret_protection_tests::{name}"), "--nocapture"])
        .env("DKS_PROTECTION_CHILD", name).env("HOME", home.path()).env("XDG_DATA_HOME", home.path().join("data"))
        .output().unwrap();
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
struct Fixture { temp: TempDir, primary: PathBuf, consumer: PathBuf, old: PathBuf, current: PathBuf }
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("primary");
        let consumer = temp.path().join("consumer");
        for (root, owner) in [(&primary, "primary-owner"), (&consumer, "consumer-owner")] {
            fs::create_dir(root).unwrap();
            fs::write(root.join("compose.ncl"), r#"let env = import "env.yaml" in
{dockstride | not_exported = {setup.secrets = if std.record.has_field "input" env then
{token = {kind = "file", path = env.input}} else {}}}"#).unwrap();
            fs::write(root.join("env.yaml"), "project: fixture\nbackend: compose\n").unwrap();
            state::save(root, "identity", &json!({"id":owner,"root":root,"backend":"compose"})).unwrap();
            state::save(root, "secrets", &json!({"revisions":[]})).unwrap();
        }
        let store = temp.path().join("private-store");
        fs::create_dir(&store).unwrap();
        let old = store.join("old-revision");
        let current = store.join("current-revision");
        for path in [&old, &current] {
            fs::write(path, "credential bytes must never be read by retention").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0)).unwrap();
        }
        fs::set_permissions(&store, fs::Permissions::from_mode(0o300)).unwrap();
        let fixture = Self { temp, primary, consumer, old, current };
        fixture.register(&[&fixture.primary, &fixture.consumer]);
        fixture.environment(&fixture.primary, json!({"project":"fixture","backend":"compose","secrets":{"token":{"file":fixture.current}}}));
        fixture
    }
    fn environment(&self, root: &Path, value: Value) {
        fs::write(root.join("env.yaml"), serde_yaml::to_string(&value).unwrap()).unwrap();
    }
    fn register(&self, roots: &[&Path]) {
        let mut entries = serde_json::Map::new();
        for root in roots {
            let owner = if *root == self.primary { "primary-owner" } else { "consumer-owner" };
            entries.insert(root.to_str().unwrap().into(), json!({"root":root,"ownerId":owner,"project":"fixture",
                "backend":"compose","connection":"fixture","daemonId":"fixture-daemon","state":"committed",
                "sourceFiles":[],"allocatedEndpoints":{}}));
        }
        state::save(&state::global_root().unwrap(), "environment-registry", &json!({"schemaVersion":1,"environments":entries})).unwrap();
    }
    fn reference(&self) -> Value { json!({"file":self.old}) }
    fn revision(&self, reference: Value, source: Option<&Path>, pending: bool) -> Value {
        let mut revision = json!({"logical":"token","reference":reference,"pending":pending,"deleted":false});
        if let Some(source) = source {
            revision["fileSource"] = json!({"kind":"file","canonicalPath":source,"origin":"cli-file","keyedDigest":"private-digest-never-output"});
        }
        revision
    }
    fn history(&self, root: &Path, revisions: Vec<Value>) {
        state::save(root, "secrets", &json!({"revisions":revisions})).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::set_permissions(self.temp.path().join("private-store"), fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[test]
fn declared_shared_source_survives_primary_rotation_and_explicit_forget_releases_it() {
    isolated("declared_shared_source_survives_primary_rotation_and_explicit_forget_releases_it", || {
        let f = Fixture::new();
        let shared = f.temp.path().join("shared.yaml");
        fs::write(&shared, serde_yaml::to_string(&json!({"input":f.old})).unwrap()).unwrap();
        f.environment(&f.consumer, json!({"project":"fixture","backend":"swarm","_dockstride":{"sources":[{"path":shared}]}}));
        let protection = observe(&f.primary).unwrap();
        assert!(protection.reason(&f.reference()).unwrap().contains("declared file source for token"));
        protection.verify().unwrap();
        f.register(&[&f.primary]);
        assert!(protection.verify().is_err());
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).is_none());
    });
}

#[test]
fn shared_references_remain_protected_behind_local_overrides_until_source_changes() {
    isolated("shared_references_remain_protected_behind_local_overrides_until_source_changes", || {
        let f = Fixture::new();
        let shared = f.temp.path().join("shared.yaml");
        fs::write(&shared, serde_yaml::to_string(&json!({"secrets":{"token":f.reference()}})).unwrap()).unwrap();
        let inherited = json!({"project":"fixture","backend":"compose","_dockstride":{"sources":[{"path":shared}]}});
        f.environment(&f.consumer, inherited.clone());
        let protection = observe(&f.primary).unwrap();
        assert!(protection.reason(&f.reference()).is_some());
        protection.verify().unwrap();

        let mut overridden = inherited.clone();
        overridden["secrets"] = json!({"token":{"file":f.current}});
        f.environment(&f.consumer, overridden);
        assert!(protection.verify().is_err());
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).is_some());

        f.environment(&f.consumer, inherited);
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).is_some());
        fs::write(&shared, serde_yaml::to_string(&json!({"secrets":{"token":{"file":f.current}}})).unwrap()).unwrap();
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).is_none());
        assert!(f.old.exists());
    });
}

#[test]
fn only_current_import_origins_protect_and_stdin_does_not_inherit_old_origin() {
    isolated("only_current_import_origins_protect_and_stdin_does_not_inherit_old_origin", || {
        let f = Fixture::new();
        let current = json!({"file":f.current});
        f.history(&f.primary, vec![f.revision(current.clone(), Some(&f.old), false)]);
        let protection = observe(&f.primary).unwrap();
        let reason = protection.reason(&f.reference()).unwrap();
        assert!(reason.contains("current imported source for token"));
        assert!(!reason.contains("private-digest"));
        f.history(&f.primary, vec![f.revision(f.reference(), Some(&f.old), false), f.revision(current, None, false)]);
        assert!(protection.verify().is_err());
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).is_none());
        f.environment(&f.primary, json!({"project":"fixture","backend":"compose","input":f.old,"secrets":{"token":{"file":f.current}}}));
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).unwrap().contains("declared file source"));
    });
}

#[test]
fn current_pending_snapshot_and_native_aliases_protect_exact_candidates() {
    isolated("current_pending_snapshot_and_native_aliases_protect_exact_candidates", || {
        let f = Fixture::new();
        let alias = f.temp.path().join("alias");
        symlink(&f.old, &alias).unwrap();
        f.environment(&f.consumer, json!({"project":"fixture","secrets":{"token":{"file":alias}}}));
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).unwrap().contains("current environment reference"));
        f.environment(&f.consumer, json!({"project":"fixture"}));
        f.history(&f.consumer, vec![f.revision(f.reference(), None, true)]);
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).unwrap().contains("pending secret journal"));
        f.history(&f.consumer, vec![]);
        state::save(&f.consumer, "deployment", &json!({"desired":{"secrets":{"token":{"file":alias}}}})).unwrap();
        let protection = observe(&f.primary).unwrap();
        assert!(protection.reason(&f.reference()).unwrap().contains("retained deployment or operation snapshot"));
        fs::remove_file(&alias).unwrap();
        symlink(&f.current, &alias).unwrap();
        assert!(protection.verify().is_err());
        fs::remove_file(f.consumer.join(".dockstride/deployment.json")).unwrap();
        state::save(&f.consumer, "deployment", &json!({"desired":{"secrets":{"token":{"name":"swarm-old","external":true}}}})).unwrap();
        assert!(observe(&f.primary).unwrap().reason(&json!({"name":"swarm-old","external":true})).is_some());
    });
}

#[test]
fn stale_and_unsafe_registered_checkouts_fail_closed() {
    isolated("stale_and_unsafe_registered_checkouts_fail_closed", || {
        let f = Fixture::new();
        fs::rename(&f.consumer, f.temp.path().join("moved-consumer")).unwrap();
        let protection = observe(&f.primary).unwrap();
        assert!(protection.reason(&f.reference()).unwrap().contains("cannot exclude secret consumers"));
        f.register(&[&f.primary]);
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).is_none());
        fs::rename(f.temp.path().join("moved-consumer"), &f.consumer).unwrap();
        f.register(&[&f.primary, &f.consumer]);
        fs::set_permissions(f.consumer.join(".dockstride/secrets.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).unwrap().contains("unsafe retention state"));
        fs::set_permissions(f.consumer.join(".dockstride/secrets.json"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(f.consumer.join(".dockstride/secrets.json")).unwrap();
        symlink(f.primary.join(".dockstride/secrets.json"), f.consumer.join(".dockstride/secrets.json")).unwrap();
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).is_some());
    });
}

#[test]
fn external_source_history_and_operation_edits_invalidate_observations() {
    isolated("external_source_history_and_operation_edits_invalidate_observations", || {
        let f = Fixture::new();
        let shared = f.temp.path().join("shared.yaml");
        fs::write(&shared, "ordinary: before\n").unwrap();
        f.environment(&f.consumer, json!({"project":"fixture","_dockstride":{"sources":[{"path":shared}]}}));
        let protection = observe(&f.primary).unwrap();
        fs::write(&shared, "ordinary: after\n").unwrap();
        assert!(protection.verify().is_err());
        let protection = observe(&f.primary).unwrap();
        f.history(&f.consumer, vec![f.revision(f.reference(), None, true)]);
        assert!(protection.verify().is_err());
        f.history(&f.consumer, vec![]);
        let protection = observe(&f.primary).unwrap();
        state::save(&f.consumer, "new-operation", &json!({"secrets":{"token":f.reference()}})).unwrap();
        assert!(protection.verify().is_err());
        let protection = observe(&f.primary).unwrap();
        state::save(&f.consumer, "new-operation", &json!({"secrets":{}})).unwrap();
        assert!(protection.verify().is_err());
        let protection = observe(&f.primary).unwrap();
        fs::write(f.consumer.join("compose.ncl"), "{dockstride={setup.secrets.token={kind=\"file\",path=\"missing-file\"}}}").unwrap();
        assert!(protection.verify().is_err());
    });
}

#[test]
fn gc_history_refresh_accepts_only_its_single_unprotected_deletion() {
    isolated("gc_history_refresh_accepts_only_its_single_unprotected_deletion", || {
        let f = Fixture::new();
        let old = f.revision(f.reference(), None, false);
        f.history(&f.primary, vec![old.clone()]);
        let mut protection = observe(&f.primary).unwrap();
        assert!(protection.reason(&f.reference()).is_none());
        let mut deleted = old.clone();
        deleted["deleted"] = json!(true);
        f.history(&f.primary, vec![deleted]);
        assert!(protection.verify().is_err());
        protection.refresh_own_history(&f.primary).unwrap();
        protection.verify().unwrap();
        f.history(&f.consumer, vec![f.revision(f.reference(), None, false)]);
        let mut foreign_deleted = f.revision(f.reference(), None, false);
        foreign_deleted["deleted"] = json!(true);
        f.history(&f.consumer, vec![foreign_deleted]);
        assert!(protection.refresh_own_history(&f.consumer).is_err());
        f.history(&f.consumer, vec![]);
        f.history(&f.primary, vec![old]);
        let mut protection = observe(&f.primary).unwrap();
        let mut edited = f.revision(f.reference(), None, false);
        edited["deleted"] = json!(true);
        edited["logical"] = json!("another-secret");
        f.history(&f.primary, vec![edited]);
        assert!(protection.refresh_own_history(&f.primary).is_err());
        f.history(&f.primary, vec![f.revision(json!({"file":f.current}), None, false)]);
        let mut protection = observe(&f.primary).unwrap();
        let mut deleted_current = f.revision(json!({"file":f.current}), None, false);
        deleted_current["deleted"] = json!(true);
        f.history(&f.primary, vec![deleted_current]);
        assert!(protection.refresh_own_history(&f.primary).is_err());
    });
}

#[test]
fn unregistered_invoking_checkout_relative_sources_and_hardlinks_remain_protected() {
    isolated("unregistered_invoking_checkout_relative_sources_and_hardlinks_remain_protected", || {
        let f = Fixture::new();
        f.register(&[]);
        let alias = f.primary.join("relative-source");
        fs::hard_link(&f.old, &alias).unwrap();
        f.environment(&f.primary, json!({"project":"fixture","input":"relative-source",
            "secrets":{"token":{"file":f.current}}}));
        let protection = observe(&f.primary).unwrap();
        assert!(protection.reason(&f.reference()).unwrap().contains("declared file source for token"));
        protection.verify().unwrap();
        f.environment(&f.primary, json!({"project":"fixture","secrets":{"token":{"file":f.current}}}));
        assert!(protection.verify().is_err());
        assert!(observe(&f.primary).unwrap().reason(&f.reference()).is_none());
    });
}

#[test]
fn missing_registered_configuration_and_credential_store_bookkeeping_fail_closed() {
    isolated("missing_registered_configuration_and_credential_store_bookkeeping_fail_closed", || {
        let f = Fixture::new();
        fs::remove_file(f.consumer.join("env.yaml")).unwrap();
        let protection = observe(&f.primary).unwrap();
        assert!(protection.reason(&f.reference()).unwrap().contains("configuration is missing"));
        f.environment(&f.consumer, json!({"project":"fixture"}));
        let store = f.consumer.join(".dockstride/secrets");
        fs::create_dir(&store).unwrap();
        fs::set_permissions(&store, fs::Permissions::from_mode(0o300)).unwrap();
        let protection = observe(&f.primary).unwrap();
        assert!(protection.reason(&f.reference()).unwrap().contains("cannot verify private retained operation bookkeeping"));
        fs::set_permissions(store, fs::Permissions::from_mode(0o700)).unwrap();
    });
}
