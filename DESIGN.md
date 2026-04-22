# Slopcoder Design

## 1. Purpose

Slopcoder coordinates coding agents across one or more hosts, each host running `slopagent` against local checked-out Git repositories.

The system has four parts:
- `slopcoder-core`: shared domain logic (environments, tasks, persistence, agent adapters, naming).
- `slopagent`: host-local worker that owns task lifecycle and Git operations.
- `slopcoder-server`: coordinator API/UI server and RPC relay.
- `frontend`: SolidJS UI for creating/running/merging tasks.

## 2. Environment Model

Implemented in `crates/slopcoder-core/src/environment.rs`.

An environment is now a **checked-out Git repository directory** (not a bare repo root).
`slopagent` configuration is CLI-driven for discovery (`REPO_ROOT` + bounds), while
environment/worktree storage roots are fixed to the XDG data directory.

Semantics:
- CLI positional `REPO_ROOT` is required and is scanned for repositories.
- Storage root is `$XDG_DATA_HOME/slopcoder` (fallback: `~/.local/share/slopcoder`).
- Worktrees live under `<storage_root>/worktrees`.
- Create-environment and environment discovery root is `<storage_root>/environments`.
- Discovery is recursive, bounded (`max_depth` default `10`, `max_repos` default `100`), skips hidden directories,
  and does not descend into directories that are already Git repositories.
- `slopagent` serves environment lists from an in-memory cache and refreshes discovery in the background on a short interval,
  so repository scans do not block request handling.
- Environment IDs are the directory paths.

Validation:
- `worktrees_directory` is created at startup if missing, then validated as a directory.
- Each environment must satisfy `git rev-parse --is-inside-work-tree`.

Core operations:
- `list_branches()` from the checked-out repository.
- `current_branch()` for in-repo HEAD branch resolution.
- `create_worktree_from_base(worktrees_directory, base_branch, merge_branch)` for isolated tasks.

## 3. Task Model

Implemented in `crates/slopcoder-core/src/task.rs`.

Task fields:
- `id`, `agent`, `environment`, `name`, `worktree_path`, `status`, `session_id`, `created_at`, `history`.
- `workspace_kind`: `environment` or `worktree`.
- `base_branch` and `merge_branch` are set only for `worktree` tasks.
- `web_search`: task-level boolean persisted with the task and reused on prompt resumes.

Task behavior:
- Every task runs in exactly one directory (`worktree_path`).
- `environment` tasks run directly in the environment repo directory.
- `worktree` tasks run in a newly created isolated worktree and are mergeable.

State transitions:
- `pending/completed/failed/interrupted -> running`
- `running -> completed|failed|interrupted`

## 4. Task Naming (DSPy)

Implemented in `crates/slopcoder-core/src/branch_picker.rs`.

Task names are no longer feature branch names.

Flow:
- `pick_task_topic(prompt, model)` asks DSPy for a short topic.
- Task names are normalized to whole words with a strict `< 25` character limit (max number of words that fit).
- On failure, fallback uses the first prompt line with the same whole-word `< 25` character rule (`fallback_topic_name`).

For isolated worktrees, merge branches are internal and generated from topic slug + random suffix:
- Example: `task/fix-login-flow-a1b2c3d4`.

## 5. Persistence

Implemented in `crates/slopcoder-core/src/persistence.rs` and `crates/slopagent/src/state.rs`.

Persistence is file-based, but now stored **outside** repository working directories:
- Root: `<worktrees_directory>/.slopcoder-state/<env-slug>/`
- Files:
  - `tasks.yaml`
  - `task-<task_id>.jsonl`
- Archive root: `<worktrees_directory>/.slopcoder-state/archive/<env-slug>/`
  - `task-<task_id>.jsonl` (moved here when archived or deleted)

Rationale:
- Keeps environment repositories clean (no metadata files showing up as untracked changes).
- Prevents merge checks from failing due to Slopcoder’s own files.

Store behavior:
- Load per-environment tasks from `tasks.yaml`.
- Remove tasks whose workspace directory no longer exists.
- Mark stale `running` tasks as `failed` on restart.
- Rewrite only affected environment task snapshots.

## 6. Agent-Side Task Creation, Merge, Archive, and Delete

Implemented in `crates/slopagent/src/main.rs`.

`create_task` modes:
- In-place (`use_worktree=false`):
  - `worktree_path = environment.directory`
  - No merge branch; task is not mergeable via UI API.
