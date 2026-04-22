//! Coordinator state for connected slopagents.

use crate::persistence::CoordinatorTaskStore;
use chrono::{DateTime, Utc};
use slopcoder_core::{
    agent_rpc::{AgentEnvelope, AgentRequest, AgentResponse},
    task::{Task, TaskId},
    AgentEvent,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio::time::timeout;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("Host must be specified when multiple agents are connected")]
    HostRequired,

    #[error("Host not connected: {0}")]
    HostNotConnected(String),

    #[error("No agents are connected")]
    NoAgentsConnected,

    #[error("Agent disconnected")]
    AgentDisconnected,

    #[error("Agent request timed out")]
    AgentTimeout,

    #[error("Remote error ({status}): {error}")]
    RemoteError { status: u16, error: String },
}

#[derive(Debug, Clone)]
pub struct ConnectedAgent {
    pub id: Uuid,
    pub host: String,
    pub hostname: String,
    pub connected_at: DateTime<Utc>,
    outbound_tx: mpsc::UnboundedSender<AgentEnvelope>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingResponse>>>>,
}

type PendingResponse = Result<AgentResponse, RemoteError>;

#[derive(Debug, Clone)]
pub struct RemoteError {
    pub status: u16,
    pub error: String,
}

#[derive(Debug, Clone)]
pub enum TerminalEvent {
    Data(Vec<u8>),
    Closed,
    Error(String),
}

#[derive(Debug, Clone)]
struct TaskTerminalBinding {
    terminal_id: Uuid,
    host: String,
}

impl ConnectedAgent {
    pub async fn request(&self, request: AgentRequest) -> Result<AgentResponse, StateError> {
        self.request_with_timeout(request, Duration::from_secs(120))
            .await
    }

    pub async fn request_with_timeout(
        &self,
        request: AgentRequest,
        timeout_duration: Duration,
    ) -> Result<AgentResponse, StateError> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<PendingResponse>();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(request_id.clone(), tx);
        }

        if self
            .outbound_tx
            .send(AgentEnvelope::Request {
                request_id: request_id.clone(),
                request,
            })
            .is_err()
        {
            let mut pending = self.pending.lock().await;
            pending.remove(&request_id);
            return Err(StateError::AgentDisconnected);
        }

        let result = match timeout(timeout_duration, rx).await {
            Ok(result) => result,
            Err(_) => {
                let mut pending = self.pending.lock().await;
                pending.remove(&request_id);
                return Err(StateError::AgentTimeout);
            }
        };

        let result = match result {
            Ok(result) => result,
            Err(_) => {
                let mut pending = self.pending.lock().await;
                pending.remove(&request_id);
                return Err(StateError::AgentDisconnected);
            }
        };

        match result {
            Ok(response) => Ok(response),
            Err(err) => Err(StateError::RemoteError {
                status: err.status,
                error: err.error,
            }),
        }
    }

    pub fn send_envelope(&self, envelope: AgentEnvelope) -> Result<(), StateError> {
        self.outbound_tx
            .send(envelope)
            .map_err(|_| StateError::AgentDisconnected)
    }
}

#[derive(Debug, Clone)]
pub struct HostInfo {
    pub host: String,
    pub hostname: String,
    pub connected_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<AppStateInner>>,
    dev_mode: bool,
    jwt_secret: String,
    task_store: Arc<RwLock<CoordinatorTaskStore>>,
}

struct AppStateInner {
    ui_auth_password: Option<String>,
    agent_auth_password: String,
    list_request_timeout_secs: u64,
    agents_by_id: HashMap<Uuid, ConnectedAgent>,
    host_to_id: HashMap<String, Uuid>,
    task_hosts: HashMap<TaskId, String>,
    task_terminals: HashMap<TaskId, TaskTerminalBinding>,
    terminal_tasks: HashMap<Uuid, TaskId>,
    event_channels: HashMap<TaskId, broadcast::Sender<AgentEvent>>,
    terminal_channels: HashMap<Uuid, broadcast::Sender<TerminalEvent>>,
}

