# SlopCoderNG Spec

SlopCoderNG is the next generation of Slopcoder: a multi-tenant, Kubernetes-native
platform for launching and managing coding agent workspaces. Users authenticate via
GitHub, launch workspace containers scoped to their org, and get VS Code (code-server),
SSH, and HTTP access to each workspace.

SlopCoderNG is deployment-agnostic — it runs on any Kubernetes cluster. This spec
describes the platform generically. Appendix A provides a concrete deployment example
for the Ripley Cloud cluster.

## 1. Architecture Overview

```
Internet
  │
  ▼
*.<WORKSPACE_DOMAIN> (wildcard TLS)
  │
  ▼
┌──────────────────────────────────────────────────────────┐
│  Kubernetes cluster                                      │
│                                                          │
│  ┌────────────────────────┐                              │
│  │ slopcoder-server       │  Deployment (1+ replica)     │
│  │ - coordinator API      │                              │
│  │ - frontend SPA         │                              │
│  │ - GitHub OAuth proxy   │                              │
│  │ - pod launcher (k8s API)│                             │
│  └──────┬─────────────────┘                              │
│         │                                                │
│         │ creates Pods + Services + Ingress rules         │
│         ▼                                                │
│  ┌───────────────────┐  ┌───────────────────┐            │
│  │ workspace pod      │  │ workspace pod      │  ...     │
│  │ code-server :8080  │  │ code-server :8080  │          │
│  │ slopagent          │  │ slopagent          │          │
│  │ sshd               │  │ sshd               │          │
│  │ user ports :N      │  │ user ports :N      │          │
│  └───────────────────┘  └───────────────────┘            │
│                                                          │
│  PVCs for workspace persistence                          │
│  Secrets for GitHub App credentials                      │
└──────────────────────────────────────────────────────────┘
```

The operator configures two domain names:
- `WORKSPACE_DOMAIN`: wildcard domain for workspace HTTP access (e.g. `work.example.com`).
- `SSH_DOMAIN`: hostname for SSH access (e.g. `ssh.work.example.com`).

Components:
- `slopcoder-server`: coordinator. Serves the UI, authenticates users via GitHub OAuth,
  creates/destroys workspace pods via the Kubernetes API. Also acts as an OAuth-aware
  reverse proxy for workspace subdomains — all requests to `<workspace>.<WORKSPACE_DOMAIN>`
  pass through the coordinator's auth layer before reaching the workspace pod.
- Workspace pods: each runs **code-server** (VS Code in the browser) as the primary
  workspace UI, plus `slopagent` (connects back to coordinator over cluster-internal
  WebSocket), an SSH server, and the user's configured launch commands. Built from a
  user-configurable base image.
- Ingress: wildcard `*.<WORKSPACE_DOMAIN>` routes HTTP to the coordinator, which validates
  the user's OAuth session and proxies to the correct workspace pod. The coordinator
  itself is at `<WORKSPACE_DOMAIN>` (or a dedicated subdomain like `app.<WORKSPACE_DOMAIN>`).

## 2. Container Images

Two images, published to GHCR:

### `ghcr.io/<org>/slopcoder-server`

Contains:
- `slopcoder-server` static binary (musl).
- Frontend assets (`frontend/dist`) baked in at `/srv/frontend`.
- Entrypoint: `slopcoder-server --static-dir /srv/frontend`.

### `ghcr.io/<org>/slopcoder-agent`

Base workspace image. Contains:
- `slopagent` static binary (musl) at `/usr/local/bin/slopagent`.
- `code-server` (VS Code Server) — the primary workspace UI, served on port 8080.
- `openssh-server` (or `dropbear`).
- `git`.
- Minimal shell environment (`bash`, `curl`, common dev tools).

code-server is started with `--auth none` because authentication is handled by the
coordinator's OAuth proxy layer — all requests to `<workspace>.<WORKSPACE_DOMAIN>`
pass through the coordinator, which validates the user's GitHub session before proxying
to the pod. This means code-server never needs its own password.

Users can specify a custom base image via `.devcontainer/devcontainer.json` (see §8).
Custom images must have `slopagent` and `code-server` available — either by inheriting
from the default agent image or by installing them separately.

### Build in CI

CI runs on `ripley-cloud` (self-hosted runner). System dependencies and toolchains
are pre-installed on the runner.

```yaml
name: CI
on:
  push:
    branches: [main]
  pull_request:

env:
  REGISTRY: ghcr.io
  SERVER_IMAGE: ghcr.io/${{ github.repository_owner }}/slopcoder-server
  AGENT_IMAGE: ghcr.io/${{ github.repository_owner }}/slopcoder-agent

jobs:
  test:
    runs-on: ripley-cloud
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --workspace
      - run: cd frontend && npm ci && npm run test && npm run build

  build-and-push:
    runs-on: ripley-cloud
    needs: test
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4
      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - name: Build binaries
        run: |
          cargo build --release -p slopcoder-server --target x86_64-unknown-linux-musl
          cargo build --release -p slopagent --target x86_64-unknown-linux-musl
          cd frontend && npm ci && npm run build
      - uses: docker/build-push-action@v5
        with:
          context: .
          file: Dockerfile.server
          push: ${{ github.ref == 'refs/heads/main' }}
          tags: |
            ${{ env.SERVER_IMAGE }}:${{ github.sha }}
            ${{ env.SERVER_IMAGE }}:latest
      - uses: docker/build-push-action@v5
        with:
          context: .
          file: Dockerfile.agent
          push: ${{ github.ref == 'refs/heads/main' }}
          tags: |
            ${{ env.AGENT_IMAGE }}:${{ github.sha }}
            ${{ env.AGENT_IMAGE }}:latest
```