- Isolated (`use_worktree=true`):
  - Resolve `base_branch` from environment current branch.
  - Create new merge branch and worktree under `worktrees_directory`.
  - Task is mergeable.

Merge rules:
- Only `workspace_kind == worktree` tasks can be merged.
- Task worktree must be clean.
- Environment repo must be clean.
- Merge availability precheck uses `git merge-tree --write-tree HEAD <merge_branch>`.
- Merge runs in environment repo directory: `git merge <merge_branch>`.
- Conflict path aborts merge and returns an error.

Archive/delete rules:
- `archive` is for `environment` tasks: move `task-<id>.jsonl` to archive directory and remove task from active list.
- `delete` is for `worktree` tasks: prune the worktree, archive `task-<id>.jsonl`, remove task from active list, and attempt branch cleanup.
- Non-force prune may fail when modified/untracked files exist; API returns a conflict instructing force prune.

Diff behavior:
- For worktree tasks, staged diff is against `base_branch`.
- For in-place tasks, staged diff is regular cached diff in current repo state.
- Unstaged includes tracked + untracked changes.

## 7. Coordinator and API

Coordinator routes live in `crates/slopcoder-server/src/routes.rs`.

Coordinator request model:
- Multi-host fan-out endpoints (environment/task listing and task lookup fallback) query hosts in parallel instead of serially.
- Environment/task list fan-out uses a per-host RPC timeout configured from the coordinator CLI (`--list-request-timeout-secs`, default `15s`) so one slow host does not stall listing for healthy hosts.
- Hosts remain visible/selectable after list timeouts; failed list calls only affect the current request and are retried on the next poll.
- Per-host coordinator RPC calls use bounded route-level timeouts to keep UI handlers responsive even when one host is slow.
- Timed-out/disconnected pending RPC entries are explicitly cleaned up in coordinator state.
- Agent RPC requests are handled concurrently per request ID, so a long-running request (for example, environment discovery)
  does not block unrelated agent operations on the same connection.

Task creation payload:
- `host`, `environment`, optional `name`, `use_worktree`, `web_search`, `prompt`, `agent`.

Task response payload now includes:
- `name`
- `workspace_kind`
- `base_branch` (optional)
- `merge_branch` (optional)

Task action endpoints:
- `PATCH /api/tasks/:id` (rename task; returns updated task)
- `POST /api/tasks/:id/merge`
- `GET /api/tasks/:id/merge-status` (returns `can_merge` + reason)
- `POST /api/tasks/:id/archive`
- `DELETE /api/tasks/:id?force=true|false`
- `GET /api/tasks/:id/terminal` (websocket PTY for interactive terminal I/O)

Environment creation via API:
- UI provides host + environment name only.
- Agent creates `<storage_root>/environments/<name>`, initializes a Git repository, and makes an empty initial commit.
- Created/discovered environments are listed immediately without local config-file writes.
- The create-environment screen also captures an initial prompt and immediately creates the first task in the new environment.

## 8. Frontend Model

Primary UI is `frontend/src/components/Workspace.tsx`.

