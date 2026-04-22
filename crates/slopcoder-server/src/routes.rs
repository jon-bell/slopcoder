//! HTTP routes for the Slopcoder coordinator API.

use crate::state::{AppState, ConnectedAgent, RemoteError, StateError, TerminalEvent};
use futures::future::join_all;
use futures::{SinkExt, StreamExt};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use slopcoder_core::{
    agent_rpc::{
        AgentCreateTaskRequest, AgentEnvelope, AgentRequest, AgentResponse, TaskOutputPageRequest,
    },
    task::{Task, TaskId},
    AgentEvent,
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use uuid::Uuid;
use warp::http::{Method, StatusCode};
use warp::reject::InvalidQuery;
use warp::ws::{Message, WebSocket};
use warp::{Filter, Reply};

#[derive(Debug)]
struct AuthError;
impl warp::reject::Reject for AuthError {}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JwtClaims {
    pub sub: String,
    pub github_id: u64,
    pub orgs: Vec<String>,
    pub teams: Vec<String>,
    pub avatar_url: String,
    pub exp: usize,
}

impl JwtClaims {
    pub fn new_dev(username: &str) -> Self {
        Self {
            sub: username.to_string(),
            github_id: 0,
            orgs: vec!["dev".to_string()],
            teams: vec!["dev".to_string()],
            avatar_url: String::new(),
            exp: (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp() as usize,
        }
    }

    pub fn encode(&self, secret: &str) -> Result<String, jsonwebtoken::errors::Error> {
        encode(
            &Header::default(),
            self,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
    }

    pub fn decode(token: &str, secret: &str) -> Result<Self, jsonwebtoken::errors::Error> {
        decode::<Self>(
            token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &Validation::default(),
        )
        .map(|data| data.claims)
    }
}

fn extract_jwt_from_cookie(cookie_header: &str) -> Option<String> {
    for cookie in cookie_header.split(';') {
        let cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix("slopcoder_session=") {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Create all API routes.
pub fn routes(
    state: AppState,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone {
    let hosts = warp::path("hosts").and(hosts_routes(state.clone()));
    let environments = warp::path("environments").and(environments_routes(state.clone()));
    let tasks = warp::path("tasks").and(tasks_routes(state.clone()));
    let secrets = warp::path("secrets").and(secrets_routes(state.clone()));

    let api_scoped = auth_filter_api(state.clone())
        .and(hosts.or(environments).or(tasks).or(secrets))
        .recover(handle_rejection);
    let api_routes = warp::path("api").and(api_scoped);

    let auth_routes = warp::path("auth").and(auth_routes(state.clone()));

    let agent_connect = warp::path!("agent" / "connect")
        .and(auth_filter_agent(state.clone()))
        .and(warp::ws())
        .and(with_state(state))
        .map(|ws: warp::ws::Ws, state: AppState| {
            ws.on_upgrade(move |socket| handle_agent_socket(socket, state))
        });

    auth_routes.or(api_routes).or(agent_connect)
}

// ============================================================================
// Auth routes
// ============================================================================

fn auth_routes(
    state: AppState,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone {
    let dev_login = warp::path("dev-login")
        .and(warp::get())
        .and(warp::query::<DevLoginQuery>())
        .and(with_state(state.clone()))
        .and_then(handle_dev_login);

    let login = warp::path("login")
        .and(warp::get())
        .and(with_state(state.clone()))
        .and_then(handle_github_login);

    let callback = warp::path("callback")
        .and(warp::get())
        .and(warp::query::<OAuthCallbackQuery>())
        .and(with_state(state.clone()))
        .and_then(handle_github_callback);

    let me = warp::path("me")
        .and(warp::get())
        .and(warp::header::optional::<String>("cookie"))
        .and(with_state(state.clone()))
        .and_then(handle_auth_me);

    let logout = warp::path("logout")
        .and(warp::post().or(warp::get()).unify())
        .and_then(handle_logout);

    dev_login.or(login).or(callback).or(me).or(logout)
}

#[derive(Deserialize)]
struct DevLoginQuery {
    user: String,
}

#[derive(Serialize)]
struct AuthMeResponse {
    github_user: String,
    github_id: u64,
    orgs: Vec<String>,
    teams: Vec<String>,
    avatar_url: String,
}

async fn handle_dev_login(
    query: DevLoginQuery,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    if !state.dev_mode() {
        return Ok(warp::reply::with_status(
            warp::reply::with_header(
                warp::reply::json(&ErrorResponse {
                    error: "Dev mode is not enabled".to_string(),
                }),
                "content-type",
                "application/json",
            ),
            StatusCode::FORBIDDEN,
        )
        .into_response());
    }
    let claims = JwtClaims::new_dev(&query.user);
    match claims.encode(state.jwt_secret()) {
        Ok(token) => {
            let cookie = format!(
                "slopcoder_session={}; HttpOnly; Path=/; SameSite=Lax; Max-Age=86400",
                token
            );
            Ok(warp::reply::with_header(
                warp::reply::with_status(warp::reply::json(&AuthMeResponse {
                    github_user: claims.sub,
                    github_id: claims.github_id,
                    orgs: claims.orgs,
                    teams: claims.teams,
                    avatar_url: claims.avatar_url,
                }), StatusCode::OK),
                "set-cookie",
                cookie,
            )
            .into_response())
        }
        Err(_) => Ok(warp::reply::with_status(
            warp::reply::with_header(
                warp::reply::json(&ErrorResponse {
                    error: "Failed to create session".to_string(),
                }),
                "content-type",
                "application/json",
            ),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .into_response()),
    }
}

#[derive(Deserialize)]
struct OAuthCallbackQuery {
    code: String,
}

async fn handle_github_login(state: AppState) -> Result<impl Reply, Infallible> {
    let Some(client_id) = state.github_client_id() else {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse { error: "GitHub OAuth not configured".to_string() }),
            StatusCode::SERVICE_UNAVAILABLE,
        ).into_response());
    };
    let url = format!(
        "https://github.com/login/oauth/authorize?client_id={}&scope=read:org%20read:user",
        client_id
    );
    Ok(warp::reply::with_header(
        warp::reply::with_status(warp::reply::json(&serde_json::json!({})), StatusCode::FOUND),
        "location",
        url,
    ).into_response())
}

async fn handle_github_callback(
    query: OAuthCallbackQuery,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let (Some(client_id), Some(client_secret)) = (state.github_client_id(), state.github_client_secret()) else {
        return Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse { error: "GitHub OAuth not configured".to_string() }),
            StatusCode::SERVICE_UNAVAILABLE,
        ).into_response());
    };

    let http = reqwest::Client::new();

    // Exchange code for access token
    let token_resp = http
        .post("https://github.com/login/oauth/access_token")
        .header("accept", "application/json")
        .json(&serde_json::json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "code": query.code,
        }))
        .send()
        .await;

    let access_token = match token_resp {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(body) => match body.get("access_token").and_then(|v| v.as_str()) {
                Some(token) => token.to_string(),
                None => {
                    let err = body.get("error_description").and_then(|v| v.as_str()).unwrap_or("unknown error");
                    return Ok(warp::reply::with_status(
                        warp::reply::json(&ErrorResponse { error: format!("GitHub OAuth failed: {}", err) }),
                        StatusCode::BAD_REQUEST,
                    ).into_response());
                }
            },
            Err(e) => return Ok(warp::reply::with_status(
                warp::reply::json(&ErrorResponse { error: format!("Failed to parse token response: {}", e) }),
                StatusCode::BAD_GATEWAY,
            ).into_response()),
        },
        Err(e) => return Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse { error: format!("Failed to exchange code: {}", e) }),
            StatusCode::BAD_GATEWAY,
        ).into_response()),
    };

    // Fetch user profile
    let user_resp = http
        .get("https://api.github.com/user")
        .header("authorization", format!("Bearer {}", access_token))
        .header("user-agent", "slopcoder-server")
        .send()
        .await;

    let (username, github_id, avatar_url) = match user_resp {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(body) => (
                body.get("login").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
                body.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
                body.get("avatar_url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            ),
            Err(_) => return Ok(warp::reply::with_status(
                warp::reply::json(&ErrorResponse { error: "Failed to parse user profile".to_string() }),
                StatusCode::BAD_GATEWAY,
            ).into_response()),
        },
        Err(e) => return Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse { error: format!("Failed to fetch user: {}", e) }),
            StatusCode::BAD_GATEWAY,
        ).into_response()),
    };

    // Fetch org memberships
    let orgs = match http
        .get("https://api.github.com/user/orgs")
        .header("authorization", format!("Bearer {}", access_token))
        .header("user-agent", "slopcoder-server")
        .send()
        .await
    {
        Ok(resp) => resp.json::<Vec<serde_json::Value>>().await
            .unwrap_or_default()
            .iter()
            .filter_map(|o| o.get("login").and_then(|v| v.as_str()).map(String::from))
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };

    // Check authorization
    if let Err(reason) = state.check_authorization(&orgs, &[]) {
        tracing::warn!("Authorization denied for user '{}': {}", username, reason);
        return Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse { error: reason }),
            StatusCode::FORBIDDEN,
        ).into_response());
    }

    let claims = JwtClaims {
        sub: username,
        github_id,
        orgs,
        teams: Vec::new(),
        avatar_url,
        exp: (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp() as usize,
    };

    match claims.encode(state.jwt_secret()) {
        Ok(token) => {
            let cookie = format!(
                "slopcoder_session={}; HttpOnly; Path=/; SameSite=Lax; Max-Age=86400",
                token
            );
            // Redirect to app root after successful login
            let mut resp = warp::reply::with_header(
                warp::reply::with_status(warp::reply::json(&serde_json::json!({})), StatusCode::FOUND),
                "set-cookie",
                cookie,
            ).into_response();
            resp.headers_mut().insert("location", "/".parse().unwrap());
            Ok(resp)
        }
        Err(_) => Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse { error: "Failed to create session".to_string() }),
            StatusCode::INTERNAL_SERVER_ERROR,
        ).into_response()),
    }
}