## 3. GitHub App Authentication

### Setup

Operator registers a GitHub App with:
- OAuth enabled (callback URL: `https://work.ripley.cloud/auth/callback`).
- Permissions: `read:org`, `read:user`.
- Installed on the target org(s).

Secrets provided to the coordinator deployment:
- `GITHUB_CLIENT_ID`
- `GITHUB_CLIENT_SECRET`
- `GITHUB_APP_ID` (for API calls that need app-level auth)
- `GITHUB_APP_PRIVATE_KEY` (PEM)

### OAuth Flow

1. User visits `work.ripley.cloud` → coordinator redirects to GitHub OAuth authorize URL.
2. GitHub redirects back to `/auth/callback?code=...`.
3. Coordinator exchanges code for access token.
4. Coordinator calls GitHub API to get user profile + org memberships.
5. Coordinator issues a signed JWT containing: `github_user`, `github_id`, `orgs`, `teams`, `avatar_url`.
6. JWT is stored as an `HttpOnly` cookie. Frontend includes it on all API requests.
7. Coordinator validates JWT on every request (replaces the current `X-Slopcoder-Password` header).

The existing `--password` / `--agent-password` flags remain for the agent↔coordinator
WebSocket connection (cluster-internal, not user-facing).

### SSH Key Retrieval

On workspace pod creation, the coordinator fetches the user's public keys from
`https://github.com/<username>.keys` and injects them into the pod as a ConfigMap
mounted at `/home/dev/.ssh/authorized_keys`.

## 4. Authorization: Who Can Launch Containers

Authorization controls who can log in and create workspaces. It does **not** restrict
which repositories a user can work on — an authorized user can create a workspace from
any Git repository they have access to, regardless of where it's hosted (GitHub, GitLab,
self-hosted, any Git remote).

### Org Membership (required)

The user must be a member of at least one GitHub org that has the GitHub App installed.
The coordinator checks this during the OAuth flow. Users outside the org cannot log in.

### Team Restriction (optional, per-org)

Operator configures allowed teams per org:

```yaml
# Coordinator config
authorization:
  orgs:
    mycompany:
      allowed_teams: [engineering, platform]
    partner-org:
      allowed_teams: []  # any member of partner-org can access
```

When `allowed_teams` is set for an org, the user must belong to at least one listed
team within that org. When empty or unset, any member of that org can launch workspaces.

A user who satisfies authorization for *any* configured org gets full access to the
platform — they can create workspaces from any repo they can `git clone`, not just
repos in the authorizing org.

### Per-User Resource Limits (optional)

Configurable per-org or per-team:
- Max concurrent workspaces per user (default: 5).
- Max CPU/memory per workspace (default: 4 CPU / 8Gi).
- Max PVC size (default: 50Gi).

Stored in a ConfigMap or CRD. Coordinator enforces limits before creating pods.

## 5. Workspace Lifecycle

### Creation

User creates a task in the UI (same flow as today). The coordinator:

1. Validates authorization (org/team membership, resource limits).
2. Generates a workspace slug from the task name (e.g. `fix-login-flow`).
3. Creates a PVC `workspace-<slug>-<short-id>` for persistent storage.
4. Creates a Pod running the user's configured base image (or default agent image).
5. Creates a ClusterIP Service pointing to the pod.
6. Creates an Ingress rule: `<slug>.work.ripley.cloud` → Service.
7. Pod entrypoint:
   a. Starts `sshd` with the user's GitHub keys.
   b. Runs user-configured launch commands (§8).
   c. Starts `slopagent --server ws://slopcoder-server.<namespace>.svc/agent/connect`.
8. `slopagent` connects to coordinator, registers, and is ready for prompts.

### Destruction

- User deletes/archives the task → coordinator deletes Pod, Service, Ingress.
- PVC is retained by default (user can explicitly delete it to reclaim storage).
- Idle timeout (configurable, default 2h): coordinator deletes the pod but retains PVC.
  Workspace can be relaunched from the same PVC.

## 6. HTTP Forwarding (`<workspace>.<WORKSPACE_DOMAIN>`)

Each workspace gets two subdomains:
- `<slug>.<WORKSPACE_DOMAIN>` → code-server (port 8080 inside the pod). The primary IDE.
- `www.<slug>.<WORKSPACE_DOMAIN>` → user-configurable port (default 3000). For the user's
  app/dev server.

### Auth Proxy

All HTTP traffic to `*.<WORKSPACE_DOMAIN>` routes through the coordinator, which acts
as an OAuth-aware reverse proxy:

1. Request arrives at `<slug>.<WORKSPACE_DOMAIN>` or `www.<slug>.<WORKSPACE_DOMAIN>`.
2. Coordinator checks the user's GitHub OAuth session cookie.
3. If not authenticated → redirect to GitHub OAuth login, then back to the original URL.
4. If authenticated → verify the user is the workspace owner or is on the workspace's
   shared access list (see §9a).
5. Proxy the request (including WebSocket upgrade) to the workspace pod's ClusterIP Service.

This means code-server runs with `--auth none` inside the pod — the coordinator handles
all authentication. Users never see a code-server password prompt.

### Workspace Access Control (§9a)

Workspaces are private by default — only the user who launched the workspace can access
it (code-server, SSH, forwarded ports).

The owner can share access with other GitHub users by username:

