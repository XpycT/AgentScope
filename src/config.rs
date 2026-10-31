//! Configuration loading and the price schedule assembled from the bundled
//! catalog plus the user's exact cards.
//!
//! Precedence: CLI flags > environment > config file > platform defaults. A
//! missing default config file is fine; a malformed or non-USD file is a fatal
//! error. Loading never mutates process-global state; `load_with_env` takes an
//! explicit environment so callers and tests stay hermetic. Root resolution
//! reads only that environment: no shell, credential, or provider-config scan.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::domain::Harness;
use crate::pricing::{
    PriceCard, PriceOverride, PriceSchedule, PricingError, Rates, ReferencePolicy,
};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read config file {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config file {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("unsupported currency {0:?}: only \"USD\" is supported")]
    Currency(String),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("duplicate price entry for billing_provider {billing_provider:?}, model {model:?}")]
    DuplicatePrice {
        billing_provider: String,
        model: String,
    },
}

fn pricing_error(error: PricingError) -> ConfigError {
    match error {
        PricingError::Duplicate { provider, model } => ConfigError::DuplicatePrice {
            billing_provider: provider,
            model,
        },
        other => ConfigError::Invalid(other.to_string()),
    }
}

/// Command-line overrides relevant to configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CliOverrides {
    pub config: Option<PathBuf>,
    /// Exact Codex sessions root.
    pub codex_sessions: Option<PathBuf>,
    /// Exact Claude projects root.
    pub claude_projects: Option<PathBuf>,
    /// Exact Claude Desktop application-data directory.
    pub claude_desktop: Option<PathBuf>,
    pub opencode_db: Option<PathBuf>,
    /// Exact OMP sessions root (resolved only; no OMP reader exists yet).
    pub omp_sessions: Option<PathBuf>,
    /// Exact Pi sessions root (resolved only; no Pi reader exists yet).
    pub pi_sessions: Option<PathBuf>,
    /// Exact Hermes `state.db` (resolved only; no Hermes reader exists yet).
    pub hermes_db: Option<PathBuf>,
    pub no_processes: bool,
    pub no_color: bool,
    pub no_mouse: bool,
}

/// Claude Desktop application-data directories whose allowlisted local stores
/// (Code session manifests and Cowork project roots) are read. Nothing else in
/// them is opened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopConfig {
    pub data_dirs: Vec<PathBuf>,
    /// The user named the directories, so a missing one is reported. Platform
    /// defaults that do not exist (no Desktop installed) stay silent.
    pub explicit: bool,
}

/// Where one harness's local store lives. `root` is a directory (or, for
/// Hermes, a home containing `state.db`), `db` a database file, and
/// `extra_roots` further explicit stores of the same kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HarnessConfig {
    pub root: Option<PathBuf>,
    /// Archived store of the same native format (`archived_sessions`), when known.
    /// Never guessed from a custom root: only a native home or an explicit
    /// `archived_root` supplies it.
    pub archived: Option<PathBuf>,
    /// The user named the archived store, so a missing one is reported.
    pub archived_explicit: bool,
    pub db: Option<PathBuf>,
    pub extra_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HarnessConfigs {
    pub codex: HarnessConfig,
    pub claude: HarnessConfig,
    pub opencode: HarnessConfig,
    pub omp: HarnessConfig,
    pub pi: HarnessConfig,
    pub hermes: HarnessConfig,
}