async fn handle_auth_me(
    cookie_header: Option<String>,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let claims = cookie_header
        .as_deref()
        .and_then(extract_jwt_from_cookie)
        .and_then(|token| JwtClaims::decode(&token, state.jwt_secret()).ok());

    match claims {
        Some(claims) => Ok(warp::reply::with_status(
            warp::reply::json(&AuthMeResponse {
                github_user: claims.sub,
                github_id: claims.github_id,
                orgs: claims.orgs,
                teams: claims.teams,
                avatar_url: claims.avatar_url,
            }),
            StatusCode::OK,
        )
        .into_response()),
        None => Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse {
                error: "Not authenticated".to_string(),
            }),
            StatusCode::UNAUTHORIZED,
        )
        .into_response()),
    }
}

async fn handle_logout() -> Result<impl Reply, Infallible> {
    let cookie = "slopcoder_session=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0";
    Ok(warp::reply::with_header(
        warp::reply::with_status(warp::reply::json(&serde_json::json!({"ok": true})), StatusCode::OK),
        "set-cookie",
        cookie,
    ))
}

// ============================================================================
// Hosts
// ============================================================================

fn hosts_routes(
    state: AppState,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone {
    warp::path::end()
        .and(warp::get())
        .and(with_state(state))
        .and_then(list_hosts)
}

#[derive(Serialize)]
struct HostResponse {
    host: String,
    hostname: String,
    connected_at: String,
}

async fn list_hosts(state: AppState) -> Result<impl Reply, Infallible> {
    let hosts = state.list_hosts().await;
    let response: Vec<HostResponse> = hosts
        .into_iter()
        .map(|h| HostResponse {
            host: h.host,
            hostname: h.hostname,
            connected_at: h.connected_at.to_rfc3339(),
        })
        .collect();
    Ok(warp::reply::json(&response))
}

// ============================================================================
// Environment routes
// ============================================================================

fn environments_routes(
    state: AppState,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone {
    let list = warp::path::end()
        .and(warp::get())
        .and(with_state(state.clone()))
        .and_then(list_environments);

    let create = warp::path::end()
        .and(warp::post())
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(create_environment);

    let branches = warp::path!(String / "branches")
        .and(warp::get())
        .and(warp::query::<HostQuery>())
        .and(with_state(state))
        .and_then(list_branches);

    list.or(create).or(branches)
}

#[derive(Serialize)]
struct EnvironmentResponse {
    host: String,
    name: String,
    directory: String,
}

async fn list_environments(state: AppState) -> Result<impl Reply, Infallible> {
    let agents = state.list_agents().await;
    let list_request_timeout_secs = state.get_list_request_timeout_secs().await;
    let mut environments = Vec::new();

    let responses = join_all(agents.into_iter().map(|agent| async move {
        let host = agent.host.clone();
        let response = request_with_timeout(
            &agent,
            AgentRequest::ListEnvironments,
            list_request_timeout_secs,
        )
        .await;
        (host, response)
    }))
    .await;

    for (host, response) in responses {
        match response {
            Ok(AgentResponse::Environments { environments: envs }) => {
                for env in envs {
                    environments.push(EnvironmentResponse {
                        host: host.clone(),
                        name: env.name,
                        directory: env.directory.to_string_lossy().to_string(),
                    });
                }
            }
            Ok(_) => {
                tracing::warn!("Unexpected response for list environments from {}", host);
            }
            Err(e) => {
                tracing::warn!("Failed to list environments from host '{}': {}", host, e);
            }
        }
    }

    environments.sort_by(|a, b| {
        (a.host.as_str(), a.name.as_str()).cmp(&(b.host.as_str(), b.name.as_str()))
    });
    Ok(warp::reply::json(&environments))
}

#[derive(Deserialize)]
struct CreateEnvironmentRequest {
    host: String,
    name: String,
}

async fn create_environment(
    req: CreateEnvironmentRequest,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let host = req.host.trim();
    if host.is_empty() {
        return Ok(error_reply(StatusCode::BAD_REQUEST, "Host is required"));
    }

    let agent = match pick_agent(state.clone(), Some(host)).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent
        .request(AgentRequest::CreateEnvironment {
            name: req.name.clone(),
        })
        .await
    {
        Ok(AgentResponse::Environment { environment }) => Ok(warp::reply::with_status(
            warp::reply::json(&EnvironmentResponse {
                host: agent.host,
                name: environment.name,
                directory: environment.directory.to_string_lossy().to_string(),
            }),
            StatusCode::CREATED,
        )),
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

#[derive(Deserialize)]
struct HostQuery {
    host: Option<String>,
}

#[derive(Serialize)]
struct BranchesResponse {
    branches: Vec<String>,
}

async fn list_branches(
    name: String,
    query: HostQuery,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let decoded_name = match urlencoding::decode(&name) {
        Ok(decoded) => decoded.into_owned(),
        Err(_) => {
            return Ok(error_reply(
                StatusCode::BAD_REQUEST,
                "Environment name must be valid URL encoding",
            ));
        }
    };

    let agent = match pick_agent(state.clone(), query.host.as_deref()).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent
        .request(AgentRequest::ListBranches {
            environment: decoded_name,
        })
        .await
    {
        Ok(AgentResponse::Branches { branches }) => Ok(warp::reply::with_status(
            warp::reply::json(&BranchesResponse { branches }),
            StatusCode::OK,
        )),
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

// ============================================================================
// Task routes
// ============================================================================

fn tasks_routes(
    state: AppState,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone {
    let list = warp::path::end()
        .and(warp::get())
        .and(with_state(state.clone()))
        .and_then(list_tasks);

    let create = warp::path::end()
        .and(warp::post())
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(create_task);

    let get = warp::path!(String)
        .and(warp::get())
        .and(with_state(state.clone()))
        .and_then(get_task);

    let rename = warp::path!(String)
        .and(warp::patch())
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(rename_task);

    let prompt = warp::path!(String / "prompt")
        .and(warp::post())
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(send_prompt);

    let output = warp::path!(String / "output")
        .and(warp::get())
        .and(warp::query::<TaskOutputQuery>())
        .and(with_state(state.clone()))
        .and_then(get_task_output);

    let diff = warp::path!(String / "diff")
        .and(warp::get())
        .and(with_state(state.clone()))
        .and_then(get_task_diff);

    let interrupt = warp::path!(String / "interrupt")
        .and(warp::post())
        .and(with_state(state.clone()))
        .and_then(interrupt_task);

    let stream = warp::path!(String / "stream")
        .and(warp::ws())
        .and(with_state(state.clone()))
        .map(|id: String, ws: warp::ws::Ws, state: AppState| {
            ws.on_upgrade(move |socket| handle_task_websocket(socket, id, state))
        });

    let terminal = warp::path!(String / "terminal")
        .and(warp::ws())
        .and(with_state(state.clone()))
        .map(|id: String, ws: warp::ws::Ws, state: AppState| {
            ws.on_upgrade(move |socket| handle_terminal_websocket(socket, id, state))
        });

    let merge = warp::path!(String / "merge")
        .and(warp::post())
        .and(with_state(state.clone()))
        .and_then(merge_task);

    let merge_status = warp::path!(String / "merge-status")
        .and(warp::get())
        .and(with_state(state.clone()))
        .and_then(get_merge_status);

    let archive = warp::path!(String / "archive")
        .and(warp::post())
        .and(with_state(state.clone()))
        .and_then(archive_task);

    let delete = warp::path!(String)
        .and(warp::delete())
        .and(warp::query::<DeleteTaskQuery>())
        .and(with_state(state.clone()))
        .and_then(delete_task);

    let list_collaborators = warp::path!(String / "collaborators")
        .and(warp::get())
        .and(with_state(state.clone()))
        .and_then(get_collaborators);

    let add_collaborator = warp::path!(String / "collaborators")
        .and(warp::post())
        .and(warp::body::json())
        .and(with_state(state.clone()))
        .and_then(add_collaborator);

    let remove_collaborator = warp::path!(String / "collaborators" / String)
        .and(warp::delete())
        .and(with_state(state))
        .and_then(remove_collaborator);

    list.or(create)
        .or(rename)
        .or(get)
        .or(prompt)
        .or(output)
        .or(diff)
        .or(interrupt)
        .or(stream)
        .or(terminal)
        .or(merge)
        .or(merge_status)
        .or(archive)
        .or(delete)
        .or(list_collaborators)
        .or(add_collaborator)
        .or(remove_collaborator)
}

#[derive(Serialize)]
struct TaskResponse {
    id: String,
    host: String,
    agent: String,
    environment: String,
    name: String,
    workspace_kind: String,
    base_branch: Option<String>,
    merge_branch: Option<String>,
    status: String,
    session_id: Option<String>,
    created_at: String,
    worktree_date: Option<String>,
    history: Vec<PromptRunResponse>,
    // SlopCoderNG fields
    owner: String,
    workspace_slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pod_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_url: Option<String>,
    http_port: u16,
    collaborators: Vec<String>,
}

#[derive(Serialize)]
struct PromptRunResponse {
    prompt: String,
    started_at: String,
    finished_at: Option<String>,
    success: Option<bool>,
}

impl TaskResponse {
    fn from_task(host: &str, task: &Task) -> Self {
        Self {
            id: task.id.to_string(),
            host: host.to_string(),
            agent: format!("{:?}", task.agent).to_lowercase(),
            environment: task.environment.clone(),
            name: task.name.clone(),
            workspace_kind: format!("{:?}", task.workspace_kind).to_lowercase(),
            base_branch: task.base_branch.clone(),
            merge_branch: task.merge_branch.clone(),
            status: format!("{:?}", task.status).to_lowercase(),
            session_id: task.session_id.map(|id| id.to_string()),
            created_at: task.created_at.to_rfc3339(),
            worktree_date: None,
            history: task
                .history
                .iter()
                .map(|r| PromptRunResponse {
                    prompt: r.prompt.clone(),
                    started_at: r.started_at.to_rfc3339(),
                    finished_at: r.finished_at.map(|t| t.to_rfc3339()),
                    success: r.success,
                })
                .collect(),
            owner: task.owner.clone(),
            workspace_slug: task.workspace_slug.clone(),
            pod_name: task.pod_name.clone(),
            ssh_port: task.ssh_port,
            ssh_command: task.ssh_command.clone(),
            workspace_url: task.workspace_url.clone(),
            app_url: task.app_url.clone(),
            http_port: task.http_port,
            collaborators: task.collaborators.clone(),
        }
    }
}

async fn list_tasks(state: AppState) -> Result<impl Reply, Infallible> {
    let agents = state.list_agents().await;
    let list_request_timeout_secs = state.get_list_request_timeout_secs().await;
    let mut tasks = Vec::new();

    let responses = join_all(agents.into_iter().map(|agent| async move {
        let host = agent.host.clone();
        let response =
            request_with_timeout(&agent, AgentRequest::ListTasks, list_request_timeout_secs).await;
        (host, response)
    }))
    .await;

    for (host, response) in responses {
        match response {
            Ok(AgentResponse::Tasks { tasks: host_tasks }) => {
                state.record_tasks_for_host(&host, &host_tasks).await;
                tasks.extend(
                    host_tasks
                        .iter()
                        .map(|task| TaskResponse::from_task(&host, task)),
                );
            }
            Ok(_) => {
                tracing::warn!("Unexpected list_tasks response from {}", host);
            }
            Err(e) => {
                tracing::warn!("Failed to list tasks from '{}': {}", host, e);
            }
        }
    }

    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(warp::reply::json(&tasks))
}

async fn get_task(id: String, state: AppState) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    match find_task(&state, task_id).await {
        Ok(Some((host, task))) => Ok(warp::reply::with_status(
            warp::reply::json(&TaskResponse::from_task(&host, &task)),
            StatusCode::OK,
        )),
        Ok(None) => Ok(error_reply(StatusCode::NOT_FOUND, "Task not found")),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

#[derive(Deserialize)]
struct CreateTaskRequest {
    host: String,
    environment: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    use_worktree: bool,
    #[serde(default)]
    web_search: bool,
    prompt: String,
    #[serde(default)]
    agent: Option<slopcoder_core::anyagent::AgentKind>,
}

#[derive(Serialize)]
struct CreateTaskResponse {
    id: String,
    worktree_path: String,
}

#[derive(Deserialize)]
struct RenameTaskRequest {
    name: String,
}

async fn create_task(req: CreateTaskRequest, state: AppState) -> Result<impl Reply, Infallible> {
    let host = req.host.trim();
    if host.is_empty() {
        return Ok(error_reply(StatusCode::BAD_REQUEST, "Host is required"));
    }
    let agent = match pick_agent(state.clone(), Some(host)).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    let request = AgentCreateTaskRequest {
        environment: req.environment,
        name: req.name,
        use_worktree: req.use_worktree,
        web_search: req.web_search,
        prompt: req.prompt,
        agent: req.agent,
    };

    match agent.request(AgentRequest::CreateTask { request }).await {
        Ok(AgentResponse::CreatedTask { id, worktree_path }) => {
            state.set_task_host(id, agent.host).await;
            Ok(warp::reply::with_status(
                warp::reply::json(&CreateTaskResponse {
                    id: id.to_string(),
                    worktree_path,
                }),
                StatusCode::CREATED,
            ))
        }
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

async fn rename_task(
    id: String,
    req: RenameTaskRequest,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent
        .request(AgentRequest::RenameTask {
            task_id,
            name: req.name,
        })
        .await
    {
        Ok(AgentResponse::RenamedTask { task }) => Ok(warp::reply::with_status(
            warp::reply::json(&TaskResponse::from_task(&agent.host, &task)),
            StatusCode::OK,
        )),
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

#[derive(Deserialize)]
struct SendPromptRequest {
    prompt: String,
}

async fn send_prompt(
    id: String,
    req: SendPromptRequest,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent
        .request(AgentRequest::SendPrompt {
            task_id,
            prompt: req.prompt,
        })
        .await
    {
        Ok(AgentResponse::Ack) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({ "status": "started" })),
            StatusCode::OK,
        )),
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

#[derive(Serialize)]
struct TaskOutputResponse {
    events: Vec<AgentEvent>,
    total_events: usize,
    has_more_before: bool,
}

#[derive(Deserialize)]
struct TaskOutputQuery {
    #[serde(default)]
    before: usize,
    #[serde(default = "default_task_output_limit")]
    limit: usize,
}

fn default_task_output_limit() -> usize {
    120
}

async fn get_task_output(
    id: String,
    query: TaskOutputQuery,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent
        .request(AgentRequest::GetTaskOutput {
            task_id,
            pagination: TaskOutputPageRequest {
                before: query.before,
                limit: query.limit.max(1).min(500),
            },
        })
        .await
    {
        Ok(AgentResponse::TaskOutput {
            events,
            total_events,
            has_more_before,
        }) => Ok(warp::reply::with_status(
            warp::reply::json(&TaskOutputResponse {
                events,
                total_events,
                has_more_before,
            }),
            StatusCode::OK,
        )),
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

#[derive(Serialize)]
struct TaskDiffResponse {
    staged: String,
    unstaged: String,
}

async fn get_task_diff(id: String, state: AppState) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent.request(AgentRequest::GetTaskDiff { task_id }).await {
        Ok(AgentResponse::TaskDiff { staged, unstaged }) => Ok(warp::reply::with_status(
            warp::reply::json(&TaskDiffResponse { staged, unstaged }),
            StatusCode::OK,
        )),
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

async fn interrupt_task(id: String, state: AppState) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent.request(AgentRequest::InterruptTask { task_id }).await {
        Ok(AgentResponse::Ack) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({ "status": "interrupted" })),
            StatusCode::OK,
        )),
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

async fn merge_task(id: String, state: AppState) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent.request(AgentRequest::MergeTask { task_id }).await {
        Ok(AgentResponse::MergeResult { status, message }) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({ "status": status, "message": message })),
            StatusCode::OK,
        )),
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

#[derive(Serialize)]
struct MergeStatusResponse {
    can_merge: bool,
    reason: Option<String>,
}

async fn get_merge_status(id: String, state: AppState) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent
        .request(AgentRequest::GetMergeReadiness { task_id })
        .await
    {
        Ok(AgentResponse::MergeReadiness { can_merge, reason }) => Ok(warp::reply::with_status(
            warp::reply::json(&MergeStatusResponse { can_merge, reason }),
            StatusCode::OK,
        )),
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

async fn archive_task(id: String, state: AppState) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent.request(AgentRequest::ArchiveTask { task_id }).await {
        Ok(AgentResponse::ArchiveResult { status, message }) => {
            close_task_terminal_session(&state, &agent, task_id).await;
            state.clear_task_host(task_id).await;
            Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({ "status": status, "message": message })),
                StatusCode::OK,
            ))
        }
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

#[derive(Deserialize)]
struct DeleteTaskQuery {
    #[serde(default)]
    force: bool,
}

async fn delete_task(
    id: String,
    query: DeleteTaskQuery,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(e) => return Ok(error_reply(state_error_status(&e), e.to_string())),
    };

    match agent
        .request(AgentRequest::DeleteTask {
            task_id,
            force: query.force,
        })
        .await
    {
        Ok(AgentResponse::DeleteResult { status, message }) => {
            close_task_terminal_session(&state, &agent, task_id).await;
            state.clear_task_host(task_id).await;
            Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({ "status": status, "message": message })),
                StatusCode::OK,
            ))
        }
        Ok(_) => Ok(error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unexpected response from agent",
        )),
        Err(e) => Ok(error_reply(state_error_status(&e), e.to_string())),
    }
}

// ============================================================================
// Collaborator routes
// ============================================================================

#[derive(Deserialize)]
struct AddCollaboratorRequest {
    username: String,
}

async fn get_collaborators(id: String, state: AppState) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply.into_response()),
    };
    let store = state.task_store().read().await;
    match store.get(task_id) {
        Some(task) => Ok(warp::reply::json(&task.collaborators).into_response()),
        None => Ok(error_reply(StatusCode::NOT_FOUND, "Task not found").into_response()),
    }
}

async fn add_collaborator(
    id: String,
    body: AddCollaboratorRequest,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply.into_response()),
    };
    let mut store = state.task_store().write().await;
    match store.get_mut(task_id) {
        Some(task) => {
            if !task.collaborators.contains(&body.username) {
                task.collaborators.push(body.username);
            }
            let collabs = task.collaborators.clone();
            let _ = store.save().await;
            Ok(warp::reply::json(&collabs).into_response())
        }
        None => Ok(error_reply(StatusCode::NOT_FOUND, "Task not found").into_response()),
    }
}

