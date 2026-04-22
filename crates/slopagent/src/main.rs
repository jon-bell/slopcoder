mod state;

use futures::{SinkExt, StreamExt};
use http::StatusCode;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use slopcoder_core::{
    agent_rpc::{
        AgentCreateTaskRequest, AgentEnvelope, AgentRequest, AgentResponse, TaskOutputPageRequest,
    },
    anyagent::{resume_anyagent, spawn_anyagent, AgentKind},
    branch_picker::{
        fallback_topic_name, normalize_task_name, pick_task_topic, topic_to_branch_slug,
    },
    task::{Task, TaskId, TaskWorkspaceKind},
    AgentEvent,
};
use state::{AppState, CreateEnvironmentError, StateError};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{copy, create_dir_all, remove_file, rename, File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, client::IntoClientRequest, Message},
};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use uuid::Uuid;

#[derive(Debug)]
struct RpcError {
    status: u16,
    error: String,
}

impl RpcError {
    fn new(status: StatusCode, error: impl Into<String>) -> Self {
        Self {
            status: status.as_u16(),
            error: error.into(),
        }
    }
}

enum PtyCommand {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Shutdown,
}

#[derive(Clone)]
struct TerminalManager {
    sessions: Arc<Mutex<HashMap<Uuid, std::sync::mpsc::Sender<PtyCommand>>>>,
    out_tx: mpsc::UnboundedSender<AgentEnvelope>,
}

