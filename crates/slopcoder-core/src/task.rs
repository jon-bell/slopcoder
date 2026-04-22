//! Task management for agent runs.
//!
//! A task represents a single agent session running either directly in an
//! environment repository or in an isolated worktree.

use crate::anyagent::AgentKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Unique identifier for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Create a new random task ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where the task executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskWorkspaceKind {
    /// Task runs directly in the configured environment repository directory.
    #[default]
    Environment,
    /// Task runs in an isolated git worktree and can be merged back.
    Worktree,
}

/// Status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task created but agent not yet started.
    Pending,
    /// Agent is currently running.
    Running,
    /// Agent completed successfully.
    Completed,
    /// Agent failed with an error.
    Failed,
    /// Agent was interrupted by the user.
    Interrupted,
}

/// A single prompt and its result in the task history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRun {
    /// The prompt text sent to the agent.
    #[serde(default, skip_serializing)]
    pub prompt: String,
    /// When this prompt was sent.
    pub started_at: DateTime<Utc>,
    /// When the agent finished (if finished).
    pub finished_at: Option<DateTime<Utc>>,
    /// Whether this run succeeded.
    pub success: Option<bool>,
}

impl PromptRun {
    /// Create a new prompt run starting now.
    pub fn new(prompt: String) -> Self {
        Self {
            prompt,
            started_at: Utc::now(),
            finished_at: None,
            success: None,
        }
    }

    /// Mark this run as finished.
    pub fn finish(&mut self, success: bool) {
        self.finished_at = Some(Utc::now());
        self.success = Some(success);
    }
}

/// A task representing an agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// Unique task identifier.
    pub id: TaskId,
    /// Which agent implementation this task uses.
    #[serde(default)]
    pub agent: AgentKind,
    /// Name of the environment this task belongs to.
    pub environment: String,
    /// Human-friendly task topic/name.
    pub name: String,
    /// Whether this runs in-place or in an isolated worktree.
    pub workspace_kind: TaskWorkspaceKind,
    /// Base branch used for isolated worktrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Branch used for merge when task runs in an isolated worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_branch: Option<String>,
    /// Whether web search is enabled for this task.
    #[serde(default)]
    pub web_search: bool,
    /// Path to the task workspace directory.
    pub worktree_path: PathBuf,
    /// Current status of the task.
    pub status: TaskStatus,
    /// Session ID (set after first run).
    pub session_id: Option<Uuid>,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// History of prompt runs.
    pub history: Vec<PromptRun>,
    // -- SlopCoderNG fields --
    /// GitHub username of the task owner.
    #[serde(default)]
    pub owner: String,
    /// Subdomain slug for workspace URLs.
    #[serde(default)]
    pub workspace_slug: String,
    /// Kubernetes pod name when running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_name: Option<String>,
    /// Allocated SSH port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    /// SSH connection command string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_command: Option<String>,
    /// code-server URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_url: Option<String>,
    /// App forwarding URL (www. subdomain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_url: Option<String>,
    /// Port inside the pod that www. routes to.
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// GitHub usernames granted access by the owner.
    #[serde(default)]
    pub collaborators: Vec<String>,
}

fn default_http_port() -> u16 {
    3000
}