async fn remove_collaborator(
    id: String,
    username: String,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(reply) => return Ok(reply.into_response()),
    };
    let mut store = state.task_store().write().await;
    match store.get_mut(task_id) {
        Some(task) => {
            task.collaborators.retain(|c| c != &username);
            let collabs = task.collaborators.clone();
            let _ = store.save().await;
            Ok(warp::reply::json(&collabs).into_response())
        }
        None => Ok(error_reply(StatusCode::NOT_FOUND, "Task not found").into_response()),
    }
}

// ============================================================================
// Secrets routes
// ============================================================================

fn secrets_routes(
    state: AppState,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone {
    let list = warp::path::end()
        .and(warp::get())
        .and(warp::header::optional::<String>("cookie"))
        .and(with_state(state.clone()))
        .and_then(list_secrets);

    let create = warp::path::end()
        .and(warp::post())
        .and(warp::body::json())
        .and(warp::header::optional::<String>("cookie"))
        .and(with_state(state.clone()))
        .and_then(create_secret);

    let delete = warp::path!(String)
        .and(warp::delete())
        .and(warp::query::<DeleteSecretQuery>())
        .and(warp::header::optional::<String>("cookie"))
        .and(with_state(state))
        .and_then(delete_secret);

    list.or(create).or(delete)
}

#[derive(Deserialize)]
struct CreateSecretRequest {
    name: String,
    value: String,
    #[serde(default)]
    environment: Option<String>,
}

#[derive(Deserialize)]
struct DeleteSecretQuery {
    #[serde(default)]
    environment: Option<String>,
}

fn extract_username_from_cookie(cookie_header: &Option<String>, jwt_secret: &str) -> Option<String> {
    cookie_header
        .as_deref()
        .and_then(extract_jwt_from_cookie)
        .and_then(|token| JwtClaims::decode(&token, jwt_secret).ok())
        .map(|claims| claims.sub)
}

async fn list_secrets(
    cookie_header: Option<String>,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let Some(username) = extract_username_from_cookie(&cookie_header, state.jwt_secret()) else {
        return Ok(error_reply(StatusCode::UNAUTHORIZED, "Not authenticated"));
    };
    let mgr = crate::secrets::LocalSecretsManager::new(
        state.task_store().read().await.data_dir().to_path_buf(),
    );
    let entries = mgr.list(&username).await;
    Ok(warp::reply::with_status(warp::reply::json(&entries), StatusCode::OK))
}

async fn create_secret(
    body: CreateSecretRequest,
    cookie_header: Option<String>,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let Some(username) = extract_username_from_cookie(&cookie_header, state.jwt_secret()) else {
        return Ok(error_reply(StatusCode::UNAUTHORIZED, "Not authenticated"));
    };
    let mgr = crate::secrets::LocalSecretsManager::new(
        state.task_store().read().await.data_dir().to_path_buf(),
    );
    match mgr.set(&username, &body.name, &body.value, body.environment.as_deref()).await {
        Ok(()) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"ok": true})),
            StatusCode::OK,
        )),
        Err(e) => Ok(error_reply(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn delete_secret(
    name: String,
    query: DeleteSecretQuery,
    cookie_header: Option<String>,
    state: AppState,
) -> Result<impl Reply, Infallible> {
    let Some(username) = extract_username_from_cookie(&cookie_header, state.jwt_secret()) else {
        return Ok(error_reply(StatusCode::UNAUTHORIZED, "Not authenticated"));
    };
    let mgr = crate::secrets::LocalSecretsManager::new(
        state.task_store().read().await.data_dir().to_path_buf(),
    );
    match mgr.delete(&username, &name, query.environment.as_deref()).await {
        Ok(()) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"ok": true})),
            StatusCode::OK,
        )),
        Err(e) => Ok(error_reply(StatusCode::NOT_FOUND, e.to_string())),
    }
}