impl TerminalManager {
    fn new(out_tx: mpsc::UnboundedSender<AgentEnvelope>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            out_tx,
        }
    }

    async fn open(&self, state: AppState, terminal_id: Uuid, task_id: TaskId) {
        self.close(terminal_id).await;
        let Some(task) = state.get_task(task_id).await else {
            let _ = self.out_tx.send(AgentEnvelope::TerminalError {
                terminal_id,
                error: format!("Task not found: {}", task_id),
            });
            let _ = self
                .out_tx
                .send(AgentEnvelope::TerminalClosed { terminal_id });
            return;
        };

        let cwd = task.worktree_path.clone();
        if !cwd.is_dir() {
            let _ = self.out_tx.send(AgentEnvelope::TerminalError {
                terminal_id,
                error: format!("Task workspace not found: {}", cwd.display()),
            });
            let _ = self
                .out_tx
                .send(AgentEnvelope::TerminalClosed { terminal_id });
            return;
        }

        let pty_system = native_pty_system();
        let pty_pair = match pty_system.openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(e) => {
                let _ = self.out_tx.send(AgentEnvelope::TerminalError {
                    terminal_id,
                    error: format!("Failed to open PTY: {}", e),
                });
                let _ = self
                    .out_tx
                    .send(AgentEnvelope::TerminalClosed { terminal_id });
                return;
            }
        };

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(cwd);

        let mut child = match pty_pair.slave.spawn_command(cmd) {
            Ok(child) => child,
            Err(e) => {
                let _ = self.out_tx.send(AgentEnvelope::TerminalError {
                    terminal_id,
                    error: format!("Failed to spawn shell: {}", e),
                });
                let _ = self
                    .out_tx
                    .send(AgentEnvelope::TerminalClosed { terminal_id });
                return;
            }
        };
        drop(pty_pair.slave);

        let mut pty_reader = match pty_pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = self.out_tx.send(AgentEnvelope::TerminalError {
                    terminal_id,
                    error: format!("Failed to clone PTY reader: {}", e),
                });
                let _ = self
                    .out_tx
                    .send(AgentEnvelope::TerminalClosed { terminal_id });
                return;
            }
        };
        let mut pty_writer = match pty_pair.master.take_writer() {
            Ok(writer) => writer,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = self.out_tx.send(AgentEnvelope::TerminalError {
                    terminal_id,
                    error: format!("Failed to acquire PTY writer: {}", e),
                });
                let _ = self
                    .out_tx
                    .send(AgentEnvelope::TerminalClosed { terminal_id });
                return;
            }
        };
        let pty_master = pty_pair.master;

        let (pty_command_tx, pty_command_rx) = std::sync::mpsc::channel::<PtyCommand>();
        self.sessions
            .lock()
            .await
            .insert(terminal_id, pty_command_tx.clone());

        let out_tx_for_reader = self.out_tx.clone();
        let sessions_for_reader = self.sessions.clone();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match std::io::Read::read(&mut pty_reader, &mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if out_tx_for_reader
                            .send(AgentEnvelope::TerminalData {
                                terminal_id,
                                data: buffer[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            sessions_for_reader.blocking_lock().remove(&terminal_id);
            let _ = out_tx_for_reader.send(AgentEnvelope::TerminalClosed { terminal_id });
        });

        let sessions_for_writer = self.sessions.clone();
        std::thread::spawn(move || {
            for cmd in pty_command_rx {
                match cmd {
                    PtyCommand::Input(data) => {
                        if std::io::Write::write_all(&mut pty_writer, &data).is_err() {
                            break;
                        }
                        if std::io::Write::flush(&mut pty_writer).is_err() {
                            break;
                        }
                    }
                    PtyCommand::Resize { rows, cols } => {
                        let _ = pty_master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    PtyCommand::Shutdown => break,
                }
            }
            sessions_for_writer.blocking_lock().remove(&terminal_id);
            let _ = child.kill();
            let _ = child.wait();
        });
    }

    async fn input(&self, terminal_id: Uuid, data: Vec<u8>) {
        let tx = { self.sessions.lock().await.get(&terminal_id).cloned() };
        if let Some(tx) = tx {
            if tx.send(PtyCommand::Input(data)).is_err() {
                self.sessions.lock().await.remove(&terminal_id);
            }
        }
    }

    async fn resize(&self, terminal_id: Uuid, rows: u16, cols: u16) {
        let tx = { self.sessions.lock().await.get(&terminal_id).cloned() };
        if let Some(tx) = tx {
            if tx.send(PtyCommand::Resize { rows, cols }).is_err() {
                self.sessions.lock().await.remove(&terminal_id);
            }
        }
    }

    async fn close(&self, terminal_id: Uuid) {
        if let Some(tx) = self.sessions.lock().await.remove(&terminal_id) {
            let _ = tx.send(PtyCommand::Shutdown);
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("slopagent=info".parse().unwrap()))
        .init();

    let mut args = std::env::args().skip(1);
    let mut server_url: Option<String> = None;
    let mut branch_model = "claude-haiku-4-5".to_string();
    let mut host_override: Option<String> = None;
    let mut repo_root: Option<PathBuf> = None;
    let mut discovery_max_depth: usize = 10;
    let mut discovery_max_repos: usize = 100;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" | "--coordinator" => server_url = args.next(),
            "--discover-max-depth" => {
                if let Some(value) = args.next() {
                    match value.parse::<usize>() {
                        Ok(parsed) => discovery_max_depth = parsed,
                        Err(_) => {
                            tracing::error!("Invalid --discover-max-depth value: {}", value);
                            std::process::exit(1);
                        }
                    }
                }
            }
            "--discover-max-repos" => {
                if let Some(value) = args.next() {
                    match value.parse::<usize>() {
                        Ok(parsed) => discovery_max_repos = parsed,
                        Err(_) => {
                            tracing::error!("Invalid --discover-max-repos value: {}", value);
                            std::process::exit(1);
                        }
                    }
                }
            }
            "--branch-model" => {
                if let Some(value) = args.next() {
                    branch_model = value;
                }
            }
            "--password" => {
                tracing::error!(
                    "--password is no longer supported for slopagent; use interactive prompt"
                );
                std::process::exit(1);
            }
            "--name" | "--hostname" => host_override = args.next(),
            "--no-password" => {
                tracing::error!(
                    "--no-password is no longer supported; slopagent password is required"
                );
                std::process::exit(1);
            }
            "-h" | "--help" => {
                println!(
                    "Usage: slopagent REPO_ROOT --server ws://HOST:PORT [options]\n\
Options:\n\
  REPO_ROOT                       Positional root scanned for repositories (required)\n\
  --name HOSTNAME                 Override host label shown in UI\n\
  --branch-model MODEL            Topic naming model (default: claude-haiku-4-5)\n\
  --discover-max-depth N          Max recursive discovery depth (default: 10)\n\
  --discover-max-repos N          Max discovered repos total (default: 100)"
                );
                return;
            }
            _ => {
                if arg.starts_with('-') {
                    tracing::error!("Unknown argument: {}", arg);
                    std::process::exit(1);
                }
                if repo_root.is_some() {
                    tracing::error!("Unexpected extra positional argument: {}", arg);
                    std::process::exit(1);
                }
                repo_root = Some(PathBuf::from(arg));
            }
        }
    }

    let repo_root = match repo_root {
        Some(path) => path,
        None => {
            tracing::error!("Missing required REPO_ROOT positional argument");
            std::process::exit(1);
        }
    };
    if discovery_max_repos == 0 {
        tracing::error!("--discover-max-repos must be greater than 0");
        std::process::exit(1);
    }

    let config = slopcoder_core::environment::EnvironmentConfig::new(
        slopcoder_core::environment::EnvironmentConfig::default_worktrees_directory(),
        None,
        Vec::new(),
    );

    if repo_root.exists() && !repo_root.is_dir() {
        tracing::error!("REPO_ROOT is not a directory: {}", repo_root.display());
        std::process::exit(1);
    }

    if config.environments_root.exists() && !config.environments_root.is_dir() {
        tracing::error!(
            "Environment root is not a directory: {}",
            config.environments_root.display()
        );
        std::process::exit(1);
    }

    let server_url = match server_url {
        Some(url) => normalize_server_url(&url),
        None => {
            tracing::error!("Missing --server argument");
            std::process::exit(1);
        }
    };

    let password = prompt_password();

    if password.is_none() {
        tracing::error!("slopagent password is required");
        std::process::exit(1);
    }

    let state = match AppState::new(
        config,
        Some(repo_root),
        discovery_max_depth,
        discovery_max_repos,
        branch_model,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Startup checks failed: {}", e);
            std::process::exit(1);
        }
    };

    let hostname = default_hostname();
    if let Some(display_name) = host_override.as_deref() {
        tracing::info!(
            "slopagent hostname: {} (display override: {})",
            hostname,
            display_name
        );
    } else {
        tracing::info!("slopagent hostname: {}", hostname);
    }

    loop {
        match run_connection(
            state.clone(),
            &server_url,
            password.clone(),
            hostname.clone(),
            host_override.clone(),
        )
        .await
        {
            Ok(()) => {
                tracing::warn!("Disconnected from coordinator; retrying in 2s");
            }
            Err(e) => {
                tracing::warn!("Connection error: {}; retrying in 2s", e);
            }
        }

        sleep(Duration::from_secs(2)).await;
    }
}

