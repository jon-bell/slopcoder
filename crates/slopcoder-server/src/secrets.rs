//! User secrets management (global + environment-scoped).
//!
//! Secrets are stored per-user. In k8s, backed by k8s Secrets.
//! Outside k8s, backed by local files in the data directory.
//! Values are never returned by the API after creation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs;

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("Secret not found: {0}")]
    NotFound(String),
}

/// A secret's metadata (name + scope). Value is never exposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretEntry {
    pub name: String,
    /// None = global, Some(env) = environment-scoped.
    pub environment: Option<String>,
}

/// Internal storage: name → value, keyed by (name, environment).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SecretStore {
    /// Map of (name, environment_or_empty) → value
    secrets: HashMap<String, String>,
}

impl SecretStore {
    fn key(name: &str, environment: Option<&str>) -> String {
        match environment {
            Some(env) => format!("{}:{}", env, name),
            None => name.to_string(),
        }
    }
}

/// File-backed secrets manager for use outside k8s.
#[derive(Debug)]
pub struct LocalSecretsManager {
    data_dir: PathBuf,
}

impl LocalSecretsManager {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    fn user_file(&self, username: &str) -> PathBuf {
        self.data_dir.join(format!("secrets-{}.yaml", username))
    }

    async fn load(&self, username: &str) -> SecretStore {
        let path = self.user_file(username);
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path).await {
                if let Ok(store) = serde_yaml::from_str(&content) {
                    return store;
                }
            }
        }
        SecretStore::default()
    }

    async fn save(&self, username: &str, store: &SecretStore) -> Result<(), SecretsError> {
        fs::create_dir_all(&self.data_dir).await?;
        let content = serde_yaml::to_string(store)?;
        fs::write(self.user_file(username), content).await?;
        Ok(())
    }

    pub async fn set(
        &self,
        username: &str,
        name: &str,
        value: &str,
        environment: Option<&str>,
    ) -> Result<(), SecretsError> {
        let mut store = self.load(username).await;
        let key = SecretStore::key(name, environment);
        store.secrets.insert(key, value.to_string());
        self.save(username, &store).await
    }

    pub async fn delete(
        &self,
        username: &str,
        name: &str,
        environment: Option<&str>,
    ) -> Result<(), SecretsError> {
        let mut store = self.load(username).await;
        let key = SecretStore::key(name, environment);
        if store.secrets.remove(&key).is_none() {
            return Err(SecretsError::NotFound(name.to_string()));
        }
        self.save(username, &store).await
    }

    /// List secret names + scopes (never values).
    pub async fn list(&self, username: &str) -> Vec<SecretEntry> {
        let store = self.load(username).await;
        store
            .secrets
            .keys()
            .map(|key| {
                if let Some((env, name)) = key.split_once(':') {
                    SecretEntry {
                        name: name.to_string(),
                        environment: Some(env.to_string()),
                    }
                } else {
                    SecretEntry {
                        name: key.clone(),
                        environment: None,
                    }
                }
            })
            .collect()
    }

    /// Merge global + environment-scoped secrets for a workspace launch.
    /// Returns name→value map. Environment-scoped wins on collision.
    pub async fn merge_for_workspace(
        &self,
        username: &str,
        environment: Option<&str>,
    ) -> HashMap<String, String> {
        let store = self.load(username).await;
        let mut result = HashMap::new();

        // Global secrets first
        for (key, value) in &store.secrets {
            if !key.contains(':') {
                result.insert(key.clone(), value.clone());
            }
        }

        // Environment-scoped secrets override
        if let Some(env) = environment {
            let prefix = format!("{}:", env);
            for (key, value) in &store.secrets {
                if let Some(name) = key.strip_prefix(&prefix) {
                    result.insert(name.to_string(), value.clone());
                }
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_secrets_crud() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalSecretsManager::new(tmp.path().to_path_buf());

        mgr.set("alice", "API_KEY", "sk-123", None).await.unwrap();
        mgr.set("alice", "DB_URL", "postgres://...", Some("myproject")).await.unwrap();

        let list = mgr.list("alice").await;
        assert_eq!(list.len(), 2);

        mgr.delete("alice", "API_KEY", None).await.unwrap();
        let list = mgr.list("alice").await;
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn test_secrets_merge() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalSecretsManager::new(tmp.path().to_path_buf());

        mgr.set("alice", "API_KEY", "global-key", None).await.unwrap();
        mgr.set("alice", "DB_URL", "global-db", None).await.unwrap();
        mgr.set("alice", "DB_URL", "project-db", Some("myproject")).await.unwrap();

        // No environment: only global
        let merged = mgr.merge_for_workspace("alice", None).await;
        assert_eq!(merged["API_KEY"], "global-key");
        assert_eq!(merged["DB_URL"], "global-db");

        // With environment: env-scoped overrides global
        let merged = mgr.merge_for_workspace("alice", Some("myproject")).await;
        assert_eq!(merged["API_KEY"], "global-key");
        assert_eq!(merged["DB_URL"], "project-db");
    }

    #[tokio::test]
    async fn test_secrets_delete_not_found() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalSecretsManager::new(tmp.path().to_path_buf());
        assert!(mgr.delete("alice", "NOPE", None).await.is_err());
    }
}