async fn resolve_agent_for_task(
    state: &AppState,
    task_id: TaskId,
) -> Result<ConnectedAgent, StateError> {
    if let Some(agent) = state.resolve_agent_for_task(task_id).await {
        return Ok(agent);
    }

    let agents = state.list_agents().await;
    let responses = join_all(agents.into_iter().map(|agent| async move {
        let host = agent.host.clone();
        let response = request_with_timeout(&agent, AgentRequest::GetTask { task_id }, 10).await;
        (agent, host, response)
    }))
    .await;

    for (agent, host, response) in responses {
        if let Ok(AgentResponse::Task { task: Some(_) }) = response {
            state.set_task_host(task_id, host).await;
            return Ok(agent);
        }
    }

    Err(StateError::RemoteError {
        status: StatusCode::NOT_FOUND.as_u16(),
        error: "Task not found".to_string(),
    })
}

async fn find_task(
    state: &AppState,
    task_id: TaskId,
) -> Result<Option<(String, Task)>, StateError> {
    if let Some(agent) = state.resolve_agent_for_task(task_id).await {
        match agent.request(AgentRequest::GetTask { task_id }).await {
            Ok(AgentResponse::Task { task: Some(task) }) => {
                return Ok(Some((agent.host, task)));
            }
            Ok(AgentResponse::Task { task: None }) => {
                state.clear_task_host(task_id).await;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Failed to fetch mapped task {}: {}", task_id, e);
            }
        }
    }

    let agents = state.list_agents().await;
    let responses = join_all(agents.into_iter().map(|agent| async move {
        let host = agent.host.clone();
        let response = request_with_timeout(&agent, AgentRequest::GetTask { task_id }, 10).await;
        (host, response)
    }))
    .await;

    for (host, response) in responses {
        match response {
            Ok(AgentResponse::Task { task: Some(task) }) => {
                state.set_task_host(task_id, host.clone()).await;
                return Ok(Some((host, task)));
            }
            Ok(AgentResponse::Task { task: None }) => {}
            Ok(_) => {}
            Err(e) => tracing::warn!("Failed to query task {} on {}: {}", task_id, host, e),
        }
    }
    Ok(None)
}

