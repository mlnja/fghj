<script>
  let { runs, serviceIds, onStart, onStop } = $props();

  let showForm = $state(false);
  let runName = $state('');
  let overrideService = $state('');
  let overrideBranch = $state('');

  function startDefault() {
    onStart({ run_id: null, overrides: {} });
  }

  function startReview() {
    if (!runName.trim()) return;
    const overrides = {};
    if (overrideService && overrideBranch.trim()) {
      overrides[overrideService] = overrideBranch.trim();
    }
    onStart({ run_id: runName.trim(), overrides });
    showForm = false;
    runName = '';
    overrideBranch = '';
  }
</script>

<div class="controls">
  <div class="row">
    <button class="btn" onclick={startDefault}>Start default environment</button>
    <button class="btn ghost" onclick={() => (showForm = !showForm)}>+ Review run</button>
  </div>

  {#if showForm}
    <div class="form">
      <input class="field" placeholder="run name (e.g. review-auth-pr123)" bind:value={runName} />
      <select class="field" bind:value={overrideService}>
        <option value="">— override branch on service (optional) —</option>
        {#each serviceIds as id}
          <option value={id}>{id}</option>
        {/each}
      </select>
      {#if overrideService}
        <input class="field" placeholder="branch" bind:value={overrideBranch} />
      {/if}
      <button class="btn" onclick={startReview}>Start review run</button>
    </div>
  {/if}

  {#if runs.length}
    <div class="run-list">
      {#each runs as r}
        <div class="run-row">
          <span class="run-id">{r.run_id}</span>
          <span class="run-net">{r.network}</span>
          <button class="btn ghost small" onclick={() => onStop(r.run_id)}>Stop</button>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .controls { display: flex; flex-direction: column; gap: 10px; padding: 0 40px 16px; }
  .row { display: flex; gap: 8px; }
  .btn {
    padding: 7px 12px; border-radius: 5px; background: var(--accent); color: var(--bg);
    font: 700 11px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer; border: none;
  }
  .btn.ghost { background: var(--panel-2); color: var(--ink); border: 1px solid var(--line-strong); }
  .btn.small { padding: 4px 8px; font-size: 10px; }
  .form { display: flex; gap: 8px; align-items: center; background: var(--panel-2); border: 1px solid var(--line-strong); border-radius: 6px; padding: 10px; }
  .field { background: var(--panel); border: 1px solid var(--line-strong); border-radius: 4px; padding: 6px 8px; font: 500 11.5px var(--font-mono); color: var(--ink); }
  .run-list { display: flex; flex-direction: column; gap: 4px; }
  .run-row { display: flex; align-items: center; gap: 10px; font: 500 11.5px var(--font-mono); color: var(--ink-dim); }
  .run-id { color: var(--ink); font-weight: 700; }
  .run-net { color: var(--ink-faint); }
</style>
