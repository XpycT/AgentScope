mod support;

use std::ffi::OsString;
use std::path::PathBuf;

use agentscope::config::{CliOverrides, Config, ConfigError, Env};
use agentscope::domain::{
    Harness, InputAccounting, MoneySource, OutputAccounting, TokenAccounting, TokenCategories,
};
use agentscope::pricing::{ReferencePolicy, estimate_cost};
use support::facts::usage;
use tempfile::TempDir;

fn env(pairs: &[(&str, &str)]) -> Env {
    pairs
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

/// Writes `toml` to a temp file and loads it with an explicit env map.
fn try_load(cli: CliOverrides, env: &Env, toml: &str) -> Result<Config, ConfigError> {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, toml).unwrap();
    let cli = CliOverrides {
        config: Some(path),
        ..cli
    };
    Config::load_with_env(&cli, env)
}

fn load_from(toml: &str) -> Result<Config, ConfigError> {
    try_load(CliOverrides::default(), &env(&[("HOME", "/home/u")]), toml)
}

fn ok(toml: &str) -> Config {
    load_from(toml).unwrap()
}

#[test]
fn defaults_match_spec() {
    let cfg = ok("");
    assert_eq!(cfg.refresh_ms, 2000);
    assert_eq!(cfg.process_ms, 5000);
    assert!(cfg.process_enabled);
    assert_eq!(cfg.quiet_after_secs, 300);
    assert_eq!(cfg.stall_after_secs, 900);
    assert_eq!(cfg.max_cached_sessions, 2000);
    assert_eq!(cfg.max_page_events, 500);
    assert_eq!(cfg.max_page_bytes, 262_144);
    assert_eq!(cfg.max_record_bytes, 1_048_576);
    assert_eq!(cfg.dedup_ledger_entries, 100_000);
    assert_eq!(cfg.dedup_ledger_bytes, 8_388_608);
    assert_eq!(cfg.refresh_busy_timeout_ms, 2000);
    assert_eq!(cfg.page_busy_timeout_ms, 250);
    assert!(cfg.redaction);
    assert!(cfg.pricing.cards("codex", "x").is_none());
}

#[test]
fn default_provider_paths_come_from_home_and_xdg() {
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/home/u")]), "").unwrap();
    assert_eq!(
        cfg.harnesses.codex.root,
        Some(PathBuf::from("/home/u").join(".codex").join("sessions"))
    );
    assert_eq!(
        cfg.harnesses.claude.root,
        Some(PathBuf::from("/home/u").join(".claude").join("projects"))
    );
    assert_eq!(
        cfg.harnesses.opencode.db,
        Some(
            PathBuf::from("/home/u")
                .join(".local")
                .join("share")
                .join("opencode")
                .join("opencode.db")
        )
    );

    let cfg = try_load(
        CliOverrides::default(),
        &env(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "/xdg")]),
        "",
    )
    .unwrap();
    assert_eq!(
        cfg.harnesses.opencode.db,
        Some(PathBuf::from("/xdg").join("opencode").join("opencode.db"))
    );
}

#[test]
fn file_values_override_defaults() {
    let cfg = ok(
        "refresh_ms = 100\nredaction = false\nprocess_enabled = false\nmax_cached_sessions = 7\n",
    );
    assert_eq!(cfg.refresh_ms, 100);
    assert!(!cfg.redaction);
    assert!(!cfg.process_enabled);
    assert_eq!(cfg.max_cached_sessions, 7);
}

#[test]
fn malformed_non_usd_and_bad_thresholds_are_fatal() {
    assert!(matches!(
        load_from("currency = \"EUR\""),
        Err(ConfigError::Currency(_))
    ));
    assert!(load_from("currency = \"USD\"").is_ok());
    assert!(load_from("quiet_after_secs = 900\nstall_after_secs = 300").is_err());
    assert!(load_from("quiet_after_secs = 300\nstall_after_secs = 300").is_err());
    assert!(matches!(
        load_from("this is not toml"),
        Err(ConfigError::Parse { .. })
    ));
    assert!(load_from("refresh_mss = 5").is_err());
}

