<script>
  import SideDrawer from './SideDrawer.svelte';

  let { onClose, onListJobs } = $props();

  let jobs = $state([]);
  let selectedKey = $state(null);
  let pollTimer = null;

  async function refresh() {
    if (!onListJobs) return;
    const next = await onListJobs();
    jobs = next ?? [];
    if (!selectedKey && jobs.length) selectedKey = jobs[0].key;
  }

  refresh();
  pollTimer = setInterval(refresh, 800);

  $effect(() => {
    return () => {
      if (pollTimer) clearInterval(pollTimer);
    };
  });

  function label(key) {
    if (key === 'pull-all') return 'Pull all repos';
    if (key.startsWith('node:')) return `Download ${key.slice('node:'.length)}`;
    return key;
  }

  let selected = $derived(jobs.find((j) => j.key === selectedKey) ?? null);
</script>

<SideDrawer onClose={onClose} width="60vw">
  <div class="head">
    <span class="eyebrow">operations</span>
    <div class="close" onclick={onClose}>×</div>
  </div>
  <h3 class="stencil">Pull queue</h3>

  <div class="ops">
    <div class="ops-list">
      {#if !jobs.length}
        <div class="empty">no operations yet</div>
      {/if}
      {#each jobs as j (j.key)}
        <div class="ops-row" class:active={j.key === selectedKey} onclick={() => (selectedKey = j.key)}>
          <span class="dot" class:running={j.status === 'running'} class:done={j.status === 'done'} class:error={j.status === 'error'}></span>
          <span class="ops-label">{label(j.key)}</span>
        </div>
      {/each}
    </div>
    <div class="ops-log">
      {#if selected}
        <pre>{selected.log || '(no output yet)'}</pre>
      {:else}
        <div class="empty">select an operation to see its log</div>
      {/if}
    </div>
  </div>
</SideDrawer>

<style>
  .head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 10px; }
  .eyebrow { font: 700 10px var(--font-mono); text-transform: uppercase; letter-spacing: 0.08em; color: var(--ink-faint); }
  .close { width: 22px; height: 22px; border-radius: 50%; background: var(--panel-2); display: flex; align-items: center; justify-content: center; cursor: pointer; color: var(--ink-dim); font-size: 14px; }
  h3 { font-size: 20px; color: var(--ink); margin-bottom: 20px; }

  .ops { display: flex; gap: 16px; height: calc(100% - 90px); }
  .ops-list { flex: 0 0 220px; display: flex; flex-direction: column; gap: 4px; overflow-y: auto; }
  .ops-row {
    display: flex; align-items: center; gap: 8px; padding: 8px 10px; border-radius: 5px; cursor: pointer;
    font: 500 11.5px var(--font-mono); color: var(--ink-dim); border: 1px solid transparent;
  }
  .ops-row:hover { background: var(--panel-2); }
  .ops-row.active { background: var(--panel-2); border-color: var(--line-strong); color: var(--ink); }
  .ops-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .dot { width: 8px; height: 8px; border-radius: 50%; flex: 0 0 auto; background: var(--ink-faint); }
  .dot.running { background: var(--accent); animation: pulse 1s ease-in-out infinite; }
  .dot.done { background: var(--success, #6fdc8c); }
  .dot.error { background: var(--danger); }
  @keyframes pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.4; } }

  .ops-log { flex: 1; min-width: 0; }
  .ops-log pre {
    height: 100%; margin: 0; background: #000; color: #b8ffb8; padding: 12px; border-radius: 6px;
    font: 400 11px var(--font-mono); overflow: auto; white-space: pre-wrap; word-break: break-all;
  }
  .empty { font: 500 12px var(--font-mono); color: var(--ink-faint); font-style: italic; padding: 10px; }
</style>
