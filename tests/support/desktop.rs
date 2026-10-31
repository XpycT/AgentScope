//! Synthetic Claude Desktop application-data layout: allowlisted manifests and
//! Cowork project roots beside credential, cache, audit and VM stores that must
//! never be read.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The allowlisted locations of one synthetic application-data directory.
pub struct Desktop {
    /// The application-data directory (what `--claude-desktop` names).
    pub data: PathBuf,
    /// The ordinary Claude project store, shared with the CLI.
    pub projects: PathBuf,
    /// `claude-code-sessions/<account>/<organization>/`.
    pub manifest_dir: PathBuf,
    /// A Cowork session working directory that carries a project store.
    pub cowork_session: PathBuf,
    /// `.../.claude/projects` of that Cowork session.
    pub cowork_projects: PathBuf,
    /// Stores that must never be opened.
    pub forbidden: Vec<PathBuf>,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Builds an empty Desktop layout under `base`.
pub fn layout(base: &Path) -> Desktop {
    layout_over(base, &base.join("claude-projects"))
}

/// Builds an empty Desktop layout under `base` that shares the ordinary Claude
/// project store `projects`.
pub fn layout_over(base: &Path, projects: &Path) -> Desktop {
    let projects = projects.to_path_buf();
    let data = base.join("Claude-3p");
    let manifest_dir = data.join("claude-code-sessions").join("acct").join("org");
    let cowork_session = data
        .join("local-agent-mode-sessions")
        .join("acct")
        .join("org")
        .join("local_cowork");
    let cowork_projects = cowork_session.join(".claude").join("projects");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&cowork_projects).unwrap();
    let forbidden = vec![
        data.join("host-creds-7f3a.json"),
        data.join("ccd-session-secrets").join("s1").join("gcp.json"),
        data.join("IndexedDB").join("leveldb").join("000003.log"),
        data.join("Local Storage")
            .join("leveldb")
            .join("000005.log"),
        cowork_session.join("audit.jsonl"),
        cowork_session.join(".audit-key"),
        data.join("vm_bundles").join("bundle.json"),
        data.join("claude-code").join("binary"),
    ];
    for path in &forbidden {
        write(path, "SENTINEL-DESKTOP-SECRET");
    }
    Desktop {
        data,
        projects,
        manifest_dir,
        cowork_session,
        cowork_projects,
        forbidden,
    }
}

/// A Code session manifest naming an existing transcript.
pub fn manifest(dir: &Path, name: &str, cli_session_id: &str, extra: Value) -> PathBuf {
    let mut object = json!({"cliSessionId": cli_session_id, "title": "Synthetic code session"});
    for (key, value) in extra.as_object().into_iter().flatten() {
        object[key] = value.clone();
    }
    let path = dir.join(name);
    write(&path, &object.to_string());
    path
}