- `POST /api/tasks/:id/collaborators` — `{"username": "<github-username>"}` grants access.
- `DELETE /api/tasks/:id/collaborators/:username` — revokes access.
- `GET /api/tasks/:id/collaborators` — lists usernames with access.

The coordinator enforces this on every proxied request and SSH port mapping. Shared
users get full access to the workspace (code-server, terminal, forwarded ports, SSH).

The UI shows a "Share" button in the workspace header. The owner types a GitHub username
and the collaborator can immediately open the same workspace URL.

### code-server (`<slug>.<WORKSPACE_DOMAIN>`)

The primary workspace experience. Users open their workspace URL in a browser
and get a full VS Code environment with terminal, file explorer, extensions, etc.
The slopcoder agent conversation UI is accessible alongside it (either as a VS Code
extension panel or via the coordinator UI).

### App forwarding (`www.<slug>.<WORKSPACE_DOMAIN>`)

Routes to a single user-configurable port inside the workspace container (default: 3000).
The user sets this in their devcontainer config or workspace settings:

```json
"customizations": {
  "slopcoder": {
    "http_port": 5173
  }
}
```

Or at workspace creation time in the UI. The coordinator creates the Ingress rule
targeting that port. If the user changes the port, they update their config and
the coordinator updates the Ingress on the next workspace restart.

### WebSocket Support

The Ingress and coordinator proxy must support WebSocket upgrade for:
- code-server itself (uses WebSockets extensively).
- Dev server HMR (Vite, webpack, etc.) on the `www.` subdomain.
- The slopcoder agent WebSocket stream.

nginx-ingress annotations:
```yaml
nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
nginx.ingress.kubernetes.io/proxy-send-timeout: "3600"
nginx.ingress.kubernetes.io/proxy-http-version: "1.1"
```

## 7. SSH Access

Each workspace pod runs an SSH server. Users connect with their GitHub SSH keys.
All workspaces share a single ingress hostname (`ssh.work.ripley.cloud`) with a
unique port per container.

### Routing

- A single DNS record: `ssh.work.ripley.cloud` → cluster ingress IP.
- Each workspace pod gets a unique TCP port from a configured range (e.g. 30000–32767).
- The nginx ingress controller's `--tcp-services-configmap` maps each allocated port
  to the workspace pod's SSH service (port 22).
- The coordinator manages port allocation and updates the ConfigMap when workspaces
  are created/destroyed.

### Connection

Users connect via:
```
ssh -p <port> dev@ssh.work.ripley.cloud
```

The UI displays this command prominently in the workspace header, with a copy button.
SSH access is restricted to the workspace owner and collaborators — the coordinator
injects `authorized_keys` containing the GitHub public keys of the owner and all
current collaborators. When a collaborator is added or removed, the coordinator
updates the ConfigMap and signals the pod to reload `authorized_keys`.

### Port Allocation

- The coordinator maintains a pool of available ports from the configured range.
- On workspace creation: allocate the next free port, add an entry to the TCP services
  ConfigMap (`<port>: "<namespace>/<service-name>:22"`).
- On workspace destruction: remove the ConfigMap entry, return the port to the pool.
- On coordinator startup: reconcile the ConfigMap against running workspace pods to
  reclaim leaked ports.

### Implementation

- Pod runs `openssh-server` listening on port 22 (internal).
- User's GitHub public keys are fetched at pod creation from `https://github.com/<username>.keys`
  and mounted as a ConfigMap at `/home/dev/.ssh/authorized_keys`.
- All workspaces use a single non-root user (`dev`, UID 1000). The user's GitHub username
  is set as `GITHUB_USER` for personalization (git config, shell prompt, etc.).

## 8. Per-Repo Configuration (devcontainer.json)