#[test]
fn zero_intervals_and_caps_are_rejected() {
    for key in [
        "refresh_ms",
        "process_ms",
        "quiet_after_secs",
        "max_cached_sessions",
        "max_page_events",
        "max_page_bytes",
        "max_record_bytes",
        "dedup_ledger_entries",
        "dedup_ledger_bytes",
        "refresh_busy_timeout_ms",
        "page_busy_timeout_ms",
    ] {
        assert!(load_from(&format!("{key} = 0")).is_err(), "{key}");
    }
    assert!(load_from("stall_after_secs = 0").is_err());
}

#[test]
fn cli_beats_env_beats_file_beats_defaults() {
    let file = "[providers.codex]\nroot = \"/file\"\n[providers.claude]\nroot = \"/file-claude\"\n[providers.opencode]\ndb = \"/file.db\"\n";

    // File beats defaults.
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/h")]), file).unwrap();
    assert_eq!(cfg.harnesses.codex.root, Some(PathBuf::from("/file")));
    assert_eq!(
        cfg.harnesses.claude.root,
        Some(PathBuf::from("/file-claude"))
    );
    assert_eq!(cfg.harnesses.opencode.db, Some(PathBuf::from("/file.db")));

    // Env beats file; homes get a suffix appended.
    let e = env(&[
        ("HOME", "/h"),
        ("CODEX_HOME", "/env-codex"),
        ("CLAUDE_CONFIG_DIR", "/env-claude"),
        ("XDG_DATA_HOME", "/xdg"),
    ]);
    let cfg = try_load(CliOverrides::default(), &e, file).unwrap();
    assert_eq!(
        cfg.harnesses.codex.root,
        Some(PathBuf::from("/env-codex").join("sessions"))
    );
    assert_eq!(
        cfg.harnesses.claude.root,
        Some(PathBuf::from("/env-claude").join("projects"))
    );
    assert_eq!(
        cfg.harnesses.opencode.db,
        Some(PathBuf::from("/xdg").join("opencode").join("opencode.db"))
    );

    // CLI beats env, and is an exact root (no suffix).
    let cli = CliOverrides {
        codex_sessions: Some(PathBuf::from("/cli")),
        claude_projects: Some(PathBuf::from("/cli-claude")),
        opencode_db: Some(PathBuf::from("/cli.db")),
        ..CliOverrides::default()
    };
    let cfg = try_load(cli, &e, file).unwrap();
    assert_eq!(cfg.harnesses.codex.root, Some(PathBuf::from("/cli")));
    assert_eq!(
        cfg.harnesses.claude.root,
        Some(PathBuf::from("/cli-claude"))
    );
    assert_eq!(cfg.harnesses.opencode.db, Some(PathBuf::from("/cli.db")));
}

#[test]
fn empty_env_values_are_ignored() {
    let e = env(&[("HOME", "/h"), ("CODEX_HOME", ""), ("XDG_DATA_HOME", "")]);
    let cfg = try_load(
        CliOverrides::default(),
        &e,
        "[providers.codex]\nroot = \"/file\"\n",
    )
    .unwrap();
    assert_eq!(cfg.harnesses.codex.root, Some(PathBuf::from("/file")));
    assert_eq!(
        cfg.harnesses.opencode.db,
        Some(
            PathBuf::from("/h")
                .join(".local")
                .join("share")
                .join("opencode")
                .join("opencode.db")
        )
    );
}

#[test]
fn no_processes_no_color_and_config_path_are_preserved() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("c.toml");
    std::fs::write(&path, "process_enabled = true\n").unwrap();
    let cli = CliOverrides {
        config: Some(path.clone()),
        no_processes: true,
        no_color: true,
        ..CliOverrides::default()
    };
    let cfg = Config::load_with_env(&cli, &env(&[("HOME", "/h")])).unwrap();
    assert!(!cfg.process_enabled, "--no-processes beats file");
    assert_eq!(cfg.config_path, Some(path));
    assert!(cfg.no_color);

    let cfg = ok("");
    assert!(cfg.process_enabled);
    assert!(!cfg.no_color);
}

#[test]
fn missing_default_config_is_fine_but_explicit_missing_is_an_error() {
    let dir = TempDir::new().unwrap();
    let e = env(&[
        ("HOME", dir.path().to_str().unwrap()),
        ("XDG_CONFIG_HOME", dir.path().to_str().unwrap()),
        ("APPDATA", dir.path().to_str().unwrap()),
    ]);
    let cfg = Config::load_with_env(&CliOverrides::default(), &e).unwrap();
    assert_eq!(cfg.refresh_ms, 2000);

    let cli = CliOverrides {
        config: Some(dir.path().join("nope.toml")),
        ..CliOverrides::default()
    };
    assert!(matches!(
        Config::load_with_env(&cli, &e),
        Err(ConfigError::Io { .. })
    ));
}