impl AppState {
    pub fn new(
        ui_auth_password: Option<String>,
        agent_auth_password: String,
        list_request_timeout_secs: u64,
        dev_mode: bool,
        task_store: CoordinatorTaskStore,
    ) -> Self {
        let jwt_secret = if dev_mode {
            "slopcoder-dev-mode-secret".to_string()
        } else {
            Uuid::new_v4().to_string()
        };
        if dev_mode {
            tracing::warn!("⚠️  DEV MODE ENABLED — authentication is bypassed. Do not use in production.");
        }
        Self {
            inner: Arc::new(RwLock::new(AppStateInner {
                ui_auth_password,
                agent_auth_password,
                list_request_timeout_secs,
                agents_by_id: HashMap::new(),
                host_to_id: HashMap::new(),
                task_hosts: HashMap::new(),
                task_terminals: HashMap::new(),
                terminal_tasks: HashMap::new(),
                event_channels: HashMap::new(),
                terminal_channels: HashMap::new(),
            })),
            dev_mode,
            jwt_secret,
            task_store: Arc::new(RwLock::new(task_store)),
        }
    }

    pub fn dev_mode(&self) -> bool {
        self.dev_mode
    }

    pub fn jwt_secret(&self) -> &str {
        &self.jwt_secret
    }

    pub fn task_store(&self) -> &Arc<RwLock<CoordinatorTaskStore>> {
        &self.task_store
    }

    pub async fn get_ui_auth_password(&self) -> Option<String> {
        self.inner.read().await.ui_auth_password.clone()
    }

    pub async fn get_agent_auth_password(&self) -> String {
        self.inner.read().await.agent_auth_password.clone()
    }

    pub async fn get_list_request_timeout_secs(&self) -> u64 {
        self.inner.read().await.list_request_timeout_secs
    }

    pub async fn register_agent(
        &self,
        hostname: String,
        display_name: Option<String>,
        outbound_tx: mpsc::UnboundedSender<AgentEnvelope>,
        pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingResponse>>>>,
    ) -> ConnectedAgent {
        let mut inner = self.inner.write().await;

        let base = display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&hostname)
            .to_string();
        let host = unique_host_label(&base, &inner.host_to_id);

        let agent = ConnectedAgent {
            id: Uuid::new_v4(),
            host: host.clone(),
            hostname,
            connected_at: Utc::now(),
            outbound_tx,
            pending,
        };

