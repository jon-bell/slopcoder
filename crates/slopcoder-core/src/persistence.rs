//! YAML-based task persistence per environment directory.
//!
//! Each environment has a `tasks.yaml` file in its directory that
//! stores all tasks for that environment.

use crate::task::{Task, TaskId, TaskStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur during persistence operations.
#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("Failed to read tasks file: {0}")]
    ReadError(#[from] std::io::Error),

    #[error("Failed to parse tasks file: {0}")]
    ParseError(#[from] serde_yaml::Error),

    #[error("Environment directory not found: {0}")]
    DirectoryNotFound(PathBuf),
}

/// YAML file format for storing tasks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TasksFile {
    #[serde(default)]
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone)]
pub struct PendingEnvironmentSave {
    env_dir: PathBuf,
    file: TasksFile,
}

impl PendingEnvironmentSave {
    pub async fn persist(self) -> Result<(), PersistenceError> {
        tokio::fs::create_dir_all(&self.env_dir).await?;
        let path = TasksFile::path_for_env(&self.env_dir);
        self.file.save(&path).await
    }
}

impl TasksFile {
    /// Get the path to the tasks.yaml file for an environment directory.
    pub fn path_for_env(env_dir: &Path) -> PathBuf {
        env_dir.join("tasks.yaml")
    }

    /// Load tasks from a YAML file, returning empty if file doesn't exist.
    pub async fn load(path: &Path) -> Result<Self, PersistenceError> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = tokio::fs::read_to_string(path).await?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }

        let file: TasksFile = serde_yaml::from_str(&content)?;
        Ok(file)
    }

    /// Save tasks to a YAML file.
    pub async fn save(&self, path: &Path) -> Result<(), PersistenceError> {
        let content = serde_yaml::to_string(self)?;
        tracing::debug!("Saving {} tasks to {}", self.tasks.len(), path.display());
        tokio::fs::write(path, content).await?;
        tracing::info!("Saved {} tasks to {}", self.tasks.len(), path.display());
        Ok(())
    }

    /// Validate worktrees exist, removing tasks whose worktrees are gone.
    /// Returns the list of task IDs that were removed.
    pub fn validate_worktrees(&mut self) -> Vec<TaskId> {
        let mut removed = Vec::new();

        self.tasks.retain(|task| {
            if task.worktree_path.exists() {
                return true;
            }

            removed.push(task.id);
            false
        });

        removed
    }

    /// Mark any tasks that were "running" as "failed" (crashed during previous run).
    pub fn recover_crashed_tasks(&mut self) {
        for task in &mut self.tasks {
            if task.status == TaskStatus::Running {
                // Mark as failed since the process died
                task.status = TaskStatus::Failed;
                if let Some(run) = task.history.last_mut() {
                    if run.success.is_none() {
                        run.success = Some(false);
                        run.finished_at = Some(chrono::Utc::now());
                    }
                }
            }
        }
    }
}

/// Persistent task store backed by per-environment YAML files.
#[derive(Debug)]
pub struct PersistentTaskStore {
    /// All tasks indexed by ID.
    tasks: HashMap<TaskId, Task>,
    /// Mapping of environment name to directory path.
    env_directories: HashMap<String, PathBuf>,
}