#[test]
fn default_config_path_is_loaded_from_platform_location() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_str().unwrap();
    let e = env(&[("HOME", root), ("XDG_CONFIG_HOME", root), ("APPDATA", root)]);
    let cfg_dir = if cfg!(target_os = "macos") {
        dir.path().join("Library").join("Application Support")
    } else {
        dir.path().to_path_buf()
    }
    .join("agentscope");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "refresh_ms = 123\n").unwrap();
    let cfg = Config::load_with_env(&CliOverrides::default(), &e).unwrap();
    assert_eq!(cfg.refresh_ms, 123);
}

const USER_PRICES: &str = r#"
[[pricing]]
billing_provider = "anthropic"
model = "claude-sonnet-5-5"
as_of = "2026-10-01"
input_per_mtok = 3.0
output_per_mtok = 15.0

[[pricing]]
billing_provider = "acme"
model = "house-model"
cached_input_per_mtok = 0.125
cache_write_per_mtok = 1.25
cache_write_5m_per_mtok = 1.5
cache_write_1h_per_mtok = 2.5
input_per_mtok = 1.0
output_per_mtok = 2.0
reasoning_per_mtok = 3.0
"#;

#[test]
fn exact_user_cards_replace_matching_bundled_cards_only() {
    let bundled = ok("");
    let stock = bundled
        .pricing
        .cards("anthropic", "claude-sonnet-5-5")
        .unwrap();
    assert!(
        stock[0].rates.cache_read.is_some(),
        "bundled card carries a cache price"
    );
    let other = bundled
        .pricing
        .cards("anthropic", "claude-opus-5-5")
        .unwrap()
        .to_vec();

    let cfg = ok(USER_PRICES);
    let replaced = cfg.pricing.cards("anthropic", "claude-sonnet-5-5").unwrap();
    assert_eq!(replaced.len(), 1);
    let card = &replaced[0];
    assert_eq!(card.rates.input, Some(3.0));
    assert_eq!(card.rates.output, Some(15.0));
    assert_eq!(card.as_of.as_deref(), Some("2026-10-01"));
    assert_eq!(
        card.rates.cache_read, None,
        "missing legs are not inherited"
    );
    assert_eq!(card.rates.cache_write_5m, None);
    // Other bundled cards are untouched.
    assert_eq!(
        cfg.pricing.cards("anthropic", "claude-opus-5-5").unwrap(),
        other.as_slice()
    );

    let house = &cfg.pricing.cards("acme", "house-model").unwrap()[0];
    assert_eq!(house.rates.cache_read, Some(0.125));
    assert_eq!(house.rates.cache_write, Some(1.25));
    assert_eq!(house.rates.cache_write_5m, Some(1.5));
    assert_eq!(house.rates.cache_write_1h, Some(2.5));
    assert_eq!(house.rates.reasoning, Some(3.0));

    // Exact: no case folding, prefixes, trailing space, or cross-provider fallback.
    for (provider, model) in [
        ("acme", "HOUSE-MODEL"),
        ("acme", "house"),
        ("acme", "house-model "),
        ("ACME", "house-model"),
        ("codex", "house-model"),
    ] {
        assert!(
            cfg.pricing.cards(provider, model).is_none(),
            "{provider}/{model}"
        );
    }

    let dup = format!(
        "{USER_PRICES}\n[[pricing]]\nbilling_provider = \"acme\"\nmodel = \"house-model\"\ninput_per_mtok = 9.0\noutput_per_mtok = 9.0\n"
    );
    assert!(matches!(
        load_from(&dup),
        Err(ConfigError::DuplicatePrice { .. })
    ));
}

