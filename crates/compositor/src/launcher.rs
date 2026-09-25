//! Smart launcher: application discovery, fuzzy search/ranking, and the
//! launcher's input state. It searches a combined list of *actions* — launch an
//! app, focus an open window, or run a system action. Rendering lives in
//! [`crate::backend`]; the dynamic action list is assembled in [`crate::state`].

use std::collections::HashSet;
use std::path::PathBuf;

use smithay::desktop::Window;

/// Maximum number of results shown at once.
pub const MAX_RESULTS: usize = 8;

/// A launchable application discovered from a `.desktop` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppEntry {
    pub name: String,
    /// Command line to run (field codes like `%U` stripped).
    pub exec: String,
    /// `Terminal=true` in the `.desktop`: a TUI app that must run in a terminal.
    pub terminal: bool,
}

/// A compositor-level action the launcher can perform.
#[derive(Clone)]
pub enum Action {
    /// Launch an application by running `exec` (in a terminal if `terminal`).
    Launch {
        name: String,
        exec: String,
        terminal: bool,
    },
    /// Focus an already-open window.
    Focus { name: String, window: Window },
    /// Run a built-in system action.
    System { name: String, kind: SystemAction },
}

/// Built-in system actions offered by the launcher.
#[derive(Clone, Copy)]
pub enum SystemAction {
    /// Quit the compositor.
    Quit,
    /// Show the built-in help (shortcuts) in the focused terminal.
    Help,
    /// Validate the config and show the result in the focused terminal.
    Validate,
    /// Render the current output to a PNG on the DRM/KMS backend.
    Screenshot,
    /// Open the on-screen settings panel.
    Settings,
}

impl Action {
    /// The display label (and search key) for this action.
    pub fn label(&self) -> &str {
        match self {
            Action::Launch { name, .. }
            | Action::Focus { name, .. }
            | Action::System { name, .. } => name,
        }
    }
}

/// State of the launcher overlay.
pub struct LauncherState {
    /// Whether the launcher is currently shown.
    pub open: bool,
    /// The current search query.
    pub query: String,
    /// All discovered applications (sorted by name); combined with dynamic
    /// sources (windows, system actions) at query time by `crate::state`.
    pub apps: Vec<AppEntry>,
    /// Current ranked results (best first).
    pub results: Vec<Action>,
    /// Index of the highlighted result.
    pub selection: usize,
}

impl LauncherState {
    /// Discover installed applications and build the initial state. The scan
    /// runs once at startup; later keystrokes filter the in-memory list.
    pub fn new() -> Self {
        let apps = discover_apps();
        tracing::info!(apps = apps.len(), "launcher index ready");
        Self {
            open: false,
            query: String::new(),
            apps,
            results: Vec::new(),
            selection: 0,
        }
    }

    /// Close the launcher.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Highlight the next / previous result (wrapping).
    pub fn select_next(&mut self) {
        if !self.results.is_empty() {
            self.selection = (self.selection + 1) % self.results.len();
        }
    }
    pub fn select_prev(&mut self) {
        if !self.results.is_empty() {
            self.selection = (self.selection + self.results.len() - 1) % self.results.len();
        }
    }

    /// The currently highlighted action, if any.
    pub fn selected(&self) -> Option<&Action> {
        self.results.get(self.selection)
    }
}

/// Score how well `query` matches `name` (higher is better); `None` if no match.
/// A case-insensitive substring wins (prefix best); otherwise a subsequence
/// match scores by how many characters matched.
pub fn score(query: &str, name: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q = query.to_lowercase();
    let n = name.to_lowercase();

    if let Some(pos) = n.find(&q) {
        let prefix_bonus = if pos == 0 { 500 } else { 0 };
        return Some(1000 - pos as i32 + prefix_bonus);
    }

    // Subsequence: every query char appears in order.
    let mut q_chars = q.chars().peekable();
    let mut matched = 0;
    for ch in n.chars() {
        if q_chars.peek() == Some(&ch) {
            q_chars.next();
            matched += 1;
        }
    }
    if q_chars.peek().is_none() {
        Some(matched)
    } else {
        None
    }
}

