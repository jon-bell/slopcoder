//! Coordinator-side task persistence.
//!
//! Stores task metadata in YAML files under a data directory.
//! The coordinator is the source of truth for task records in SlopCoderNG.

use slopcoder_core::task::{Task, TaskId, TaskStatus};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs;

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

/// Coordinator-side persistent task store.
#[derive(Debug)]
pub struct CoordinatorTaskStore {
    data_dir: PathBuf,
    tasks: HashMap<TaskId, Task>,
}

impl CoordinatorTaskStore {
    /// Create a new store, loading existing tasks from disk.
    pub async fn new(data_dir: PathBuf) -> Result<Self, PersistenceError> {
        fs::create_dir_all(&data_dir).await?;
        let tasks_file = data_dir.join("tasks.yaml");
        let tasks = if tasks_file.exists() {
            let content = fs::read_to_string(&tasks_file).await?;
            let task_list: Vec<Task> = serde_yaml::from_str(&content)?;
            task_list.into_iter().map(|t| (t.id, t)).collect()
        } else {
            HashMap::new()
        };
        Ok(Self { data_dir, tasks })
    }

    /// Mark any tasks that were running as failed (crash recovery).
    pub fn recover_crashed_tasks(&mut self) -> Vec<TaskId> {
        let mut recovered = Vec::new();
        for task in self.tasks.values_mut() {
            if task.status == TaskStatus::Running {
                task.status = TaskStatus::Failed;
                recovered.push(task.id);
            }
        }
        recovered
    }

    pub fn insert(&mut self, task: Task) {
        self.tasks.insert(task.id, task);
    }

    pub fn get(&self, id: TaskId) -> Option<&Task> {
        self.tasks.get(&id)
    }

    pub fn get_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.get_mut(&id)
    }

    pub fn remove(&mut self, id: TaskId) -> Option<Task> {
        self.tasks.remove(&id)
    }

    pub fn list(&self) -> Vec<&Task> {
        self.tasks.values().collect()
    }

    pub fn list_by_owner(&self, owner: &str) -> Vec<&Task> {
        self.tasks.values().filter(|t| t.owner == owner).collect()
    }

    /// Persist all tasks to disk.
    pub async fn save(&self) -> Result<(), PersistenceError> {
        let tasks: Vec<&Task> = self.tasks.values().collect();
        let content = serde_yaml::to_string(&tasks)?;
        let tasks_file = self.data_dir.join("tasks.yaml");
        fs::write(&tasks_file, content).await?;
        Ok(())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopcoder_core::anyagent::AgentKind;
    use slopcoder_core::task::{TaskStatus, TaskWorkspaceKind};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_task(owner: &str) -> Task {
        let mut t = Task::new(
            AgentKind::Claude,
            "env".to_string(),
            "test task".to_string(),
            TaskWorkspaceKind::Environment,
            None,
            None,
            false,
            PathBuf::from("/tmp/test"),
        );
        t.owner = owner.to_string();
        t.workspace_slug = "test-task".to_string();
        t
    }

    #[tokio::test]
    async fn test_coordinator_store_crud() {
        let tmp = TempDir::new().unwrap();
        let mut store = CoordinatorTaskStore::new(tmp.path().to_path_buf()).await.unwrap();

        let task = make_task("alice");
        let id = task.id;
        store.insert(task);

        assert!(store.get(id).is_some());
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list_by_owner("alice").len(), 1);
        assert_eq!(store.list_by_owner("bob").len(), 0);

        store.remove(id);
        assert!(store.get(id).is_none());
    }

    #[tokio::test]
    async fn test_coordinator_store_persistence() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();

        let task_id;
        {
            let mut store = CoordinatorTaskStore::new(path.clone()).await.unwrap();
            let task = make_task("alice");
            task_id = task.id;
            store.insert(task);
            store.save().await.unwrap();
        }

        let store = CoordinatorTaskStore::new(path).await.unwrap();
        assert!(store.get(task_id).is_some());
        assert_eq!(store.get(task_id).unwrap().owner, "alice");
    }

    #[tokio::test]
    async fn test_coordinator_store_crash_recovery() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();

        {
            let mut store = CoordinatorTaskStore::new(path.clone()).await.unwrap();
            let mut task = make_task("alice");
            task.status = TaskStatus::Running;
            store.insert(task);
            store.save().await.unwrap();
        }

        let mut store = CoordinatorTaskStore::new(path).await.unwrap();
        let recovered = store.recover_crashed_tasks();
        assert_eq!(recovered.len(), 1);
        assert_eq!(store.get(recovered[0]).unwrap().status, TaskStatus::Failed);
    }
}