#[test]
fn invalid_price_entries_are_rejected() {
    let bad = [
        "billing_provider = \"acme\"\nmodel = \"\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0",
        "billing_provider = \"acme\"\nmodel = \"  \"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0",
        "billing_provider = \"\"\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0",
        "model = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0",
        "billing_provider = \"acme\"\nmodel = \"m\"\ninput_per_mtok = -1.0\noutput_per_mtok = 1.0",
        "billing_provider = \"acme\"\nmodel = \"m\"\ninput_per_mtok = 1.0",
        "billing_provider = \"acme\"\nmodel = \"m\"\ninput_per_mtok = nan\noutput_per_mtok = 1.0",
        "billing_provider = \"acme\"\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = inf",
        "billing_provider = \"acme\"\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0\nbogus = 1",
        "billing_provider = \"acme\"\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0\ncached_input_per_mtok = 1.0\ncache_read_per_mtok = 2.0",
        "billing_provider = \"acme\"\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0\neffective_from = \"not a date\"",
    ];
    for entry in bad {
        let toml = format!("[[pricing]]\n{entry}\n");
        assert!(load_from(&toml).is_err(), "{entry}");
    }
}

#[test]
fn removed_keys_fail_with_the_replacement_name_and_are_not_rewritten() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    let cli = CliOverrides {
        config: Some(path.clone()),
        ..CliOverrides::default()
    };
    for (text, expected) in [
        (
            "[[pricing]]\nprovider = \"claude\"\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0\n",
            "billing_provider",
        ),
        ("max_sessions = 5\n", "max_cached_sessions"),
    ] {
        std::fs::write(&path, text).unwrap();
        let error = Config::load_with_env(&cli, &env(&[("HOME", "/h")])).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(expected), "{message}");
        assert!(message.contains("never rewritten"), "{message}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "file untouched"
        );
    }
}

#[test]
fn bundled_prices_load_without_user_config() {
    let cfg = ok("");
    assert!(!cfg.pricing.is_empty());
    assert!(!cfg.pricing.catalog().inventory.is_empty());
}

#[test]
fn native_reference_estimates_are_on_by_default_and_can_be_disabled() {
    let cfg = ok("");
    assert!(cfg.native_api_reference_estimates);
    assert_eq!(
        cfg.reference_policy(Harness::Claude),
        ReferencePolicy::NativeApi(Harness::Claude)
    );
    assert_eq!(
        cfg.reference_policy(Harness::OpenCode),
        ReferencePolicy::Disabled
    );
    assert_eq!(
        cfg.reference_policy(Harness::Omp),
        ReferencePolicy::Disabled
    );

    let cfg = ok("native_api_reference_estimates = false\n");
    assert_eq!(
        cfg.reference_policy(Harness::Claude),
        ReferencePolicy::Disabled
    );
    // With the reference disabled, a native request with no recorded route stays unpriced.
    let cats = TokenCategories {
        input: Some(1_000_000),
        output: Some(1_000_000),
        cache_read: Some(0),
        cache_write: Some(0),
        ..TokenCategories::default()
    };
    let accounting = TokenAccounting {
        input: InputAccounting::AdditiveCaches,
        output: OutputAccounting::IncludesReasoning,
    };
    let bare = usage("r", None, "claude-sonnet-5-5", cats, accounting);
    let on = estimate_cost(
        &bare,
        &ok("").pricing,
        ok("").reference_policy(Harness::Claude),
    );
    assert!(on.usd.is_some());
    assert_eq!(on.source, Some(MoneySource::BundledReference));
    let off = estimate_cost(&bare, &cfg.pricing, cfg.reference_policy(Harness::Claude));
    assert_eq!(off.usd, None);
}

#[test]
fn new_limits_and_flags_have_documented_defaults_and_overrides() {
    let cfg = ok("");
    assert_eq!(cfg.max_cached_sessions, 2000);
    assert_eq!(cfg.max_metadata_sources, 100_000);
    assert_eq!(cfg.max_metadata_bytes, 67_108_864);
    assert!(cfg.mouse_enabled);
    assert!(cfg.root_conflicts.is_empty());
    let cfg = ok("max_metadata_sources = 5\nmax_metadata_bytes = 6\nmouse_enabled = false\n");
    assert_eq!((cfg.max_metadata_sources, cfg.max_metadata_bytes), (5, 6));
    assert!(!cfg.mouse_enabled);
    let cli = CliOverrides {
        no_mouse: true,
        ..CliOverrides::default()
    };
    assert!(
        !try_load(cli, &env(&[("HOME", "/h")]), "mouse_enabled = true\n")
            .unwrap()
            .mouse_enabled
    );
}

