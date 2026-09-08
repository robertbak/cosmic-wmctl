use std::{collections::HashSet, fmt};

use anyhow::{Result, bail};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1;

use crate::wayland::{WorkspaceRecord};

pub fn match_window<'a>(
    windows: &'a [crate::wayland::WindowRecord],
    query: &crate::wayland::Query,
) -> Result<&'a crate::wayland::WindowRecord> {
    let matches = windows.iter().filter(|window| query.matches(window)).collect::<Vec<_>>();

    match matches.as_slice() {
        [window] => Ok(*window),
        [] => bail!("no window matched {}", query),
        _ => bail!(
            "window selector {} matched multiple windows: {}",
            query,
            matches.iter().map(|window| window.summary()).collect::<Vec<_>>().join(" | ")
        ),
    }
}

pub fn match_workspace<'a>(
    workspaces: &'a [WorkspaceRecord],
    selector: &str,
) -> Result<&'a WorkspaceRecord> {
    let exact = workspaces
        .iter()
        .filter(|workspace| workspace.matches_selector(selector))
        .collect::<Vec<_>>();

    match exact.as_slice() {
        [workspace] => Ok(*workspace),
        [] => bail!("no workspace matched '{selector}'"),
        _ => bail!(
            "workspace selector '{selector}' matched multiple workspaces: {}",
            exact.iter().map(|workspace| workspace.summary()).collect::<Vec<_>>().join(" | ")
        ),
    }
}

pub fn resolve_workspace_output(
    workspace: &WorkspaceRecord,
    window: &crate::wayland::WindowRecord,
    requested_output: Option<&str>,
) -> Result<wayland_client::protocol::wl_output::WlOutput> {
    if let Some(requested_output) = requested_output {
        if let Some(output) = workspace.output_by_name(requested_output) {
            return Ok(output);
        }

        bail!(
            "workspace '{}' is not on output '{}'; available outputs: {}",
            workspace.name,
            requested_output,
            workspace.outputs_csv()
        );
    }

    if let Some(output) = workspace
        .output_handles
        .iter()
        .find(|output| window.output_handles.iter().any(|current| current == *output))
        .cloned()
    {
        return Ok(output);
    }

    workspace
        .output_handles
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("workspace '{}' has no associated outputs", workspace.name))
}

/// Wayland proxy handles are valid hash keys; their interior mutability is just
/// an alive-refcount, so the key never changes while hashed.
#[allow(clippy::mutable_key_type)]
pub fn workspace_names(
    handles: &HashSet<ext_workspace_handle_v1::ExtWorkspaceHandleV1>,
    workspaces: &[WorkspaceRecord],
) -> Vec<String> {
    let mut names = handles
        .iter()
        .filter_map(|handle| {
            workspaces
                .iter()
                .find(|workspace| workspace.handle == *handle)
                .map(|workspace| workspace.name.clone())
        })
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coordinates(pub Vec<u32>);

impl Coordinates {
    pub fn parse(value: &str) -> Option<Self> {
        let coordinates = value
            .split(',')
            .map(|component| component.trim().parse().ok())
            .collect::<Option<Vec<u32>>>()?;
        Some(Self(coordinates))
    }
}

impl fmt::Display for Coordinates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("-")
        } else {
            let value = self.0.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            f.write_str(&value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Coordinates;
    use crate::wayland::{Query, WindowSelector, WindowSelectorField};

    #[test]
    fn coordinates_parser_accepts_csv() {
        assert_eq!(Coordinates::parse("1,2,3"), Some(Coordinates(vec![1, 2, 3])));
        assert_eq!(Coordinates::parse("nope"), None);
    }

    #[test]
    fn window_selector_matches_case_insensitive_substring() {
        let selector = WindowSelector {
            field: WindowSelectorField::AppId,
            value: "fire".to_owned(),
            exact: false,
        };

        assert!(selector.matches_field("org.mozilla.firefox"));
        assert!(!selector.matches_field("org.gnome.Terminal"));
    }

    #[test]
    fn query_parse_single_field() {
        let q = Query::parse("app_id=firefox").expect("valid query");
        assert_eq!(q.selectors.len(), 1);
        assert_eq!(q.selectors[0].field, WindowSelectorField::AppId);
        assert_eq!(q.selectors[0].value, "firefox");
    }

    #[test]
    fn query_parse_multi_predicate() {
        let q = Query::parse("app_id=firefox and title=Main").expect("valid query");
        assert_eq!(q.selectors.len(), 2);
        assert_eq!(q.selectors[0].field, WindowSelectorField::AppId);
        assert_eq!(q.selectors[1].field, WindowSelectorField::Title);
    }

    #[test]
    fn query_parse_bare_token_defaults_app_id() {
        let q = Query::parse("firefox").expect("valid query");
        assert_eq!(q.selectors.len(), 1);
        assert_eq!(q.selectors[0].field, WindowSelectorField::AppId);
        assert_eq!(q.selectors[0].value, "firefox");
    }

    #[test]
    fn query_parse_invalid_field() {
        assert!(Query::parse("foo=bar").is_err());
    }

    #[test]
    fn query_parse_empty() {
        assert!(Query::parse("").is_err());
    }
}