// ============================================================================
// Agent websocket
// ============================================================================

async fn handle_agent_socket(ws: WebSocket, state: AppState) {
    let (mut sink, mut stream) = ws.split();

    let hello = match stream.next().await {
        Some(Ok(msg)) if msg.is_text() => match msg.to_str() {
            Ok(text) => match serde_json::from_str::<AgentEnvelope>(text) {
                Ok(AgentEnvelope::Hello {
                    hostname,
                    display_name,
                }) => (hostname, display_name),
                _ => {
                    let _ = sink.send(Message::text("expected hello")).await;
                    return;
                }
            },
            Err(_) => return,
        },
        _ => return,
    };

    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<AgentEnvelope>();
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<AgentResponse, RemoteError>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let agent = state
        .register_agent(
            hello.0.clone(),
            hello.1.clone(),
            outbound_tx.clone(),
            pending.clone(),
        )
        .await;
    tracing::info!(
        "Agent connected host='{}' hostname='{}'",
        agent.host,
        agent.hostname
    );

    let writer = tokio::spawn(async move {
        while let Some(envelope) = outbound_rx.recv().await {
            let payload = match serde_json::to_string(&envelope) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Failed to serialize envelope for agent write: {}", e);
                    continue;
                }
            };
            if sink.send(Message::text(payload)).await.is_err() {
                break;
            }
        }
    });

    while let Some(incoming) = stream.next().await {
        let Ok(message) = incoming else {
            break;
        };
        if message.is_close() {
            break;
        }
        if !message.is_text() {
            continue;
        }

        let text = match message.to_str() {
            Ok(t) => t,
            Err(_) => continue,
        };

        let envelope = match serde_json::from_str::<AgentEnvelope>(text) {
            Ok(env) => env,
            Err(e) => {
                tracing::warn!("Failed to decode agent envelope from {}: {}", agent.host, e);
                continue;
            }
        };

        match envelope {
            AgentEnvelope::Response {
                request_id,
                response,
            } => {
                if let Some(tx) = pending.lock().await.remove(&request_id) {
                    let _ = tx.send(Ok(response));
                }
            }
            AgentEnvelope::Error {
                request_id,
                status,
                error,
            } => {
                if let Some(tx) = pending.lock().await.remove(&request_id) {
                    let _ = tx.send(Err(RemoteError { status, error }));
                }
            }
            AgentEnvelope::TaskEvent { task_id, event } => {
                state.set_task_host(task_id, agent.host.clone()).await;
                state.broadcast_task_event(task_id, event).await;
            }
            AgentEnvelope::TerminalData { terminal_id, data } => {
                state
                    .broadcast_terminal_event(terminal_id, TerminalEvent::Data(data))
                    .await;
            }
            AgentEnvelope::TerminalClosed { terminal_id } => {
                state
                    .broadcast_terminal_event(terminal_id, TerminalEvent::Closed)
                    .await;
            }
            AgentEnvelope::TerminalError { terminal_id, error } => {
                state
                    .broadcast_terminal_event(terminal_id, TerminalEvent::Error(error))
                    .await;
            }
            AgentEnvelope::TerminalOpen { .. }
            | AgentEnvelope::TerminalInput { .. }
            | AgentEnvelope::TerminalResize { .. }
            | AgentEnvelope::TerminalClose { .. } => {
                tracing::warn!(
                    "Ignoring unexpected terminal command envelope from agent '{}'",
                    agent.host
                );
            }
            AgentEnvelope::Hello { .. } | AgentEnvelope::Request { .. } => {
                tracing::warn!("Ignoring unexpected envelope from agent '{}'", agent.host);
            }
        }
    }

    let mut pending_locked = pending.lock().await;
    for (_, tx) in pending_locked.drain() {
        let _ = tx.send(Err(RemoteError {
            status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
            error: "Agent disconnected".to_string(),
        }));
    }
    drop(pending_locked);

    writer.abort();
    state.unregister_agent(agent.id).await;
}