#[test]
fn omp_pi_and_hermes_roots_follow_cli_env_file_then_platform_defaults() {
    let sep = |a: &str, b: &str| PathBuf::from(a).join(b);
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/h")]), "").unwrap();
    assert_eq!(
        cfg.harnesses.omp.root,
        Some(
            PathBuf::from("/h")
                .join(".omp")
                .join("agent")
                .join("sessions")
        )
    );
    assert_eq!(
        cfg.harnesses.pi.root,
        Some(
            PathBuf::from("/h")
                .join(".pi")
                .join("agent")
                .join("sessions")
        )
    );

    let file = "[providers.omp]\nroot = \"/f-omp\"\nextra_roots = [\"/f-omp-profile\"]\n[providers.pi]\nroot = \"/f-pi\"\n[providers.hermes]\ndb = \"/f-hermes/state.db\"\nextra_roots = [\"/f-profile\"]\n";
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/h")]), file).unwrap();
    assert_eq!(cfg.harnesses.omp.root, Some(PathBuf::from("/f-omp")));
    assert_eq!(
        cfg.harnesses.omp.extra_roots,
        vec![PathBuf::from("/f-omp-profile")]
    );
    assert_eq!(cfg.harnesses.pi.root, Some(PathBuf::from("/f-pi")));
    assert_eq!(cfg.harnesses.hermes.db, Some(sep("/f-hermes", "state.db")));
    assert_eq!(
        cfg.harnesses.hermes.extra_roots,
        vec![PathBuf::from("/f-profile")]
    );

    // Pi: CLI, then PI_CODING_AGENT_DIR/sessions, then file, then default.
    let e = env(&[("HOME", "/h"), ("PI_CODING_AGENT_DIR", "/pi-env")]);
    let cfg = try_load(CliOverrides::default(), &e, file).unwrap();
    assert_eq!(cfg.harnesses.pi.root, Some(sep("/pi-env", "sessions")));
    let cli = CliOverrides {
        pi_sessions: Some(PathBuf::from("/pi-cli")),
        omp_sessions: Some(PathBuf::from("/omp-cli")),
        hermes_db: Some(PathBuf::from("/h-cli.db")),
        ..CliOverrides::default()
    };
    let cfg = try_load(cli, &e, file).unwrap();
    assert_eq!(cfg.harnesses.pi.root, Some(PathBuf::from("/pi-cli")));
    assert_eq!(cfg.harnesses.omp.root, Some(PathBuf::from("/omp-cli")));
    assert_eq!(cfg.harnesses.hermes.db, Some(PathBuf::from("/h-cli.db")));
    assert_eq!(cfg.harnesses.hermes.root, None);

    // Hermes: HERMES_HOME is a home containing state.db.
    let e = env(&[("HOME", "/h"), ("HERMES_HOME", "/hermes-home")]);
    let cfg = try_load(CliOverrides::default(), &e, file).unwrap();
    assert_eq!(
        cfg.harnesses.hermes.root,
        Some(PathBuf::from("/hermes-home"))
    );
    assert_eq!(
        cfg.harnesses.hermes.db,
        Some(sep("/hermes-home", "state.db"))
    );
}

#[test]
fn hermes_platform_home_honors_the_literal_suffix() {
    let (e, expected) = if cfg!(windows) {
        (
            env(&[
                ("LOCALAPPDATA", "C:\\L"),
                ("HERMES_DATA_DIR_SUFFIX", "-work"),
            ]),
            PathBuf::from("C:\\L").join("hermes-work"),
        )
    } else {
        (
            env(&[("HOME", "/h"), ("HERMES_DATA_DIR_SUFFIX", "-work")]),
            PathBuf::from("/h").join(".hermes-work"),
        )
    };
    let cfg = try_load(CliOverrides::default(), &e, "").unwrap();
    assert_eq!(cfg.harnesses.hermes.root, Some(expected.clone()));
    assert_eq!(cfg.harnesses.hermes.db, Some(expected.join("state.db")));
    let plain = env(&[("HOME", "/h"), ("LOCALAPPDATA", "C:\\L")]);
    let cfg = try_load(CliOverrides::default(), &plain, "").unwrap();
    let home = cfg.harnesses.hermes.root.unwrap();
    assert!(home.ends_with(if cfg!(windows) { "hermes" } else { ".hermes" }));
}