fn prompt_password() -> Option<String> {
    // Check env var first (for docker-compose / CI)
    if let Ok(password) = std::env::var("SLOPCODER_AGENT_PASSWORD") {
        if !password.is_empty() {
            return Some(password);
        }
    }
    print!("Enter slopagent connection password: ");
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return None;
    }
    let trimmed = input.trim_end_matches(&['\r', '\n'][..]).to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn default_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn normalize_server_url(input: &str) -> String {
    let trimmed = input.trim_end_matches('/');
    if trimmed.ends_with("/agent/connect") {
        return trimmed.to_string();
    }
    format!("{}/agent/connect", trimmed)
}

async fn run_connection(
    state: AppState,
    server_url: &str,
    password: Option<String>,
    hostname: String,
    display_name: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut request = server_url.into_client_request()?;
    if let Some(password) = password {
        request.headers_mut().insert(
            "x-slopcoder-password",
            password
                .parse()
                .map_err(|e| format!("invalid password header: {e}"))?,
        );
    }
    let (ws_stream, _) = connect_async(request).await?;
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    ws_sink
        .send(Message::Text(
            serde_json::to_string(&AgentEnvelope::Hello {
                hostname,
                display_name,
            })?
            .into(),
        ))
        .await?;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<AgentEnvelope>();
    let terminal_manager = TerminalManager::new(out_tx.clone());
    let writer = tokio::spawn(async move {
        while let Some(envelope) = out_rx.recv().await {
            let payload = match serde_json::to_string(&envelope) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("Failed to serialize outgoing envelope: {}", e);
                    continue;
                }
            };
            if ws_sink.send(Message::Text(payload.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(message) = ws_stream.next().await {
        let message = message?;
        if message.is_close() {
            break;
        }
        if !message.is_text() {
            continue;
        }

        let text = message.into_text()?;
        let envelope: AgentEnvelope = match serde_json::from_str(&text) {
            Ok(env) => env,
            Err(e) => {
                tracing::warn!("Failed to parse coordinator message: {}", e);
                continue;
            }
        };

        match envelope {
            AgentEnvelope::Request {
                request_id,
                request,
            } => {
                let state = state.clone();
                let out_tx = out_tx.clone();
                tokio::spawn(async move {
                    let response = handle_request(state, request, out_tx.clone()).await;
                    let outgoing = match response {
                        Ok(response) => AgentEnvelope::Response {
                            request_id,
                            response,
                        },
                        Err(err) => AgentEnvelope::Error {
                            request_id,
                            status: err.status,
                            error: err.error,
                        },
                    };
                    let _ = out_tx.send(outgoing);
                });
            }
            AgentEnvelope::TerminalOpen {
                terminal_id,
                task_id,
            } => {
                let manager = terminal_manager.clone();
                let state = state.clone();
                tokio::spawn(async move {
                    manager.open(state, terminal_id, task_id).await;
                });
            }
            AgentEnvelope::TerminalInput { terminal_id, data } => {
                let manager = terminal_manager.clone();
                tokio::spawn(async move {
                    manager.input(terminal_id, data).await;
                });
            }
            AgentEnvelope::TerminalResize {
                terminal_id,
                rows,
                cols,
            } => {
                let manager = terminal_manager.clone();
                tokio::spawn(async move {
                    manager.resize(terminal_id, rows, cols).await;
                });
            }
            AgentEnvelope::TerminalClose { terminal_id } => {
                let manager = terminal_manager.clone();
                tokio::spawn(async move {
                    manager.close(terminal_id).await;
                });
            }
            _ => {
                tracing::warn!("Ignoring unexpected envelope from coordinator");
            }
        }
    }

    writer.abort();
    Ok(())
}

async fn handle_request(
    state: AppState,
    request: AgentRequest,
    out_tx: mpsc::UnboundedSender<AgentEnvelope>,
) -> Result<AgentResponse, RpcError> {
    match request {
        AgentRequest::ListEnvironments => Ok(AgentResponse::Environments {
            environments: state.list_environments().await,
        }),
        AgentRequest::CreateEnvironment { name } => create_environment(state, &name).await,
        AgentRequest::ListBranches { environment } => list_branches(state, &environment).await,
        AgentRequest::ListTasks => Ok(AgentResponse::Tasks {
            tasks: state.list_tasks().await,
        }),
        AgentRequest::GetTask { task_id } => Ok(AgentResponse::Task {
            task: state.get_task(task_id).await,
        }),
        AgentRequest::CreateTask { request } => create_task(state, request, out_tx).await,
        AgentRequest::RenameTask { task_id, name } => rename_task(state, task_id, &name).await,
        AgentRequest::SendPrompt { task_id, prompt } => {
            send_prompt(state, task_id, prompt, out_tx).await
        }
        AgentRequest::GetTaskOutput {
            task_id,
            pagination,
        } => get_task_output(state, task_id, pagination).await,
        AgentRequest::GetTaskDiff { task_id } => get_task_diff(state, task_id).await,
        AgentRequest::InterruptTask { task_id } => interrupt_task(state, task_id).await,
        AgentRequest::MergeTask { task_id } => merge_task(state, task_id).await,
        AgentRequest::GetMergeReadiness { task_id } => get_merge_readiness(state, task_id).await,
        AgentRequest::ArchiveTask { task_id } => archive_task(state, task_id).await,
        AgentRequest::DeleteTask { task_id, force } => delete_task(state, task_id, force).await,
    }
}

async fn create_environment(state: AppState, raw_name: &str) -> Result<AgentResponse, RpcError> {
    match state.create_environment(raw_name).await {
        Ok(environment) => Ok(AgentResponse::Environment { environment }),
        Err(CreateEnvironmentError::NameRequired | CreateEnvironmentError::InvalidName) => {
            Err(RpcError::new(
                StatusCode::BAD_REQUEST,
                "Environment name must be a simple directory name",
            ))
        }
        Err(CreateEnvironmentError::AlreadyExists(path)) => Err(RpcError::new(
            StatusCode::CONFLICT,
            format!("Environment already exists at {}", path.display()),
        )),
        Err(CreateEnvironmentError::CreateDirectory(e)) => Err(RpcError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
        )),
        Err(CreateEnvironmentError::GitInit(e)) => {
            Err(RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e))
        }
    }
}

async fn list_branches(state: AppState, name: &str) -> Result<AgentResponse, RpcError> {
    let Some(env) = state.find_environment(name).await else {
        return Err(RpcError::new(
            StatusCode::NOT_FOUND,
            format!("Environment '{}' not found", name),
        ));
    };
    match env.list_branches().await {
        Ok(branches) => Ok(AgentResponse::Branches { branches }),
        Err(e) => Err(RpcError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
        )),
    }
}