// ============================================================================
// Task event websocket for UI
// ============================================================================

async fn handle_task_websocket(ws: WebSocket, id: String, state: AppState) {
    let Ok(uuid) = Uuid::parse_str(&id) else {
        tracing::warn!("Invalid task ID in websocket: {}", id);
        return;
    };
    let task_id = TaskId(uuid);
    let mut rx = state.subscribe_to_task(task_id).await;
    let (mut tx, mut _rx) = ws.split();

    while let Ok(event) = rx.recv().await {
        let json = match serde_json::to_string(&event) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("Failed to serialize task event: {}", e);
                continue;
            }
        };
        if tx.send(Message::text(json)).await.is_err() {
            break;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalClientMessage {
    Resize { rows: u16, cols: u16 },
}

async fn handle_terminal_websocket(ws: WebSocket, id: String, state: AppState) {
    let task_id = match parse_task_id(&id) {
        Ok(id) => id,
        Err(_) => {
            tracing::warn!("Invalid task ID in terminal websocket: {}", id);
            return;
        }
    };

    let agent = match resolve_agent_for_task(&state, task_id).await {
        Ok(agent) => agent,
        Err(_) => match find_task(&state, task_id).await {
            Ok(Some((host, _))) => match state.get_agent_for_host(&host).await {
                Some(agent) => agent,
                None => {
                    tracing::warn!("Task host '{}' is not connected for terminal {}", host, id);
                    return;
                }
            },
            Ok(None) => {
                tracing::warn!("Task not found for terminal websocket: {}", id);
                return;
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to resolve task host for terminal websocket {}: {}",
                    id,
                    e
                );
                return;
            }
        },
    };

    let previous_terminal = state.get_task_terminal(task_id).await;
    let (terminal_id, needs_open) = state.ensure_task_terminal(task_id, &agent.host).await;
    if needs_open {
        if let Some((previous_terminal_id, previous_host)) = previous_terminal {
            if previous_host != agent.host {
                if let Some(previous_agent) = state.get_agent_for_host(&previous_host).await {
                    let _ = previous_agent.send_envelope(AgentEnvelope::TerminalClose {
                        terminal_id: previous_terminal_id,
                    });
                }
            }
        }
    }

    let mut terminal_events = state.subscribe_to_terminal(terminal_id).await;
    if needs_open {
        if let Err(e) = agent.send_envelope(AgentEnvelope::TerminalOpen {
            terminal_id,
            task_id,
        }) {
            tracing::warn!(
                "Failed to send terminal open to host '{}' for task {}: {}",
                agent.host,
                task_id,
                e
            );
            let _ = state.take_task_terminal(task_id).await;
            return;
        }
    }

    let (mut ws_tx, mut ws_rx) = ws.split();

    let mut to_ws = tokio::spawn(async move {
        while let Ok(event) = terminal_events.recv().await {
            match event {
                TerminalEvent::Data(data) => {
                    if ws_tx.send(Message::binary(data)).await.is_err() {
                        break;
                    }
                }
                TerminalEvent::Closed => break,
                TerminalEvent::Error(error) => {
                    tracing::warn!("Remote terminal error {}: {}", terminal_id, error);
                    break;
                }
            }
        }
    });

    let agent_for_input = agent.clone();
    let mut from_ws = tokio::spawn(async move {
        while let Some(incoming) = ws_rx.next().await {
            let Ok(message) = incoming else {
                break;
            };
            if message.is_close() {
                break;
            }

            if message.is_binary() {
                let _ = agent_for_input.send_envelope(AgentEnvelope::TerminalInput {
                    terminal_id,
                    data: message.into_bytes(),
                });
                continue;
            }

            if !message.is_text() {
                continue;
            }

            let Ok(text) = message.to_str() else {
                continue;
            };
            if let Ok(TerminalClientMessage::Resize { rows, cols }) =
                serde_json::from_str::<TerminalClientMessage>(text)
            {
                let _ = agent_for_input.send_envelope(AgentEnvelope::TerminalResize {
                    terminal_id,
                    rows,
                    cols,
                });
            }
        }
    });

    tokio::select! {
        _ = (&mut to_ws) => {
            from_ws.abort();
        }
        _ = (&mut from_ws) => {
            to_ws.abort();
        }
    }
}

