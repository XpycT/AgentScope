//! Synthetic fixture builders shared by integration tests.
#![allow(dead_code)]

pub mod claude;
pub mod codex;
pub mod desktop;
pub mod facts;
pub mod hermes;
pub mod omp;
pub mod opencode;
pub mod pi;
pub mod world;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agentscope::config::{CliOverrides, Config, Env};

/// Fixed clock for every test (2026-10-30T00:00:00Z in epoch milliseconds).
pub const NOW: i64 = 1_793_318_400_000;

/// Config whose Codex root is exactly `root`.
pub fn config_for(root: &Path) -> Config {
    let cli = CliOverrides {
        codex_sessions: Some(root.to_path_buf()),
        ..CliOverrides::default()
    };
    Config::load_with_env(&cli, &Env::new()).expect("valid test config")
}

/// Config whose Codex sessions and archived stores are exactly `sessions` and
/// `archived`, as one native store.
pub fn config_for_store(sessions: &Path, archived: &Path) -> Config {
    let mut config = config_for(sessions);
    config.harnesses.codex.archived = Some(archived.to_path_buf());
    config.harnesses.codex.archived_explicit = true;
    config
}

pub fn write_lines(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(path, body).expect("write fixture");
}

pub fn set_modified(path: &Path, secs_after_epoch: u64) {
    let file = File::options().write(true).open(path).expect("open");
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs_after_epoch))
        .expect("set mtime");
}

pub fn dated(root: &Path, name: &str) -> PathBuf {
    root.join("2026").join("10").join("29").join(name)
}

/// Creates a directory symlink (Unix) or junction (Windows, no privilege needed).
/// Returns false when the platform refuses.
pub fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|o| o.status.success())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

/// True when `path` can no longer be opened for reading after `lock` ran.
pub struct Unreadable {
    #[cfg(windows)]
    _held: Option<File>,
    #[cfg(unix)]
    path: PathBuf,
    pub active: bool,
}

impl Unreadable {
    pub fn new(path: &Path) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let held = File::options().read(true).share_mode(0).open(path).ok();
            let active = File::open(path).is_err();
            Self {
                _held: held,
                active,
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).ok();
            let active = File::open(path).is_err();
            Self {
                path: path.to_path_buf(),
                active,
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Self { active: false }
        }
    }

    pub fn release(self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o644)).ok();
        }
    }
}

/// Every source the allowlisted traversal of a provider root yields, in traversal order.
pub struct Walked {
    pub sources: Vec<agentscope::discovery::SourceFile>,
    pub warnings: Vec<agentscope::domain::Warning>,
}

fn walk_sources(harness: agentscope::domain::Harness, root: Option<&Path>) -> Walked {
    let mut walk = agentscope::discovery::Walk::new(harness, root);
    let mut sources = Vec::new();
    while !walk.is_done() {
        sources.extend(walk.step(64, std::time::Instant::now() + Duration::from_secs(60)));
    }
    Walked {
        sources,
        warnings: walk.take_warnings(),
    }
}

pub fn codex_sources(config: &Config) -> Walked {
    walk_sources(
        agentscope::domain::Harness::Codex,
        config.harnesses.codex.root.as_deref(),
    )
}

pub fn claude_sources(config: &Config) -> Walked {
    walk_sources(
        agentscope::domain::Harness::Claude,
        config.harnesses.claude.root.as_deref(),
    )
}

/// A frozen roster generation whose loaded roots are `sessions`, in order, all hydrated.
pub fn snapshot(
    generation: u64,
    sessions: Vec<agentscope::domain::SessionSummary>,
    scan: agentscope::domain::ScanProgress,
) -> agentscope::roster::RosterSnapshot {
    let roots: Vec<agentscope::domain::SessionKey> =
        sessions.iter().map(|s| s.meta.key.clone()).collect();
    agentscope::roster::RosterSnapshot {
        generation,
        roots: roots.into(),
        metadata: sessions
            .iter()
            .map(|s| std::sync::Arc::new(s.meta.clone()))
            .collect(),
        summaries: sessions.into_iter().map(std::sync::Arc::new).collect(),
        scan: vec![scan],
        has_more: false,
        limited: None,
        refresh_available: false,
        provisional: false,
    }
}