async fn create_task(
    state: AppState,
    req: AgentCreateTaskRequest,
    out_tx: mpsc::UnboundedSender<AgentEnvelope>,
) -> Result<AgentResponse, RpcError> {
    let Some(env) = state.find_environment(&req.environment).await else {
        return Err(RpcError::new(
            StatusCode::NOT_FOUND,
            format!("Environment '{}' not found", req.environment),
        ));
    };

    let task_name = match req.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => normalize_task_name(name).unwrap_or_else(|| "task".to_string()),
        None => {
            let model = state.get_branch_model().await;
            pick_task_topic(&req.prompt, &model)
                .await
                .unwrap_or_else(|_| fallback_topic_name(&req.prompt))
        }
    };

    let (workspace_kind, base_branch, merge_branch, worktree_path) = if req.use_worktree {
        let base_branch = env.current_branch().await.map_err(|e| {
            RpcError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to resolve environment branch: {}", e),
            )
        })?;
        let slug = topic_to_branch_slug(&task_name);
        let suffix: String = Uuid::new_v4().to_string().chars().take(8).collect();
        let merge_branch = format!("task/{}-{}", slug, suffix);
        let worktrees_directory = state.get_worktrees_directory().await;
        let worktree_path = match env
            .create_worktree_from_base(&worktrees_directory, &base_branch, &merge_branch)
            .await
        {
            Ok(path) => path,
            Err(e) => {
                let status = match e {
                    slopcoder_core::environment::EnvironmentError::BranchExists(_)
                    | slopcoder_core::environment::EnvironmentError::WorktreeExists(_) => {
                        StatusCode::CONFLICT
                    }
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                return Err(RpcError::new(
                    status,
                    format!("Failed to create worktree: {}", e),
                ));
            }
        };

        (
            TaskWorkspaceKind::Worktree,
            Some(base_branch),
            Some(merge_branch),
            worktree_path,
        )
    } else {
        (
            TaskWorkspaceKind::Environment,
            None,
            None,
            env.directory.clone(),
        )
    };

    let task = Task::new(
        req.agent.unwrap_or_default(),
        req.environment,
        task_name,
        workspace_kind,
        base_branch,
        merge_branch,
        req.web_search,
        worktree_path.clone(),
    );
    let task_id = task.id;

    state
        .insert_task(task)
        .await
        .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let prompt = req.prompt;
    let state_clone = state.clone();
    tokio::spawn(async move {
        run_agent(state_clone, task_id, prompt, None, out_tx).await;
    });

    Ok(AgentResponse::CreatedTask {
        id: task_id,
        worktree_path: worktree_path.to_string_lossy().to_string(),
    })
}

async fn send_prompt(
    state: AppState,
    task_id: TaskId,
    prompt: String,
    out_tx: mpsc::UnboundedSender<AgentEnvelope>,
) -> Result<AgentResponse, RpcError> {
    let Some(task) = state.get_task(task_id).await else {
        return Err(RpcError::new(StatusCode::NOT_FOUND, "Task not found"));
    };

    if !task.can_run() {
        return Err(RpcError::new(
            StatusCode::CONFLICT,
            "Task is currently running",
        ));
    }

    if !state.validate_task_worktree(task_id).await {
        return Err(RpcError::new(
            StatusCode::GONE,
            "Task workspace no longer exists (may have been removed from CLI)",
        ));
    }

    let session_id = task.session_id;
    let state_clone = state.clone();
    tokio::spawn(async move {
        run_agent(state_clone, task_id, prompt, session_id, out_tx).await;
    });

    Ok(AgentResponse::Ack)
}

