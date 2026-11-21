mod common;

use std::collections::BTreeMap;

use agyva::core::endpoints::NewEndpoint;
use agyva::core::error::ErrorKind;
use agyva::core::profile::{
    GlobalProfiles, ProfileSource, ProfileSpec, discover_local_profile, load_global_profiles,
    resolve_profile, save_global_profiles, set_profile_field, write_local_profile,
};
use agyva::core::protocol::EndpointProtocol;
use agyva::core::usage::RefreshMode;
use common::*;
use zeroize::Zeroizing;

#[test]
fn local_profile_discovery_stops_at_git_root_and_respects_depth() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path();
    let repo = outside.join("repo");
    let nested = repo.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir(repo.join(".git")).unwrap();

    // Profile outside git root must NOT be discovered from inside repo.
    write_local_profile(
        outside,
        &ProfileSpec {
            name: Some("outer".into()),
            ..ProfileSpec::default()
        },
    )
    .unwrap();
    assert!(discover_local_profile(&nested).unwrap().is_none());

    // Profile at git root IS discovered from nested subdir.
    write_local_profile(
        &repo,
        &ProfileSpec {
            name: Some("repo-prof".into()),
            account: Some("alice".into()),
            warn_percent: Some(30.0),
            critical_percent: Some(10.0),
            monitor_interval: Some("30s".into()),
            ..ProfileSpec::default()
        },
    )
    .unwrap();
    let found = discover_local_profile(&nested).unwrap().unwrap();
    assert_eq!(found.spec.name.as_deref(), Some("repo-prof"));
    assert_eq!(found.path, repo.join(".agyva.toml"));

    // Depth bound: 35 levels deep without .git must not discover profile 33 levels up.
    let deep_root = outside.join("deep");
    std::fs::create_dir_all(&deep_root).unwrap();
    write_local_profile(
        &deep_root,
        &ProfileSpec {
            name: Some("too-far".into()),
            ..ProfileSpec::default()
        },
    )
    .unwrap();
    let mut deep = deep_root.clone();
    for i in 0..34 {
        deep = deep.join(format!("d{i}"));
    }
    std::fs::create_dir_all(&deep).unwrap();
    assert!(discover_local_profile(&deep).unwrap().is_none());
}

#[test]
fn profile_rejects_secrets_unknown_fields_and_invalid_values() {
    let dir = tempfile::tempdir().unwrap();

    for key in [
        "OPENAI_API_KEY",
        "my_secret_val",
        "AUTH_TOKEN",
        "DB_PASSWORD",
        "vault_passphrase",
    ] {
        let mut spec = ProfileSpec::default();
        spec.env.insert(key.into(), "value".into());
        let err = spec.validate().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Refused, "{key}");
        assert!(
            err.hint_text()
                .is_some_and(|h| h.contains("endpoint add") || h.contains("vault")),
            "{key}: {:?}",
            err.hint_text()
        );
    }

    // Invalid thresholds & empty fields & invalid monitor_interval
    for bad in [
        ProfileSpec {
            warn_percent: Some(101.0),
            ..ProfileSpec::default()
        },
        ProfileSpec {
            critical_percent: Some(-1.0),
            ..ProfileSpec::default()
        },
        ProfileSpec {
            warn_percent: Some(10.0),
            critical_percent: Some(20.0),
            ..ProfileSpec::default()
        },
        ProfileSpec {
            account: Some("  ".into()),
            ..ProfileSpec::default()
        },
        ProfileSpec {
            monitor_interval: Some("1s".into()),
            ..ProfileSpec::default()
        },
    ] {
        assert!(bad.validate().is_err());
    }

    // Unknown fields in .agyva.toml are rejected.
    let bad_path = dir.path().join(".agyva.toml");
    std::fs::write(&bad_path, "exec = \"rm -rf /\"\n").unwrap();
    assert!(discover_local_profile(dir.path()).is_err());
}