async fn close_task_terminal_session(state: &AppState, agent: &ConnectedAgent, task_id: TaskId) {
    let Some((terminal_id, terminal_host)) = state.take_task_terminal(task_id).await else {
        return;
    };

    let close_agent = if terminal_host == agent.host {
        Some(agent.clone())
    } else {
        state.get_agent_for_host(&terminal_host).await
    };
    if let Some(close_agent) = close_agent {
        if let Err(e) = close_agent.send_envelope(AgentEnvelope::TerminalClose { terminal_id }) {
            tracing::warn!(
                "Failed to close terminal {} for task {} on host '{}': {}",
                terminal_id,
                task_id,
                close_agent.host,
                e
            );
        }
    } else {
        tracing::warn!(
            "Terminal host '{}' disconnected before closing terminal {} for task {}",
            terminal_host,
            terminal_id,
            task_id
        );
    }
}

// ============================================================================
// Helpers
// ============================================================================

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

fn error_reply(
    status: StatusCode,
    error: impl Into<String>,
) -> warp::reply::WithStatus<warp::reply::Json> {
    warp::reply::with_status(
        warp::reply::json(&ErrorResponse {
            error: error.into(),
        }),
        status,
    )
}

fn parse_task_id(id: &str) -> Result<TaskId, warp::reply::WithStatus<warp::reply::Json>> {
    Uuid::parse_str(id)
        .map(TaskId)
        .map_err(|_| error_reply(StatusCode::BAD_REQUEST, "Invalid task ID"))
}