async fn rename_task(
    state: AppState,
    task_id: TaskId,
    name: &str,
) -> Result<AgentResponse, RpcError> {
    let task = state
        .rename_task(task_id, name)
        .await
        .map_err(|err| match err {
            StateError::TaskNotFound(_) => RpcError::new(StatusCode::NOT_FOUND, "Task not found"),
            StateError::InvalidTaskName => {
                RpcError::new(StatusCode::BAD_REQUEST, "Task name is required")
            }
            other => RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?;

    Ok(AgentResponse::RenamedTask { task })
}

async fn interrupt_task(state: AppState, task_id: TaskId) -> Result<AgentResponse, RpcError> {
    if state.send_interrupt(task_id).await {
        Ok(AgentResponse::Ack)
    } else {
        Err(RpcError::new(
            StatusCode::CONFLICT,
            "Task is not running or interrupt channel not found",
        ))
    }
}

async fn get_task_output(
    state: AppState,
    task_id: TaskId,
    pagination: TaskOutputPageRequest,
) -> Result<AgentResponse, RpcError> {
    let Some(task) = state.get_task(task_id).await else {
        return Err(RpcError::new(StatusCode::NOT_FOUND, "Task not found"));
    };

    let Some(env_dir) = state.get_environment_directory(&task.environment).await else {
        return Err(RpcError::new(
            StatusCode::NOT_FOUND,
            "Environment not found",
        ));
    };

    let output_path = task_output_path(&env_dir, task_id);
    let page = read_output_events_page(&output_path, pagination.before, pagination.limit)
        .await
        .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(AgentResponse::TaskOutput {
        events: page.events,
        total_events: page.total_events,
        has_more_before: page.has_more_before,
    })
}

async fn get_task_diff(state: AppState, task_id: TaskId) -> Result<AgentResponse, RpcError> {
    let Some(task) = state.get_task(task_id).await else {
        return Err(RpcError::new(StatusCode::NOT_FOUND, "Task not found"));
    };

    let diff = load_git_diff(&task.worktree_path, task.base_branch.as_deref())
        .await
        .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(AgentResponse::TaskDiff {
        staged: diff.staged,
        unstaged: diff.unstaged,
    })
}

async fn merge_task(state: AppState, task_id: TaskId) -> Result<AgentResponse, RpcError> {
    let Some(task) = state.get_task(task_id).await else {
        return Err(RpcError::new(StatusCode::NOT_FOUND, "Task not found"));
    };

    let readiness = evaluate_merge_readiness(&state, &task).await?;
    if !readiness.can_merge {
        return Err(RpcError::new(
            StatusCode::CONFLICT,
            readiness
                .reason
                .unwrap_or_else(|| "Task cannot be merged right now.".to_string()),
        ));
    }

    let Some(env) = state.find_environment(&task.environment).await else {
        return Err(RpcError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Environment not found",
        ));
    };
    let Some(merge_branch) = task.merge_branch.as_deref() else {
        return Err(RpcError::new(
            StatusCode::BAD_REQUEST,
            "Task has no merge branch; only worktree tasks are mergeable",
        ));
    };

    let merge_output = Command::new("git")
        .args(["merge", merge_branch])
        .current_dir(&env.directory)
        .output()
        .await
        .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if merge_output.status.success() {
        let base = task.base_branch.as_deref().unwrap_or("current");
        Ok(AgentResponse::MergeResult {
            status: "merged".to_string(),
            message: format!("Successfully merged {} into {}", merge_branch, base),
        })
    } else {
        let stdout = String::from_utf8_lossy(&merge_output.stdout);
        let _ = Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(&env.directory)
            .output()
            .await;

        Err(RpcError::new(
            StatusCode::CONFLICT,
            format!("Merge failed (reverted): {}", stdout.trim()),
        ))
    }
}

struct MergeReadinessResult {
    can_merge: bool,
    reason: Option<String>,
}

async fn get_merge_readiness(state: AppState, task_id: TaskId) -> Result<AgentResponse, RpcError> {
    let Some(task) = state.get_task(task_id).await else {
        return Err(RpcError::new(StatusCode::NOT_FOUND, "Task not found"));
    };

    let readiness = evaluate_merge_readiness(&state, &task).await?;
    Ok(AgentResponse::MergeReadiness {
        can_merge: readiness.can_merge,
        reason: readiness.reason,
    })
}

async fn evaluate_merge_readiness(
    state: &AppState,
    task: &Task,
) -> Result<MergeReadinessResult, RpcError> {
    if task.workspace_kind != TaskWorkspaceKind::Worktree {
        return Ok(MergeReadinessResult {
            can_merge: false,
            reason: Some("Only isolated worktree tasks can be merged.".to_string()),
        });
    }

    if task.is_running() {
        return Ok(MergeReadinessResult {
            can_merge: false,
            reason: Some("Task is still running.".to_string()),
        });
    }

    if has_unstaged_changes(&task.worktree_path).await {
        return Ok(MergeReadinessResult {
            can_merge: false,
            reason: Some(
                "Task worktree has uncommitted or untracked changes. Commit or stash first."
                    .to_string(),
            ),
        });
    }

    let Some(merge_branch) = task.merge_branch.as_deref() else {
        return Ok(MergeReadinessResult {
            can_merge: false,
            reason: Some("Task has no merge branch.".to_string()),
        });
    };

    let Some(env) = state.find_environment(&task.environment).await else {
        return Ok(MergeReadinessResult {
            can_merge: false,
            reason: Some("Environment not found.".to_string()),
        });
    };

    if has_unstaged_changes(&env.directory).await {
        return Ok(MergeReadinessResult {
            can_merge: false,
            reason: Some(
                "Environment repository has uncommitted or untracked changes.".to_string(),
            ),
        });
    }

    let merge_tree = Command::new("git")
        .args(["merge-tree", "--write-tree", "HEAD", merge_branch])
        .current_dir(&env.directory)
        .output()
        .await
        .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if merge_tree.status.success() {
        return Ok(MergeReadinessResult {
            can_merge: true,
            reason: None,
        });
    }

    let stderr = String::from_utf8_lossy(&merge_tree.stderr)
        .trim()
        .to_string();
    let stdout = String::from_utf8_lossy(&merge_tree.stdout)
        .trim()
        .to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "Merge would conflict.".to_string()
    };

    Ok(MergeReadinessResult {
        can_merge: false,
        reason: Some(format!("Merge precheck failed: {}", detail)),
    })
}

