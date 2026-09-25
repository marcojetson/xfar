//! Session persistence.
//!
//! With `session.keep_state` enabled, xfar records still-open tracked clients
//! during a normal shutdown and replays their commands at the next start. The
//! snapshot keeps one entry per workspace, with commands in stacking order and
//! that workspace's split weights. Independently connected clients cannot be
//! restored because their launch commands are unknown.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The saved session: one entry per workspace, plus which one was active.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Session {
    /// Index of the workspace that was active at quit; it becomes active again.
    pub active: usize,
    /// Saved workspaces in slot order.
    pub desktops: Vec<Desktop>,
}

/// One saved workspace: tracked client commands in stacking order and the grid's
/// split weights.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Desktop {
    /// App launch commands (as recorded at spawn time), one per open tracked
    /// client, in stacking order.
    pub commands: Vec<String>,
    /// The desktop grid's manual divider weights. Empty vectors mean an equal
    /// split; restored vectors must match the tiled-window count.
    pub weights: crate::layout::GridWeights,
}

/// `$XDG_STATE_HOME/xfar/session.toml`, or `~/.local/state/xfar/session.toml`
/// when `XDG_STATE_HOME` is unset or empty. `None` only when neither
/// `XDG_STATE_HOME` nor `HOME` is set.
pub fn session_path() -> Option<PathBuf> {
    crate::config::xdg_dir("XDG_STATE_HOME", ".local/state", "session.toml")
}

/// Load the saved session; errors (missing, malformed) restore nothing.
pub fn load_from(path: &Path) -> Result<Session, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    toml::from_str::<Session>(&contents)
        .map_err(|e| format!("could not parse {}: {e}", path.display()))
}

/// Write `session` to `path` as TOML, creating parent directories. An empty
/// session replaces any stale file so a later start relaunches nothing.
pub fn save_to(session: &Session, path: &Path) -> Result<(), String> {
    let text =
        toml::to_string_pretty(session).map_err(|e| format!("could not serialize session: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("xfar-session-test-{}", std::process::id()));
        let path = dir.join("session.toml");
        let session = Session {
            active: 1,
            desktops: vec![
                Desktop {
                    commands: vec!["foot".into(), "firefox".into()],
                    weights: crate::layout::GridWeights {
                        cols: vec![1.0, 2.0],
                        rows: vec![vec![1.0, 1.0], vec![1.0]],
                    },
                },
                Desktop {
                    commands: vec!["htop".into()],
                    ..Default::default()
                },
            ],
        };
        save_to(&session, &path).unwrap();
        assert_eq!(load_from(&path).unwrap().active, 1);
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.desktops.len(), 2);
        assert_eq!(loaded.desktops[0].commands, ["foot", "firefox"]);
        assert_eq!(loaded.desktops[0].weights.cols, [1.0, 2.0]);
        assert_eq!(
            loaded.desktops[0].weights.rows,
            vec![vec![1.0, 1.0], vec![1.0]]
        );
        assert_eq!(loaded.desktops[1].commands, ["htop"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_nothing() {
        let path = Path::new("/nonexistent/xfar/session.toml");
        assert!(load_from(path).is_err());
    }

    #[test]
    fn malformed_file_loads_nothing() {
        let dir = std::env::temp_dir().join(format!("xfar-session-bad-{}", std::process::id()));
        let path = dir.join("session.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "nonsense(=").unwrap();
        assert!(load_from(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_save_replaces_stale_contents() {
        let dir = std::env::temp_dir().join(format!("xfar-session-emp-{}", std::process::id()));
        let path = dir.join("session.toml");
        save_to(&Session::default(), &path).unwrap();
        assert_eq!(load_from(&path).unwrap().desktops.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
