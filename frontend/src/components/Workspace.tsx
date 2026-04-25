import {
  Show,
  For,
  createMemo,
  createResource,
  createSignal,
  createEffect,
  onCleanup,
} from "solid-js";
import { useParams } from "@solidjs/router";
import {
  listHosts,
  listEnvironments,
  createEnvironment,
  listTasks,
  createTask,
  createWorkspace,
  getTask,
  getTaskOutput,
  sendPrompt,
  subscribeToTask,
  getTaskDiff,
  renameTask,
  mergeTask,
  getMergeStatus,
  archiveTask,
  deleteTask,
} from "../api/client";
import {
  agentSupportsWebSearch,
  type AgentEvent,
  type AgentKind,
  type CompletedItem,
  type Environment,
  type Host,
  type Task,
} from "../types";
import { DiffViewer } from "./DiffViewer";
import { TerminalPane } from "./TerminalPane";
import { marked } from "marked";
import DOMPurify from "dompurify";
import { formatCommandExecutionPreview } from "../utils/messageFormatting";
import { getTaskMessageDraft, setTaskMessageDraft } from "../utils/taskMessageDraftCache";
import { shouldFinalizeInitialTaskScroll } from "../utils/taskTranscriptScroll";

type RightMode =
  | { kind: "new-environment" }
  | { kind: "new-workspace" }
  | { kind: "new-task"; host: string; environment: string }
  | { kind: "task"; taskId: string };

type RightTab = "conversation" | "diff" | "terminal";
const TASK_OUTPUT_PAGE_SIZE = 120;

function basenameFromPath(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/\/+$/, "");
  const parts = normalized.split("/").filter(Boolean);
  return parts.length > 0 ? parts[parts.length - 1] : path;
}

function StatusBadge(props: { status: Task["status"] }) {
  const colors = {
    pending: "bg-gray-500",
    running: "bg-blue-500 animate-pulse",
    completed: "bg-green-500",
    failed: "bg-red-500",
    interrupted: "bg-amber-500",
  };

  return (
    <span
      class={`inline-block px-2 py-1 text-xs font-medium text-white rounded ${colors[props.status]}`}
    >
      {props.status}
    </span>
  );
}