async fn archive_task(state: AppState, task_id: TaskId) -> Result<AgentResponse, RpcError> {
    let Some(task) = state.get_task(task_id).await else {
        return Err(RpcError::new(StatusCode::NOT_FOUND, "Task not found"));
    };

    if task.is_running() {
        return Err(RpcError::new(
            StatusCode::CONFLICT,
            "Stop the running task before archiving.",
        ));
    }

    let archived_path = archive_task_output(&state, &task).await?;
    state
        .remove_task(task_id)
        .await
        .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let message = match archived_path {
        Some(path) => format!("Archived conversation to {}", path.display()),
        None => "Task removed; no conversation file was found to archive.".to_string(),
    };
    Ok(AgentResponse::ArchiveResult {
        status: "archived".to_string(),
        message,
    })
}

async fn delete_task(
    state: AppState,
    task_id: TaskId,
    force: bool,
) -> Result<AgentResponse, RpcError> {
    let Some(task) = state.get_task(task_id).await else {
        return Err(RpcError::new(StatusCode::NOT_FOUND, "Task not found"));
    };

    if task.workspace_kind != TaskWorkspaceKind::Worktree {
        return Err(RpcError::new(
            StatusCode::BAD_REQUEST,
            "Delete is only supported for isolated worktree tasks",
        ));
    }
    if task.is_running() {
        return Err(RpcError::new(
            StatusCode::CONFLICT,
            "Stop the running task before deleting its worktree.",
        ));
    }

    let env = state
        .find_environment(&task.environment)
        .await
        .ok_or_else(|| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, "Environment not found"))?;

    prune_task_worktree(&task, &env.directory, force).await?;

    if let Some(branch) = task.merge_branch.as_deref() {
        let branch_args = if force {
            vec!["branch", "-D", branch]
        } else {
            vec!["branch", "-d", branch]
        };
        let _ = Command::new("git")
            .args(branch_args)
            .current_dir(&env.directory)
            .output()
            .await;
    }

    let archived_path = archive_task_output(&state, &task).await?;
    state
        .remove_task(task_id)
        .await
        .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let message = match archived_path {
        Some(path) => format!(
            "Deleted worktree and archived conversation to {}",
            path.display()
        ),
        None => "Deleted worktree. No conversation file was found to archive.".to_string(),
    };

    Ok(AgentResponse::DeleteResult {
        status: "deleted".to_string(),
        message,
    })
}

async fn prune_task_worktree(task: &Task, repo_dir: &Path, force: bool) -> Result<(), RpcError> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("-f");
    }
    let worktree_path = task.worktree_path.to_string_lossy().to_string();
    args.push(&worktree_path);

    let output = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .await
        .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "unknown git error".to_string()
    };
    let lower = detail.to_ascii_lowercase();
    if !force
        && (lower.contains("untracked") || lower.contains("modified") || lower.contains("--force"))
    {
        return Err(RpcError::new(
            StatusCode::CONFLICT,
            format!(
                "Prune failed: {}. Use force prune to remove it anyway.",
                detail
            ),
        ));
    }

    Err(RpcError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Failed to remove worktree: {}", detail),
    ))
}

