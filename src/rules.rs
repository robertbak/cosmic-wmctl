use anyhow::{Context, Result};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// A rule: move windows whose `app_id` matches `app_id` (and, when given,
/// whose `title` matches `title`) to the workspace selected by `workspace`.
///
/// The workspace selector uses the same syntax as `window-move`: a workspace
/// name, id, or coordinates like `1,0`.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Original app_id glob pattern.
    pub app_id: String,
    /// Original title glob pattern, if any.
    pub title: Option<String>,
    /// Where to move matching windows (name, id, or coordinates).
    pub workspace: String,
    app_id_matcher: GlobMatcher,
    title_matcher: Option<GlobMatcher>,
}

impl Rule {
    /// Build a rule from raw glob patterns.
    pub fn new(app_id: &str, title: Option<&str>, workspace: &str) -> Result<Self> {
        let app_id_matcher = compile_glob(app_id, "app_id")?;
        let title_matcher = title.map(|pattern| compile_glob(pattern, "title")).transpose()?;
        Ok(Self {
            app_id: app_id.to_owned(),
            title: title.map(str::to_owned),
            workspace: workspace.to_owned(),
            app_id_matcher,
            title_matcher,
        })
    }

    /// Does this rule apply to a window with the given `app_id` and `title`?
    pub fn matches(&self, app_id: &str, title: &str) -> bool {
        self.app_id_matcher.is_match(app_id)
            && self.title_matcher.as_ref().is_none_or(|title_glob| title_glob.is_match(title))
    }
}

fn compile_glob(pattern: &str, what: &str) -> Result<GlobMatcher> {
    let glob =
        Glob::new(pattern).with_context(|| format!("invalid {what} glob pattern '{pattern}'"))?;
    Ok(glob.compile_matcher())
}

/// The on-disk rules file, e.g. `~/.config/cosmic-wmctl/rules.toml`.
#[derive(Debug, Deserialize)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<RuleFile>,
}

#[derive(Debug, Deserialize)]
struct RuleFile {
    app_id: String,
    title: Option<String>,
    workspace: String,
}

/// Default location of the rules file: `$XDG_CONFIG_HOME/cosmic-wmctl/rules.toml`
/// (falling back to `$HOME/.config/cosmic-wmctl/rules.toml`).
pub fn default_config_path() -> PathBuf {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    config_home.unwrap_or_else(|| PathBuf::from(".config")).join("cosmic-wmctl").join("rules.toml")
}

/// Load rules from a TOML file at `path`.
pub fn load_rules(path: &Path) -> Result<Vec<Rule>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read rules file {}", path.display()))?;
    let file: RulesFile = toml::from_str(&contents)
        .with_context(|| format!("invalid rules file {}", path.display()))?;

    let mut rules = Vec::with_capacity(file.rules.len());
    for (index, rule) in file.rules.into_iter().enumerate() {
        let rule = Rule::new(&rule.app_id, rule.title.as_deref(), &rule.workspace)
            .with_context(|| format!("rules[{}]", index))?;
        rules.push(rule);
    }
    Ok(rules)
}

/// An example rules file, written out when `daemon --write-example-config` is used.
pub const EXAMPLE_CONFIG: &str = r#"# cosmic-wmctl rules
#
# Move a window to a workspace when it opens. The first matching rule wins.
# Workspace selectors accept a workspace name, id, or coordinates (e.g. "1,0").
#
# app_id  (required) glob matched against the window's app_id, e.g. "org.mozilla.firefox"
# title   (optional) glob matched against the window's title
# workspace (required) where to move matching windows

[[rules]]
app_id = "org.mozilla.firefox"
workspace = "2"

[[rules]]
app_id = "org.gnome.Terminal"
title = "*htop*"
workspace = "3"

[[rules]]
app_id = "com.slack.Slack"
workspace = "4"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_matches_app_id_and_title() {
        let rule = Rule::new("org.mozilla.firefox", Some("*DevTools*"), "2").expect("valid rule");

        assert!(rule.matches("org.mozilla.firefox", "Firefox — DevTools"));
        assert!(!rule.matches("org.mozilla.firefox", "Firefox"));
        assert!(!rule.matches("org.gnome.Terminal", "Firefox — DevTools"));
    }

    #[test]
    fn rule_without_title_glob_matches_any_title() {
        let rule = Rule::new("org.gnome.Terminal", None, "3").expect("valid rule");

        assert!(rule.matches("org.gnome.Terminal", ""));
        assert!(rule.matches("org.gnome.Terminal", "anything"));
    }

    #[test]
    fn glob_is_case_sensitive() {
        let rule = Rule::new("Code", None, "1").expect("valid rule");

        assert!(rule.matches("Code", ""));
        assert!(!rule.matches("code", ""));
    }

    #[test]
    fn invalid_glob_is_rejected() {
        assert!(Rule::new("[", None, "1").is_err());
    }

    #[test]
    fn load_rules_from_toml() {
        let dir =
            std::env::temp_dir().join(format!("cosmic-wmctl-test-{}-load", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("rules.toml");
        std::fs::write(&path, "[[rules]]\napp_id = \"org.mozilla.firefox\"\nworkspace = \"2\"\n")
            .expect("write rules file");

        let rules = load_rules(&path).expect("load rules");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].workspace, "2");
        assert!(rules[0].matches("org.mozilla.firefox", "Firefox"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_rules_file_is_valid() {
        let dir =
            std::env::temp_dir().join(format!("cosmic-wmctl-test-{}-empty", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("rules.toml");
        std::fs::write(&path, "# no rules yet\n").expect("write rules file");

        let rules = load_rules(&path).expect("load rules");
        assert!(rules.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_rules_file_is_an_error() {
        assert!(load_rules(Path::new("/nonexistent/cosmic-wmctl/rules.toml")).is_err());
    }

    #[test]
    fn config_path_uses_xdg_config_home() {
        // Without env vars set, the path still resolves to something sane.
        let path = default_config_path();
        assert!(path.ends_with("cosmic-wmctl/rules.toml"));
    }

    #[test]
    fn example_config_parses() {
        // Written to disk so load_rules can read it, matching how `daemon --init` uses it.
        let dir =
            std::env::temp_dir().join(format!("cosmic-wmctl-test-{}-example", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("rules.toml");
        std::fs::write(&path, EXAMPLE_CONFIG).expect("write example config");

        let rules = load_rules(&path).expect("example config should parse");
        assert_eq!(rules.len(), 3);
        assert!(rules[0].matches("org.mozilla.firefox", "Firefox"));
        assert!(rules[1].matches("org.gnome.Terminal", "htop"));
        assert!(!rules[1].matches("org.gnome.Terminal", "bash"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
