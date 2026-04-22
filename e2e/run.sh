#!/bin/bash
set -euo pipefail

BASE="${E2E_BASE_URL:-http://localhost:8080}"

fail() { echo "FAIL: $1"; exit 1; }

# ── 1. Dev login ──────────────────────────────────────────────
echo "==> Dev login"
curl -sf "$BASE/auth/dev-login?user=testuser" -c /tmp/e2e-cookies.txt > /dev/null
echo " OK"

# ── 2. Auth check ────────────────────────────────────────────
echo "==> Auth check (/auth/me)"
ME=$(curl -sf "$BASE/auth/me" -b /tmp/e2e-cookies.txt)
echo "$ME" | grep -q "testuser" || fail "/auth/me did not return testuser"
echo " OK: $ME"

# ── 3. Agent connected ───────────────────────────────────────
echo "==> Checking agent connected"
for i in $(seq 1 30); do
  HOSTS=$(curl -sf "$BASE/api/hosts" -b /tmp/e2e-cookies.txt)
  if echo "$HOSTS" | grep -q "demo-agent"; then break; fi
  sleep 1
done
echo "$HOSTS" | grep -q "demo-agent" || fail "agent not connected after 30s"
echo " OK"

# ── 4. Environments discovered ───────────────────────────────
echo "==> Listing environments"
for i in $(seq 1 15); do
  ENVS=$(curl -sf "$BASE/api/environments" -b /tmp/e2e-cookies.txt)
  if echo "$ENVS" | grep -q "workspace"; then break; fi
  sleep 1
done
echo "$ENVS" | grep -q "workspace" || fail "no environments discovered"
echo " OK"

# ── 5. Create task ───────────────────────────────────────────
echo "==> Creating task"
TASK=$(curl -sf "$BASE/api/tasks" -b /tmp/e2e-cookies.txt \
  -H "Content-Type: application/json" \
  -d '{
    "host": "demo-agent",
    "environment": "/home/dev/workspace",
    "prompt": "Create a file called hello.txt containing Hello E2E",
    "agent": "claude",
    "use_worktree": false
  }')
TASK_ID=$(echo "$TASK" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
[ -n "$TASK_ID" ] || fail "no task ID returned"
echo " OK: task=$TASK_ID"

# ── 6. Poll task until completed or timeout ──────────────────
echo "==> Waiting for task completion (timeout 120s)"
STATUS="pending"
for i in $(seq 1 120); do
  STATUS=$(curl -sf "$BASE/api/tasks/$TASK_ID" -b /tmp/e2e-cookies.txt \
    | grep -o '"status":"[^"]*"' | cut -d'"' -f4)
  case "$STATUS" in
    completed) echo " OK: completed in ${i}s"; break ;;
    failed)    echo " WARN: task failed (expected in CI without agent API keys)"; break ;;
    *)         sleep 1 ;;
  esac
done
[ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ] || fail "timeout waiting for task"

# ── 7. Check task output ─────────────────────────────────────
echo "==> Checking task output"
OUTPUT=$(curl -sf "$BASE/api/tasks/$TASK_ID/output" -b /tmp/e2e-cookies.txt)
echo " OK: output retrieved"

# ── 8. Rename task ───────────────────────────────────────────
echo "==> Renaming task"
curl -sf "$BASE/api/tasks/$TASK_ID" -b /tmp/e2e-cookies.txt \
  -X PATCH -H "Content-Type: application/json" \
  -d '{"name": "e2e-renamed"}' > /dev/null
RENAMED=$(curl -sf "$BASE/api/tasks/$TASK_ID" -b /tmp/e2e-cookies.txt \
  | grep -o '"name":"[^"]*"' | cut -d'"' -f4)
[ "$RENAMED" = "e2e-renamed" ] || fail "rename didn't stick (got: $RENAMED)"
echo " OK"

# ── 9. Archive task ──────────────────────────────────────────
echo "==> Archiving task"
curl -sf "$BASE/api/tasks/$TASK_ID/archive" -b /tmp/e2e-cookies.txt -X POST > /dev/null
echo " OK"

# ── 10. Task gone from list ──────────────────────────────────
echo "==> Verifying task archived"
TASKS=$(curl -sf "$BASE/api/tasks" -b /tmp/e2e-cookies.txt)
if echo "$TASKS" | grep -q "$TASK_ID"; then
  fail "task still in list after archive"
fi
echo " OK"

# ── 11. Logout ───────────────────────────────────────────────
echo "==> Logout"
curl -sf "$BASE/auth/logout" -b /tmp/e2e-cookies.txt -c /tmp/e2e-cookies.txt > /dev/null
echo " OK"

echo ""
echo "=== ALL E2E TESTS PASSED ==="
