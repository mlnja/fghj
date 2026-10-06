<script>
  // There is deliberately no control here for starting a *named* run.
  //
  // A named run varies only three things: its identity (network, container
  // names, run-qualified domains), the contents of its non-`stable` volumes
  // (each run gets fresh, empty ones), and which of its nodes are up. It does
  // not get its own checkout — the per-run branch pin was removed as unsound
  // — and the config→run map is a constant function, so two runs built from
  // one workspace are necessarily identical code. Offering "+ Review run" in
  // the primary controls promised PR review, which is the one thing it cannot
  // do.
  //
  // The mechanism is intact: `POST /runs` takes a `run_id`, every derived
  // domain and volume name folds it in, and `fghj exec --run` targets one. A
  // run started that way still shows up in the list below with its own Stop.
  // See `reference/runs` in the docs.
  let { runs, onStart, onStop, onOpenOperations } = $props();

  function startDefault() {
    onStart({ run_id: null });
  }
</script>

<div class="controls">
  <div class="row">
    <button class="btn" onclick={startDefault}>Start default environment</button>
    <button class="btn ghost icon" onclick={onOpenOperations} title="view pull/download operations queue">☰ Operations</button>
  </div>

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
  .run-list { display: flex; flex-direction: column; gap: 4px; }
  .run-row { display: flex; align-items: center; gap: 10px; font: 500 11.5px var(--font-mono); color: var(--ink-dim); }
  .run-id { color: var(--ink); font-weight: 700; }
  .run-net { color: var(--ink-faint); }
</style>