        inner.host_to_id.insert(host.clone(), agent.id);
        inner.agents_by_id.insert(agent.id, agent.clone());
        agent
    }

    pub async fn unregister_agent(&self, agent_id: Uuid) {
        let mut channels_to_close = Vec::new();
        let mut inner = self.inner.write().await;
        let Some(agent) = inner.agents_by_id.remove(&agent_id) else {
            return;
        };

        inner.host_to_id.remove(&agent.host);
        inner.task_hosts.retain(|_, host| host != &agent.host);
        let terminal_tasks: Vec<TaskId> = inner
            .task_terminals
            .iter()
            .filter_map(|(task_id, binding)| {
                if binding.host == agent.host {
                    Some(*task_id)
                } else {
                    None
                }
            })
            .collect();
        for task_id in terminal_tasks {
            if let Some(binding) = inner.task_terminals.remove(&task_id) {
                inner.terminal_tasks.remove(&binding.terminal_id);
                if let Some(tx) = inner.terminal_channels.remove(&binding.terminal_id) {
                    channels_to_close.push(tx);
                }
            }
        }
        tracing::info!("Agent '{}' disconnected", agent.host);
        drop(inner);

        for tx in channels_to_close {
            let _ = tx.send(TerminalEvent::Closed);
        }
    }

    pub async fn list_hosts(&self) -> Vec<HostInfo> {
        let inner = self.inner.read().await;
        let mut hosts: Vec<_> = inner
            .agents_by_id
            .values()
            .map(|agent| HostInfo {
                host: agent.host.clone(),
                hostname: agent.hostname.clone(),
                connected_at: agent.connected_at,
            })
            .collect();
        hosts.sort_by(|a, b| a.host.cmp(&b.host));
        hosts
    }

    pub async fn list_agents(&self) -> Vec<ConnectedAgent> {
        self.inner
            .read()
            .await
            .agents_by_id
            .values()
            .cloned()
            .collect()
    }

    pub async fn get_agent_for_host(&self, host: &str) -> Option<ConnectedAgent> {
        let inner = self.inner.read().await;
        let id = inner.host_to_id.get(host)?;
        inner.agents_by_id.get(id).cloned()
    }

    pub async fn get_host_for_task(&self, task_id: TaskId) -> Option<String> {
        self.inner.read().await.task_hosts.get(&task_id).cloned()
    }

    pub async fn set_task_host(&self, task_id: TaskId, host: String) {
        self.inner.write().await.task_hosts.insert(task_id, host);
    }

    pub async fn clear_task_host(&self, task_id: TaskId) {
        let mut inner = self.inner.write().await;
        inner.task_hosts.remove(&task_id);
        if let Some(binding) = inner.task_terminals.remove(&task_id) {
            inner.terminal_tasks.remove(&binding.terminal_id);
            inner.terminal_channels.remove(&binding.terminal_id);
        }
    }

    pub async fn record_tasks_for_host(&self, host: &str, tasks: &[Task]) {
        let mut inner = self.inner.write().await;
        for task in tasks {
            inner.task_hosts.insert(task.id, host.to_string());
        }
    }

    pub async fn resolve_agent_for_task(&self, task_id: TaskId) -> Option<ConnectedAgent> {
        let host = self.get_host_for_task(task_id).await?;
        self.get_agent_for_host(&host).await
    }

    pub async fn ensure_task_terminal(&self, task_id: TaskId, host: &str) -> (Uuid, bool) {
        let mut inner = self.inner.write().await;
        if let Some(binding) = inner.task_terminals.get(&task_id).cloned() {
            if binding.host == host {
                return (binding.terminal_id, false);
            }
        }

        if let Some(binding) = inner.task_terminals.remove(&task_id) {
            inner.terminal_tasks.remove(&binding.terminal_id);
            inner.terminal_channels.remove(&binding.terminal_id);
        }

        let terminal_id = Uuid::new_v4();
        inner.task_terminals.insert(
            task_id,
            TaskTerminalBinding {
                terminal_id,
                host: host.to_string(),
            },
        );
        inner.terminal_tasks.insert(terminal_id, task_id);
        (terminal_id, true)
    }

    pub async fn get_task_terminal(&self, task_id: TaskId) -> Option<(Uuid, String)> {
        self.inner
            .read()
            .await
            .task_terminals
            .get(&task_id)
            .map(|binding| (binding.terminal_id, binding.host.clone()))
    }

    pub async fn take_task_terminal(&self, task_id: TaskId) -> Option<(Uuid, String)> {
        let mut inner = self.inner.write().await;
        let binding = inner.task_terminals.remove(&task_id)?;
        inner.terminal_tasks.remove(&binding.terminal_id);
        inner.terminal_channels.remove(&binding.terminal_id);
        Some((binding.terminal_id, binding.host))
    }

    pub async fn subscribe_to_task(&self, id: TaskId) -> broadcast::Receiver<AgentEvent> {
        let mut inner = self.inner.write().await;
        let tx = inner
            .event_channels
            .entry(id)
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(200);
                tx
            })
            .clone();
        tx.subscribe()
    }

    pub async fn broadcast_task_event(&self, task_id: TaskId, event: AgentEvent) {
        let mut inner = self.inner.write().await;
        let tx = inner
            .event_channels
            .entry(task_id)
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(200);
                tx
            })
            .clone();
        let _ = tx.send(event);
    }

    pub async fn subscribe_to_terminal(
        &self,
        terminal_id: Uuid,
    ) -> broadcast::Receiver<TerminalEvent> {
        let mut inner = self.inner.write().await;
        let tx = inner
            .terminal_channels
            .entry(terminal_id)
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(200);
                tx
            })
            .clone();
        tx.subscribe()
    }

    pub async fn broadcast_terminal_event(&self, terminal_id: Uuid, event: TerminalEvent) {
        let mut inner = self.inner.write().await;
        let tx = inner
            .terminal_channels
            .entry(terminal_id)
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(200);
                tx
            })
            .clone();
        let _ = tx.send(event.clone());
        if matches!(event, TerminalEvent::Closed | TerminalEvent::Error(_)) {
            inner.terminal_channels.remove(&terminal_id);
            if let Some(task_id) = inner.terminal_tasks.remove(&terminal_id) {
                inner.task_terminals.remove(&task_id);
            }
        }
    }
}

