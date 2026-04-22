//! devcontainer.json parser for per-repo workspace configuration.
//!
//! Reads `.devcontainer/devcontainer.json` and extracts standard fields
//! plus SlopCoderNG-specific config from `customizations.slopcoder`.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Parsed devcontainer.json configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DevcontainerConfig {
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default, rename = "postCreateCommand")]
    pub post_create_command: Option<StringOrVec>,
    #[serde(default, rename = "postStartCommand")]
    pub post_start_command: Option<StringOrVec>,
    #[serde(default)]
    pub customizations: Option<Customizations>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    String(String),
    Vec(Vec<String>),
}

impl StringOrVec {
    pub fn as_shell_command(&self) -> String {
        match self {
            StringOrVec::String(s) => s.clone(),
            StringOrVec::Vec(v) => v.join(" && "),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Customizations {
    #[serde(default)]
    pub vscode: Option<VscodeCustomizations>,
    #[serde(default)]
    pub slopcoder: Option<SlopcoderCustomizations>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VscodeCustomizations {
    #[serde(default)]
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlopcoderCustomizations {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub web_search: Option<bool>,
    #[serde(default)]
    pub http_port: Option<u16>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub setup_workflow: Option<String>,
}

/// Resolved workspace configuration after merging defaults + user + devcontainer.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub image: String,
    pub post_create_command: Option<String>,
    pub post_start_command: Option<String>,
    pub vscode_extensions: Vec<String>,
    pub agent: String,
    pub web_search: bool,
    pub http_port: u16,
    pub secrets: Vec<String>,
    pub setup_workflow: Option<String>,
}

impl ResolvedConfig {
    /// Merge platform defaults with an optional devcontainer.json.
    pub fn resolve(devcontainer: Option<&DevcontainerConfig>, default_image: &str) -> Self {
        let mut config = Self {
            image: default_image.to_string(),
            post_create_command: None,
            post_start_command: None,
            vscode_extensions: Vec::new(),
            agent: "claude".to_string(),
            web_search: false,
            http_port: 3000,
            secrets: Vec::new(),
            setup_workflow: None,
        };

        if let Some(dc) = devcontainer {
            if let Some(ref image) = dc.image {
                config.image = image.clone();
            }
            if let Some(ref cmd) = dc.post_create_command {
                config.post_create_command = Some(cmd.as_shell_command());
            }
            if let Some(ref cmd) = dc.post_start_command {
                config.post_start_command = Some(cmd.as_shell_command());
            }
            if let Some(ref customizations) = dc.customizations {
                if let Some(ref vscode) = customizations.vscode {
                    config.vscode_extensions = vscode.extensions.clone();
                }
                if let Some(ref sc) = customizations.slopcoder {
                    if let Some(ref agent) = sc.agent {
                        config.agent = agent.clone();
                    }
                    if let Some(ws) = sc.web_search {
                        config.web_search = ws;
                    }
                    if let Some(port) = sc.http_port {
                        config.http_port = port;
                    }
                    if !sc.secrets.is_empty() {
                        config.secrets = sc.secrets.clone();
                    }
                    config.setup_workflow = sc.setup_workflow.clone();
                }
            }
        }

        config
    }
}

/// Parse a devcontainer.json file.
pub fn parse_devcontainer(content: &str) -> Result<DevcontainerConfig, serde_json::Error> {
    serde_json::from_str(content)
}

/// Try to read and parse `.devcontainer/devcontainer.json` from a directory.
pub async fn read_devcontainer(repo_dir: &Path) -> Option<DevcontainerConfig> {
    let path = repo_dir.join(".devcontainer/devcontainer.json");
    if let Ok(content) = tokio::fs::read_to_string(&path).await {
        parse_devcontainer(&content).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal() {
        let json = r#"{"image": "ubuntu:22.04"}"#;
        let dc = parse_devcontainer(json).unwrap();
        assert_eq!(dc.image.as_deref(), Some("ubuntu:22.04"));
    }

    #[test]
    fn test_parse_full() {
        let json = r#"{
            "image": "ghcr.io/org/dev:latest",
            "postCreateCommand": "npm ci",
            "postStartCommand": ["npm", "run", "dev"],
            "customizations": {
                "vscode": {
                    "extensions": ["rust-lang.rust-analyzer"]
                },
                "slopcoder": {
                    "agent": "codex",
                    "web_search": true,
                    "http_port": 5173,
                    "secrets": ["API_KEY"],
                    "setup_workflow": ".slopcoder/setup.yml"
                }
            }
        }"#;
        let dc = parse_devcontainer(json).unwrap();
        assert_eq!(dc.image.as_deref(), Some("ghcr.io/org/dev:latest"));
        assert_eq!(dc.post_create_command.as_ref().unwrap().as_shell_command(), "npm ci");
        assert_eq!(dc.post_start_command.as_ref().unwrap().as_shell_command(), "npm && run && dev");

        let sc = dc.customizations.as_ref().unwrap().slopcoder.as_ref().unwrap();
        assert_eq!(sc.agent.as_deref(), Some("codex"));
        assert_eq!(sc.http_port, Some(5173));
        assert_eq!(sc.secrets, vec!["API_KEY"]);
    }

    #[test]
    fn test_resolve_defaults() {
        let config = ResolvedConfig::resolve(None, "default:latest");
        assert_eq!(config.image, "default:latest");
        assert_eq!(config.agent, "claude");
        assert_eq!(config.http_port, 3000);
        assert!(!config.web_search);
    }

    #[test]
    fn test_resolve_with_devcontainer() {
        let json = r#"{
            "image": "custom:latest",
            "postCreateCommand": "make setup",
            "customizations": {
                "slopcoder": {
                    "http_port": 8000,
                    "agent": "codex"
                }
            }
        }"#;
        let dc = parse_devcontainer(json).unwrap();
        let config = ResolvedConfig::resolve(Some(&dc), "default:latest");
        assert_eq!(config.image, "custom:latest");
        assert_eq!(config.post_create_command.as_deref(), Some("make setup"));
        assert_eq!(config.http_port, 8000);
        assert_eq!(config.agent, "codex");
    }

    #[test]
    fn test_parse_empty() {
        let dc = parse_devcontainer("{}").unwrap();
        assert!(dc.image.is_none());
        assert!(dc.customizations.is_none());
    }
}