#[test]
fn global_and_local_profile_resolution_and_app_wiring() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();

    let mut ep_spec = NewEndpoint::default();
    ep_spec.alias = "local-llm".into();
    ep_spec.base_url = "http://127.0.0.1:11434/v1".into();
    ep_spec.protocol = EndpointProtocol::OpenAi;
    ep_spec.api_key = Zeroizing::new("sk-secret-ep-key".into());
    ep_spec.default_model = Some("qwen3-32b".into());
    let ep = app.add_endpoint(ep_spec).unwrap();
    assert_eq!(ep.alias, "local-llm");

    // Global profiles.toml roundtrip & precedence
    let mut global = GlobalProfiles::default();
    let mut work_spec = ProfileSpec {
        description: Some("Work profile".into()),
        account: Some("alice".into()),
        warn_percent: Some(30.0),
        critical_percent: Some(10.0),
        env: BTreeMap::from([("AGYVA_FOO".into(), "bar".into())]),
        ..ProfileSpec::default()
    };
    set_profile_field(&mut work_spec, "model", Some("gemini-2.5-pro")).unwrap();
    global.profiles.insert("work".into(), work_spec);
    global.default = Some("work".into());
    let profiles_path = rig.dirs().config.join("profiles.toml");
    save_global_profiles(&profiles_path, &global).unwrap();
    assert_eq!(load_global_profiles(&profiles_path).unwrap(), global);

    let proj_dir = rig.tmp.path().join("project");
    std::fs::create_dir_all(&proj_dir).unwrap();

    // Without local .agyva.toml -> resolves global default
    let r = resolve_profile(&rig.dirs(), &proj_dir, None)
        .unwrap()
        .unwrap();
    assert_eq!(r.source, ProfileSource::Global);
    assert_eq!(r.name.as_deref(), Some("work"));

    // Create local .agyva.toml via App::init_profile -> overrides global default
    app.init_profile(
        None,
        Some(&proj_dir),
        ProfileSpec {
            name: Some("proj".into()),
            endpoint: Some("local-llm".into()),
            ..ProfileSpec::default()
        },
        false,
    )
    .unwrap();
    let r2 = app
        .resolve_active_profile(None, Some(&proj_dir))
        .unwrap()
        .unwrap();
    assert_eq!(r2.source, ProfileSource::Local);
    assert_eq!(r2.spec.endpoint.as_deref(), Some("local-llm"));

    // Explicit name beats local .agyva.toml
    let r3 = app
        .resolve_active_profile(Some("work"), Some(&proj_dir))
        .unwrap()
        .unwrap();
    assert_eq!(r3.source, ProfileSource::Explicit);
    assert_eq!(r3.spec.account.as_deref(), Some("alice"));

    // Use local profile (activates custom endpoint)
    let used_ep = app.use_profile("proj", Some(&proj_dir), false).unwrap();
    assert_eq!(used_ep.active_endpoint.unwrap().alias, "local-llm");

    // Statusline & Dashboard reflect endpoint + profile without leaking secrets
    let sl = app.statusline(None, Some(&proj_dir)).unwrap();
    assert_eq!(sl.active_endpoint.as_deref(), Some("local-llm"));
    assert_eq!(sl.active_profile.as_deref(), Some("proj"));
    let sl_text = sl.render("compact").unwrap();
    assert!(sl_text.contains("ep:local-llm"));
    assert!(!sl_text.contains("sk-secret-ep-key"));

    let dash = app.dashboard(RefreshMode::Skip).unwrap();
    assert_eq!(dash.active_endpoint.as_deref(), Some("local-llm"));
    assert!(!dash.profiles.is_empty());
    let dash_json = serde_json::to_string(&dash).unwrap();
    assert!(!dash_json.contains("sk-secret-ep-key"));

    // Unified use switches back to OAuth account and clears active_endpoint + agy custom provider
    app.unified_use("bob", false).unwrap();
    assert!(app.active_endpoint_record().unwrap().is_none());
    let settings =
        std::fs::read_to_string(rig.agy_home().join("settings.json")).unwrap_or_default();
    assert!(!settings.contains("customModelsConfig"));

    // Mutate & delete global profile
    app.set_profile_field(Some("work"), None, "env.EXTRA", Some("1"))
        .unwrap();
    app.delete_profile("work").unwrap();
    assert!(
        app.resolve_active_profile(Some("work"), Some(&proj_dir))
            .is_err()
    );
}