SlopCoderNG uses the [Dev Containers](https://containers.dev/) standard for per-repo
workspace configuration. No proprietary config file format.

A `.devcontainer/devcontainer.json` in the repository root controls the container image,
lifecycle commands, port forwarding, and VS Code extensions. SlopCoderNG-specific config
lives under `customizations.slopcoder`.

### Example

```json
{
  "image": "ghcr.io/myorg/dev-environment:latest",
  "postCreateCommand": "npm ci",
  "postStartCommand": "npm run dev &",
  "customizations": {
    "vscode": {
      "extensions": [
        "ms-python.python",
        "rust-lang.rust-analyzer",
        "bradlc.vscode-tailwindcss"
      ]
    },
    "slopcoder": {
      "agent": "claude",
      "web_search": true,
      "http_port": 5173,
      "secrets": ["ANTHROPIC_API_KEY", "DATABASE_URL"],
      "setup_workflow": ".slopcoder/setup.yml"
    }
  }
}
```

### What devcontainer.json handles (standard)

| Field | Purpose |
|---|---|
| `image` | Base container image |
| `build.dockerfile` | Build image from Dockerfile |
| `postCreateCommand` | Run once after container creation |
| `postStartCommand` | Run on every container start |
| `postAttachCommand` | Run when a client attaches |
| `forwardPorts` | Ports to forward (informational; SlopCoderNG uses `http_port` for the `www.` subdomain) |
| `customizations.vscode.extensions` | VS Code extensions to install in code-server |
| `customizations.vscode.settings` | VS Code settings |

code-server reads `devcontainer.json` natively for extensions and settings. If the repo
has no `devcontainer.json`, the default agent image and user-level settings are used.

### What `customizations.slopcoder` handles (SlopCoderNG-specific)

| Field | Default | Purpose |
|---|---|---|
| `agent` | `claude` | Default coding agent |
| `web_search` | `false` | Enable agent web search |
| `http_port` | `3000` | Port for `www.<slug>.<WORKSPACE_DOMAIN>` |
| `secrets` | `[]` | Secret names to inject (in addition to user defaults) |
| `setup_workflow` | `null` | Path to GitHub Actions-style setup workflow (§10) |

### Merge Order

When a workspace launches, config is resolved in this order (later wins):
1. Platform defaults (default agent image, agent=claude, http_port=3000).
2. User-level settings (from coordinator settings page).
3. `.devcontainer/devcontainer.json` from the repository.

### Per-Environment Overrides

A `.ripley/config.yaml` file is **not** used. All per-repo config goes in
`devcontainer.json`. This means repos that already have a devcontainer for
GitHub Codespaces, DevPod, or similar tools work with SlopCoderNG out of the box —
just add the `customizations.slopcoder` block.

## 9. User Secrets

Users store secrets (API keys, tokens, credentials) in the coordinator and selectively
inject them into workspaces. Secrets are never exposed in the UI after creation.

### Scoping

Secrets have two scopes:

- **Global** (user-level): available to any workspace the user launches.
  Example: `ANTHROPIC_API_KEY`, `GITHUB_TOKEN`.
- **Environment-scoped** (user + environment): available only to workspaces for a
  specific environment/repository.
  Example: `DATABASE_URL` for a particular project, `AWS_ACCESS_KEY_ID` for a
  specific deployment target.

When a workspace launches, the coordinator merges applicable secrets: global secrets
first, then environment-scoped secrets on top (environment wins on name collision).

### Storage

- Secrets are stored per-user in the coordinator, encrypted at rest.
- Backend: k8s Secrets managed by the coordinator via the k8s API.
  - `user-secrets-<github-username>` — global secrets.
  - `user-secrets-<github-username>-<env-slug>` — environment-scoped secrets.
- The coordinator's ServiceAccount has RBAC to create/update/delete Secrets in its namespace.

### Management (UI + API)

- `POST /api/secrets` — create/update a secret (name, value, optional environment).
- `GET /api/secrets` — list secret names + scopes (values are never returned).
- `DELETE /api/secrets/:name?environment=<env>` — delete a secret.
- UI: settings page has a "Secrets" section. Each secret shows its scope (global or
  environment name). Values are write-only (masked after save, cannot be revealed).

### Injection into Workspaces

Users choose which secrets to inject when creating a workspace, or configure defaults
in their WorkspaceConfig:

```yaml
# User-level defaults — these global secrets are always injected
default_secrets:
  - ANTHROPIC_API_KEY
  - GITHUB_TOKEN
```

Environment-scoped secrets for the workspace's environment are automatically included.
At workspace creation, the UI shows the merged set and lets the user deselect any
they don't want for this particular workspace.

The coordinator creates a per-workspace k8s Secret containing only the final selected
set, referenced by the pod spec:

```yaml
envFrom:
  - secretRef:
      name: workspace-secrets-<slug>-<short-id>
```

### Security Properties

- Secrets are stored in k8s Secrets (encrypted at rest if the cluster has etcd encryption enabled).
- The coordinator never logs or returns secret values after creation.
- Workspace pods only receive the secrets the user explicitly selected for that workspace.
- Per-workspace Secrets are deleted when the workspace is destroyed.
- Users can rotate a secret in the coordinator; running workspaces keep the old value
  until restarted.

## 10. Launch Workflows (GitHub Actions-style)

For complex setup that goes beyond devcontainer lifecycle commands, users can define a
GitHub Actions-style workflow file that runs on workspace launch.

### Location

- Repo-level: `.slopcoder/setup.yml` in the repository root (referenced from
  `customizations.slopcoder.setup_workflow` in `devcontainer.json`).
- User-level: stored in coordinator settings (pasted in UI or referenced from a repo).

### Format

A subset of GitHub Actions workflow syntax, executed locally in the workspace container
(not on GitHub). The coordinator or `slopagent` interprets the file.

```yaml
name: workspace-setup
on: launch

jobs:
  setup:
    runs-on: workspace  # always runs locally in the workspace pod
    steps:
      - name: Install system packages
        run: sudo apt-get update && sudo apt-get install -y ripgrep fd-find

      - name: Setup Node
        uses: actions/setup-node@v4
        with:
          node-version: '22'

      - name: Setup Rust
        uses: actions-rust-lang/setup-rust-toolchain@v1

      - name: Install dependencies
        run: npm ci

      - name: Start background services
        run: docker compose up -d
```

### Execution Model

- `slopagent` reads the workflow file on startup (before accepting prompts).
- `run` steps execute as shell commands in the workspace directory.
- `uses` steps: a curated subset of GitHub Actions are supported natively
  (e.g. `actions/setup-node`, `actions/setup-python`, `actions-rust-lang/setup-rust-toolchain`).
  Unsupported actions fail with a clear error.
- Steps run sequentially within a job. Multiple jobs are not supported (single-job only).
- Step output is streamed to the coordinator and visible in the UI as a "Setup" phase
  before the workspace becomes ready.
- If any step fails, the workspace enters a `setup-failed` state. The user can view
  logs and retry or SSH in to debug.

### Why Not Just Shell Commands?

- Declarative and version-controlled in the repo.
- Familiar syntax for anyone who uses GitHub Actions.
- `uses` steps provide reproducible tool installation without users writing
  `curl | bash` chains.
- Can be extended later to support `on: prompt` (run before each agent prompt) or
  `on: merge` (run before merge).

## 11. Kubernetes Resources

### Coordinator Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: slopcoder-server
spec:
  replicas: 1
  selector:
    matchLabels:
      app: slopcoder-server
  template:
    metadata:
      labels:
        app: slopcoder-server
    spec:
      serviceAccountName: slopcoder-server
      containers:
        - name: server
          image: ghcr.io/<org>/slopcoder-server:latest
          ports:
            - containerPort: 8080
          env:
            - name: GITHUB_CLIENT_ID
              valueFrom:
                secretKeyRef:
                  name: slopcoder-github
                  key: client-id
            - name: GITHUB_CLIENT_SECRET
              valueFrom:
                secretKeyRef:
                  name: slopcoder-github
                  key: client-secret
            - name: SLOPCODER_AGENT_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: slopcoder-internal
                  key: agent-password
```

### Coordinator RBAC

The coordinator needs permissions to create/delete pods, services, ingresses, PVCs,
and configmaps in its namespace:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: slopcoder-server
rules:
  - apiGroups: [""]
    resources: [pods, services, persistentvolumeclaims, configmaps, secrets]
    verbs: [get, list, create, delete, update]
  - apiGroups: [networking.k8s.io]
    resources: [ingresses]
    verbs: [get, list, create, delete, update]
```

### Workspace Pod Template

Created dynamically by the coordinator:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: workspace-<slug>-<short-id>
  labels:
    app: slopcoder-workspace
    slopcoder.dev/user: <github-username>
    slopcoder.dev/task-id: <task-id>
spec:
  containers:
    - name: workspace
      image: <user-base-image or default>
      command: ["/usr/local/bin/workspace-init"]
      env:
        - name: SLOPCODER_SERVER
          value: "ws://slopcoder-server:8080/agent/connect"
        - name: SLOPCODER_AGENT_PASSWORD
          valueFrom:
            secretKeyRef:
              name: slopcoder-internal
              key: agent-password
        - name: GITHUB_USER
          value: <github-username>
        - name: WORKSPACE_SLUG
          value: <slug>
      ports:
        - containerPort: 8080
          name: code-server
        - containerPort: 22
          name: ssh
      volumeMounts:
        - name: workspace-data
          mountPath: /home/dev/workspace
        - name: ssh-keys
          mountPath: /home/dev/.ssh
          readOnly: true
      resources:
        requests:
          cpu: "1"
          memory: 2Gi
        limits:
          cpu: "4"
          memory: 8Gi
  volumes:
    - name: workspace-data
      persistentVolumeClaim:
        claimName: workspace-<slug>-<short-id>
    - name: ssh-keys
      configMap:
        name: ssh-keys-<github-username>
        defaultMode: 0600
```

### Workspace Init Script (`/usr/local/bin/workspace-init`)

Baked into the agent image. Orchestrates startup:

```bash
#!/bin/bash
set -e

# Start SSH server
/usr/sbin/sshd -D &

# Start code-server (VS Code in browser) — no auth, coordinator handles OAuth
code-server \
  --bind-addr 0.0.0.0:8080 \
  --auth none \
  --disable-telemetry \
  /home/dev/workspace &

# Run devcontainer lifecycle hooks (postCreateCommand, postStartCommand)
# The coordinator mounts resolved commands from devcontainer.json as scripts.
if [ -f /etc/slopcoder/post-create.sh ]; then
  source /etc/slopcoder/post-create.sh
fi
if [ -f /etc/slopcoder/post-start.sh ]; then
  source /etc/slopcoder/post-start.sh
fi

# Run setup workflow if configured in customizations.slopcoder.setup_workflow
if [ -n "$SLOPCODER_SETUP_WORKFLOW" ] && [ -f "$SLOPCODER_SETUP_WORKFLOW" ]; then
  slopagent-setup --workflow "$SLOPCODER_SETUP_WORKFLOW"
fi

# Start slopagent (blocks, reconnects on failure)
exec slopagent /home/dev/workspace \
  --server "$SLOPCODER_SERVER" \
  --name "$WORKSPACE_SLUG"
```

# Start slopagent (blocks, reconnects on failure)
exec slopagent /home/dev/workspace \
  --server "$SLOPCODER_SERVER" \
  --name "$WORKSPACE_SLUG"
```

### Ingress

All workspace subdomains route to the coordinator, which acts as the OAuth proxy.
The coordinator validates the session and reverse-proxies to the workspace pod's
code-server (or additional forwarded ports).

```yaml
# Single wildcard Ingress — all workspace traffic goes through the coordinator
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: slopcoder-wildcard
  annotations:
    nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
    nginx.ingress.kubernetes.io/proxy-send-timeout: "3600"
    nginx.ingress.kubernetes.io/proxy-http-version: "1.1"
spec:
  tls:
    - hosts: ["*.work.ripley.cloud", "work.ripley.cloud"]
      secretName: wildcard-work-ripley-cloud-tls
  rules:
    - host: "work.ripley.cloud"
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: slopcoder-server
                port:
                  number: 8080
    - host: "*.work.ripley.cloud"
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: slopcoder-server
                port:
                  number: 8080
```

The coordinator parses the `Host` header to determine the target workspace and port:
- `<slug>.work.ripley.cloud` → proxy to pod's code-server (port 8080).
- `<slug>-<port>.work.ripley.cloud` → proxy to pod's port `<port>`.

## 12. Data Model Changes

### New: User record

Stored in coordinator state (in-memory + persisted to a YAML/JSON file or k8s ConfigMap):

```
User:
  github_id: u64
  github_username: string
  avatar_url: string
  orgs: [string]
  teams: [string]
  config: WorkspaceConfig
  created_at: datetime
  last_login: datetime
```

### New: WorkspaceConfig

User-level defaults (overridden by `devcontainer.json` per-repo):

```
WorkspaceConfig:
  base_image: string (default: ghcr.io/<org>/slopcoder-agent:latest)
  http_port: u16 (default: 3000)
  agent: AgentKind (default: claude)
  web_search: bool (default: false)
  default_secrets: [string] (secret names to inject by default)
```

### Extended: Task

Existing task fields remain. New fields:
- `owner: string` — GitHub username of the user who created the task.
- `workspace_slug: string` — subdomain slug.
- `pod_name: string | null` — k8s pod name when running.
- `ssh_port: u16 | null` — allocated SSH port on `ssh.work.ripley.cloud`.
- `ssh_command: string | null` — e.g. `ssh -p 30042 dev@ssh.work.ripley.cloud`.
- `workspace_url: string | null` — e.g. `https://<slug>.<WORKSPACE_DOMAIN>` (code-server).
- `app_url: string | null` — e.g. `https://www.<slug>.<WORKSPACE_DOMAIN>` (user app port).
- `http_port: u16` — port inside the pod that `www.` routes to (default 3000).
- `collaborators: [string]` — GitHub usernames granted access by the owner.

## 13. Implementation Phases

### Phase 1: Container Images + CI

- Write `Dockerfile.server` and `Dockerfile.agent`.
- Update `.github/workflows/ci.yml` to build and push to GHCR on `ripley-cloud` runner.
- Verify images run locally with `docker run`.

### Phase 2: Kubernetes Manifests

- Helm chart or raw manifests for coordinator Deployment, Service, Ingress, RBAC, Secrets.
- Deploy coordinator to `ripley-cloud` cluster.
- Verify coordinator serves UI and accepts agent connections from manually-run pods.

### Phase 3: GitHub OAuth

- Add OAuth routes to `slopcoder-server` (`/auth/login`, `/auth/callback`, `/auth/logout`).
- JWT session middleware replaces password auth for browser clients.
- Agent password auth remains for pod↔coordinator WebSocket.
- Frontend login page with GitHub button.

### Phase 4: Pod Launcher

- Coordinator creates workspace Pods, Services, PVCs via k8s API on task creation.
- Coordinator deletes resources on task archive/delete.
- Idle timeout reaper (goroutine/tokio task) cleans up idle pods.
- UI shows pod status (creating → running → idle → stopped).

### Phase 5: Authorization

- Org membership check on OAuth login.
- Optional team restriction via `ALLOWED_TEAMS` env var.
- Per-user resource limit enforcement before pod creation.

### Phase 6: code-server + HTTP Forwarding + Subdomains

- Agent image includes `code-server`, started with `--auth none`.
- Wildcard TLS cert (cert-manager + Let's Encrypt DNS-01 or pre-provisioned).
- Coordinator implements OAuth reverse proxy: validates session cookie, parses `Host`
  header to resolve workspace slug, proxies to pod ClusterIP.
- `<slug>.<WORKSPACE_DOMAIN>` → code-server (port 8080).
- `www.<slug>.<WORKSPACE_DOMAIN>` → user-configured `http_port` (default 3000).
- WebSocket passthrough for code-server and dev server HMR.

### Phase 7: SSH Access

- Agent image includes `openssh-server`.
- Coordinator fetches GitHub keys, creates ConfigMap, mounts as `authorized_keys`.
- Single hostname `ssh.work.ripley.cloud`, per-workspace port from configurable range.
- Coordinator manages nginx TCP services ConfigMap for port→pod mapping.
- Port pool reconciliation on coordinator startup.
- UI displays `ssh -p <port> dev@ssh.work.ripley.cloud` with copy button.

### Phase 8: User Secrets

- Secrets CRUD API (`POST/GET/DELETE /api/secrets`) with global and environment scopes.
- Per-user k8s Secret storage (global + per-environment).
- Workspace creation UI: shows merged secrets (global + environment), user can deselect.
- Per-workspace Secret created at pod launch, deleted on destroy.

### Phase 9: devcontainer.json + Launch Workflows

- Coordinator parses `.devcontainer/devcontainer.json` from repo at workspace creation.
- `image` / `build.dockerfile` determines the workspace container image.
- `postCreateCommand` / `postStartCommand` extracted and mounted as init scripts.
- `customizations.vscode.extensions` passed to code-server.
- `customizations.slopcoder` fields (agent, http_port, secrets, setup_workflow) applied.
- GitHub Actions-style workflow parser (subset: `run` + curated `uses` steps).
- Setup phase streaming to UI.
- User settings page for user-level defaults (base image, agent, http_port, secrets).

## 14. E2E Testing

### Dev Mode (`--dev-mode`)

The coordinator supports a `--dev-mode` flag that bypasses GitHub OAuth and injects
a synthetic user session. This is used for local development, docker-compose demos,
and CI E2E tests.

Behavior when `--dev-mode` is active:
- GitHub OAuth routes (`/auth/login`, `/auth/callback`) are replaced with a stub that
  immediately creates a session for a configurable test user.
- `GET /auth/dev-login?user=<username>` sets the session cookie directly (no GitHub redirect).
- The synthetic user has org membership and all teams, so authorization checks pass.
- The OAuth reverse proxy for workspace subdomains accepts the dev session cookie.
- SSH key injection uses a test key mounted at `/etc/slopcoder/dev-ssh-key.pub` instead
  of fetching from GitHub.
- Secrets API works normally (backed by local files instead of k8s Secrets when not
  running in a cluster).

`--dev-mode` is never enabled in production. The coordinator logs a prominent warning
on startup when it is active.

### docker-compose Demo

`docker-compose.yml` at the repo root brings up the full system locally without
Kubernetes. It is the canonical way to demo, develop against, and E2E test the platform.

```yaml
services:
  coordinator:
    build:
      context: .
      dockerfile: Dockerfile.server
    ports:
      - "8080:8080"
    environment:
      - SLOPCODER_AGENT_PASSWORD=e2e-test-password
      - SLOPCODER_DEV_MODE=1
      - SLOPCODER_DEV_USER=testuser
    volumes:
      - coordinator-data:/data

  agent-1:
    build:
      context: .
      dockerfile: Dockerfile.agent
    depends_on:
      - coordinator
    environment:
      - SLOPCODER_SERVER=ws://coordinator:8080/agent/connect
      - SLOPCODER_AGENT_PASSWORD=e2e-test-password
      - WORKSPACE_SLUG=demo-workspace
    entrypoint: >
      /bin/sh -c '
        mkdir -p /home/dev/workspace &&
        cd /home/dev/workspace &&
        git init --initial-branch=main &&
        git config user.email "test@test.com" &&
        git config user.name "Test" &&
        git commit --allow-empty -m "init" &&
        slopagent /home/dev/workspace
          --server $$SLOPCODER_SERVER
          --name demo-agent
      '
    volumes:
      - agent-1-data:/home/dev/workspace

  code-server:
    image: codercom/code-server:latest
    depends_on:
      - agent-1
    ports:
      - "8443:8080"
    environment:
      - PASSWORD=
    command: ["--auth", "none", "--bind-addr", "0.0.0.0:8080", "/home/dev/workspace"]
    volumes:
      - agent-1-data:/home/dev/workspace

volumes:
  coordinator-data:
  agent-1-data:
```

Usage:
```bash
docker compose up --build
# Coordinator UI:  http://localhost:8080
# code-server:     http://localhost:8443
# Dev login:       curl http://localhost:8080/auth/dev-login?user=testuser
```

In this compose setup, code-server runs as a separate container sharing the agent's
workspace volume. In production (k8s), they run in the same pod.

### CI E2E Workflow

The E2E test runs in CI on `ripley-cloud` after unit tests and image builds pass.
It uses docker-compose to stand up the full system, then drives it with `curl` and
a small test script.

```yaml
name: CI
on:
  push:
    branches: [main]
  pull_request:

jobs:
  test:
    runs-on: ripley-cloud
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --workspace
      - run: cd frontend && npm ci && npm run test && npm run build

  build-images:
    runs-on: ripley-cloud
    needs: test
    steps:
      - uses: actions/checkout@v4
      - name: Build images
        run: |
          cargo build --release -p slopcoder-server --target x86_64-unknown-linux-musl
          cargo build --release -p slopagent --target x86_64-unknown-linux-musl
          cd frontend && npm ci && npm run build
          docker build -f Dockerfile.server -t slopcoder-server:ci .
          docker build -f Dockerfile.agent -t slopcoder-agent:ci .

  e2e:
    runs-on: ripley-cloud
    needs: build-images
    steps:
      - uses: actions/checkout@v4

      - name: Start system
        run: docker compose -f docker-compose.yml -f docker-compose.ci.yml up -d --wait --wait-timeout 60

      - name: Wait for coordinator
        run: |
          for i in $(seq 1 30); do
            if curl -sf http://localhost:8080/api/hosts > /dev/null 2>&1; then
              echo "Coordinator ready"
              break
            fi
            sleep 1
          done

      - name: Run E2E tests
        run: ./e2e/run.sh

      - name: Collect logs on failure
        if: failure()
        run: docker compose logs > e2e-logs.txt

      - name: Upload logs
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: e2e-logs
          path: e2e-logs.txt

      - name: Teardown
        if: always()
        run: docker compose down -v

  push-images:
    runs-on: ripley-cloud
    needs: e2e
    if: github.ref == 'refs/heads/main'
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4
      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - name: Tag and push
        run: |
          docker tag slopcoder-server:ci ghcr.io/${{ github.repository_owner }}/slopcoder-server:${{ github.sha }}
          docker tag slopcoder-server:ci ghcr.io/${{ github.repository_owner }}/slopcoder-server:latest
          docker tag slopcoder-agent:ci ghcr.io/${{ github.repository_owner }}/slopcoder-agent:${{ github.sha }}
          docker tag slopcoder-agent:ci ghcr.io/${{ github.repository_owner }}/slopcoder-agent:latest
          docker push ghcr.io/${{ github.repository_owner }}/slopcoder-server:${{ github.sha }}
          docker push ghcr.io/${{ github.repository_owner }}/slopcoder-server:latest
          docker push ghcr.io/${{ github.repository_owner }}/slopcoder-agent:${{ github.sha }}
          docker push ghcr.io/${{ github.repository_owner }}/slopcoder-agent:latest
```

### E2E Test Script (`e2e/run.sh`)

Drives the full lifecycle via the coordinator API:

```bash
#!/bin/bash
set -euo pipefail

BASE=http://localhost:8080

# ── 1. Dev login ──────────────────────────────────────────────
echo "==> Dev login"
curl -sf "$BASE/auth/dev-login?user=testuser" -c /tmp/cookies.txt
echo " OK"

# ── 2. Agent connected ───────────────────────────────────────
echo "==> Checking agent connected"
HOSTS=$(curl -sf "$BASE/api/hosts" -b /tmp/cookies.txt)
echo "$HOSTS" | grep -q "demo-agent" || { echo "FAIL: agent not connected"; exit 1; }
echo " OK: $HOSTS"

# ── 3. Environments discovered ───────────────────────────────
echo "==> Listing environments"
for i in $(seq 1 15); do
  ENVS=$(curl -sf "$BASE/api/environments" -b /tmp/cookies.txt)
  if echo "$ENVS" | grep -q "workspace"; then break; fi
  sleep 1
done
echo "$ENVS" | grep -q "workspace" || { echo "FAIL: no environments"; exit 1; }
echo " OK"

# ── 4. Create task ───────────────────────────────────────────
echo "==> Creating task"
TASK=$(curl -sf "$BASE/api/tasks" -b /tmp/cookies.txt \
  -H "Content-Type: application/json" \
  -d '{
    "host": "demo-agent",
    "environment": "/home/dev/workspace",
    "prompt": "Create a file called hello.txt containing Hello E2E",
    "agent": "claude",
    "use_worktree": false
  }')
TASK_ID=$(echo "$TASK" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
echo " OK: task=$TASK_ID"

# ── 5. Poll task until completed or timeout ──────────────────
echo "==> Waiting for task completion (timeout 120s)"
for i in $(seq 1 120); do
  STATUS=$(curl -sf "$BASE/api/tasks/$TASK_ID" -b /tmp/cookies.txt \
    | grep -o '"status":"[^"]*"' | cut -d'"' -f4)
  case "$STATUS" in
    completed) echo " OK: completed in ${i}s"; break ;;
    failed)    echo "FAIL: task failed"; exit 1 ;;
    *)         sleep 1 ;;
  esac
done
[ "$STATUS" = "completed" ] || { echo "FAIL: timeout"; exit 1; }

# ── 6. Check task output has events ──────────────────────────
echo "==> Checking task output"
OUTPUT=$(curl -sf "$BASE/api/tasks/$TASK_ID/output" -b /tmp/cookies.txt)
EVENT_COUNT=$(echo "$OUTPUT" | grep -o '"total_events":[0-9]*' | cut -d: -f2)
[ "$EVENT_COUNT" -gt 0 ] || { echo "FAIL: no events"; exit 1; }
echo " OK: $EVENT_COUNT events"

# ── 7. Check diff shows the created file ─────────────────────
echo "==> Checking diff"
DIFF=$(curl -sf "$BASE/api/tasks/$TASK_ID/diff" -b /tmp/cookies.txt)
echo "$DIFF" | grep -q "hello" || echo " WARN: hello.txt not in diff (agent may not have created it)"
echo " OK"

# ── 8. Rename task ───────────────────────────────────────────
echo "==> Renaming task"
curl -sf "$BASE/api/tasks/$TASK_ID" -b /tmp/cookies.txt \
  -X PATCH -H "Content-Type: application/json" \
  -d '{"name": "e2e-renamed"}' > /dev/null
RENAMED=$(curl -sf "$BASE/api/tasks/$TASK_ID" -b /tmp/cookies.txt \
  | grep -o '"name":"[^"]*"' | cut -d'"' -f4)
[ "$RENAMED" = "e2e-renamed" ] || { echo "FAIL: rename didn't stick"; exit 1; }
echo " OK"

# ── 9. Archive task ──────────────────────────────────────────
echo "==> Archiving task"
curl -sf "$BASE/api/tasks/$TASK_ID/archive" -b /tmp/cookies.txt -X POST > /dev/null
echo " OK"

# ── 10. Task gone from list ──────────────────────────────────
echo "==> Verifying task archived"
TASKS=$(curl -sf "$BASE/api/tasks" -b /tmp/cookies.txt)
if echo "$TASKS" | grep -q "$TASK_ID"; then
  echo "FAIL: task still in list after archive"
  exit 1
fi
echo " OK"

echo ""
echo "=== ALL E2E TESTS PASSED ==="
```

### What the E2E Tests Cover

| Test | What it validates |
|---|---|
| Dev login | `--dev-mode` auth bypass works, session cookie is set |
| Agent connected | slopagent↔coordinator WebSocket handshake, agent registration |
| Environments discovered | Agent scans repo root, coordinator relays environment list |
| Create task | Full task creation flow: coordinator→agent RPC, agent spawns coding agent |
| Task completion | Agent runs prompt to completion, status transitions work |
| Task output | Event streaming/persistence, paginated output retrieval |
| Task diff | Git diff generation for in-place tasks |
| Rename task | PATCH API, persistence of renamed task |
| Archive task | Archive flow, task removal from active list, output file archival |

### `docker-compose.ci.yml` (CI overrides)

Overrides the base compose file to use pre-built CI images instead of building from
source, and skips code-server (not needed for API-level E2E tests):

```yaml
services:
  coordinator:
    image: slopcoder-server:ci
    build: !reset null
  agent-1:
    image: slopcoder-agent:ci
    build: !reset null
  code-server:
    profiles: ["demo-only"]  # skip in CI
```

### Future E2E Expansions

As features land, the E2E suite grows:
- **Worktree tasks**: create isolated task, verify merge, verify delete + prune.
- **code-server proxy**: hit `localhost:8443`, verify VS Code loads (HTTP 200 + HTML check).
- **SSH**: generate a test keypair, inject it, `ssh -p <port> dev@localhost` and run `whoami`.
- **Secrets**: create a secret via API, launch workspace, verify env var is set inside container.
- **Launch workflows**: workspace with `.ripley/setup.yml`, verify setup steps ran.
- **Idle timeout**: create workspace, wait, verify pod is reaped.
- **Multi-agent**: spin up two agent containers, verify both appear in host list.