#[test]
fn a_root_shared_by_pi_and_omp_is_never_labeled_as_both() {
    let shared = "[providers.omp]\nextra_roots = [\"/shared\"]\n[providers.pi]\nextra_roots = [\"/shared\"]\n";
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/h")]), shared).unwrap();
    assert!(cfg.harnesses.omp.extra_roots.is_empty());
    assert!(cfg.harnesses.pi.extra_roots.is_empty());
    assert_eq!(cfg.root_conflicts.len(), 1);
    assert!(cfg.root_conflicts[0].contains("/shared") || cfg.root_conflicts[0].contains("shared"));

    // An explicit CLI root keeps the path for exactly that harness.
    let cli = CliOverrides {
        omp_sessions: Some(PathBuf::from("/shared")),
        ..CliOverrides::default()
    };
    let e = env(&[("HOME", "/h"), ("PI_CODING_AGENT_DIR", "/shared-parent")]);
    let cfg = try_load(
        cli.clone(),
        &e,
        "[providers.pi]\nextra_roots = [\"/shared\"]\n",
    )
    .unwrap();
    assert_eq!(cfg.harnesses.omp.root, Some(PathBuf::from("/shared")));
    assert!(cfg.harnesses.pi.extra_roots.is_empty());
    assert_eq!(cfg.root_conflicts.len(), 1);

    // The Pi env root colliding with the OMP root drops both unless one is explicit.
    let e = env(&[("HOME", "/h"), ("PI_CODING_AGENT_DIR", "/agent")]);
    let file = "[providers.omp]\nroot = \"/agent/sessions\"\n";
    let cfg = try_load(CliOverrides::default(), &e, file).unwrap();
    assert_eq!(cfg.harnesses.omp.root, None);
    assert_eq!(cfg.harnesses.pi.root, None);
    assert_eq!(cfg.root_conflicts.len(), 1);
}

#[test]
fn old_pricing_provider_never_selects_a_harness_route() {
    // `billing_provider` is free text; a harness name is just another provider string.
    let cfg = ok(
        "[[pricing]]\nbilling_provider = \"claude\"\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 1.0\n",
    );
    assert!(cfg.pricing.cards("claude", "m").is_some());
    assert!(cfg.pricing.cards("anthropic", "m").is_none());
}

#[cfg(windows)]
#[test]
fn windows_env_names_are_case_insensitive() {
    let e = env(&[
        ("USERPROFILE", "C:\\h"),
        ("codex_home", "C:\\cx"),
        ("Claude_Config_Dir", "C:\\cl"),
        ("xdg_data_home", "C:\\x"),
    ]);
    let cfg = try_load(CliOverrides::default(), &e, "").unwrap();
    assert_eq!(
        cfg.harnesses.codex.root,
        Some(PathBuf::from("C:\\cx").join("sessions"))
    );
    assert_eq!(
        cfg.harnesses.claude.root,
        Some(PathBuf::from("C:\\cl").join("projects"))
    );
    assert_eq!(
        cfg.harnesses.opencode.db,
        Some(PathBuf::from("C:\\x").join("opencode").join("opencode.db"))
    );
}