/// Rank `actions` against `query`, best match first (ties broken by label),
/// keeping at most [`MAX_RESULTS`].
pub fn rank(query: &str, actions: &[Action]) -> Vec<Action> {
    let mut scored: Vec<(i32, &Action)> = actions
        .iter()
        .filter_map(|action| score(query, action.label()).map(|s| (s, action)))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| system_priority(a.1).cmp(&system_priority(b.1)))
            .then_with(|| a.1.label().to_lowercase().cmp(&b.1.label().to_lowercase()))
    });
    scored
        .into_iter()
        .take(MAX_RESULTS)
        .map(|(_, action)| action.clone())
        .collect()
}

/// Compositor actions sort ahead of applications on exact score ties, so a
/// built-in command like `xfar.quit` never loses to an app of the same name.
fn system_priority(action: &Action) -> u8 {
    match action {
        Action::System { .. } => 0,
        _ => 1,
    }
}

/// Directories to scan for `.desktop` files (XDG data dirs).
fn app_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':') {
        if !dir.is_empty() {
            dirs.push(PathBuf::from(dir).join("applications"));
        }
    }
    dirs
}

/// Discover installed applications, de-duplicated by name and sorted.
fn discover_apps() -> Vec<AppEntry> {
    let mut apps = Vec::new();
    let mut seen = HashSet::new();
    for dir in app_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(app) = parse_desktop(&path) {
                if seen.insert(app.name.clone()) {
                    apps.push(app);
                }
            }
        }
    }
    apps.sort_by_key(|app| app.name.to_lowercase());
    apps
}

/// Parse a `.desktop` file into an [`AppEntry`], or `None` if it should be
/// skipped (hidden, not an application, or missing fields).
fn parse_desktop(path: &std::path::Path) -> Option<AppEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let (mut name, mut exec, mut kind) = (None, None, None);
    let (mut hidden, mut no_display, mut terminal) = (false, false, false);

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            name.get_or_insert_with(|| v.to_string());
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec.get_or_insert_with(|| v.to_string());
        } else if let Some(v) = line.strip_prefix("Type=") {
            kind = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("NoDisplay=") {
            no_display = v.eq_ignore_ascii_case("true");
        } else if let Some(v) = line.strip_prefix("Hidden=") {
            hidden = v.eq_ignore_ascii_case("true");
        } else if let Some(v) = line.strip_prefix("Terminal=") {
            terminal = v.eq_ignore_ascii_case("true");
        }
    }

    if hidden || no_display || kind.as_deref() != Some("Application") {
        return None;
    }
    let name = name?;
    let exec = clean_exec(&exec?);
    if exec.is_empty() {
        return None;
    }
    Some(AppEntry {
        name,
        exec,
        terminal,
    })
}

/// Strip `.desktop` Exec field codes (`%f`, `%U`, …).
fn clean_exec(exec: &str) -> String {
    exec.split_whitespace()
        .filter(|token| !token.starts_with('%'))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(name: &str) -> Action {
        Action::Launch {
            name: name.to_string(),
            exec: "true".to_string(),
            terminal: false,
        }
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(score("", "Anything"), Some(0));
    }

    #[test]
    fn substring_and_subsequence() {
        assert!(score("fire", "Firefox").unwrap() > score("ff", "Firefox").unwrap());
        assert!(score("ff", "Firefox").is_some()); // subsequence f..f
        assert!(score("zq", "Firefox").is_none());
    }

    #[test]
    fn prefix_ranks_first() {
        let actions = vec![launch("LibreOffice Writer"), launch("Writer")];
        let results = rank("writ", &actions);
        assert_eq!(results[0].label(), "Writer");
    }

    #[test]
    fn system_actions_win_exact_ties() {
        let app = Action::Launch {
            name: "xfar.quit".to_string(),
            exec: "true".to_string(),
            terminal: false,
        };
        let quit = Action::System {
            name: "xfar.quit".to_string(),
            kind: SystemAction::Quit,
        };
        let results = rank("xfar.quit", &[app, quit]);
        assert!(matches!(results[0], Action::System { .. }));
    }

    #[test]
    fn rank_excludes_non_matches() {
        let actions = vec![launch("Firefox"), launch("Terminal")];
        let results = rank("term", &actions);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].label(), "Terminal");
    }

    #[test]
    fn clean_exec_strips_field_codes() {
        assert_eq!(clean_exec("firefox %u"), "firefox");
        assert_eq!(clean_exec("gimp-2.10 %U"), "gimp-2.10");
    }
}