function EditableTaskName(props: {
  taskId: string;
  name: string;
  class: string;
  inputClass?: string;
  disabled?: boolean;
  onRenamed?: (name: string) => void | Promise<void>;
}) {
  const [editing, setEditing] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [draft, setDraft] = createSignal(props.name);
  const [displayName, setDisplayName] = createSignal(props.name);
  const [error, setError] = createSignal("");
  let inputRef: HTMLInputElement | undefined;

  createEffect(() => {
    setDisplayName(props.name);
  });

  const focusEditor = () => {
    requestAnimationFrame(() => {
      inputRef?.focus();
      inputRef?.select();
    });
  };

  const startEditing = (event: MouseEvent) => {
    if (props.disabled || saving()) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    setDraft(displayName());
    setError("");
    setEditing(true);
    focusEditor();
  };

  const cancelEditing = () => {
    if (saving()) {
      return;
    }
    setDraft(displayName());
    setError("");
    setEditing(false);
  };

  const submit = async () => {
    if (saving()) {
      return;
    }

    const nextName = draft().trim();
    if (!nextName) {
      setError("Task name is required.");
      focusEditor();
      return;
    }

    if (nextName === displayName()) {
      setEditing(false);
      setError("");
      return;
    }

    setSaving(true);
    setError("");
    try {
      setDisplayName(nextName);
      setDraft(nextName);
      await renameTask(props.taskId, { name: nextName });
      setEditing(false);
      await props.onRenamed?.(nextName);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Rename failed");
      focusEditor();
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="min-w-0">
      <Show
        when={editing()}
        fallback={
          <div
            class={`${props.class} ${props.disabled ? "" : "cursor-text"}`}
            onDblClick={startEditing}
            title={props.disabled ? displayName() : "Double-click to rename"}
          >
            {displayName()}
          </div>
        }
      >
        <input
          ref={inputRef}
          type="text"
          value={draft()}
          onInput={(e) => setDraft(e.currentTarget.value)}
          onClick={(e) => e.stopPropagation()}
          onDblClick={(e) => e.stopPropagation()}
          onBlur={() => void submit()}
          onKeyDown={(e) => {
            e.stopPropagation();
            if (e.key === "Enter") {
              e.preventDefault();
              void submit();
            } else if (e.key === "Escape") {
              e.preventDefault();
              cancelEditing();
            }
          }}
          class={props.inputClass ?? props.class}
          disabled={saving()}
        />
      </Show>
      <Show when={error()}>
        <div class="mt-1 text-xs text-red-600 dark:text-red-400">{error()}</div>
      </Show>
    </div>
  );
}

function EventRow(props: { event: AgentEvent }) {
  const e = props.event;
  if (e.type === "prompt.sent") {
    return (
      <div class="min-w-0 rounded-lg border border-blue-200 dark:border-blue-800 bg-blue-50 dark:bg-blue-950/30 px-3 py-2">
        <div class="text-xs uppercase tracking-wide text-blue-700 dark:text-blue-300">Prompt</div>
        <div class="text-sm text-gray-900 dark:text-gray-100 whitespace-pre-wrap">{e.prompt}</div>
      </div>
    );
  }

  if (e.type === "turn.started") {
    return <div class="text-xs text-blue-600 dark:text-blue-400">Turn started</div>;
  }

  if (e.type === "session.started") {
    return <div class="text-xs text-gray-500 dark:text-gray-400">Session started</div>;
  }

  if (e.type === "turn.completed") {
    return (
      <div class="text-xs text-gray-500 dark:text-gray-400">
        Turn completed
        <Show when={e.usage}>
          <span>
            {" "}
            ({e.usage?.input_tokens ?? 0} in / {e.usage?.output_tokens ?? 0} out)
          </span>
        </Show>
      </div>
    );
  }

  if (e.type === "background_event") {
    return <div class="text-xs text-gray-500 dark:text-gray-400">Background: {e.event ?? "event"}</div>;
  }

  if (e.type === "item.completed") {
    return <CompletedItemRow item={e.item} />;
  }

  return null;
}

function CompletedItemRow(props: { item: CompletedItem }) {
  const item = props.item;

  if (item.type === "agent_message") {
    const html = createMemo(() => {
      const raw = item.text || "";
      const parsed = marked.parse(raw, { breaks: true });
      return DOMPurify.sanitize(typeof parsed === "string" ? parsed : "");
    });
    return (
      <div class="min-w-0 rounded-lg border border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-900 px-3 py-2">
        <div class="text-xs uppercase tracking-wide text-green-700 dark:text-green-300 mb-1">Agent</div>
        <div class="markdown text-sm text-gray-900 dark:text-gray-100" innerHTML={html()} />
      </div>
    );
  }

  if (item.type === "reasoning") {
    return (
      <div class="text-sm text-gray-600 dark:text-gray-400 italic">
        Thinking: {item.text}
      </div>
    );
  }

  if (item.type === "tool_call") {
    return (
      <div class="min-w-0 rounded-lg border border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-900 px-3 py-2">
        <div class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400">Tool call</div>
        <div class="text-sm text-gray-900 dark:text-gray-100 font-mono">{item.name ?? "tool"}</div>
        <Show when={item.arguments}>
          <pre class="mt-1 text-xs whitespace-pre-wrap overflow-x-auto text-gray-600 dark:text-gray-300">
            {item.arguments}
          </pre>
        </Show>
      </div>
    );
  }

  if (item.type === "tool_output") {
    return (
      <div class="min-w-0 rounded-lg border border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-900 px-3 py-2">
        <div class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400">Tool output</div>
        <pre class="mt-1 text-xs whitespace-pre-wrap overflow-x-auto text-gray-700 dark:text-gray-200">
          {item.output ?? ""}
        </pre>
      </div>
    );
  }

  if (item.type === "command_execution") {
    const preview = formatCommandExecutionPreview(item);
    return (
      <div class="min-w-0 rounded-lg border border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-900 px-3 py-2">
        <div class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400">Command</div>
        <div class="text-sm text-gray-900 dark:text-gray-100 font-mono">
          {preview.command ?? "unknown command"}
        </div>
        <Show when={preview.outputText}>
          <pre class="mt-1 text-xs whitespace-pre-wrap overflow-x-auto text-gray-700 dark:text-gray-200">
            {preview.outputText}
          </pre>
        </Show>
      </div>
    );
  }

  return (
    <div class="text-xs text-gray-500 dark:text-gray-400">
      Event: {item.type}
    </div>
  );
}

function TaskPane(props: {
  taskId: string;
  activeTab: () => RightTab;
  hostConnected: () => boolean;
  hideDiff?: boolean;
  nameOverride?: () => string | null;
  onTaskRenamed?: (name: string) => void | Promise<void>;
  onTaskRemoved: () => void;
}) {
  const [task, { refetch: refetchTask }] = createResource(() => props.taskId, getTask);
  const [persistedOutput, { refetch: refetchOutput }] = createResource(
    () => props.taskId,
    async (id) => ({
      taskId: id,
      ...(await getTaskOutput(id, { limit: TASK_OUTPUT_PAGE_SIZE })),
    })
  );
  const [diff, { refetch: refetchDiff }] = createResource(
    () => (props.hideDiff ? null : props.taskId),
    async (id) => {
      if (!id) return null;
      return getTaskDiff(id);
    }
  );
  const taskData = createMemo(() => task.latest ?? task());
  const [mergeStatus, { refetch: refetchMergeStatus }] = createResource(
    () => {
      const currentTask = taskData();
      if (!currentTask || currentTask.workspace_kind !== "worktree") {
        return null;
      }
      return currentTask.id;
    },
    async (id) => {
      if (!id) return null;
      return getMergeStatus(id);
    }
  );
  const diffData = createMemo(() => diff.latest ?? diff());

  const [persistedEvents, setPersistedEvents] = createSignal<AgentEvent[]>([]);
  const [persistedTotalEvents, setPersistedTotalEvents] = createSignal(0);
  const [hasMorePersistedBefore, setHasMorePersistedBefore] = createSignal(false);
  const [loadingOlderEvents, setLoadingOlderEvents] = createSignal(false);
  const [liveEvents, setLiveEvents] = createSignal<AgentEvent[]>([]);
  const allEvents = createMemo(() => [...persistedEvents(), ...liveEvents()]);
  const [prompt, setPrompt] = createSignal("");
  const [sending, setSending] = createSignal(false);
  const [merging, setMerging] = createSignal(false);
  const [archiving, setArchiving] = createSignal(false);
  const [deleting, setDeleting] = createSignal(false);
  const [error, setError] = createSignal("");
  const [taskMessage, setTaskMessage] = createSignal<{ type: "success" | "error"; text: string } | null>(null);
  const [pendingInitialScroll, setPendingInitialScroll] = createSignal(true);
  const [actionDialog, setActionDialog] = createSignal<"merge" | "delete" | null>(null);
  const [showForcePrune, setShowForcePrune] = createSignal(false);
  const [draftHydratedTaskId, setDraftHydratedTaskId] = createSignal<string | null>(null);
  const taskStatus = createMemo(() => taskData()?.status ?? "pending");
  const hostConnected = createMemo(() => props.hostConnected());
  const mergeReady = createMemo(() => mergeStatus.latest ?? mergeStatus());
  const displayTaskName = createMemo(() => props.nameOverride?.() ?? taskData()?.name ?? "");

  let outputRef: HTMLDivElement | undefined;
  let promptRef: HTMLTextAreaElement | undefined;
  const scrollOutputToBottom = () => {
    if (outputRef) {
      outputRef.scrollTop = outputRef.scrollHeight;
    }
  };
  const loadOlderEvents = async () => {
    if (loadingOlderEvents() || !hasMorePersistedBefore()) {
      return;
    }

    const previousHeight = outputRef?.scrollHeight ?? 0;
    setLoadingOlderEvents(true);
    try {
      const page = await getTaskOutput(props.taskId, {
        before: persistedEvents().length,
        limit: TASK_OUTPUT_PAGE_SIZE,
      });
      setPersistedEvents((prev) => [...page.events, ...prev]);
      setPersistedTotalEvents(page.total_events);
      setHasMorePersistedBefore(page.has_more_before);
      if (outputRef) {
        requestAnimationFrame(() => {
          if (!outputRef) return;
          const nextHeight = outputRef.scrollHeight;
          outputRef.scrollTop += nextHeight - previousHeight;
        });
      }
    } catch (err) {
      console.error("Failed to load older task events:", err);
    } finally {
      setLoadingOlderEvents(false);
    }
  };

  createEffect(() => {
    const taskId = props.taskId;
    setPrompt(getTaskMessageDraft(taskId));
    setDraftHydratedTaskId(taskId);
    setPersistedEvents([]);
    setPersistedTotalEvents(0);
    setHasMorePersistedBefore(false);
    setLiveEvents([]);
    setPendingInitialScroll(true);
    setTaskMessage(null);
    setActionDialog(null);
    setShowForcePrune(false);
  });

  createEffect(() => {
    const page = persistedOutput.latest ?? persistedOutput();
    if (!page || page.taskId !== props.taskId) {
      return;
    }
    setPersistedEvents(page.events);
    setPersistedTotalEvents(page.total_events);
    setHasMorePersistedBefore(page.has_more_before);
  });

  createEffect(() => {
    const taskId = props.taskId;
    if (draftHydratedTaskId() !== taskId) {
      return;
    }
    setTaskMessageDraft(taskId, prompt());
  });

  createEffect(() => {
    const tab = props.activeTab();
    if (tab !== "conversation") {
      return;
    }
    requestAnimationFrame(scrollOutputToBottom);
  });

  createEffect(() => {
    const shouldScroll = shouldFinalizeInitialTaskScroll({
      pendingInitialScroll: pendingInitialScroll(),
      activeTab: props.activeTab(),
      persistedOutputLoading: persistedOutput.loading,
      renderedEventCount: persistedEvents().length,
      totalEvents: allEvents().length,
    });
    if (!shouldScroll) {
      return;
    }
    requestAnimationFrame(() => {
      scrollOutputToBottom();
      setPendingInitialScroll(false);
    });
  });

  createEffect(() => {
    const status = taskStatus();
    if (status === "running") {
      const unsubscribe = subscribeToTask(
        props.taskId,
        (event) => {
          setLiveEvents((prev) => [...prev, event]);
          requestAnimationFrame(scrollOutputToBottom);
        },
        () => {
          setTimeout(() => refetchTask(), 300);
          setTimeout(() => refetchOutput(), 300);
          if (!props.hideDiff) {
            setTimeout(() => refetchDiff(), 300);
          }
          setLiveEvents([]);
        }
      );
      onCleanup(unsubscribe);
    }
  });

  createEffect(() => {
    if (taskStatus() === "running") {
      const id = setInterval(() => refetchTask(), 3000);
      onCleanup(() => clearInterval(id));
    }
  });

  createEffect(() => {
    taskStatus();
    const currentTask = taskData();
    if (!currentTask || currentTask.workspace_kind !== "worktree") {
      return;
    }
    refetchMergeStatus();
  });

  const sendFollowup = async (e: Event) => {
    e.preventDefault();
    if (!prompt().trim() || sending() || !hostConnected()) return;
    const value = prompt();
    setSending(true);
    setError("");
    setLiveEvents([{ type: "prompt.sent", prompt: value }]);
    try {
      await sendPrompt(props.taskId, { prompt: value });
      setPrompt("");
      refetchTask();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to send prompt");
    } finally {
      setSending(false);
    }
  };

  const executeMerge = async () => {
    if (!taskData() || merging()) return;
    setMerging(true);
    setTaskMessage(null);
    try {
      const res = await mergeTask(taskData()!.id);
      setTaskMessage({ type: "success", text: res.message });
      setActionDialog(null);
      await refetchTask();
      await refetchMergeStatus();
      if (!props.hideDiff) {
        refetchDiff();
      }
    } catch (err) {
      setTaskMessage({
        type: "error",
        text: err instanceof Error ? err.message : "Merge failed",
      });
      await refetchMergeStatus();
    } finally {
      setMerging(false);
    }
  };

  const executeArchive = async () => {
    if (!taskData() || archiving()) return;
    setArchiving(true);
    setTaskMessage(null);
    try {
      const res = await archiveTask(taskData()!.id);
      setTaskMessage({ type: "success", text: res.message });
      props.onTaskRemoved();
    } catch (err) {
      setTaskMessage({
        type: "error",
        text: err instanceof Error ? err.message : "Archive failed",
      });
    } finally {
      setArchiving(false);
    }
  };

  const executeDelete = async (force: boolean) => {
    if (!taskData() || deleting()) return;
    setDeleting(true);
    setTaskMessage(null);
    try {
      const res = await deleteTask(taskData()!.id, force);
      setTaskMessage({ type: "success", text: res.message });
      setActionDialog(null);
      setShowForcePrune(false);
      props.onTaskRemoved();
    } catch (err) {
      const message = err instanceof Error ? err.message : "Delete failed";
      setTaskMessage({ type: "error", text: message });
      const lower = message.toLowerCase();
      if (!force && (lower.includes("force prune") || lower.includes("untracked"))) {
        setShowForcePrune(true);
        setActionDialog("delete");
      }
    } finally {
      setDeleting(false);
    }
  };

  return (
    <div class="h-full min-w-0 flex flex-col min-h-0 overflow-hidden">
      <Show when={task.loading && !taskData()}>
        <div class="text-gray-500 dark:text-gray-400">Loading task...</div>
      </Show>
      <Show when={task.error}>
        <div class="rounded-lg border border-red-300 dark:border-red-700 bg-red-100 dark:bg-red-900/50 p-3 text-red-700 dark:text-red-200">
          {task.error?.message}
        </div>
      </Show>
      <Show when={taskData()}>
        <div class="sticky top-0 z-20 mb-3 border-b border-gray-200/80 dark:border-gray-800/80 bg-white/95 dark:bg-gray-950/95 pb-3 backdrop-blur supports-[backdrop-filter]:bg-white/80 supports-[backdrop-filter]:dark:bg-gray-950/80">
          <div class="flex items-center justify-between gap-3">
            <div>
              <div class="flex items-center gap-2">
                <EditableTaskName
                  taskId={taskData()!.id}
                  name={displayTaskName()}
                  class="text-xl font-bold text-gray-900 dark:text-gray-100"
                  inputClass="w-full rounded-md border border-blue-400 bg-white px-2 py-1 text-xl font-bold text-gray-900 outline-none dark:border-blue-500 dark:bg-gray-900 dark:text-gray-100"
                  disabled={archiving() || deleting()}
                  onRenamed={async (name) => {
                    await refetchTask();
                    await props.onTaskRenamed?.(name);
                  }}
                />
                <Show when={taskData()!.workspace_kind === "environment"}>
                  <button
                    type="button"
                    disabled={archiving() || taskData()!.status === "running"}
                    onClick={executeArchive}
                    class="rounded-md border border-gray-300 dark:border-gray-600 px-2 py-1 text-sm hover:bg-gray-100 dark:hover:bg-gray-800 disabled:opacity-50 disabled:cursor-not-allowed"
                    title="Archive conversation"
                  >
                    📁
                  </button>
                </Show>
              </div>
              <div class="text-xs text-gray-500 dark:text-gray-400">
                {taskData()!.host}/{taskData()!.environment} • {taskData()!.workspace_kind} • agent: {taskData()!.agent}
              </div>
            </div>
            <div class="flex items-center gap-2">
              <Show when={taskData()!.workspace_kind === "worktree"}>
                <div class="flex items-center gap-2">
                  <button
                    type="button"
                    disabled={merging() || deleting() || taskData()!.status === "running" || !mergeReady()?.can_merge}
                    onClick={() => setActionDialog("merge")}
                    class="rounded-md bg-purple-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-purple-700 disabled:opacity-50 disabled:cursor-not-allowed"
                    title={mergeReady()?.reason ?? "Merge into base branch"}
                  >
                    {merging() ? "Merging..." : "Merge"}
                  </button>
                  <button
                    type="button"
                    disabled={merging() || deleting() || taskData()!.status === "running"}
                    onClick={() => {
                      setShowForcePrune(false);
                      setActionDialog("delete");
                    }}
                    class="rounded-md border border-red-400 px-3 py-1.5 text-sm font-medium text-red-700 dark:text-red-300 hover:bg-red-50 dark:hover:bg-red-950/30 disabled:opacity-50 disabled:cursor-not-allowed"
                    title="Delete worktree task"
                  >
                    {deleting() ? "Deleting..." : "Delete"}
                  </button>
                </div>
              </Show>
              <StatusBadge status={taskData()!.status} />
            </div>
          </div>
        </div>

        <Show when={taskData()!.workspace_kind === "worktree" && mergeReady()?.reason && !mergeReady()?.can_merge}>
          <div class="mb-3 rounded-lg border border-amber-300 dark:border-amber-700 bg-amber-100 dark:bg-amber-900/40 p-3 text-sm text-amber-800 dark:text-amber-200">
            {mergeReady()!.reason}
          </div>
        </Show>

        <Show when={!hostConnected()}>
          <div class="mb-3 rounded-lg border border-gray-300 dark:border-gray-700 bg-gray-100 dark:bg-gray-900/60 p-3 text-sm text-gray-700 dark:text-gray-300">
            Host disconnected. Conversation history remains available, but sending prompts is disabled until this host reconnects.
          </div>
        </Show>

        <Show when={actionDialog() === "merge"}>
          <div class="mb-3 rounded-lg border border-purple-300 dark:border-purple-700 bg-purple-50 dark:bg-purple-950/30 p-3">
            <div class="text-sm font-medium text-purple-800 dark:text-purple-200">
              Merge <span class="font-mono">{taskData()!.merge_branch || displayTaskName()}</span> into{" "}
              <span class="font-mono">{taskData()!.base_branch || "current"}</span>?
            </div>
            <Show when={mergeReady()?.reason && !mergeReady()?.can_merge}>
              <div class="mt-2 text-sm text-amber-700 dark:text-amber-300">{mergeReady()!.reason}</div>
            </Show>
            <div class="mt-3 flex gap-2">
              <button
                type="button"
                disabled={merging() || !mergeReady()?.can_merge}
                onClick={executeMerge}
                class="rounded-md bg-purple-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-purple-700 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                Confirm Merge
              </button>
              <button
                type="button"
                onClick={() => setActionDialog(null)}
                class="rounded-md border border-gray-300 dark:border-gray-600 px-3 py-1.5 text-sm hover:bg-gray-100 dark:hover:bg-gray-800"
              >
                Cancel
              </button>
            </div>
          </div>
        </Show>

        <Show when={actionDialog() === "delete"}>
          <div class="mb-3 rounded-lg border border-red-300 dark:border-red-700 bg-red-50 dark:bg-red-950/30 p-3">
            <div class="text-sm font-medium text-red-800 dark:text-red-200">
              Delete this worktree task?
            </div>
            <div class="mt-2 text-sm text-red-700 dark:text-red-300">
              This prunes the isolated worktree and removes the task from the active list. The conversation log is still archived.
            </div>
            <div class="mt-2 text-xs text-red-600 dark:text-red-400">
              Prune can fail when untracked or modified files exist.
            </div>
            <div class="mt-3 flex flex-wrap gap-2">
              <button
                type="button"
                disabled={deleting()}
                onClick={() => executeDelete(false)}
                class="rounded-md bg-red-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-red-700 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                Confirm Delete
              </button>
              <Show when={showForcePrune()}>
                <button
                  type="button"
                  disabled={deleting()}
                  onClick={() => executeDelete(true)}
                  class="rounded-md border border-red-500 px-3 py-1.5 text-sm font-medium text-red-700 dark:text-red-300 hover:bg-red-100 dark:hover:bg-red-900/40 disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  Force Prune
                </button>
              </Show>
              <button
                type="button"
                onClick={() => {
                  setActionDialog(null);
                  setShowForcePrune(false);
                }}
                class="rounded-md border border-gray-300 dark:border-gray-600 px-3 py-1.5 text-sm hover:bg-gray-100 dark:hover:bg-gray-800"
              >
                Cancel
              </button>
            </div>
          </div>
        </Show>

        <Show when={taskMessage()}>
          <div
            class={`mb-3 rounded-lg border p-3 text-sm ${
              taskMessage()!.type === "success"
                ? "border-green-300 dark:border-green-700 bg-green-100 dark:bg-green-900/50 text-green-700 dark:text-green-200"
                : "border-red-300 dark:border-red-700 bg-red-100 dark:bg-red-900/50 text-red-700 dark:text-red-200"
            }`}
          >
            {taskMessage()!.text}
          </div>
        </Show>

        <Show when={props.activeTab() === "conversation"}>
          <div class="flex-1 min-w-0 min-h-0 flex flex-col overflow-hidden">
            <div
              ref={outputRef}
              onScroll={() => {
                if (outputRef && outputRef.scrollTop <= 160) {
                  void loadOlderEvents();
                }
              }}
              class="flex-1 min-w-0 min-h-0 overflow-x-auto overflow-y-auto rounded-lg border border-gray-200 dark:border-gray-700 bg-gray-100 dark:bg-gray-900 p-4 space-y-3"
            >
              <Show when={hasMorePersistedBefore()}>
                <div class="text-center text-xs text-gray-500 dark:text-gray-400">
                  {loadingOlderEvents()
                    ? "Loading older messages..."
                    : `Scroll up to load older messages (${persistedEvents().length}/${persistedTotalEvents()} loaded)`}
                </div>
              </Show>
              <For each={allEvents()}>{(event) => <EventRow event={event} />}</For>
              <Show when={taskData()!.status === "running" && allEvents().length === 0}>
                <div class="text-gray-500 dark:text-gray-400 animate-pulse">Waiting for output...</div>
              </Show>
            </div>

            <Show when={taskData()!.status !== "running"}>
              <div class="sticky bottom-0 z-20 mt-3 border-t border-gray-200/80 dark:border-gray-800/80 bg-white/95 dark:bg-gray-950/95 pt-3 backdrop-blur supports-[backdrop-filter]:bg-white/80 supports-[backdrop-filter]:dark:bg-gray-950/80">
                <form onSubmit={sendFollowup} class="flex min-w-0 gap-2">
                  <textarea
                    ref={promptRef}
                    rows={3}
                    value={prompt()}
                    onInput={(e) => setPrompt(e.currentTarget.value)}
                    placeholder={
                      hostConnected()
                        ? "Continue the conversation..."
                        : "Host disconnected. Reconnect to continue the conversation..."
                    }
                    class="min-w-0 flex-1 rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100"
                    disabled={!hostConnected()}
                  />
                  <button
                    type="submit"
                    disabled={!hostConnected() || !prompt().trim() || sending()}
                    class="shrink-0 rounded-lg bg-blue-600 px-5 py-2 text-white hover:bg-blue-700 disabled:opacity-50 disabled:cursor-not-allowed"
                  >
                    {sending() ? "Sending..." : "Send"}
                  </button>
                </form>
                <Show when={error()}>
                  <div class="mt-2 text-sm text-red-600 dark:text-red-400">{error()}</div>
                </Show>
              </div>
            </Show>
          </div>
        </Show>

        <Show when={!props.hideDiff && props.activeTab() === "diff"}>
          <div class="flex-1 min-h-0">
            <Show when={diff.loading && !diffData()}>
              <div class="text-gray-500 dark:text-gray-400">Loading diff...</div>
            </Show>
            <Show when={diff.error}>
              <div class="text-red-600 dark:text-red-400">{diff.error?.message}</div>
            </Show>
            <Show when={diffData()}>
              <DiffViewer staged={diffData()!.staged} unstaged={diffData()!.unstaged} />
            </Show>
          </div>
        </Show>

        <Show when={props.activeTab() === "terminal"}>
          <div class="flex-1 min-h-0">
            <TerminalPane taskId={props.taskId} />
          </div>
        </Show>
      </Show>
    </div>
  );
}

function NewTaskPane(props: {
  host: string;
  environment: string;
  hostConnected: boolean;
  onCreated: (taskId: string) => void;
}) {
  const [taskName, setTaskName] = createSignal("");
  const [useWorktree, setUseWorktree] = createSignal(false);
  const [webSearch, setWebSearch] = createSignal(false);
  const [agent, setAgent] = createSignal<AgentKind>("codex");
  const [prompt, setPrompt] = createSignal("");
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal("");
  const searchSupported = () => agentSupportsWebSearch(agent());

  let promptRef: HTMLTextAreaElement | undefined;
  createEffect(() => {
    setTimeout(() => promptRef?.focus(), 0);
  });

  const submit = async (e: Event) => {
    e.preventDefault();
    if (!props.hostConnected || !prompt().trim()) return;
    setLoading(true);
    setError("");
    try {
      const task = await createTask({
        host: props.host,
        environment: props.environment,
        prompt: prompt(),
        name: taskName().trim() || undefined,
        use_worktree: useWorktree(),
        web_search: searchSupported() && webSearch(),
        agent: agent(),
      });
      props.onCreated(task.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create task");
    } finally {
      setLoading(false);
    }
  };

  return (
    <form onSubmit={submit} class="h-full flex flex-col min-h-0">
      <div class="flex-1 min-h-0 flex items-center justify-center">
        <div class="text-center">
          <div class="text-4xl font-extrabold tracking-tight text-gray-900 dark:text-gray-100">
            Let's Build
          </div>
          <div class="text-sm text-gray-500 dark:text-gray-400 mt-2" title={props.environment}>
            {props.host}:{basenameFromPath(props.environment)}
          </div>
          <div class="text-xs text-gray-400 dark:text-gray-500 mt-1 break-all" title={props.environment}>
            {props.environment}
          </div>
        </div>
      </div>

      <div class="mt-4">
        <div class="grid gap-3 md:grid-cols-2 mb-3">
          <input
            value={taskName()}
            onInput={(e) => setTaskName(e.currentTarget.value)}
            placeholder="Task topic (optional)"
            class="rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100"
          />
          <select
            value={agent()}
            onChange={(e) => setAgent(e.currentTarget.value as AgentKind)}
            class="rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100"
          >
            <option value="codex">Codex</option>
            <option value="claude">Claude</option>
            <option value="cursor">Cursor</option>
            <option value="gemini">Gemini</option>
            <option value="opencode">OpenCode</option>
          </select>
        </div>
        <label class="mb-3 flex items-center gap-2 text-sm text-gray-700 dark:text-gray-300">
          <input
            type="checkbox"
            checked={useWorktree()}
            onChange={(e) => setUseWorktree(e.currentTarget.checked)}
          />
          Run task in isolated worktree (mergeable)
        </label>
        <Show when={searchSupported()}>
          <label class="mb-3 flex items-center gap-2 text-sm text-gray-700 dark:text-gray-300">
            <input
              type="checkbox"
              checked={webSearch()}
              onChange={(e) => setWebSearch(e.currentTarget.checked)}
            />
            Enable web search
          </label>
        </Show>
        <textarea
          ref={promptRef}
          rows={4}
          value={prompt()}
          onInput={(e) => setPrompt(e.currentTarget.value)}
          onKeyDown={(e) => {
            if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
              e.preventDefault();
              if (props.hostConnected && !loading() && prompt().trim()) {
                void submit(new Event("submit"));
              }
            }
          }}
          placeholder={
            props.hostConnected
              ? "Describe what you want built..."
              : "Host disconnected. Reconnect to create a task..."
          }
          class="w-full rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100"
          disabled={!props.hostConnected}
        />
        <Show when={!props.hostConnected}>
          <div class="mt-2 rounded-lg border border-gray-300 dark:border-gray-700 bg-gray-100 dark:bg-gray-900/60 px-3 py-2 text-sm text-gray-700 dark:text-gray-300">
            This host is disconnected. Task creation will resume when it reconnects.
          </div>
        </Show>
        <div class="mt-3 flex justify-end">
          <button
            type="submit"
            disabled={!props.hostConnected || loading() || !prompt().trim()}
            class="rounded-lg bg-blue-600 px-5 py-2 text-white hover:bg-blue-700 disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {loading() ? "Creating..." : "Create Task"}
          </button>
        </div>
        <Show when={error()}>
          <div class="mt-2 text-sm text-red-600 dark:text-red-400">{error()}</div>
        </Show>
      </div>
    </form>
  );
}

function NewWorkspacePane(props: {
  onCreated: (taskId: string) => void;
}) {
  const [repoUrl, setRepoUrl] = createSignal("");
  const [name, setName] = createSignal("");
  const [agent, setAgent] = createSignal<AgentKind>("claude");
  const [prompt, setPrompt] = createSignal("");
  const [useWorktree, setUseWorktree] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [status, setStatus] = createSignal("");
  const [error, setError] = createSignal("");

  const submit = async (e: Event) => {
    e.preventDefault();
    if (!repoUrl().trim() || !prompt().trim()) return;
    setLoading(true);
    setError("");
    setStatus("Creating workspace pod...");
    try {
      const result = await createWorkspace({
        repo_url: repoUrl().trim(),
        name: name().trim() || undefined,
        prompt: prompt(),
        use_worktree: useWorktree(),
        agent: agent(),
      });
      props.onCreated(result.task_id);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create workspace");
    } finally {
      setLoading(false);
      setStatus("");
    }
  };

  return (
    <form onSubmit={submit} class="h-full flex flex-col min-h-0">
      <div class="flex-1 min-h-0 flex items-center justify-center">
        <div class="text-center">
          <div class="text-4xl font-extrabold tracking-tight text-gray-900 dark:text-gray-100">
            New Workspace
          </div>
          <div class="text-sm text-gray-500 dark:text-gray-400 mt-2">
            Launch a workspace from a Git repository
          </div>
        </div>
      </div>
      <div class="mt-4">
        <Show when={error()}>
          <div class="mb-3 p-3 bg-red-100 dark:bg-red-900/50 text-red-700 dark:text-red-200 rounded-lg text-sm">{error()}</div>
        </Show>
        <div class="grid gap-3 md:grid-cols-3 mb-3">
          <input value={repoUrl()} onInput={(e) => setRepoUrl(e.currentTarget.value)} placeholder="Git repo URL (https://...)" class="rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100" />
          <input value={name()} onInput={(e) => setName(e.currentTarget.value)} placeholder="Workspace name (optional)" class="rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100" />
          <select value={agent()} onChange={(e) => setAgent(e.currentTarget.value as AgentKind)} class="rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100">
            <option value="claude">Claude</option>
            <option value="codex">Codex</option>
            <option value="cursor">Cursor</option>
            <option value="gemini">Gemini</option>
          </select>
        </div>
        <label class="mb-3 flex items-center gap-2 text-sm text-gray-700 dark:text-gray-300">
          <input type="checkbox" checked={useWorktree()} onChange={(e) => setUseWorktree(e.currentTarget.checked)} />
          Run in isolated worktree (mergeable)
        </label>
        <textarea value={prompt()} onInput={(e) => setPrompt(e.currentTarget.value)} placeholder="What should the agent do?" rows={4} class="w-full rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100 resize-none mb-3" />
        <button type="submit" disabled={!repoUrl().trim() || !prompt().trim() || loading()} class="w-full rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-700 disabled:opacity-50">
          {loading() ? status() || "Creating..." : "Create Workspace"}
        </button>
      </div>
    </form>
  );
}

function NewEnvironmentPane(props: {
  hosts: string[];
  onCreated: (taskId: string) => void;
}) {
  const [host, setHost] = createSignal("");
  const [name, setName] = createSignal("");
  const [agent, setAgent] = createSignal<AgentKind>("codex");
  const [prompt, setPrompt] = createSignal("");
  const [useWorktree, setUseWorktree] = createSignal(false);
  const [webSearch, setWebSearch] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal("");
  const searchSupported = () => agentSupportsWebSearch(agent());

  createEffect(() => {
    if (!host() && props.hosts.length > 0) {
      setHost(props.hosts[0]);
    }
  });

  const submit = async (e: Event) => {
    e.preventDefault();
    if (!host().trim() || !name().trim() || !prompt().trim()) return;
    setLoading(true);
    setError("");
    try {
      const env = await createEnvironment({
        host: host().trim(),
        name: name().trim(),
      });
      const task = await createTask({
        host: env.host,
        environment: env.name,
        prompt: prompt(),
        use_worktree: useWorktree(),
        web_search: searchSupported() && webSearch(),
        agent: agent(),
      });
      props.onCreated(task.id);
      setName("");
      setPrompt("");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create environment");
    } finally {
      setLoading(false);
    }
  };

  return (
    <form onSubmit={submit} class="h-full flex flex-col min-h-0">
      <div class="flex-1 min-h-0 flex items-center justify-center">
        <div class="text-center">
          <div class="text-4xl font-extrabold tracking-tight text-gray-900 dark:text-gray-100">
            Let's Build
          </div>
          <div class="text-sm text-gray-500 dark:text-gray-400 mt-2">
            Create a new environment and kick off the first task immediately.
          </div>
        </div>
      </div>

      <div class="mt-4">
        <div class="grid gap-3 md:grid-cols-2 mb-3">
          <input
            list="hosts-list"
            value={host()}
            onInput={(e) => setHost(e.currentTarget.value)}
            placeholder="Host"
            class="rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100"
          />
          <datalist id="hosts-list">
            <For each={props.hosts}>{(host) => <option value={host} />}</For>
          </datalist>
          <input
            value={name()}
            onInput={(e) => setName(e.currentTarget.value)}
            placeholder="Environment name"
            class="rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100"
          />
        </div>
        <div class="grid gap-3 md:grid-cols-2 mb-3">
          <select
            value={agent()}
            onChange={(e) => setAgent(e.currentTarget.value as AgentKind)}
            class="rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100"
          >
            <option value="codex">Codex</option>
            <option value="claude">Claude</option>
            <option value="cursor">Cursor</option>
            <option value="gemini">Gemini</option>
            <option value="opencode">OpenCode</option>
          </select>
          <label class="flex items-center gap-2 text-sm text-gray-700 dark:text-gray-300">
            <input
              type="checkbox"
              checked={useWorktree()}
              onChange={(e) => setUseWorktree(e.currentTarget.checked)}
            />
            Run task in isolated worktree
          </label>
          <Show when={searchSupported()}>
            <label class="flex items-center gap-2 text-sm text-gray-700 dark:text-gray-300">
              <input
                type="checkbox"
                checked={webSearch()}
                onChange={(e) => setWebSearch(e.currentTarget.checked)}
              />
              Enable web search
            </label>
          </Show>
        </div>
        <textarea
          rows={4}
          value={prompt()}
          onInput={(e) => setPrompt(e.currentTarget.value)}
          onKeyDown={(e) => {
            if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
              e.preventDefault();
              if (!loading() && host().trim() && name().trim() && prompt().trim()) {
                void submit(new Event("submit"));
              }
            }
          }}
          placeholder="Initial prompt for the first task..."
          class="w-full rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100"
        />
        <div class="mt-3 flex justify-end">
          <button
            type="submit"
            disabled={loading() || !host().trim() || !name().trim() || !prompt().trim()}
            class="rounded-lg bg-blue-600 px-5 py-2 text-white hover:bg-blue-700 disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {loading() ? "Creating..." : "Create Environment + Start Task"}
          </button>
        </div>
        <Show when={error()}>
          <div class="mt-2 text-sm text-red-600 dark:text-red-400">{error()}</div>
        </Show>
      </div>
    </form>
  );
}