async fn archive_task_output(state: &AppState, task: &Task) -> Result<Option<PathBuf>, RpcError> {
    let Some(env_state_dir) = state.get_environment_directory(&task.environment).await else {
        return Err(RpcError::new(
            StatusCode::NOT_FOUND,
            "Environment state directory not found",
        ));
    };

    let source = task_output_path(&env_state_dir, task.id);
    if !source.exists() {
        return Ok(None);
    }

    let worktrees_root = state.get_worktrees_directory().await;
    let archive_dir = worktrees_root
        .join(".slopcoder-state")
        .join("archive")
        .join(sanitize_for_path(&task.environment));
    create_dir_all(&archive_dir)
        .await
        .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let destination = archive_dir.join(format!("task-{}.jsonl", task.id));
    if destination.exists() {
        remove_file(&destination)
            .await
            .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    match rename(&source, &destination).await {
        Ok(_) => Ok(Some(destination)),
        Err(_) => {
            copy(&source, &destination)
                .await
                .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            remove_file(&source)
                .await
                .map_err(|e| RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            Ok(Some(destination))
        }
    }
}

async fn run_agent(
    state: AppState,
    task_id: TaskId,
    prompt: String,
    session_id: Option<Uuid>,
    event_tx: mpsc::UnboundedSender<AgentEnvelope>,
) {
    let task = match state.get_task(task_id).await {
        Some(t) => t,
        None => {
            tracing::error!("Task {} not found", task_id);
            return;
        }
    };

    let mut output_file = match state.get_environment_directory(&task.environment).await {
        Some(env_dir) => {
            let output_path = task_output_path(&env_dir, task_id);
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&output_path)
                .await
            {
                Ok(file) => Some(file),
                Err(e) => {
                    tracing::warn!("Failed to open output log {}: {}", output_path.display(), e);
                    None
                }
            }
        }
        None => None,
    };

    if let Err(e) = state.start_task_run(task_id, prompt.clone()).await {
        tracing::error!("Failed to start task run for {}: {}", task_id, e);
        return;
    }

    let mut interrupt_rx = state.register_interrupt_channel(task_id).await;
    let agent_config = state.get_agent_config().await;
    if task.web_search && task.agent != AgentKind::Codex {
        tracing::warn!(
            "Task {} requested web search, but '{}' does not currently support it in slopcoder",
            task_id,
            format!("{:?}", task.agent).to_lowercase()
        );
    }

    let prompt_event = AgentEvent::PromptSent {
        prompt: prompt.clone(),
    };
    if let Some(file) = output_file.as_mut() {
        if let Ok(line) = serde_json::to_string(&prompt_event) {
            if file.write_all(line.as_bytes()).await.is_err()
                || file.write_all(b"\n").await.is_err()
            {
                output_file = None;
            }
        }
    }
    let _ = event_tx.send(AgentEnvelope::TaskEvent {
        task_id,
        event: prompt_event,
    });

    let agent_result = if let Some(sid) = session_id {
        resume_anyagent(
            task.agent,
            &agent_config,
            &task.worktree_path,
            sid,
            &prompt,
            task.web_search,
        )
        .await
    } else {
        spawn_anyagent(
            task.agent,
            &agent_config,
            &task.worktree_path,
            &prompt,
            task.web_search,
        )
        .await
    };

    let mut agent = match agent_result {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("Failed to spawn agent: {}", e);
            let _ = state.complete_task_run(task_id, false).await;
            return;
        }
    };

    let mut interrupted = false;
    loop {
        tokio::select! {
            result = agent.next_event() => {
                match result {
                    Some(Ok(event)) => {
                        if let Some(sid) = event.session_id() {
                            if let Err(e) = state.set_task_session_id(task_id, sid).await {
                                tracing::warn!("Failed to save session ID: {}", e);
                            }
                        }
                        if let Some(file) = output_file.as_mut() {
                            match serde_json::to_string(&event) {
                                Ok(line) => {
                                    if file.write_all(line.as_bytes()).await.is_err()
                                        || file.write_all(b"\n").await.is_err()
                                    {
                                        output_file = None;
                                    }
                                }
                                Err(e) => tracing::warn!("Failed to serialize event for {}: {}", task_id, e),
                            }
                        }
                        let _ = event_tx.send(AgentEnvelope::TaskEvent { task_id, event });
                    }
                    Some(Err(e)) => tracing::warn!("Error reading event: {}", e),
                    None => break,
                }
            }
            _ = &mut interrupt_rx => {
                interrupted = true;
                if let Err(e) = agent.kill().await {
                    tracing::warn!("Failed to kill agent for task {}: {}", task_id, e);
                }
                break;
            }
        }
    }

    if interrupted {
        if let Err(e) = state.interrupt_task_run(task_id).await {
            tracing::warn!("Failed to persist interrupt for {}: {}", task_id, e);
        }
    } else {
        let result = agent.wait().await;
        let success = match &result {
            Ok(r) => {
                if let Err(e) = state.set_task_session_id(task_id, r.session_id).await {
                    tracing::warn!("Failed to save session ID: {}", e);
                }
                r.success
            }
            Err(_) => false,
        };

        if let Err(e) = state.complete_task_run(task_id, success).await {
            tracing::warn!("Failed to persist completion for {}: {}", task_id, e);
        }
    }
}

fn task_output_path(env_dir: &Path, task_id: TaskId) -> PathBuf {
    env_dir.join(format!("task-{}.jsonl", task_id))
}

struct OutputEventsPage {
    events: Vec<AgentEvent>,
    total_events: usize,
    has_more_before: bool,
}

async fn read_output_events_page(
    path: &Path,
    before: usize,
    limit: usize,
) -> Result<OutputEventsPage, std::io::Error> {
    if !path.exists() {
        return Ok(OutputEventsPage {
            events: Vec::new(),
            total_events: 0,
            has_more_before: false,
        });
    }

    let limit = limit.max(1);
    let window_size = before.saturating_add(limit);
    let file = File::open(path).await?;
    let mut lines = BufReader::new(file).lines();
    let mut window = std::collections::VecDeque::with_capacity(window_size);
    let mut total_events = 0usize;

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<AgentEvent>(trimmed) {
            total_events += 1;
            if window.len() == window_size {
                window.pop_front();
            }
            window.push_back(event.normalize());
        }
    }

    let available_before = total_events.saturating_sub(before);
    let take_count = available_before.min(limit);
    let drop_count = window
        .len()
        .saturating_sub(before.saturating_add(take_count));
    let events = window
        .into_iter()
        .skip(drop_count)
        .take(take_count)
        .collect::<Vec<_>>();

    Ok(OutputEventsPage {
        events,
        total_events,
        has_more_before: total_events > before.saturating_add(take_count),
    })
}

#[cfg(test)]
mod tests {
    use super::read_output_events_page;
    use slopcoder_core::AgentEvent;
    use tempfile::NamedTempFile;
    use tokio::fs;

    async fn write_events(lines: &[&str]) -> NamedTempFile {
        let file = NamedTempFile::new().expect("temp file");
        let body = format!("{}\n", lines.join("\n"));
        fs::write(file.path(), body).await.expect("write events");
        file
    }

