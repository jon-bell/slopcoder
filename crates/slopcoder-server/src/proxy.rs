//! OAuth-aware reverse proxy for workspace subdomains.
//!
//! Routes `<slug>.<WORKSPACE_DOMAIN>` to code-server (port 8080) and
//! `www.<slug>.<WORKSPACE_DOMAIN>` to the user's app port.
//! All requests are authenticated via JWT cookie and checked against
//! workspace owner/collaborator access.

use crate::state::AppState;
use crate::routes::JwtClaims;

/// Parse a workspace slug from a Host header given the workspace domain.
/// Returns (slug, is_www) or None if the host doesn't match.
pub fn parse_workspace_host(host: &str, workspace_domain: &str) -> Option<(String, bool)> {
    // Strip port if present
    let host = host.split(':').next().unwrap_or(host);
    let suffix = format!(".{}", workspace_domain);
    if !host.ends_with(&suffix) {
        return None;
    }
    let prefix = &host[..host.len() - suffix.len()];
    if prefix.is_empty() || prefix == workspace_domain {
        return None;
    }
    if let Some(slug) = prefix.strip_prefix("www.") {
        if !slug.is_empty() {
            return Some((slug.to_string(), true));
        }
        return None;
    }
    Some((prefix.to_string(), false))
}

/// Check if a user has access to a workspace (owner or collaborator).
pub async fn check_workspace_access(
    state: &AppState,
    slug: &str,
    username: &str,
) -> Result<u16, WorkspaceAccessError> {
    let store = state.task_store().read().await;
    for task in store.list() {
        if task.workspace_slug == slug {
            if task.owner == username || task.collaborators.contains(&username.to_string()) {
                return Ok(task.http_port);
            }
            return Err(WorkspaceAccessError::Forbidden);
        }
    }
    Err(WorkspaceAccessError::NotFound)
}

#[derive(Debug)]
pub enum WorkspaceAccessError {
    NotFound,
    Forbidden,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workspace_host_basic() {
        let (slug, is_www) = parse_workspace_host("fix-login.work.example.com", "work.example.com").unwrap();
        assert_eq!(slug, "fix-login");
        assert!(!is_www);
    }

    #[test]
    fn test_parse_workspace_host_www() {
        let (slug, is_www) = parse_workspace_host("www.fix-login.work.example.com", "work.example.com").unwrap();
        assert_eq!(slug, "fix-login");
        assert!(is_www);
    }

    #[test]
    fn test_parse_workspace_host_with_port() {
        let (slug, is_www) = parse_workspace_host("my-ws.work.example.com:8080", "work.example.com").unwrap();
        assert_eq!(slug, "my-ws");
        assert!(!is_www);
    }

    #[test]
    fn test_parse_workspace_host_no_match() {
        assert!(parse_workspace_host("other.example.com", "work.example.com").is_none());
        assert!(parse_workspace_host("work.example.com", "work.example.com").is_none());
        assert!(parse_workspace_host("", "work.example.com").is_none());
    }

    #[test]
    fn test_parse_workspace_host_bare_www() {
        assert!(parse_workspace_host("www..work.example.com", "work.example.com").is_none());
    }
}
