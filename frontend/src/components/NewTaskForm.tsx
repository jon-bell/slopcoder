import { createSignal, Show } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { createWorkspace } from "../api/client";
import type { AgentKind } from "../types";

export default function NewTaskForm() {
  const navigate = useNavigate();

  const [repoUrl, setRepoUrl] = createSignal("");
  const [taskName, setTaskName] = createSignal("");
  const [useWorktree, setUseWorktree] = createSignal(false);
  const [prompt, setPrompt] = createSignal("");
  const [agent, setAgent] = createSignal<AgentKind>("claude");
  const [submitting, setSubmitting] = createSignal(false);
  const [status, setStatus] = createSignal("");
  const [error, setError] = createSignal("");

  const handleSubmit = async (e: Event) => {
    e.preventDefault();
    setError("");
    setSubmitting(true);
    setStatus("Creating workspace pod...");

    try {
      const result = await createWorkspace({
        repo_url: repoUrl(),
        name: taskName().trim() || undefined,
        use_worktree: useWorktree(),
        prompt: prompt(),
        agent: agent(),
      });
      navigate(`/tasks/${result.task_id}`);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create workspace");
    } finally {
      setSubmitting(false);
      setStatus("");
    }
  };

  const isValid = () => repoUrl().trim() && prompt().trim();

  const inputClass = "w-full px-3 py-1.5 bg-white dark:bg-gray-800 text-gray-900 dark:text-gray-100 border border-gray-300 dark:border-gray-600 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent placeholder-gray-400 dark:placeholder-gray-500";

  return (
    <div class="max-w-2xl">
      <Show when={error()}>
        <div class="mb-4 p-4 bg-red-100 dark:bg-red-900/50 text-red-700 dark:text-red-200 rounded-lg border border-red-300 dark:border-red-700">
          {error()}
        </div>
      </Show>

      <form onSubmit={handleSubmit} class="space-y-4">
        <div class="grid gap-3 md:grid-cols-3">
          <div>
            <label class="block text-xs font-medium text-gray-700 dark:text-gray-300 mb-1">
              Git Repository URL
            </label>
            <input
              type="text"
              value={repoUrl()}
              onInput={(e) => setRepoUrl(e.currentTarget.value)}
              placeholder="https://github.com/org/repo"
              class={inputClass}
            />
          </div>

          <div>
            <label class="block text-xs font-medium text-gray-700 dark:text-gray-300 mb-1">
              Workspace Name
            </label>
            <input
              type="text"
              value={taskName()}
              onInput={(e) => setTaskName(e.currentTarget.value)}
              placeholder="Optional (auto-generate)"
              class={inputClass}
            />
          </div>

          <div>
            <label class="block text-xs font-medium text-gray-700 dark:text-gray-300 mb-1">
              Agent
            </label>
            <select
              value={agent()}
              onChange={(e) => setAgent(e.currentTarget.value as AgentKind)}
              class={inputClass}
            >
              <option value="claude">Claude</option>
              <option value="codex">Codex</option>
              <option value="cursor">Cursor</option>
              <option value="gemini">Gemini</option>
            </select>
          </div>
        </div>

        <label class="flex items-center gap-2 text-xs text-gray-700 dark:text-gray-300">
          <input
            type="checkbox"
            checked={useWorktree()}
            onChange={(e) => setUseWorktree(e.currentTarget.checked)}
          />
          Run in isolated worktree (mergeable)
        </label>

        <div>
          <label class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-2">
            Initial Prompt
          </label>
          <textarea
            value={prompt()}
            onInput={(e) => setPrompt(e.currentTarget.value)}
            onKeyDown={(e) => {
              if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
                e.preventDefault();
                if (!submitting() && isValid()) {
                  void handleSubmit(new Event("submit"));
                }
              }
            }}
            placeholder="Describe what you want the agent to do..."
            rows={6}
            class={`${inputClass} resize-none`}
          />
        </div>

        <div class="flex gap-3">
          <button
            type="submit"
            disabled={!isValid() || submitting()}
            class="flex-1 px-4 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
          >
            <Show when={submitting()} fallback="Create Workspace">
              <span class="inline-flex items-center gap-2">
                <svg class="animate-spin h-4 w-4" viewBox="0 0 24 24"><circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4" fill="none"/><path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/></svg>
                {status()}
              </span>
            </Show>
          </button>
        </div>
      </form>
    </div>
  );
}