impl PersistentTaskStore {
    /// Create a new persistent task store.
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            env_directories: HashMap::new(),
        }
    }

    /// Register an environment directory for persistence.
    pub fn register_environment(&mut self, name: String, directory: PathBuf) {
        self.env_directories.insert(name, directory);
    }

    /// Check whether an environment is registered for persistence.
    pub fn has_environment(&self, name: &str) -> bool {
        self.env_directories.contains_key(name)
    }

    /// Load all tasks from all registered environments.
    /// Validates worktrees and recovers crashed tasks.
    pub async fn load_all(&mut self) -> Result<(), PersistenceError> {
        self.tasks.clear();

        for (env_name, env_dir) in &self.env_directories {
            let path = TasksFile::path_for_env(env_dir);
            let mut file = TasksFile::load(&path).await?;

            // Validate worktrees exist
            let removed = file.validate_worktrees();
            if !removed.is_empty() {
                tracing::warn!(
                    "Removed {} tasks from {} with missing worktrees",
                    removed.len(),
                    env_name
                );
            }

            // Recover any tasks that were running when we crashed
            file.recover_crashed_tasks();

            // Save if we made changes
            if !removed.is_empty() || file.tasks.iter().any(|t| t.status == TaskStatus::Failed) {
                file.save(&path).await?;
            }

            // Add tasks to store
            for task in file.tasks {
                self.tasks.insert(task.id, task);
            }
        }

        Ok(())
    }

    fn snapshot_environment(
        &self,
        env_name: &str,
    ) -> Result<PendingEnvironmentSave, PersistenceError> {
        let env_dir = self.env_directories.get(env_name).ok_or_else(|| {
            tracing::error!(
                "Environment '{}' not found in directories: {:?}",
                env_name,
                self.env_directories.keys().collect::<Vec<_>>()
            );
            PersistenceError::DirectoryNotFound(PathBuf::from(env_name))
        })?;

        let tasks: Vec<Task> = self
            .tasks
            .values()
            .filter(|t| t.environment == env_name)
            .cloned()
            .collect();

        tracing::debug!("Found {} tasks for environment '{}'", tasks.len(), env_name);

        Ok(PendingEnvironmentSave {
            env_dir: env_dir.clone(),
            file: TasksFile { tasks },
        })
    }

    /// Save tasks for a specific environment.
    async fn save_environment(&self, env_name: &str) -> Result<(), PersistenceError> {
        self.snapshot_environment(env_name)?.persist().await
    }

    /// Insert a task and persist to disk.
    pub async fn insert(&mut self, task: Task) -> Result<(), PersistenceError> {
        let env_name = task.environment.clone();
        self.tasks.insert(task.id, task);
        self.save_environment(&env_name).await
    }

    /// Insert a task and return a persistence snapshot for later saving.
    pub fn insert_and_snapshot(
        &mut self,
        task: Task,
    ) -> Result<PendingEnvironmentSave, PersistenceError> {
        let env_name = task.environment.clone();
        self.tasks.insert(task.id, task);
        self.snapshot_environment(&env_name)
    }

    /// Get a task by ID.
    pub fn get(&self, id: TaskId) -> Option<&Task> {
        self.tasks.get(&id)
    }

    /// Get a mutable reference to a task.
    /// Note: You must call `save_task` after modifying!
    pub fn get_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.get_mut(&id)
    }

    /// Save a specific task's environment to disk.
    pub async fn save_task(&self, id: TaskId) -> Result<(), PersistenceError> {
        if let Some(task) = self.tasks.get(&id) {
            self.save_environment(&task.environment).await
        } else {
            Ok(())
        }
    }

    /// Build a persistence snapshot for the task's environment.
    pub fn save_task_snapshot(
        &self,
        id: TaskId,
    ) -> Result<Option<PendingEnvironmentSave>, PersistenceError> {
        if let Some(task) = self.tasks.get(&id) {
            Ok(Some(self.snapshot_environment(&task.environment)?))
        } else {
            Ok(None)
        }
    }

    /// List all tasks.
    pub fn list(&self) -> Vec<&Task> {
        self.tasks.values().collect()
    }

    /// List tasks for a specific environment.
    pub fn list_by_environment(&self, environment: &str) -> Vec<&Task> {
        self.tasks
            .values()
            .filter(|t| t.environment == environment)
            .collect()
    }

    /// Get the persistence directory registered for an environment.
    pub fn get_environment_directory(&self, environment: &str) -> Option<PathBuf> {
        self.env_directories.get(environment).cloned()
    }

    /// Check if a task's worktree still exists.
    /// Returns false if the worktree was deleted.
    pub fn validate_task_worktree(&self, id: TaskId) -> bool {
        self.tasks
            .get(&id)
            .map(|t| t.worktree_path.exists())
            .unwrap_or(false)
    }

    /// Remove a task and persist to disk.
    pub async fn remove(&mut self, id: TaskId) -> Result<Option<Task>, PersistenceError> {
        if let Some(task) = self.tasks.remove(&id) {
            let env_name = task.environment.clone();
            self.save_environment(&env_name).await?;
            Ok(Some(task))
        } else {
            Ok(None)
        }
    }

    /// Remove a task and return a persistence snapshot for later saving.
    pub fn remove_and_snapshot(
        &mut self,
        id: TaskId,
    ) -> Result<(Option<Task>, Option<PendingEnvironmentSave>), PersistenceError> {
        if let Some(task) = self.tasks.remove(&id) {
            let snapshot = self.snapshot_environment(&task.environment)?;
            Ok((Some(task), Some(snapshot)))
        } else {
            Ok((None, None))
        }
    }

    /// Validate all worktrees and remove tasks whose worktrees no longer exist.
    /// Returns the number of tasks removed.
    pub async fn cleanup_stale_tasks(&mut self) -> Result<usize, PersistenceError> {
        // Find tasks with missing worktrees
        let stale_tasks: Vec<(TaskId, String)> = self
            .tasks
            .values()
            .filter(|t| !t.worktree_path.exists())
            .map(|t| (t.id, t.environment.clone()))
            .collect();

        if stale_tasks.is_empty() {
            return Ok(0);
        }

        let count = stale_tasks.len();
        tracing::info!("Cleaning up {} tasks with missing worktrees", count);

        // Collect affected environments
        let mut affected_envs: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (id, env) in stale_tasks {
            self.tasks.remove(&id);
            affected_envs.insert(env);
        }

        // Save affected environments
        for env in affected_envs {
            self.save_environment(&env).await?;
        }

        Ok(count)
    }

    /// Remove stale tasks and return environment snapshots for later saving.
    pub fn cleanup_stale_tasks_and_snapshot(
        &mut self,
    ) -> Result<(usize, Vec<PendingEnvironmentSave>), PersistenceError> {
        let stale_tasks: Vec<(TaskId, String)> = self
            .tasks
            .values()
            .filter(|t| !t.worktree_path.exists())
            .map(|t| (t.id, t.environment.clone()))
            .collect();

        if stale_tasks.is_empty() {
            return Ok((0, Vec::new()));
        }

        let count = stale_tasks.len();
        tracing::info!("Cleaning up {} tasks with missing worktrees", count);

        let mut affected_envs: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (id, env) in stale_tasks {
            self.tasks.remove(&id);
            affected_envs.insert(env);
        }

        let mut snapshots = Vec::with_capacity(affected_envs.len());
        for env in affected_envs {
            snapshots.push(self.snapshot_environment(&env)?);
        }

        Ok((count, snapshots))
    }
}