#[cfg(unix)]
#[test]
fn unix_env_names_are_case_sensitive() {
    let e = env(&[("HOME", "/h"), ("codex_home", "/wrong")]);
    let cfg = try_load(CliOverrides::default(), &e, "").unwrap();
    assert_eq!(
        cfg.harnesses.codex.root,
        Some(PathBuf::from("/h").join(".codex").join("sessions"))
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_env_paths_still_win() {
    use std::os::unix::ffi::OsStringExt;
    let bad = OsString::from_vec(b"/tmp/\xff\xfe".to_vec());
    let mut e = env(&[("HOME", "/h")]);
    e.insert("CODEX_HOME".into(), bad.clone());
    e.insert("CLAUDE_CONFIG_DIR".into(), bad.clone());
    e.insert("XDG_DATA_HOME".into(), bad.clone());
    let cfg = try_load(
        CliOverrides::default(),
        &e,
        "[providers.codex]\nroot = \"/file\"\n",
    )
    .unwrap();
    assert_eq!(
        cfg.harnesses.codex.root,
        Some(PathBuf::from(&bad).join("sessions"))
    );
    assert_eq!(
        cfg.harnesses.claude.root,
        Some(PathBuf::from(&bad).join("projects"))
    );
    assert_eq!(
        cfg.harnesses.opencode.db,
        Some(PathBuf::from(&bad).join("opencode").join("opencode.db"))
    );
}

#[test]
fn hermes_home_expands_tilde_and_variables_from_the_injected_env_only() {
    let expected = PathBuf::from("/h").join(".hermes-work");
    for value in [
        "~/.hermes-work",
        "$HOME/.hermes-work",
        "${HOME}/.hermes-work",
    ] {
        let e = env(&[("HOME", "/h"), ("HERMES_HOME", value)]);
        let cfg = try_load(CliOverrides::default(), &e, "").unwrap();
        assert_eq!(cfg.harnesses.hermes.root, Some(expected.clone()), "{value}");
        assert_eq!(
            cfg.harnesses.hermes.db,
            Some(expected.join("state.db")),
            "{value}"
        );
    }
    // An unset variable stays literal; the process environment is never consulted.
    let e = env(&[
        ("HOME", "/h"),
        ("HERMES_HOME", "$AGENTSCOPE_SURELY_UNSET/x"),
    ]);
    let cfg = try_load(CliOverrides::default(), &e, "").unwrap();
    assert_eq!(
        cfg.harnesses.hermes.root,
        Some(PathBuf::from("$AGENTSCOPE_SURELY_UNSET/x"))
    );
}

#[test]
fn root_conflicts_are_forwarded_as_sanitized_startup_notes() {
    let cfg = ok("");
    assert!(cfg.startup_notes().is_empty());
    let shared = "[providers.omp]\nextra_roots = [\"/shared\\u001b[31m\"]\n[providers.pi]\nextra_roots = [\"/shared\\u001b[31m\"]\n";
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/h")]), shared).unwrap();
    let notes = cfg.startup_notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].starts_with("Root conflict:"));
    assert!(notes[0].contains("--omp-sessions") && notes[0].contains("--pi-sessions"));
    assert!(
        !notes[0].contains('\u{1b}'),
        "terminal escapes are neutralized"
    );
}

#[test]
fn claude_desktop_cli_root_overrides_the_configured_primary() {
    let toml = "[providers.claude_desktop]\nroot = \"/configured\"\nextra_roots = [\"/extra\"]\n";
    let cli = CliOverrides {
        claude_desktop: Some(PathBuf::from("/cli-desktop")),
        ..CliOverrides::default()
    };
    let cfg = try_load(cli, &env(&[("HOME", "/home/u")]), toml).unwrap();
    assert_eq!(
        cfg.claude_desktop.data_dirs,
        [PathBuf::from("/cli-desktop"), PathBuf::from("/extra")],
        "the CLI picks the primary; configured extra roots are appended"
    );
    assert!(cfg.claude_desktop.explicit);

    // Without the CLI, the configured primary is used.
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/home/u")]), toml).unwrap();
    assert_eq!(
        cfg.claude_desktop.data_dirs,
        [PathBuf::from("/configured"), PathBuf::from("/extra")]
    );

    // With neither, platform defaults are resolved but not marked explicit.
    let cfg = try_load(CliOverrides::default(), &env(&[("HOME", "/home/u")]), "").unwrap();
    assert!(!cfg.claude_desktop.explicit);
    assert!(
        cfg.claude_desktop
            .data_dirs
            .iter()
            .all(|p| p.starts_with("/home/u"))
    );
}

#[test]
fn codex_explicit_archived_root_wins_over_the_derived_home_store() {
    let e = env(&[("HOME", "/h"), ("CODEX_HOME", "/cx")]);
    let cfg = try_load(
        CliOverrides::default(),
        &e,
        "[providers.codex]\narchived_root = \"/explicit-archive\"\n",
    )
    .unwrap();
    assert_eq!(
        cfg.harnesses.codex.root,
        Some(PathBuf::from("/cx").join("sessions"))
    );
    assert_eq!(
        cfg.harnesses.codex.archived,
        Some(PathBuf::from("/explicit-archive")),
        "the explicit archive root replaces the derived default"
    );
    assert!(cfg.harnesses.codex.archived_explicit);

    // Without an explicit archive, the native sibling is derived silently.
    let cfg = try_load(CliOverrides::default(), &e, "").unwrap();
    assert_eq!(
        cfg.harnesses.codex.archived,
        Some(PathBuf::from("/cx").join("archived_sessions"))
    );
    assert!(!cfg.harnesses.codex.archived_explicit);
}