    #[tokio::test]
    async fn read_output_events_page_returns_latest_window() {
        let file = write_events(&[
            r#"{"type":"prompt.sent","prompt":"one"}"#,
            r#"{"type":"prompt.sent","prompt":"two"}"#,
            r#"{"type":"prompt.sent","prompt":"three"}"#,
            r#"{"type":"prompt.sent","prompt":"four"}"#,
        ])
        .await;

        let page = read_output_events_page(file.path(), 0, 2)
            .await
            .expect("page");

        assert_eq!(page.total_events, 4);
        assert!(page.has_more_before);
        let prompts = page
            .events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::PromptSent { prompt } => Some(prompt.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(prompts, vec!["three", "four"]);
    }

    #[tokio::test]
    async fn read_output_events_page_returns_older_window() {
        let file = write_events(&[
            r#"{"type":"prompt.sent","prompt":"one"}"#,
            r#"{"type":"prompt.sent","prompt":"two"}"#,
            r#"{"type":"prompt.sent","prompt":"three"}"#,
            r#"{"type":"prompt.sent","prompt":"four"}"#,
            r#"{"type":"prompt.sent","prompt":"five"}"#,
        ])
        .await;

        let page = read_output_events_page(file.path(), 2, 2)
            .await
            .expect("page");

        assert_eq!(page.total_events, 5);
        assert!(page.has_more_before);
        let prompts = page
            .events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::PromptSent { prompt } => Some(prompt.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(prompts, vec!["two", "three"]);
    }
}

struct DiffResult {
    staged: String,
    unstaged: String,
}

async fn load_git_diff(
    worktree_path: &Path,
    base_branch: Option<&str>,
) -> Result<DiffResult, std::io::Error> {
    let mut staged_cmd = Command::new("git");
    staged_cmd.args(["diff", "--cached"]);
    if let Some(base_branch) = base_branch {
        staged_cmd.arg(base_branch);
    }
    let staged_output = staged_cmd.current_dir(worktree_path).output().await?;
    if !staged_output.status.success() {
        let stderr = String::from_utf8_lossy(&staged_output.stderr);
        if !stderr.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                stderr.to_string(),
            ));
        }
    }
    let staged = String::from_utf8_lossy(&staged_output.stdout).to_string();

    let unstaged_output = Command::new("git")
        .args(["diff"])
        .current_dir(worktree_path)
        .output()
        .await?;
    if !unstaged_output.status.success() {
        let stderr = String::from_utf8_lossy(&unstaged_output.stderr);
        if !stderr.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                stderr.to_string(),
            ));
        }
    }
    let mut unstaged = String::from_utf8_lossy(&unstaged_output.stdout).to_string();

    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(worktree_path)
        .output()
        .await?;
    if !untracked.status.success() {
        let stderr = String::from_utf8_lossy(&untracked.stderr);
        if !stderr.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                stderr.to_string(),
            ));
        }
    }

    for path in String::from_utf8_lossy(&untracked.stdout).lines() {
        if path.trim().is_empty() {
            continue;
        }
        let untracked_diff = Command::new("git")
            .args(["diff", "--no-index", "--", "/dev/null", path])
            .current_dir(worktree_path)
            .output()
            .await?;
        let status = untracked_diff.status;
        if !status.success() && status.code() != Some(1) {
            let stderr = String::from_utf8_lossy(&untracked_diff.stderr);
            if !stderr.trim().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    stderr.to_string(),
                ));
            }
        }

        let chunk = String::from_utf8_lossy(&untracked_diff.stdout);
        if !chunk.trim().is_empty() {
            if !unstaged.is_empty() && !unstaged.ends_with('\n') {
                unstaged.push('\n');
            }
            unstaged.push_str(&chunk);
        }
    }

    Ok(DiffResult { staged, unstaged })
}

async fn has_unstaged_changes(worktree_path: &Path) -> bool {
    let unstaged_output = Command::new("git")
        .args(["diff", "--quiet"])
        .current_dir(worktree_path)
        .output()
        .await;
    if let Ok(output) = unstaged_output {
        if output.status.code() == Some(1) {
            return true;
        }
    }

    let staged_output = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(worktree_path)
        .output()
        .await;
    if let Ok(output) = staged_output {
        if output.status.code() == Some(1) {
            return true;
        }
    }

    let untracked_output = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(worktree_path)
        .output()
        .await;
    if let Ok(output) = untracked_output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            return true;
        }
    }

    false
}

fn sanitize_for_path(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '-' || lower == '_' {
            out.push(lower);
        } else {
            out.push('-');
        }
    }

    let compact = out
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if compact.is_empty() {
        "env".to_string()
    } else {
        compact
    }
}

#[allow(dead_code)]
fn map_state_error(err: StateError) -> RpcError {
    match err {
        StateError::TaskNotFound(_) => RpcError::new(StatusCode::NOT_FOUND, "Task not found"),
        StateError::WorktreeMissing(_) => RpcError::new(StatusCode::GONE, "Task worktree missing"),
        StateError::InvalidTaskName => {
            RpcError::new(StatusCode::BAD_REQUEST, "Task name is required")
        }
        StateError::TaskNotReady => RpcError::new(StatusCode::CONFLICT, "Task not ready"),
        StateError::PersistenceError(e) => {
            RpcError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
    }
}

#[allow(dead_code)]
fn map_ws_error(err: tungstenite::Error) -> String {
    err.to_string()
}