export default function Workspace() {
  const params = useParams();
  const [hosts, { refetch: refetchHosts }] = createResource(listHosts);
  const [environments, { refetch: refetchEnvironments }] = createResource(listEnvironments);
  const [tasks, { refetch: refetchTasks }] = createResource(listTasks);
  const hostsData = createMemo(() => hosts.latest ?? hosts() ?? []);
  const environmentsData = createMemo(() => environments.latest ?? environments() ?? []);
  const tasksData = createMemo(() => tasks.latest ?? tasks() ?? []);
  const [knownHosts, setKnownHosts] = createSignal<Record<string, Host>>({});
  const [knownEnvironments, setKnownEnvironments] = createSignal<Record<string, Environment>>({});
  const [knownTasks, setKnownTasks] = createSignal<Record<string, Task>>({});
  const hostsById = createMemo(() => new Map(Object.entries(knownHosts())));
  const hostIds = createMemo(() => Object.keys(knownHosts()).sort((a, b) => a.localeCompare(b)));
  const connectedHostIds = createMemo(() => new Set(hostsData().map((host) => host.host)));
  const environmentsById = createMemo(() => new Map(Object.entries(knownEnvironments())));
  const environmentIds = createMemo(() => Object.keys(knownEnvironments()).sort((a, b) => a.localeCompare(b)));
  const tasksById = createMemo(() => new Map(Object.entries(knownTasks())));
  const [expanded, setExpanded] = createSignal<Record<string, boolean>>({});
  const [hasExpandedRunningTasks, setHasExpandedRunningTasks] = createSignal(false);
  const [taskNameOverrides, setTaskNameOverrides] = createSignal<Record<string, string>>({});
  const [mode, setMode] = createSignal<RightMode>({ kind: "new-workspace" });
  const [tab, setTab] = createSignal<RightTab>("conversation");
  const [mobileMenuOpen, setMobileMenuOpen] = createSignal(false);
  const [isMobile, setIsMobile] = createSignal(false);

  createEffect(() => {
    const id = setInterval(() => {
      refetchHosts();
      refetchEnvironments();
      refetchTasks();
    }, 4000);
    onCleanup(() => clearInterval(id));
  });

  createEffect(() => {
    setKnownHosts((prev) => {
      const next = { ...prev };
      for (const host of hostsData()) {
        next[host.host] = host;
      }
      return next;
    });
  });

  createEffect(() => {
    const connectedHosts = connectedHostIds();
    setKnownEnvironments((prev) => {
      const next = Object.fromEntries(
        Object.entries(prev).filter(([, env]) => !connectedHosts.has(env.host))
      );
      for (const env of environmentsData()) {
        next[`${env.host}::${env.name}`] = env;
      }
      return next;
    });
  });

  createEffect(() => {
    const connectedHosts = connectedHostIds();
    setKnownTasks((prev) => {
      const next = Object.fromEntries(
        Object.entries(prev).filter(([, task]) => !connectedHosts.has(task.host))
      );
      for (const task of tasksData()) {
        next[task.id] = task;
      }
      return next;
    });
  });

  createEffect(() => {
    const id = params.id;
    if (id) {
      setMode({ kind: "task", taskId: id });
      setTab("conversation");
    }
  });

  createEffect(() => {
    const query = window.matchMedia("(max-width: 1023px)");
    const apply = () => setIsMobile(query.matches);
    apply();
    query.addEventListener("change", apply);
    onCleanup(() => query.removeEventListener("change", apply));
  });

  createEffect(() => {
    if (isMobile()) {
      setTab("conversation");
    }
  });

  const tasksByEnvironment = createMemo(() => {
    const grouped: Record<string, string[]> = {};
    for (const task of Object.values(knownTasks())) {
      const key = `${task.host}::${task.environment}`;
      if (!grouped[key]) grouped[key] = [];
      grouped[key].push(task.id);
    }
    for (const key of Object.keys(grouped)) {
      grouped[key].sort((a, b) => {
        const left = tasksById().get(a);
        const right = tasksById().get(b);
        return +new Date(right?.created_at ?? 0) - +new Date(left?.created_at ?? 0);
      });
    }
    return grouped;
  });

  createEffect(() => {
    if (hasExpandedRunningTasks() || tasks.loading) {
      return;
    }

    const runningEnvironmentIds = new Set(
      Object.values(knownTasks())
        .filter((task) => task.status === "running")
        .map((task) => `${task.host}::${task.environment}`)
    );

    setExpanded((prev) => {
      if (runningEnvironmentIds.size === 0) {
        return prev;
      }
      const next = { ...prev };
      let changed = false;
      for (const environmentId of runningEnvironmentIds) {
        if (!next[environmentId]) {
          next[environmentId] = true;
          changed = true;
        }
      }
      return changed ? next : prev;
    });

    setHasExpandedRunningTasks(true);
  });

  const selectedTaskId = createMemo(() => {
    const currentMode = mode();
    return currentMode.kind === "task" ? currentMode.taskId : null;
  });
  const isHostConnected = (host: string) => connectedHostIds().has(host);

  const rememberRenamedTask = async (taskId: string, name: string) => {
    setTaskNameOverrides((prev) => ({ ...prev, [taskId]: name }));
    await refetchTasks();
  };

  const toggleEnvironment = (name: string) => {
    setExpanded((prev) => ({ ...prev, [name]: !prev[name] }));
  };

  const sidebarContent = () => (
    <>
      <div class="mb-4">
        <h1 class="text-sm font-semibold uppercase tracking-wide text-gray-500 dark:text-gray-400">Hosts</h1>
        <Show when={hosts.loading && hostsData().length === 0}>
          <div class="text-xs text-gray-500 dark:text-gray-400 mt-1">Loading hosts...</div>
        </Show>
        <div class="mt-2 space-y-1">
          <For each={hostIds()}>
            {(hostId) => {
              const host = createMemo(() => hostsById().get(hostId));
              const connected = createMemo(() => isHostConnected(hostId));
              return (
                <div
                  class={`rounded px-2 py-1 text-xs ${
                    connected()
                      ? "text-gray-700 dark:text-gray-300"
                      : "text-gray-400 dark:text-gray-500"
                  }`}
                >
                  <div class="font-medium">{host()?.host}</div>
                  <Show when={host() && host()!.host !== host()!.hostname}>
                    <div class="text-[11px] text-gray-500 dark:text-gray-400">{host()?.hostname}</div>
                  </Show>
                </div>
              );
            }}
          </For>
          <Show when={!hosts.loading && hostsData().length === 0}>
            <div class="text-xs text-amber-600 dark:text-amber-400">No connected slopagents.</div>
          </Show>
        </div>
      </div>

      <div class="mb-4">
        <button
          onClick={() => {
            setMode({ kind: "new-workspace" });
            setTab("conversation");
            if (isMobile()) setMobileMenuOpen(false);
          }}
          class="w-full rounded-lg bg-blue-600 px-3 py-2 text-sm font-medium text-white hover:bg-blue-700"
        >
          + New Workspace
        </button>
      </div>

      <div class="mb-3 flex items-center justify-between">
        <h1 class="text-sm font-semibold uppercase tracking-wide text-gray-500 dark:text-gray-400">Environments</h1>
        <button
          onClick={() => {
            setMode({ kind: "new-environment" });
            setTab("conversation");
            if (isMobile()) {
              setMobileMenuOpen(false);
            }
          }}
          class="rounded-md border border-gray-300 dark:border-gray-600 px-2 py-0.5 text-xs hover:bg-gray-100 dark:hover:bg-gray-800"
          title="Create environment"
        >
          + New
        </button>
      </div>

      <Show when={environments.loading && environmentsData().length === 0}>
        <div class="text-xs text-gray-500 dark:text-gray-400">Loading environments...</div>
      </Show>

      <For each={environmentIds()}>
        {(environmentId) => {
          const env = createMemo(() => environmentsById().get(environmentId));
          const hostConnected = createMemo(() => isHostConnected(env()?.host ?? ""));
          return (
            <div class="mb-2">
              <div class="flex items-center justify-between px-2 py-2">
                <button
                  onClick={() => toggleEnvironment(environmentId)}
                  class={`flex items-center gap-2 text-sm font-medium ${
                    hostConnected()
                      ? "text-gray-800 dark:text-gray-200"
                      : "text-gray-400 dark:text-gray-500"
                  }`}
                  title={env()?.directory || env()?.name || ""}
                >
                  <span
                    class={`inline-block transition-transform ${
                      expanded()[environmentId] ? "rotate-90" : ""
                    }`}
                  >
                    ▸
                  </span>
                  {env()?.host}:{basenameFromPath(env()?.directory || env()?.name || "")}
                </button>
                <button
                  onClick={() => {
                    if (!env()) return;
                    setMode({ kind: "new-task", host: env()!.host, environment: env()!.name });
                    setTab("conversation");
                    if (isMobile()) {
                      setMobileMenuOpen(false);
                    }
                  }}
                  disabled={!hostConnected()}
                  class="rounded-md border border-gray-300 dark:border-gray-600 px-2 py-0.5 text-xs hover:bg-gray-100 dark:hover:bg-gray-800"
                  title={`Create task in ${env()?.directory || env()?.name}`}
                >
                  +
                </button>
              </div>

              <Show when={expanded()[environmentId]}>
                <div class="pl-5 pr-1 py-1 space-y-1">
                  <For each={tasksByEnvironment()[environmentId] || []}>
                    {(taskId) => {
                      const task = createMemo(() => tasksById().get(taskId));
                      const hostConnected = createMemo(() => isHostConnected(task()?.host ?? ""));
                      return (
                        <div
                          role="button"
                          tabindex="0"
                          onClick={() => {
                            setMode({ kind: "task", taskId });
                            setTab("conversation");
                            if (isMobile()) {
                              setMobileMenuOpen(false);
                            }
                          }}
                          onKeyDown={(e) => {
                            if (e.key === "Enter" || e.key === " ") {
                              e.preventDefault();
                              setMode({ kind: "task", taskId });
                              setTab("conversation");
                              if (isMobile()) {
                                setMobileMenuOpen(false);
                              }
                            }
                          }}
                          class={`w-full rounded-md border px-2 py-2 text-left ${
                            selectedTaskId() === taskId
                              ? hostConnected()
                                ? "border-blue-500 bg-blue-50 dark:bg-blue-950/30"
                                : "border-gray-300 bg-gray-100 dark:border-gray-700 dark:bg-gray-900/60"
                              : hostConnected()
                                ? "border-transparent hover:border-gray-200 dark:hover:border-gray-700 hover:bg-gray-50 dark:hover:bg-gray-800"
                                : "border-transparent bg-gray-100/80 dark:bg-gray-900/50 opacity-70"
                          }`}
                        >
                          <EditableTaskName
                            taskId={taskId}
                            name={taskNameOverrides()[taskId] ?? task()?.name ?? ""}
                            class={`truncate text-sm font-medium ${
                              hostConnected()
                                ? "text-gray-900 dark:text-gray-100"
                                : "text-gray-500 dark:text-gray-400"
                            }`}
                            inputClass="w-full rounded border border-blue-400 bg-white px-1 py-0.5 text-sm font-medium text-gray-900 outline-none dark:border-blue-500 dark:bg-gray-900 dark:text-gray-100"
                            onRenamed={(name) => rememberRenamedTask(taskId, name)}
                          />
                          <div class="mt-1 flex items-center justify-between">
                            <span class="text-[11px] text-gray-500 dark:text-gray-400">
                              {new Date(task()?.created_at || "").toLocaleDateString()}
                            </span>
                            <Show when={task()}>
                              <StatusBadge status={task()!.status} />
                            </Show>
                          </div>
                        </div>
                      );
                    }}
                  </For>
                  <Show when={(tasksByEnvironment()[environmentId] || []).length === 0}>
                    <div class="text-xs text-gray-500 dark:text-gray-400 px-2 py-1">No tasks yet.</div>
                  </Show>
                </div>
              </Show>
            </div>
          );
        }}
      </For>
    </>
  );

  return (
    <div class="h-screen h-dvh w-screen max-w-[100vw] overflow-hidden bg-white dark:bg-gray-950 text-gray-900 dark:text-gray-100">
      <div class="lg:hidden sticky top-0 z-30 flex items-center justify-between px-3 py-2 border-b border-gray-200 dark:border-gray-800 bg-white/95 dark:bg-gray-950/95 backdrop-blur supports-[backdrop-filter]:bg-white/80 supports-[backdrop-filter]:dark:bg-gray-950/80">
        <button
          onClick={() => setMobileMenuOpen(true)}
          class="rounded-md border border-gray-300 dark:border-gray-700 px-2 py-1 text-sm"
          title="Open environments"
        >
          ☰
        </button>
        <div class="text-sm font-semibold text-gray-800 dark:text-gray-200">Slopcoder</div>
        <div class="w-7" />
      </div>

      <div class="h-[calc(100vh-41px)] h-[calc(100dvh-41px)] overflow-hidden lg:h-full lg:grid lg:grid-cols-[320px_1fr] lg:grid-rows-[minmax(0,1fr)]">
        <aside class="hidden lg:block border-r border-gray-200 dark:border-gray-800 bg-gray-50 dark:bg-gray-900/50 p-3 overflow-y-auto">
          {sidebarContent()}
        </aside>

        <Show when={isMobile() && mobileMenuOpen()}>
          <div class="fixed inset-0 z-50 lg:hidden">
            <button
              class="absolute inset-0 bg-black/40"
              onClick={() => setMobileMenuOpen(false)}
              aria-label="Close menu"
            />
            <aside class="absolute left-0 top-0 h-full w-[88%] max-w-sm border-r border-gray-200 dark:border-gray-800 bg-gray-50 dark:bg-gray-900 p-3 overflow-y-auto">
              <div class="mb-3 flex items-center justify-between">
                <div class="text-sm font-semibold text-gray-800 dark:text-gray-200">Navigation</div>
                <button
                  onClick={() => setMobileMenuOpen(false)}
                  class="rounded-md border border-gray-300 dark:border-gray-700 px-2 py-1 text-xs"
                >
                  Close
                </button>
              </div>
              {sidebarContent()}
            </aside>
          </div>
        </Show>

        <main class="h-full p-3 lg:p-5 min-w-0 min-h-0 flex flex-col overflow-hidden">
          <Show when={mode().kind === "task"}>
            <div class="mb-4 hidden lg:flex gap-2 border-b border-gray-200 dark:border-gray-800">
              <button
                onClick={() => setTab("conversation")}
                class={`px-3 py-2 text-sm font-medium ${
                  tab() === "conversation"
                    ? "text-blue-600 dark:text-blue-400 border-b-2 border-blue-600 dark:border-blue-400"
                    : "text-gray-500 dark:text-gray-400"
                }`}
              >
                Conversation
              </button>
              <Show when={!isMobile()}>
                <button
                  onClick={() => setTab("diff")}
                  class={`px-3 py-2 text-sm font-medium ${
                    tab() === "diff"
                      ? "text-blue-600 dark:text-blue-400 border-b-2 border-blue-600 dark:border-blue-400"
                      : "text-gray-500 dark:text-gray-400"
                  }`}
                >
                  Diff
                </button>
                <button
                  onClick={() => setTab("terminal")}
                  class={`px-3 py-2 text-sm font-medium ${
                    tab() === "terminal"
                      ? "text-blue-600 dark:text-blue-400 border-b-2 border-blue-600 dark:border-blue-400"
                      : "text-gray-500 dark:text-gray-400"
                  }`}
                >
                  Terminal
                </button>
              </Show>
            </div>
          </Show>

          <div class="h-full flex-1 min-w-0 min-h-0">
            <Show when={mode().kind === "new-environment"}>
              <NewEnvironmentPane
                hosts={hostIds()}
                onCreated={(taskId) => {
                  refetchEnvironments();
                  refetchTasks();
                  setMode({ kind: "task", taskId });
                  setTab("conversation");
                }}
              />
            </Show>

            <Show when={mode().kind === "new-workspace"}>
              <NewWorkspacePane
                onCreated={(taskId) => {
                  refetchTasks();
                  setMode({ kind: "task", taskId });
                  setTab("conversation");
                }}
              />
            </Show>

            <Show when={mode().kind === "new-task"}>
              <NewTaskPane
                host={(mode() as { kind: "new-task"; host: string; environment: string }).host}
                environment={(mode() as { kind: "new-task"; host: string; environment: string }).environment}
                hostConnected={isHostConnected(
                  (mode() as { kind: "new-task"; host: string; environment: string }).host
                )}
                onCreated={(taskId) => {
                  refetchTasks();
                  setMode({ kind: "task", taskId });
                  setTab("conversation");
                }}
              />
            </Show>

            <Show when={mode().kind === "task"}>
              <TaskPane
                taskId={(mode() as { kind: "task"; taskId: string }).taskId}
                activeTab={tab}
                hostConnected={() => {
                  const taskId = (mode() as { kind: "task"; taskId: string }).taskId;
                  const task = tasksById().get(taskId);
                  return task ? isHostConnected(task.host) : false;
                }}
                hideDiff={isMobile()}
                nameOverride={() => {
                  const taskId = (mode() as { kind: "task"; taskId: string }).taskId;
                  return taskNameOverrides()[taskId] ?? null;
                }}
                onTaskRenamed={(name) =>
                  rememberRenamedTask((mode() as { kind: "task"; taskId: string }).taskId, name)
                }
                onTaskRemoved={() => {
                  refetchTasks();
                  const current = mode();
                  if (current.kind === "task" && current.taskId === selectedTaskId()) {
                    setMode({ kind: "new-environment" });
                    setTab("conversation");
                  }
                }}
              />
            </Show>
          </div>
        </main>
      </div>
    </div>
  );
}