Behavior:
- Environment list includes repositories auto-discovered under `environments_root` and optional `repo_root`.
- Environment list includes repositories auto-discovered under `<storage_root>/environments` and optional `repo_root`.
- On initial UI load, any environment with a currently `running` task is auto-expanded so active work is visible in the sidebar.
- Host entries in the sidebar are shown without bordered cards.
- "Create Environment" button appears above the list and opens a host+name form.
- The create-environment form uses the same centered "Let's Build" visual style and starts the first task immediately after environment creation.
- New task form no longer asks for base/feature branch.
- User can toggle `Run task in isolated worktree (mergeable)`.
- `Enable web search` is shown only when the selected agent supports it.
- `web_search` is currently wired to Codex (`--search`) and ignored for other agents.
- Prompt textareas in task-creation panes support `Ctrl+Enter` (and `Cmd+Enter`) to submit without clicking.
- Task conversation follow-up drafts are cached locally per task ID in browser `localStorage`; drafts are not persisted on the server.
- Task list and task header display task `name` (topic).
- Double-clicking a task title in the sidebar or task header switches it into an inline rename input; save occurs on `Enter` or blur, and `Escape` cancels.
- The workspace keeps the last known hosts, environments, and tasks in client memory so a transient host disconnect does not remove that host's task list from the sidebar before it reconnects.
- Disconnected hosts remain visible but dimmed; their environments/tasks are also dimmed rather than disappearing.
- When a host is disconnected, the UI disables task creation for that host and disables the conversation `Send` composer for its existing tasks while leaving the transcript visible.
- `environment` tasks show an archive button beside the task title.
- `worktree` tasks show merge/delete actions (no archive button).
- Merge action uses server-side merge readiness and is disabled when merge cannot currently succeed.
- Delete action uses an inline warning dialog (no JS modal) and supports force prune when normal prune fails.
- Unused legacy `frontend/src/components/TaskDetail.tsx` has been removed; `Workspace.tsx` is the only task conversation UI.
- Task detail reactive resources must not reference `taskData` before it is initialized (prevents runtime TDZ errors when opening tasks).
- Opening a different task conversation now waits for the latest transcript page to load, then auto-scrolls to the very bottom.
- Switching back to the Conversation tab also auto-scrolls the transcript to the newest message.
- Task transcript loading is paginated end-to-end: the browser requests only the newest page first, and older events are fetched on demand as the user scrolls upward.
- Transcript pagination is serviced by `slopagent`; the coordinator and browser no longer transfer an entire persisted task log just to open a task.
- Conversation pane constrains message width (`min-w-0`) and enables horizontal scrolling so long unbroken lines (for example, markdown code-fence lines) do not widen the whole page.
- Task conversation view keeps the task header pinned at the top and the follow-up composer pinned at the bottom (mobile and desktop), with the transcript as the primary scrollable region.
- Follow-up prompt form keeps the send button on-screen by allowing the textarea to shrink and forcing the button to remain non-shrinking.
- On mobile, the app shell is clamped to the visual viewport (`100dvh` / `100vw`) with page-level overflow hidden, so the browser window does not scroll and the conversation transcript remains the primary vertical scroller.
- Live conversation streaming avoids subscription churn during task polling to reduce update flicker.
- Transcript item normalization happens in Rust before events are persisted or streamed to the browser, so oversized message/tool payloads are clipped on the agent side instead of being shipped raw to the client.
- `command_execution` transcript items now render as command cards showing the command text and a Rust-truncated output preview capped at 5 lines and 1000 characters; the preview text itself carries any truncation marker, and no separate UI truncation badge is shown.
- Task detail tabs now include `Terminal` beside `Conversation` and `Diff` on desktop.
- Terminal uses `xterm` over a coordinator websocket that proxies I/O to the owning `slopagent` host.
- Terminal starts in the selected task workspace directory on that remote host (`worktree_path` for isolated tasks, environment directory for in-place tasks).
- Terminal sessions are now task-scoped and persistent: reconnecting the websocket for the same task reattaches to the same remote PTY instead of spawning a fresh shell.
- Terminal sessions are torn down only when the task is archived/deleted (or when the owning agent disconnects), not when a browser tab closes or the user switches task tabs.
- Terminal websocket supports dynamic PTY resize so the shell tracks pane/window dimensions.
- `slopagent` task-state mutations now snapshot persistence data while holding the in-memory state lock, then perform async disk writes only after releasing that lock so long-running task updates cannot stall unrelated websocket RPC handling.

## 9. Testing

- Rust unit/integration tests cover environment operations, persistence behavior, and task lifecycle.
- Frontend build runs TypeScript typecheck and Vite build.
- End-to-end behavior remains host-local on `slopagent`, with coordinator acting as RPC relay.

## 10. SlopCoderNG Extensions

SlopCoderNG extends Slopcoder into a multi-tenant, Kubernetes-native platform.
The full spec is in `SLOPCODERNG-SPEC.md`. This section documents the implemented modules.

### 10.1 Container Images

- `Dockerfile.server`: debian-slim with musl `slopcoder-server` binary + frontend assets at `/srv/frontend`.
- `Dockerfile.agent`: debian-slim with `slopagent`, `code-server`, `openssh-server`, `git`. Entrypoint: `workspace-init`.
- `scripts/workspace-init`: orchestrates sshd, code-server, devcontainer lifecycle hooks, setup workflows, then slopagent.
- CI builds and pushes both images to GHCR on `ripley-cloud` runner.

### 10.2 Authentication

Implemented in `crates/slopcoder-server/src/routes.rs`.

- `--dev-mode` flag (or `SLOPCODER_DEV_MODE=1`): bypasses GitHub OAuth for local dev and E2E tests.
- `GET /auth/dev-login?user=<name>`: sets JWT session cookie (dev mode only).
- `GET /auth/login`: redirects to GitHub OAuth authorize URL.
- `GET /auth/callback?code=...`: exchanges code for token, fetches user profile + orgs, issues JWT.
- `GET /auth/me`: returns current user from JWT cookie.
- `POST /auth/logout`: clears session cookie.
- JWT claims: `sub` (username), `github_id`, `orgs`, `teams`, `avatar_url`, `exp`.
- `auth_filter_api` accepts JWT cookie or password header. When GitHub OAuth is configured, JWT is required.