impl Default for PersistentTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anyagent::AgentKind;
    use crate::task::PromptRun;
    use chrono::Utc;
    use tempfile::TempDir;

    fn create_test_task(
        env: &str,
        base_branch: Option<&str>,
        merge_branch: &str,
        worktree: PathBuf,
    ) -> Task {
        Task {
            id: TaskId::new(),
            agent: AgentKind::Codex,
            environment: env.to_string(),
            name: "test task".to_string(),
            workspace_kind: crate::task::TaskWorkspaceKind::Worktree,
            base_branch: base_branch.map(|b| b.to_string()),
            merge_branch: Some(merge_branch.to_string()),
            web_search: false,
            worktree_path: worktree,
            status: TaskStatus::Completed,
            session_id: None,
            created_at: Utc::now(),
            history: vec![PromptRun::new("test prompt".to_string())],
            owner: String::new(),
            workspace_slug: String::new(),
            pod_name: None,
            ssh_port: None,
            ssh_command: None,
            workspace_url: None,
            app_url: None,
            http_port: 3000,
            collaborators: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_tasks_file_save_load() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("tasks.yaml");

        let task = create_test_task(
            "test-env",
            Some("main"),
            "feature/test",
            temp_dir.path().join("worktree"),
        );
        let file = TasksFile {
            tasks: vec![task.clone()],
        };

        file.save(&path).await.unwrap();

        let loaded = TasksFile::load(&path).await.unwrap();
        assert_eq!(loaded.tasks.len(), 1);
        assert_eq!(
            loaded.tasks[0].merge_branch.as_deref(),
            Some("feature/test")
        );
    }

    #[tokio::test]
    async fn test_validate_worktrees() {
        let temp_dir = TempDir::new().unwrap();

        // Create a worktree directory that exists
        let existing_worktree = temp_dir.path().join("existing");
        tokio::fs::create_dir(&existing_worktree).await.unwrap();

        // Reference a worktree that doesn't exist
        let missing_worktree = temp_dir.path().join("missing");

        let task1 = create_test_task("env", Some("main"), "feature/a", existing_worktree);
        let task2 = create_test_task("env", Some("main"), "feature/b", missing_worktree);
        let id2 = task2.id;

        let mut file = TasksFile {
            tasks: vec![task1, task2],
        };

        let removed = file.validate_worktrees();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], id2);
        assert_eq!(file.tasks.len(), 1);
    }

    #[tokio::test]
    async fn test_recover_crashed_tasks() {
        let temp_dir = TempDir::new().unwrap();
        let worktree = temp_dir.path().join("worktree");
        tokio::fs::create_dir(&worktree).await.unwrap();

        let mut task = create_test_task("env", Some("main"), "feature/a", worktree);
        task.status = TaskStatus::Running;
        task.history[0].success = None;

        let mut file = TasksFile { tasks: vec![task] };

        file.recover_crashed_tasks();

        assert_eq!(file.tasks[0].status, TaskStatus::Failed);
        assert_eq!(file.tasks[0].history[0].success, Some(false));
    }

    #[tokio::test]
    async fn test_persistent_store() {
        let temp_dir = TempDir::new().unwrap();
        let worktree = temp_dir.path().join("main");
        tokio::fs::create_dir(&worktree).await.unwrap();

        let mut store = PersistentTaskStore::new();
        store.register_environment("test-env".to_string(), temp_dir.path().to_path_buf());

        let task = create_test_task("test-env", Some("main"), "feature/test", worktree);
        let task_id = task.id;

        store.insert(task).await.unwrap();

        // Verify file was created
        let path = TasksFile::path_for_env(temp_dir.path());
        assert!(path.exists());

        // Verify we can load it back
        let mut new_store = PersistentTaskStore::new();
        new_store.register_environment("test-env".to_string(), temp_dir.path().to_path_buf());
        new_store.load_all().await.unwrap();

        assert!(new_store.get(task_id).is_some());
    }

    #[tokio::test]
    async fn test_save_task_persists_renamed_task_name() {
        let temp_dir = TempDir::new().unwrap();
        let worktree = temp_dir.path().join("main");
        tokio::fs::create_dir(&worktree).await.unwrap();

        let mut store = PersistentTaskStore::new();
        store.register_environment("test-env".to_string(), temp_dir.path().to_path_buf());

        let task = create_test_task("test-env", Some("main"), "feature/test", worktree);
        let task_id = task.id;
        store.insert(task).await.unwrap();

        let renamed = "renamed task".to_string();
        store.get_mut(task_id).unwrap().rename(renamed.clone());
        store.save_task(task_id).await.unwrap();

        let path = TasksFile::path_for_env(temp_dir.path());
        let loaded = TasksFile::load(&path).await.unwrap();
        assert_eq!(loaded.tasks.len(), 1);
        assert_eq!(loaded.tasks[0].name, renamed);
    }

    /// Test scenario: worktree is deleted between task runs (simulating CLI removal)
    #[tokio::test]
    async fn test_worktree_deleted_between_runs() {
        let temp_dir = TempDir::new().unwrap();
        let worktree = temp_dir.path().join("feature-branch");
        tokio::fs::create_dir(&worktree).await.unwrap();

        // Create store and add a task
        let mut store = PersistentTaskStore::new();
        store.register_environment("my-project".to_string(), temp_dir.path().to_path_buf());

        let task = create_test_task(
            "my-project",
            Some("main"),
            "feature-branch",
            worktree.clone(),
        );
        let task_id = task.id;
        store.insert(task).await.unwrap();

        // Verify task exists and worktree is valid
        assert!(store.get(task_id).is_some());
        assert!(store.validate_task_worktree(task_id));

        // Simulate user deleting worktree from CLI: rm -rf /path/to/worktree
        tokio::fs::remove_dir_all(&worktree).await.unwrap();

        // Worktree validation should now fail
        assert!(!store.validate_task_worktree(task_id));

        // Simulate server restart: create new store and load
        let mut new_store = PersistentTaskStore::new();
        new_store.register_environment("my-project".to_string(), temp_dir.path().to_path_buf());
        new_store.load_all().await.unwrap();

        // Task should have been removed during load because worktree is gone
        assert!(new_store.get(task_id).is_none());
        assert_eq!(new_store.list().len(), 0);
    }

    /// Test scenario: some worktrees exist, others were removed
    #[tokio::test]
    async fn test_partial_worktree_cleanup() {
        let temp_dir = TempDir::new().unwrap();

        // Create three worktrees
        let worktree1 = temp_dir.path().join("main");
        let worktree2 = temp_dir.path().join("feature-a");
        let worktree3 = temp_dir.path().join("feature-b");
        tokio::fs::create_dir(&worktree1).await.unwrap();
        tokio::fs::create_dir(&worktree2).await.unwrap();
        tokio::fs::create_dir(&worktree3).await.unwrap();

        // Create store with three tasks
        let mut store = PersistentTaskStore::new();
        store.register_environment("project".to_string(), temp_dir.path().to_path_buf());

        let task1 = create_test_task("project", Some("main"), "feature/main", worktree1.clone());
        let task2 = create_test_task("project", Some("main"), "feature-a", worktree2.clone());
        let task3 = create_test_task("project", Some("main"), "feature-b", worktree3.clone());
        let id1 = task1.id;
        let id2 = task2.id;
        let id3 = task3.id;

        store.insert(task1).await.unwrap();
        store.insert(task2).await.unwrap();
        store.insert(task3).await.unwrap();

        assert_eq!(store.list().len(), 3);

        // User removes two worktrees from CLI
        tokio::fs::remove_dir_all(&worktree1).await.unwrap();
        tokio::fs::remove_dir_all(&worktree3).await.unwrap();

        // Simulate server restart
        let mut new_store = PersistentTaskStore::new();
        new_store.register_environment("project".to_string(), temp_dir.path().to_path_buf());
        new_store.load_all().await.unwrap();

        // Only task2 should remain (feature-a still has worktree)
        assert_eq!(new_store.list().len(), 1);
        assert!(new_store.get(id1).is_none());
        assert!(new_store.get(id2).is_some());
        assert!(new_store.get(id3).is_none());
    }

    /// Test scenario: server crashes while task is running
    #[tokio::test]
    async fn test_server_crash_recovery() {
        let temp_dir = TempDir::new().unwrap();
        let worktree = temp_dir.path().join("main");
        tokio::fs::create_dir(&worktree).await.unwrap();

        // Create a task that's in "running" state (simulating crash mid-execution)
        let mut task = create_test_task("project", Some("main"), "feature/a", worktree.clone());
        task.status = TaskStatus::Running;
        task.history[0].success = None;
        task.history[0].finished_at = None;
        let task_id = task.id;

        // Save directly to YAML (simulating state before crash)
        let file = TasksFile { tasks: vec![task] };
        let path = TasksFile::path_for_env(temp_dir.path());
        file.save(&path).await.unwrap();

        // Simulate server restart after crash
        let mut store = PersistentTaskStore::new();
        store.register_environment("project".to_string(), temp_dir.path().to_path_buf());
        store.load_all().await.unwrap();

        // Task should be recovered and marked as failed
        let recovered_task = store.get(task_id).unwrap();
        assert_eq!(recovered_task.status, TaskStatus::Failed);
        assert_eq!(recovered_task.history[0].success, Some(false));
        assert!(recovered_task.history[0].finished_at.is_some());
    }

    /// Test scenario: multiple environments with mixed task states
    #[tokio::test]
    async fn test_multiple_environments_cleanup() {
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        // Setup first environment with two tasks
        let worktree1a = temp_dir1.path().join("main");
        let worktree1b = temp_dir1.path().join("dev");
        tokio::fs::create_dir(&worktree1a).await.unwrap();
        tokio::fs::create_dir(&worktree1b).await.unwrap();

        // Setup second environment with one task
        let worktree2 = temp_dir2.path().join("main");
        tokio::fs::create_dir(&worktree2).await.unwrap();

        let mut store = PersistentTaskStore::new();
        store.register_environment("project-a".to_string(), temp_dir1.path().to_path_buf());
        store.register_environment("project-b".to_string(), temp_dir2.path().to_path_buf());

        let task1a = create_test_task("project-a", Some("main"), "feature/a", worktree1a.clone());
        let task1b = create_test_task("project-a", Some("main"), "feature/b", worktree1b.clone());
        let task2 = create_test_task("project-b", Some("main"), "feature/c", worktree2.clone());
        let id1a = task1a.id;
        let id1b = task1b.id;
        let id2 = task2.id;

        store.insert(task1a).await.unwrap();
        store.insert(task1b).await.unwrap();
        store.insert(task2).await.unwrap();

        // Verify both environments have tasks.yaml
        assert!(TasksFile::path_for_env(temp_dir1.path()).exists());
        assert!(TasksFile::path_for_env(temp_dir2.path()).exists());

        // Delete one worktree from each environment
        tokio::fs::remove_dir_all(&worktree1a).await.unwrap();
        tokio::fs::remove_dir_all(&worktree2).await.unwrap();

        // Reload
        let mut new_store = PersistentTaskStore::new();
        new_store.register_environment("project-a".to_string(), temp_dir1.path().to_path_buf());
        new_store.register_environment("project-b".to_string(), temp_dir2.path().to_path_buf());
        new_store.load_all().await.unwrap();

        // Only task1b should remain
        assert_eq!(new_store.list().len(), 1);
        assert!(new_store.get(id1a).is_none());
        assert!(new_store.get(id1b).is_some());
        assert!(new_store.get(id2).is_none());
    }

    /// Test scenario: empty tasks.yaml file
    #[tokio::test]
    async fn test_empty_tasks_file() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("tasks.yaml");

        // Create an empty file
        tokio::fs::write(&path, "").await.unwrap();

        let loaded = TasksFile::load(&path).await.unwrap();
        assert!(loaded.tasks.is_empty());
    }

    /// Test scenario: tasks.yaml doesn't exist (fresh environment)
    #[tokio::test]
    async fn test_missing_tasks_file() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("tasks.yaml");

        // Don't create the file
        assert!(!path.exists());

        let loaded = TasksFile::load(&path).await.unwrap();
        assert!(loaded.tasks.is_empty());
    }

    /// Test that validate_task_worktree works correctly during runtime
    #[tokio::test]
    async fn test_runtime_worktree_validation() {
        let temp_dir = TempDir::new().unwrap();
        let worktree = temp_dir.path().join("feature");
        tokio::fs::create_dir(&worktree).await.unwrap();

        let mut store = PersistentTaskStore::new();
        store.register_environment("project".to_string(), temp_dir.path().to_path_buf());

        let task = create_test_task("project", Some("main"), "feature", worktree.clone());
        let task_id = task.id;
        store.insert(task).await.unwrap();

        // Worktree exists
        assert!(store.validate_task_worktree(task_id));

        // Delete worktree at runtime (simulating concurrent CLI operation)
        tokio::fs::remove_dir_all(&worktree).await.unwrap();

        // Should now fail validation
        assert!(!store.validate_task_worktree(task_id));

        // Nonexistent task should also fail
        assert!(!store.validate_task_worktree(TaskId::new()));
    }

    /// Test cleanup_stale_tasks removes tasks with missing worktrees
    #[tokio::test]
    async fn test_cleanup_stale_tasks() {
        let temp_dir = TempDir::new().unwrap();
        let worktree1 = temp_dir.path().join("main");
        let worktree2 = temp_dir.path().join("feature");
        tokio::fs::create_dir(&worktree1).await.unwrap();
        tokio::fs::create_dir(&worktree2).await.unwrap();

        let mut store = PersistentTaskStore::new();
        store.register_environment("project".to_string(), temp_dir.path().to_path_buf());

        let task1 = create_test_task("project", Some("main"), "feature/a", worktree1.clone());
        let task2 = create_test_task("project", Some("main"), "feature/b", worktree2.clone());
        let id1 = task1.id;
        let id2 = task2.id;

        store.insert(task1).await.unwrap();
        store.insert(task2).await.unwrap();

        assert_eq!(store.list().len(), 2);

        // Delete one worktree (simulating `git worktree remove`)
        tokio::fs::remove_dir_all(&worktree1).await.unwrap();

        // Cleanup should remove the stale task
        let removed = store.cleanup_stale_tasks().await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.list().len(), 1);
        assert!(store.get(id1).is_none());
        assert!(store.get(id2).is_some());

        // Verify tasks.yaml was updated
        let path = TasksFile::path_for_env(temp_dir.path());
        let file = TasksFile::load(&path).await.unwrap();
        assert_eq!(file.tasks.len(), 1);
        assert_eq!(file.tasks[0].id, id2);
    }

    /// Test cleanup_stale_tasks removes missing worktrees for non-completed tasks
    #[tokio::test]
    async fn test_cleanup_stale_tasks_non_completed() {
        let temp_dir = TempDir::new().unwrap();
        let worktree1 = temp_dir.path().join("main");
        let worktree2 = temp_dir.path().join("feature");
        tokio::fs::create_dir(&worktree1).await.unwrap();
        tokio::fs::create_dir(&worktree2).await.unwrap();

        let mut store = PersistentTaskStore::new();
        store.register_environment("project".to_string(), temp_dir.path().to_path_buf());

        let mut task1 = create_test_task("project", Some("main"), "feature/a", worktree1.clone());
        task1.status = TaskStatus::Running;
        let task2 = create_test_task("project", Some("main"), "feature/b", worktree2.clone());
        let id1 = task1.id;
        let id2 = task2.id;

        store.insert(task1).await.unwrap();
        store.insert(task2).await.unwrap();

        // Delete one worktree (simulating `git worktree remove`)
        tokio::fs::remove_dir_all(&worktree1).await.unwrap();

        // Cleanup should remove the missing task even if it wasn't completed
        let removed = store.cleanup_stale_tasks().await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.list().len(), 1);
        assert!(store.get(id1).is_none());
        assert!(store.get(id2).is_some());
    }

    #[tokio::test]
    async fn test_snapshot_persistence_helpers() {
        let temp_dir = TempDir::new().unwrap();
        let worktree = temp_dir.path().join("feature");
        tokio::fs::create_dir(&worktree).await.unwrap();

        let mut store = PersistentTaskStore::new();
        store.register_environment("project".to_string(), temp_dir.path().to_path_buf());

        let task = create_test_task("project", Some("main"), "feature/a", worktree);
        let task_id = task.id;

        let snapshot = store.insert_and_snapshot(task).unwrap();
        snapshot.persist().await.unwrap();

        let loaded = TasksFile::load(&TasksFile::path_for_env(temp_dir.path()))
            .await
            .unwrap();
        assert_eq!(loaded.tasks.len(), 1);
        assert_eq!(loaded.tasks[0].id, task_id);

        let saved_snapshot = store.save_task_snapshot(task_id).unwrap().unwrap();
        saved_snapshot.persist().await.unwrap();

        let (_removed, removal_snapshot) = store.remove_and_snapshot(task_id).unwrap();
        removal_snapshot.unwrap().persist().await.unwrap();

        let reloaded = TasksFile::load(&TasksFile::path_for_env(temp_dir.path()))
            .await
            .unwrap();
        assert!(reloaded.tasks.is_empty());
    }
}