fn with_state(state: AppState) -> impl Filter<Extract = (AppState,), Error = Infallible> + Clone {
    warp::any().map(move || state.clone())
}

fn auth_filter_api(state: AppState) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    let raw_query = warp::query::raw()
        .or(warp::any().map(|| "".to_string()))
        .unify();

    warp::any()
        .and(with_state(state))
        .and(warp::method())
        .and(warp::header::optional::<String>("x-slopcoder-password"))
        .and(warp::header::optional::<String>("cookie"))
        .and(raw_query)
        .and_then(check_api_auth)
        .untuple_one()
}

fn auth_filter_agent(
    state: AppState,
) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    let raw_query = warp::query::raw()
        .or(warp::any().map(|| "".to_string()))
        .unify();

    warp::any()
        .and(with_state(state))
        .and(warp::method())
        .and(warp::header::optional::<String>("x-slopcoder-password"))
        .and(raw_query)
        .and_then(check_agent_auth)
        .untuple_one()
}

async fn check_api_auth(
    state: AppState,
    method: Method,
    header_password: Option<String>,
    cookie_header: Option<String>,
    raw_query: String,
) -> Result<(), warp::Rejection> {
    if method == Method::OPTIONS {
        return Ok(());
    }
    // Check JWT cookie first
    if let Some(ref cookies) = cookie_header {
        if let Some(token) = extract_jwt_from_cookie(cookies) {
            if JwtClaims::decode(&token, state.jwt_secret()).is_ok() {
                return Ok(());
            }
        }
    }
    // If GitHub OAuth is configured, JWT is required (no password fallback for browsers)
    if state.github_oauth_configured() {
        return Err(warp::reject::custom(AuthError));
    }
    // Fall back to password auth
    let required = state.get_ui_auth_password().await;
    if let Some(required) = required {
        let query_password = extract_password_from_query(&raw_query);
        let provided = header_password.or(query_password);
        if provided.as_deref() != Some(required.as_str()) {
            return Err(warp::reject::custom(AuthError));
        }
    }
    Ok(())
}

async fn check_agent_auth(
    state: AppState,
    method: Method,
    header_password: Option<String>,
    raw_query: String,
) -> Result<(), warp::Rejection> {
    if method == Method::OPTIONS {
        return Ok(());
    }
    let required = state.get_agent_auth_password().await;
    let query_password = extract_password_from_query(&raw_query);
    let provided = header_password.or(query_password);
    if provided.as_deref() != Some(required.as_str()) {
        return Err(warp::reject::custom(AuthError));
    }
    Ok(())
}

fn extract_password_from_query(raw_query: &str) -> Option<String> {
    if raw_query.is_empty() {
        return None;
    }

    for pair in raw_query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let decoded_key = urlencoding::decode(key).ok()?;
        if decoded_key == "password" {
            let value = parts.next().unwrap_or("");
            return urlencoding::decode(value).ok().map(|v| v.into_owned());
        }
    }
    None
}

async fn handle_rejection(err: warp::Rejection) -> Result<impl Reply, Infallible> {
    if err.find::<AuthError>().is_some() {
        return Ok(error_reply(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    if err.is_not_found() {
        return Ok(error_reply(StatusCode::NOT_FOUND, "Not Found"));
    }
    if err.find::<InvalidQuery>().is_some() {
        return Ok(error_reply(StatusCode::BAD_REQUEST, "Invalid query"));
    }
    tracing::error!("Unhandled API rejection: {:?}", err);
    Ok(error_reply(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal Server Error",
    ))
}

async fn pick_agent(state: AppState, host: Option<&str>) -> Result<ConnectedAgent, StateError> {
    if let Some(host) = host {
        return state
            .get_agent_for_host(host)
            .await
            .ok_or_else(|| StateError::HostNotConnected(host.to_string()));
    }

    let agents = state.list_agents().await;
    match agents.len() {
        0 => Err(StateError::NoAgentsConnected),
        1 => Ok(agents[0].clone()),
        _ => Err(StateError::HostRequired),
    }
}

fn state_error_status(err: &StateError) -> StatusCode {
    match err {
        StateError::HostRequired => StatusCode::BAD_REQUEST,
        StateError::HostNotConnected(_) => StatusCode::NOT_FOUND,
        StateError::NoAgentsConnected => StatusCode::SERVICE_UNAVAILABLE,
        StateError::AgentDisconnected | StateError::AgentTimeout => StatusCode::SERVICE_UNAVAILABLE,
        StateError::RemoteError { status, .. } => {
            StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn request_with_timeout(
    agent: &ConnectedAgent,
    request: AgentRequest,
    timeout_seconds: u64,
) -> Result<AgentResponse, StateError> {
    agent
        .request_with_timeout(request, Duration::from_secs(timeout_seconds))
        .await
}

#[cfg(test)]
mod tests {
    use super::{extract_jwt_from_cookie, extract_password_from_query, JwtClaims};

    #[test]
    fn test_extract_password() {
        assert_eq!(
            extract_password_from_query("foo=bar&password=abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            extract_password_from_query("password=hello%20world"),
            Some("hello world".to_string())
        );
        assert_eq!(extract_password_from_query("foo=bar"), None);
        assert_eq!(extract_password_from_query(""), None);
    }

    #[test]
    fn test_jwt_roundtrip() {
        let secret = "test-secret";
        let claims = JwtClaims::new_dev("testuser");
        let token = claims.encode(secret).unwrap();
        let decoded = JwtClaims::decode(&token, secret).unwrap();
        assert_eq!(decoded.sub, "testuser");
        assert_eq!(decoded.github_id, 0);
        assert_eq!(decoded.orgs, vec!["dev"]);
    }

    #[test]
    fn test_jwt_wrong_secret_fails() {
        let claims = JwtClaims::new_dev("testuser");
        let token = claims.encode("secret1").unwrap();
        assert!(JwtClaims::decode(&token, "secret2").is_err());
    }

    #[test]
    fn test_extract_jwt_from_cookie() {
        assert_eq!(
            extract_jwt_from_cookie("slopcoder_session=abc123; other=val"),
            Some("abc123".to_string())
        );
        assert_eq!(
            extract_jwt_from_cookie("other=val; slopcoder_session=xyz"),
            Some("xyz".to_string())
        );
        assert_eq!(extract_jwt_from_cookie("other=val"), None);
        assert_eq!(extract_jwt_from_cookie("slopcoder_session="), None);
    }
}