### 10.3 Authorization

Implemented in `crates/slopcoder-server/src/state.rs`.

- `SLOPCODER_ALLOWED_ORGS`: comma-separated list of GitHub orgs.
- `SLOPCODER_ALLOWED_TEAMS_<ORG>`: per-org team restrictions.
- `SLOPCODER_MAX_WORKSPACES_PER_USER`: workspace limit (default 5).
- `check_authorization(orgs, teams)`: verifies user is in an allowed org/team. Dev mode bypasses.

### 10.4 Coordinator Task Store

Implemented in `crates/slopcoder-server/src/persistence.rs`.

- `CoordinatorTaskStore`: YAML-based persistence in `--data-dir` (default `./data`).
- Coordinator is the source of truth for task metadata (owner, slug, pod_name, ssh_port, etc.).
- Crash recovery: running tasks marked failed on startup.
- Task struct extended with: `owner`, `workspace_slug`, `pod_name`, `ssh_port`, `ssh_command`, `workspace_url`, `app_url`, `http_port`, `collaborators`.

### 10.5 Kubernetes Pod Launcher

Implemented in `crates/slopcoder-server/src/k8s.rs`.

- `K8sClient`: auto-detects k8s via `KUBERNETES_SERVICE_HOST`. No-op when not in cluster.
- `create_workspace()`: creates PVC + Pod + ClusterIP Service.
- `delete_workspace()`: removes Pod + Service, retains PVC.
- `workspace_pod_spec()`: generates full Pod spec with code-server, sshd, slopagent, volume mounts.
- `fetch_github_ssh_keys()`: fetches user's public keys from `github.com/<user>.keys`.

### 10.6 OAuth Reverse Proxy

Implemented in `crates/slopcoder-server/src/proxy.rs`.

- `parse_workspace_host()`: extracts workspace slug from Host header.
- `<slug>.<WORKSPACE_DOMAIN>` → code-server (port 8080).
- `www.<slug>.<WORKSPACE_DOMAIN>` → user's app port (configurable, default 3000).
- `check_workspace_access()`: validates owner or collaborator access.
- Collaborator CRUD: `GET/POST/DELETE /api/tasks/:id/collaborators`.

### 10.7 SSH Port Allocation

Implemented in `crates/slopcoder-server/src/ssh.rs`.

- `SshPortPool`: allocates unique ports from a configurable range (default 30000-32767).
- Single hostname `ssh.<SSH_DOMAIN>`, per-workspace port.
- `ssh_command()`: formats `ssh -p <port> dev@<domain>` for UI display.

### 10.8 User Secrets

Implemented in `crates/slopcoder-server/src/secrets.rs`.

- `LocalSecretsManager`: file-backed YAML storage per user.
- Global secrets (user-level) and environment-scoped secrets.
- Merge logic: global first, environment-scoped overrides on collision.
- API: `POST/GET/DELETE /api/secrets`. Values never returned after creation.

### 10.9 devcontainer.json Support

Implemented in `crates/slopcoder-core/src/devcontainer.rs`.

- Parses `.devcontainer/devcontainer.json`: `image`, `postCreateCommand`, `postStartCommand`, `customizations.vscode.extensions`, `customizations.slopcoder.*`.
- `ResolvedConfig`: merges platform defaults → user settings → devcontainer.json.
- SlopCoderNG-specific fields under `customizations.slopcoder`: `agent`, `web_search`, `http_port`, `secrets`, `setup_workflow`.

### 10.10 Launch Workflows

Implemented in `crates/slopcoder-core/src/workflow.rs`.

- Parses `.slopcoder/setup.yml` (GitHub Actions-style subset).
- `run` steps → shell commands.
- Curated `uses` steps: `actions/setup-node`, `actions/setup-python`, `actions-rust-lang/setup-rust-toolchain`.
- Unsupported `uses` → clear error.

### 10.11 E2E Testing

- `e2e/run.sh`: full lifecycle test driven by curl against docker-compose.
- `docker-compose.yml`: coordinator + agent + code-server for local dev/demo.
- `docker-compose.ci.yml`: CI overrides using pre-built images.
- CI pipeline: test → build-images → e2e → push-images (4 stages on `ripley-cloud`).
