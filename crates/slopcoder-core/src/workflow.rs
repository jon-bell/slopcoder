//! GitHub Actions-style launch workflow parser.
//!
//! Parses `.slopcoder/setup.yml` files with a subset of GitHub Actions syntax.
//! Only `run` steps and a curated set of `uses` steps are supported.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    #[serde(default)]
    pub name: Option<String>,
    pub jobs: std::collections::HashMap<String, Job>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub uses: Option<String>,
    #[serde(default)]
    pub with: std::collections::HashMap<String, String>,
}

/// A resolved step ready for execution.
#[derive(Debug, Clone)]
pub enum ResolvedStep {
    Shell { name: String, command: String },
    Error { name: String, reason: String },
}

/// Curated `uses` actions mapped to shell commands.
fn resolve_uses(action: &str, with: &std::collections::HashMap<String, String>) -> ResolvedStep {
    let name = action.to_string();
    if action.starts_with("actions/setup-node") {
        let version = with.get("node-version").map(|v| v.as_str()).unwrap_or("lts");
        return ResolvedStep::Shell {
            name,
            command: format!(
                "curl -fsSL https://deb.nodesource.com/setup_{}.x | sudo -E bash - && sudo apt-get install -y nodejs",
                version
            ),
        };
    }
    if action.starts_with("actions/setup-python") {
        let version = with.get("python-version").map(|v| v.as_str()).unwrap_or("3");
        return ResolvedStep::Shell {
            name,
            command: format!("sudo apt-get update && sudo apt-get install -y python{}", version),
        };
    }
    if action.starts_with("actions-rust-lang/setup-rust-toolchain") {
        return ResolvedStep::Shell {
            name,
            command: "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y".to_string(),
        };
    }
    ResolvedStep::Error {
        name,
        reason: format!("Unsupported action: {}", action),
    }
}

/// Parse a workflow YAML file and resolve all steps.
pub fn parse_workflow(content: &str) -> Result<Vec<ResolvedStep>, serde_yaml::Error> {
    let workflow: Workflow = serde_yaml::from_str(content)?;
    let mut steps = Vec::new();

    // Take the first job (single-job only)
    if let Some((_, job)) = workflow.jobs.into_iter().next() {
        for step in job.steps {
            let step_name = step.name.unwrap_or_else(|| "unnamed step".to_string());
            if let Some(cmd) = step.run {
                steps.push(ResolvedStep::Shell { name: step_name, command: cmd });
            } else if let Some(action) = step.uses {
                steps.push(resolve_uses(&action, &step.with));
            }
        }
    }

    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_run_steps() {
        let yaml = r#"
name: setup
jobs:
  setup:
    steps:
      - name: Install deps
        run: npm ci
      - name: Build
        run: npm run build
"#;
        let steps = parse_workflow(yaml).unwrap();
        assert_eq!(steps.len(), 2);
        match &steps[0] {
            ResolvedStep::Shell { name, command } => {
                assert_eq!(name, "Install deps");
                assert_eq!(command, "npm ci");
            }
            _ => panic!("expected shell step"),
        }
    }

    #[test]
    fn test_parse_uses_steps() {
        let yaml = r#"
name: setup
jobs:
  setup:
    steps:
      - uses: actions/setup-node@v4
        with:
          node-version: "22"
      - uses: unknown/action@v1
"#;
        let steps = parse_workflow(yaml).unwrap();
        assert_eq!(steps.len(), 2);
        match &steps[0] {
            ResolvedStep::Shell { command, .. } => assert!(command.contains("22")),
            _ => panic!("expected shell step"),
        }
        match &steps[1] {
            ResolvedStep::Error { reason, .. } => assert!(reason.contains("Unsupported")),
            _ => panic!("expected error step"),
        }
    }

    #[test]
    fn test_parse_rust_toolchain() {
        let yaml = r#"
name: setup
jobs:
  setup:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
"#;
        let steps = parse_workflow(yaml).unwrap();
        assert_eq!(steps.len(), 1);
        match &steps[0] {
            ResolvedStep::Shell { command, .. } => assert!(command.contains("rustup")),
            _ => panic!("expected shell step"),
        }
    }
}
