<script>
  // Rendered whether or not the graph resolved: a workspace whose directory
  // is gone never resolves, and that's the one most worth removing.
  let { workspace, workspaceName = null, onRemove } = $props();

  let confirming = $state(false);
  let removing = $state(false);
  let removeError = $state(null);

  let label = $derived(
    workspaceName ?? String(workspace?.workspace ?? '').split('/').filter(Boolean).pop() ?? '',
  );

  async function remove() {
    removing = true;
    removeError = null;
    try {
      await onRemove(workspace.id);
    } catch (e) {
      removeError = e.message;
      removing = false;
      confirming = false;
    }
  }
</script>

<div class="config">
  {#if !workspace}
    <span class="eyebrow">Workspace</span>
    <p class="body-sm">No workspace is selected. Run <code>fghj wire</code> in one to add it.</p>
  {:else}
    <section>
      <span class="eyebrow">Workspace</span>
      <h3 class="stencil">{label}</h3>
      <dl>
        <dt>Directory</dt>
        <dd><code>{workspace.workspace}</code></dd>
        <dt>Domains</dt>
        <dd><code>*.{label}.fghj.internal</code></dd>
        <dt>Id</dt>
        <dd><code>{workspace.id}</code></dd>
      </dl>
    </section>

    <section class="danger-zone">
      <span class="eyebrow">Remove from fghj</span>
      <p class="body-sm">
        Stops and removes this workspace's containers and network, and takes it off this list.
        Volumes and the repos on disk are kept, so <code>fghj wire</code> brings it back with its data.
      </p>
      {#if confirming}
        <div class="confirm">
          <span class="body-sm">Remove <b>{label}</b>?</span>
          <button class="ctrl-btn danger" onclick={remove} disabled={removing}>
            {#if removing}<span class="spinner"></span> Removing…{:else}Remove{/if}
          </button>
          <button class="ctrl-btn" onclick={() => (confirming = false)} disabled={removing}>Cancel</button>
        </div>
      {:else}
        <button class="ctrl-btn danger" onclick={() => (confirming = true)}>Remove workspace</button>
      {/if}
      {#if removeError}
        <p class="error body-sm">Not removed: {removeError}</p>
      {/if}
    </section>
  {/if}
</div>

<style>
  .config { padding: 0 40px 40px; max-width: 720px; display: flex; flex-direction: column; gap: 32px; }
  section { display: flex; flex-direction: column; gap: 10px; align-items: flex-start; }
  h3 { font-size: 20px; color: var(--ink); word-break: break-all; }
  dl { display: grid; grid-template-columns: max-content 1fr; gap: 6px 16px; margin: 0; }
  dt { font: 600 11px/1.6 var(--font-mono); color: var(--ink-faint); }
  dd { margin: 0; font: 400 12.5px/1.6 var(--font-body); color: var(--ink-dim); word-break: break-all; }
  code { font-family: var(--font-mono); font-size: 0.92em; }
  .danger-zone { padding-top: 24px; border-top: 1px solid var(--line); align-self: stretch; }
  .confirm { display: flex; align-items: center; gap: 8px; }
  .ctrl-btn {
    padding: 6px 12px; border-radius: 4px; background: var(--panel-2);
    border: 1px solid var(--line-strong); color: var(--ink); font: 700 10px var(--font-mono);
    text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer;
    display: inline-flex; align-items: center; gap: 6px;
  }
  .ctrl-btn:disabled { opacity: 0.4; cursor: default; }
  .ctrl-btn.danger:not(:disabled) { border-color: var(--danger); color: var(--danger); }
  .error { color: var(--danger); margin: 0; }
  .spinner {
    display: inline-block; width: 9px; height: 9px; border-radius: 50%;
    border: 2px solid var(--line-strong); border-top-color: var(--danger);
    animation: spin 0.7s linear infinite;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
</style>