impl HarnessConfigs {
    pub fn get(&self, harness: Harness) -> &HarnessConfig {
        match harness {
            Harness::Codex => &self.codex,
            Harness::Claude => &self.claude,
            Harness::OpenCode => &self.opencode,
            Harness::Omp => &self.omp,
            Harness::Hermes => &self.hermes,
            Harness::Pi => &self.pi,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub refresh_ms: u64,
    pub process_ms: u64,
    pub process_enabled: bool,
    pub quiet_after_secs: u64,
    pub stall_after_secs: u64,
    /// Cached roster rows kept per refresh pass.
    pub max_cached_sessions: usize,
    /// Metadata sources scanned per pass.
    pub max_metadata_sources: usize,
    /// Bytes of metadata read per pass.
    pub max_metadata_bytes: usize,
    pub max_page_events: usize,
    pub max_page_bytes: usize,
    pub max_record_bytes: usize,
    pub dedup_ledger_entries: usize,
    pub dedup_ledger_bytes: usize,
    pub refresh_busy_timeout_ms: u64,
    pub page_busy_timeout_ms: u64,
    pub redaction: bool,
    pub harnesses: HarnessConfigs,
    pub claude_desktop: DesktopConfig,
    /// Bundled official cards with the user's exact cards applied.
    pub pricing: PriceSchedule,
    /// Price native Claude/Codex usage without a recorded route at the
    /// vendor's API rate, labeled as a reference estimate.
    pub native_api_reference_estimates: bool,
    pub mouse_enabled: bool,
    /// Root collisions that were left unresolved, for display.
    pub root_conflicts: Vec<String>,
    /// Config file path that was requested or resolved by default, if any.
    pub config_path: Option<PathBuf>,
    pub no_color: bool,
}

impl Config {
    /// Sanitized one-line diagnostics for configuration problems that must be
    /// shown at startup (currently unresolved root collisions).
    pub fn startup_notes(&self) -> Vec<String> {
        self.root_conflicts
            .iter()
            .map(|c| format!("Root conflict: {}", crate::ingest::text::display_safe(c)))
            .collect()
    }

    /// The policy readers use for usage that recorded no billing route.
    pub fn reference_policy(&self, harness: Harness) -> ReferencePolicy {
        if self.native_api_reference_estimates
            && matches!(harness, Harness::Claude | Harness::Codex)
        {
            ReferencePolicy::NativeApi(harness)
        } else {
            ReferencePolicy::Disabled
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    currency: Option<String>,
    refresh_ms: Option<u64>,
    process_ms: Option<u64>,
    process_enabled: Option<bool>,
    quiet_after_secs: Option<u64>,
    stall_after_secs: Option<u64>,
    /// Removed key, accepted only to fail with a corrective message.
    max_sessions: Option<toml::Value>,
    max_cached_sessions: Option<usize>,
    max_metadata_sources: Option<usize>,
    max_metadata_bytes: Option<usize>,
    max_page_events: Option<usize>,
    max_page_bytes: Option<usize>,
    max_record_bytes: Option<usize>,
    dedup_ledger_entries: Option<usize>,
    dedup_ledger_bytes: Option<usize>,
    refresh_busy_timeout_ms: Option<u64>,
    page_busy_timeout_ms: Option<u64>,
    redaction: Option<bool>,
    native_api_reference_estimates: Option<bool>,
    mouse_enabled: Option<bool>,
    #[serde(default)]
    providers: RawProviders,
    #[serde(default)]
    pricing: Vec<RawPrice>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawProviders {
    #[serde(default)]
    codex: RawRoots,
    #[serde(default)]
    claude: RawRoots,
    #[serde(default)]
    claude_desktop: RawRoots,
    #[serde(default)]
    opencode: RawDb,
    #[serde(default)]
    omp: RawRoots,
    #[serde(default)]
    pi: RawRoots,
    #[serde(default)]
    hermes: RawHermes,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRoots {
    root: Option<PathBuf>,
    /// Archived store of the same native format, read beside the live store.
    archived_root: Option<PathBuf>,
    #[serde(default)]
    extra_roots: Vec<PathBuf>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawDb {
    db: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawHermes {
    root: Option<PathBuf>,
    db: Option<PathBuf>,
    #[serde(default)]
    extra_roots: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPrice {
    /// Removed key, accepted only to fail with a corrective message.
    provider: Option<toml::Value>,
    billing_provider: Option<String>,
    model: String,
    as_of: Option<String>,
    effective_from: Option<String>,
    effective_until: Option<String>,
    input_per_mtok: f64,
    cached_input_per_mtok: Option<f64>,
    cache_write_per_mtok: Option<f64>,
    cache_write_5m_per_mtok: Option<f64>,
    cache_write_1h_per_mtok: Option<f64>,
    cache_read_per_mtok: Option<f64>,
    output_per_mtok: f64,
    reasoning_per_mtok: Option<f64>,
}

impl RawPrice {
    fn into_override(self) -> Result<PriceOverride, ConfigError> {
        if self.provider.is_some() {
            return Err(ConfigError::Invalid(
                "[[pricing]] key `provider` was replaced by `billing_provider` (the route that \
                 bills the model, such as \"anthropic\", \"openai\", or \"opencode\"); rename it \
                 in the config file, which is never rewritten automatically"
                    .into(),
            ));
        }
        let billing_provider = self.billing_provider.ok_or_else(|| {
            ConfigError::Invalid("[[pricing]] entry needs a `billing_provider` key".into())
        })?;
        let cache_read = match (self.cached_input_per_mtok, self.cache_read_per_mtok) {
            (Some(a), Some(b)) if a != b => {
                return Err(ConfigError::Invalid(format!(
                    "pricing entry {:?}: cached_input_per_mtok and cache_read_per_mtok disagree; set one",
                    self.model
                )));
            }
            (a, b) => a.or(b),
        };
        let mut card = PriceCard::new(
            &billing_provider,
            &self.model,
            Rates {
                input: Some(self.input_per_mtok),
                output: Some(self.output_per_mtok),
                cache_read,
                cache_write_5m: self.cache_write_5m_per_mtok,
                cache_write_1h: self.cache_write_1h_per_mtok,
                cache_write: self.cache_write_per_mtok,
                reasoning: self.reasoning_per_mtok,
            },
        );
        card.as_of = self.as_of;
        card.effective_from = self.effective_from;
        card.effective_until = self.effective_until;
        Ok(PriceOverride { card })
    }
}

/// Environment snapshot. Values stay `OsString` so non-Unicode paths survive.
pub type Env = HashMap<OsString, OsString>;

fn env_value<'a>(env: &'a Env, key: &str) -> Option<&'a OsString> {
    let value = if cfg!(windows) {
        // Windows environment names are case-insensitive.
        env.iter()
            .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    } else {
        env.get(OsStr::new(key))
    };
    value.filter(|v| !v.is_empty())
}

fn env_path(env: &Env, key: &str) -> Option<PathBuf> {
    env_value(env, key).map(PathBuf::from)
}

fn home_dir(env: &Env) -> Option<PathBuf> {
    env_path(env, "HOME").or_else(|| env_path(env, "USERPROFILE"))
}

/// Expands a leading `~` and `$VAR`/`${VAR}` from the injected environment only
/// (no process-global lookup, no shell). Unknown variables and a `~` without a
/// home stay literal. Paths with neither marker are returned untouched.
fn expand_path(path: PathBuf, env: &Env) -> PathBuf {
    let text = path.to_string_lossy().into_owned();
    if !text.contains('$') && !text.starts_with('~') {
        return path;
    }
    let mut rest = text.as_str();
    let mut out = String::new();
    if let Some(tail) = rest.strip_prefix('~')
        && (tail.is_empty() || tail.starts_with('/') || tail.starts_with('\\'))
        && let Some(home) = home_dir(env)
    {
        out.push_str(&home.to_string_lossy());
        rest = tail;
    }
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let tail = &rest[i + 1..];
        let (name, used) = if let Some(inner) = tail.strip_prefix('{') {
            match inner.find('}') {
                Some(end) => (&inner[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let len = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            (&tail[..len], len)
        };
        match env_value(env, name).filter(|_| !name.is_empty()) {
            Some(value) => {
                out.push_str(&value.to_string_lossy());
                for _ in 0..used {
                    chars.next();
                }
            }
            None => out.push('$'),
        }
    }
    PathBuf::from(out)
}

/// Claude Desktop application-data directories for this platform: the standard
/// app and the third-party (`-3p`) deployment, plus the legacy Windows roaming
/// location of the latter. No directory is probed here.
fn desktop_platform_dirs(env: &Env) -> Vec<PathBuf> {
    if cfg!(windows) {
        let local = env_path(env, "LOCALAPPDATA")
            .or_else(|| home_dir(env).map(|h| h.join("AppData").join("Local")));
        let roaming = env_path(env, "APPDATA")
            .or_else(|| home_dir(env).map(|h| h.join("AppData").join("Roaming")));
        let mut dirs = Vec::new();
        dirs.extend(roaming.as_ref().map(|d| d.join("Claude")));
        dirs.extend(local.as_ref().map(|d| d.join("Claude-3p")));
        dirs.extend(roaming.as_ref().map(|d| d.join("Claude-3p")));
        dirs
    } else {
        let base = if cfg!(target_os = "macos") {
            home_dir(env).map(|h| h.join("Library").join("Application Support"))
        } else {
            env_path(env, "XDG_CONFIG_HOME").or_else(|| home_dir(env).map(|h| h.join(".config")))
        };
        base.into_iter()
            .flat_map(|b| [b.join("Claude"), b.join("Claude-3p")])
            .collect()
    }
}

fn default_config_path(env: &Env) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        env_path(env, "APPDATA")?
    } else if cfg!(target_os = "macos") {
        home_dir(env)?.join("Library").join("Application Support")
    } else {
        env_path(env, "XDG_CONFIG_HOME").or_else(|| home_dir(env).map(|h| h.join(".config")))?
    };
    Some(base.join("agentscope").join("config.toml"))
}

/// Platform home of Hermes: `%LOCALAPPDATA%/hermes<suffix>` on Windows (user
/// home `AppData/Local` when unset), `~/.hermes<suffix>` elsewhere. The suffix
/// is `HERMES_DATA_DIR_SUFFIX`, used literally.
fn hermes_platform_home(env: &Env) -> Option<PathBuf> {
    let suffix = env_value(env, "HERMES_DATA_DIR_SUFFIX")
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if cfg!(windows) {
        let base = env_path(env, "LOCALAPPDATA")
            .or_else(|| home_dir(env).map(|h| h.join("AppData").join("Local")))?;
        Some(base.join(format!("hermes{suffix}")))
    } else {
        home_dir(env).map(|h| h.join(format!(".hermes{suffix}")))
    }
}

fn positive<T: Default + PartialEq>(name: &str, value: T) -> Result<T, ConfigError> {
    if value == T::default() {
        Err(ConfigError::Invalid(format!(
            "{name} must be greater than 0"
        )))
    } else {
        Ok(value)
    }
}

/// A path both Pi and OMP would read must not be labeled as both. An explicit
/// CLI root keeps it for its harness; otherwise neither harness gets it.
fn resolve_collisions(
    omp: &mut HarnessConfig,
    pi: &mut HarnessConfig,
    cli: &CliOverrides,
) -> Vec<String> {
    let paths = |c: &HarnessConfig| -> Vec<PathBuf> {
        c.root.iter().chain(&c.extra_roots).cloned().collect()
    };
    let omp_paths = paths(omp);
    let shared: Vec<PathBuf> = paths(pi)
        .into_iter()
        .filter(|p| omp_paths.contains(p))
        .collect();
    let mut conflicts = Vec::new();
    for path in shared {
        let omp_explicit = cli.omp_sessions.as_ref() == Some(&path);
        let pi_explicit = cli.pi_sessions.as_ref() == Some(&path);
        let drop_from = |c: &mut HarnessConfig| {
            if c.root.as_ref() == Some(&path) {
                c.root = None;
            }
            c.extra_roots.retain(|p| *p != path);
        };
        let detail = match (omp_explicit, pi_explicit) {
            (true, false) => {
                drop_from(pi);
                "kept for OMP, which was given it explicitly"
            }
            (false, true) => {
                drop_from(omp);
                "kept for Pi, which was given it explicitly"
            }
            _ => {
                drop_from(omp);
                drop_from(pi);
                "used by neither; pass --omp-sessions or --pi-sessions to choose"
            }
        };
        conflicts.push(format!(
            "Pi and OMP were both pointed at {} ({detail})",
            path.display()
        ));
    }
    conflicts
}

impl Config {
    /// Loads configuration using the process environment.
    pub fn load(cli: &CliOverrides) -> Result<Self, ConfigError> {
        let env: Env = std::env::vars_os().collect();
        Self::load_with_env(cli, &env)
    }

    /// Loads configuration with an explicit environment map.
    ///
    /// A missing default config file is fine; an explicit `--config` path that
    /// cannot be read is an error.
    pub fn load_with_env(cli: &CliOverrides, env: &Env) -> Result<Self, ConfigError> {
        let explicit = cli.config.is_some();
        let config_path = cli.config.clone().or_else(|| default_config_path(env));
        let raw = match &config_path {
            Some(path) => read_raw(path, explicit)?,
            None => RawConfig::default(),
        };

        if let Some(currency) = &raw.currency
            && currency != "USD"
        {
            return Err(ConfigError::Currency(currency.clone()));
        }
        if raw.max_sessions.is_some() {
            return Err(ConfigError::Invalid(
                "`max_sessions` was replaced by `max_cached_sessions` (cached roster rows); \
                 rename it in the config file, which is never rewritten automatically"
                    .into(),
            ));
        }

        let quiet = positive("quiet_after_secs", raw.quiet_after_secs.unwrap_or(300))?;
        let stall = positive("stall_after_secs", raw.stall_after_secs.unwrap_or(900))?;
        if quiet >= stall {
            return Err(ConfigError::Invalid(format!(
                "quiet_after_secs ({quiet}) must be less than stall_after_secs ({stall})"
            )));
        }

        let overrides = raw
            .pricing
            .into_iter()
            .map(RawPrice::into_override)
            .collect::<Result<Vec<_>, _>>()?;
        let pricing = PriceSchedule::bundled()
            .and_then(|bundled| bundled.with_overrides(overrides))
            .map_err(pricing_error)?;

        let home = home_dir(env);
        let file = |p: Option<PathBuf>| p.map(|p| expand_path(p, env));
        let files = |v: Vec<PathBuf>| -> Vec<PathBuf> {
            v.into_iter().map(|p| expand_path(p, env)).collect()
        };
        let providers = raw.providers;

        // The native archived store lives beside the native sessions root. It is
        // derived only from CODEX_HOME or the platform home; a custom root keeps
        // exactly the archive the user configured, and nothing is crawled.
        let codex_archived_config = file(providers.codex.archived_root.clone());
        let codex_archived_explicit = codex_archived_config.is_some();
        let (codex_root, derived_archive) = if let Some(root) = cli.codex_sessions.clone() {
            (Some(root), None)
        } else if let Some(home) = env_path(env, "CODEX_HOME") {
            (
                Some(home.join("sessions")),
                Some(home.join("archived_sessions")),
            )
        } else if let Some(root) = file(providers.codex.root.clone()) {
            (Some(root), None)
        } else if let Some(home) = home.as_ref() {
            let home = home.join(".codex");
            (
                Some(home.join("sessions")),
                Some(home.join("archived_sessions")),
            )
        } else {
            (None, None)
        };
        // An explicit `archived_root` names the physical store the scanner reads,
        // even when a native sessions root was derived from CODEX_HOME/home; the
        // derived directory is only the default.
        let codex_archived = codex_archived_config.or(derived_archive);
        let claude_root = cli
            .claude_projects
            .clone()
            .or_else(|| env_path(env, "CLAUDE_CONFIG_DIR").map(|h| h.join("projects")))
            .or(file(providers.claude.root))
            .or_else(|| home.as_ref().map(|h| h.join(".claude").join("projects")));
        // The CLI picks the primary directory; the configured primary is the
        // fallback, and configured extra roots are appended either way.
        let desktop_named: Vec<PathBuf> = cli
            .claude_desktop
            .clone()
            .or(file(providers.claude_desktop.root))
            .into_iter()
            .chain(files(providers.claude_desktop.extra_roots))
            .collect();
        let claude_desktop = if desktop_named.is_empty() {
            DesktopConfig {
                data_dirs: desktop_platform_dirs(env),
                explicit: false,
            }
        } else {
            DesktopConfig {
                data_dirs: desktop_named,
                explicit: true,
            }
        };
        let opencode_db = cli
            .opencode_db
            .clone()
            .or_else(|| {
                env_path(env, "XDG_DATA_HOME").map(|d| d.join("opencode").join("opencode.db"))
            })
            .or(file(providers.opencode.db))
            .or_else(|| {
                home.as_ref().map(|h| {
                    h.join(".local")
                        .join("share")
                        .join("opencode")
                        .join("opencode.db")
                })
            });
        let mut omp = HarnessConfig {
            root: cli
                .omp_sessions
                .clone()
                .or(file(providers.omp.root))
                .or_else(|| {
                    home.as_ref()
                        .map(|h| h.join(".omp").join("agent").join("sessions"))
                }),
            archived: None,
            archived_explicit: false,
            db: None,
            extra_roots: files(providers.omp.extra_roots),
        };
        let mut pi = HarnessConfig {
            root: cli
                .pi_sessions
                .clone()
                .or_else(|| env_path(env, "PI_CODING_AGENT_DIR").map(|d| d.join("sessions")))
                .or(file(providers.pi.root))
                .or_else(|| {
                    home.as_ref()
                        .map(|h| h.join(".pi").join("agent").join("sessions"))
                }),
            archived: None,
            archived_explicit: false,
            db: None,
            extra_roots: files(providers.pi.extra_roots),
        };
        let root_conflicts = resolve_collisions(&mut omp, &mut pi, cli);

        // Hermes: CLI db, HERMES_HOME, configured db, configured root, then the
        // platform home. A home is a directory that contains state.db.
        let (hermes_root, hermes_db) = if let Some(db) = cli.hermes_db.clone() {
            (None, Some(db))
        } else if let Some(home) = env_path(env, "HERMES_HOME").map(|h| expand_path(h, env)) {
            let db = home.join("state.db");
            (Some(home), Some(db))
        } else if let Some(db) = file(providers.hermes.db) {
            (None, Some(db))
        } else if let Some(home) = file(providers.hermes.root).or_else(|| hermes_platform_home(env))
        {
            let db = home.join("state.db");
            (Some(home), Some(db))
        } else {
            (None, None)
        };
        let hermes = HarnessConfig {
            root: hermes_root,
            archived: None,
            archived_explicit: false,
            db: hermes_db,
            extra_roots: files(providers.hermes.extra_roots),
        };

        Ok(Config {
            refresh_ms: positive("refresh_ms", raw.refresh_ms.unwrap_or(2000))?,
            process_ms: positive("process_ms", raw.process_ms.unwrap_or(5000))?,
            process_enabled: !cli.no_processes && raw.process_enabled.unwrap_or(true),
            quiet_after_secs: quiet,
            stall_after_secs: stall,
            max_cached_sessions: positive(
                "max_cached_sessions",
                raw.max_cached_sessions.unwrap_or(2000),
            )?,
            max_metadata_sources: positive(
                "max_metadata_sources",
                raw.max_metadata_sources.unwrap_or(100_000),
            )?,
            max_metadata_bytes: positive(
                "max_metadata_bytes",
                raw.max_metadata_bytes.unwrap_or(67_108_864),
            )?,
            max_page_events: positive("max_page_events", raw.max_page_events.unwrap_or(500))?,
            max_page_bytes: positive("max_page_bytes", raw.max_page_bytes.unwrap_or(262_144))?,
            max_record_bytes: positive(
                "max_record_bytes",
                raw.max_record_bytes.unwrap_or(1_048_576),
            )?,
            dedup_ledger_entries: positive(
                "dedup_ledger_entries",
                raw.dedup_ledger_entries.unwrap_or(100_000),
            )?,
            dedup_ledger_bytes: positive(
                "dedup_ledger_bytes",
                raw.dedup_ledger_bytes.unwrap_or(8_388_608),
            )?,
            refresh_busy_timeout_ms: positive(
                "refresh_busy_timeout_ms",
                raw.refresh_busy_timeout_ms.unwrap_or(2000),
            )?,
            page_busy_timeout_ms: positive(
                "page_busy_timeout_ms",
                raw.page_busy_timeout_ms.unwrap_or(250),
            )?,
            redaction: raw.redaction.unwrap_or(true),
            harnesses: HarnessConfigs {
                codex: HarnessConfig {
                    root: codex_root,
                    archived: codex_archived,
                    archived_explicit: codex_archived_explicit,
                    db: None,
                    extra_roots: files(providers.codex.extra_roots),
                },
                claude: HarnessConfig {
                    root: claude_root,
                    archived: None,
                    archived_explicit: false,
                    db: None,
                    extra_roots: files(providers.claude.extra_roots),
                },
                opencode: HarnessConfig {
                    root: None,
                    archived: None,
                    archived_explicit: false,
                    db: opencode_db,
                    extra_roots: Vec::new(),
                },
                omp,
                pi,
                hermes,
            },
            claude_desktop,
            pricing,
            native_api_reference_estimates: raw.native_api_reference_estimates.unwrap_or(true),
            mouse_enabled: !cli.no_mouse && raw.mouse_enabled.unwrap_or(true),
            root_conflicts,
            config_path,
            no_color: cli.no_color,
        })
    }
}

fn read_raw(path: &Path, explicit: bool) -> Result<RawConfig, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound && !explicit => return Ok(RawConfig::default()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}