fn unique_host_label(base: &str, existing: &HashMap<String, Uuid>) -> String {
    if !existing.contains_key(base) {
        return base.to_string();
    }
    for idx in 2..1000 {
        let candidate = format!("{}-{}", base, idx);
        if !existing.contains_key(&candidate) {
            return candidate;
        }
    }
    format!("{}-{}", base, Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::{AppState, PendingResponse, TerminalEvent};
    use crate::persistence::CoordinatorTaskStore;
    use slopcoder_core::task::TaskId;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{oneshot, Mutex};

    async fn test_state() -> AppState {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = CoordinatorTaskStore::new(tmp.path().to_path_buf()).await.unwrap();
        // Leak the TempDir so it lives for the test duration
        std::mem::forget(tmp);
        AppState::new(None, "test-password".to_string(), 15, false, store)
    }

    #[tokio::test]
    async fn terminals_are_reused_for_task_and_host() {
        let state = test_state().await;
        let task_id = TaskId::new();

        let (first_id, first_created) = state.ensure_task_terminal(task_id, "boa").await;
        assert!(first_created);
        let (second_id, second_created) = state.ensure_task_terminal(task_id, "boa").await;
        assert!(!second_created);
        assert_eq!(first_id, second_id);
    }

    #[tokio::test]
    async fn terminal_binding_is_cleared_when_terminal_closes() {
        let state = test_state().await;
        let task_id = TaskId::new();

        let (terminal_id, created) = state.ensure_task_terminal(task_id, "boa").await;
        assert!(created);
        assert_eq!(
            state.get_task_terminal(task_id).await,
            Some((terminal_id, "boa".to_string()))
        );

        state
            .broadcast_terminal_event(terminal_id, TerminalEvent::Closed)
            .await;

        assert!(state.get_task_terminal(task_id).await.is_none());
        let (next_id, next_created) = state.ensure_task_terminal(task_id, "boa").await;
        assert!(next_created);
        assert_ne!(terminal_id, next_id);
    }

    #[tokio::test]
    async fn unregister_agent_closes_bound_terminal_sessions() {
        let state = test_state().await;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let agent = state
            .register_agent("boa-host".to_string(), Some("boa".to_string()), tx, pending)
            .await;

        let task_id = TaskId::new();
        state.set_task_host(task_id, agent.host.clone()).await;
        let (terminal_id, created) = state.ensure_task_terminal(task_id, &agent.host).await;
        assert!(created);
        let mut terminal_events = state.subscribe_to_terminal(terminal_id).await;

        state.unregister_agent(agent.id).await;

        let received = tokio::time::timeout(Duration::from_secs(1), terminal_events.recv())
            .await
            .expect("terminal close event timeout")
            .expect("terminal close event missing");
        assert!(matches!(received, TerminalEvent::Closed));
        assert!(state.get_host_for_task(task_id).await.is_none());
        assert!(state.get_task_terminal(task_id).await.is_none());
    }
}