impl Task {
    /// Create a new task.
    pub fn new(
        agent: AgentKind,
        environment: String,
        name: String,
        workspace_kind: TaskWorkspaceKind,
        base_branch: Option<String>,
        merge_branch: Option<String>,
        web_search: bool,
        worktree_path: PathBuf,
    ) -> Self {
        Self {
            id: TaskId::new(),
            agent,
            environment,
            name,
            workspace_kind,
            base_branch,
            merge_branch,
            web_search,
            worktree_path,
            status: TaskStatus::Pending,
            session_id: None,
            created_at: Utc::now(),
            history: Vec::new(),
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

    /// Check if this task can accept new prompts.
    pub fn can_run(&self) -> bool {
        matches!(
            self.status,
            TaskStatus::Pending
                | TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Interrupted
        )
    }

    /// Check if the agent is currently running.
    pub fn is_running(&self) -> bool {
        self.status == TaskStatus::Running
    }

    /// Start a new prompt run.
    pub fn start_run(&mut self, prompt: String) {
        self.status = TaskStatus::Running;
        self.history.push(PromptRun::new(prompt));
    }

    /// Mark the current run as completed.
    pub fn complete_run(&mut self, success: bool) {
        if let Some(run) = self.history.last_mut() {
            run.finish(success);
        }
        self.status = if success {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        };
    }

    /// Mark the current run as interrupted.
    pub fn interrupt_run(&mut self) {
        if let Some(run) = self.history.last_mut() {
            run.finish(false);
        }
        self.status = TaskStatus::Interrupted;
    }

    /// Rename the task to a new human-friendly topic.
    pub fn rename(&mut self, name: String) {
        self.name = name;
    }

    /// Get the last prompt that was run.
    pub fn last_prompt(&self) -> Option<&str> {
        self.history.last().map(|r| r.prompt.as_str())
    }
}

/// In-memory storage for tasks.
#[derive(Debug, Default)]
pub struct TaskStore {
    tasks: std::collections::HashMap<TaskId, Task>,
}

impl TaskStore {
    /// Create a new empty task store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a task into the store.
    pub fn insert(&mut self, task: Task) {
        self.tasks.insert(task.id, task);
    }

    /// Get a task by ID.
    pub fn get(&self, id: TaskId) -> Option<&Task> {
        self.tasks.get(&id)
    }

    /// Get a mutable reference to a task by ID.
    pub fn get_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.get_mut(&id)
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

    /// Remove a task.
    pub fn remove(&mut self, id: TaskId) -> Option<Task> {
        self.tasks.remove(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_task_creation() {
        let task = Task::new(
            AgentKind::Codex,
            "my-env".to_string(),
            "login fixes".to_string(),
            TaskWorkspaceKind::Worktree,
            Some("main".to_string()),
            Some("task/login-fixes".to_string()),
            false,
            PathBuf::from("/tmp/worktree"),
        );

        assert_eq!(task.base_branch.as_deref(), Some("main"));
        assert_eq!(task.merge_branch.as_deref(), Some("task/login-fixes"));
        assert_eq!(task.name, "login fixes");
        assert_eq!(task.workspace_kind, TaskWorkspaceKind::Worktree);
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.session_id.is_none());
        assert!(task.history.is_empty());
        assert!(task.can_run());
    }

    #[test]
    fn test_task_run_lifecycle() {
        let mut task = Task::new(
            AgentKind::Codex,
            "env".to_string(),
            "topic".to_string(),
            TaskWorkspaceKind::Environment,
            None,
            None,
            false,
            PathBuf::from("/tmp"),
        );

        assert!(task.can_run());
        assert!(!task.is_running());

        task.start_run("Hello world".to_string());
        assert!(!task.can_run());
        assert!(task.is_running());
        assert_eq!(task.history.len(), 1);

        task.complete_run(true);
        assert!(task.can_run());
        assert!(!task.is_running());
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.history[0].success, Some(true));
    }

    #[test]
    fn test_task_rename() {
        let mut task = Task::new(
            AgentKind::Codex,
            "env".to_string(),
            "old topic".to_string(),
            TaskWorkspaceKind::Environment,
            None,
            None,
            false,
            PathBuf::from("/tmp"),
        );

        task.rename("new topic".to_string());

        assert_eq!(task.name, "new topic");
    }

    #[test]
    fn test_task_store() {
        let mut store = TaskStore::new();

        let task1 = Task::new(
            AgentKind::Codex,
            "env1".to_string(),
            "topic one".to_string(),
            TaskWorkspaceKind::Environment,
            None,
            None,
            false,
            PathBuf::from("/tmp/1"),
        );
        let task2 = Task::new(
            AgentKind::Codex,
            "env2".to_string(),
            "topic two".to_string(),
            TaskWorkspaceKind::Worktree,
            Some("main".to_string()),
            Some("task/topic-two".to_string()),
            false,
            PathBuf::from("/tmp/2"),
        );

        let id1 = task1.id;
        let id2 = task2.id;

        store.insert(task1);
        store.insert(task2);

        assert_eq!(store.list().len(), 2);
        assert!(store.get(id1).is_some());
        assert!(store.get(id2).is_some());

        assert_eq!(store.list_by_environment("env1").len(), 1);
    }

    #[test]
    fn test_task_interrupt() {
        let mut task = Task::new(
            AgentKind::Codex,
            "env".to_string(),
            "topic".to_string(),
            TaskWorkspaceKind::Environment,
            None,
            None,
            false,
            PathBuf::from("/tmp"),
        );

        task.start_run("Test prompt".to_string());
        assert!(task.is_running());
        assert!(!task.can_run());

        task.interrupt_run();
        assert_eq!(task.status, TaskStatus::Interrupted);
        assert!(task.can_run());
        assert!(!task.is_running());
        assert_eq!(task.history.len(), 1);
        assert_eq!(task.history[0].success, Some(false));
    }

    #[test]
    fn test_task_resume_after_interrupt() {
        let mut task = Task::new(
            AgentKind::Codex,
            "env".to_string(),
            "topic".to_string(),
            TaskWorkspaceKind::Environment,
            None,
            None,
            false,
            PathBuf::from("/tmp"),
        );

        task.start_run("First prompt".to_string());
        task.interrupt_run();
        assert_eq!(task.status, TaskStatus::Interrupted);
        assert!(task.can_run());

        task.start_run("Second prompt".to_string());
        assert!(task.is_running());
        assert_eq!(task.history.len(), 2);

        task.complete_run(true);
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.history.len(), 2);
        assert_eq!(task.history[0].success, Some(false));
        assert_eq!(task.history[1].success, Some(true));
    }

    #[test]
    fn test_task_double_interrupt() {
        let mut task = Task::new(
            AgentKind::Codex,
            "env".to_string(),
            "topic".to_string(),
            TaskWorkspaceKind::Environment,
            None,
            None,
            false,
            PathBuf::from("/tmp"),
        );

        task.start_run("First prompt".to_string());
        task.interrupt_run();
        assert_eq!(task.status, TaskStatus::Interrupted);

        task.start_run("Second prompt".to_string());
        task.interrupt_run();
        assert_eq!(task.status, TaskStatus::Interrupted);

        task.start_run("Third prompt".to_string());
        task.complete_run(true);
        assert_eq!(task.status, TaskStatus::Completed);

        assert_eq!(task.history.len(), 3);
        assert_eq!(task.history[0].success, Some(false));
        assert_eq!(task.history[1].success, Some(false));
        assert_eq!(task.history[2].success, Some(true));
    }
}
