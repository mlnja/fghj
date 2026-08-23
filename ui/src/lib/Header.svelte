<script>
  let {
    flows,
    currentFlow,
    hasConflict,
    activeTab,
    onSelectFlow,
    onSelectTab,
    onPullAll,
    onPullAllStatus,
    onPullAllComplete,
    onPullFlow,
    onPullFlowStatus,
    onPullFlowComplete,
    onRunFlow,
    onOpenOperations,
    workspaces,
    currentWorkspaceId,
    onOpenWorkspaces,
    onSelectWorkspace,
  } = $props();
  let menuOpen = $state(false);
  let wsMenuOpen = $state(false);
  let helpOpen = $state(false);
  let pulling = $state(false);
  let pullState = $state(null); // null | 'running' | 'done' | 'error'
  let pollTimer = null;

  // The flow-scoped action button: git ("Pull flow") on the Repos tab,
  // docker ("Run flow") on the Actual tab — same currentFlow, different verb
  // depending which half of fghj's model that tab represents.
  let flowPulling = $state(false);
  let flowPullState = $state(null); // null | 'running' | 'done' | 'error'
  let flowPollTimer = null;
  let runningFlow = $state(false);

  // Only tracks completion so the graph can be reloaded and the button's
  // spinner cleared — the actual log lives in the shared operations drawer.
  async function poll() {
    if (!onPullAllStatus) return;
    const s = await onPullAllStatus();
    if (!s) return;
    pullState = s.status;
    if (s.status !== 'running') {
      clearInterval(pollTimer);
      pollTimer = null;
      pulling = false;
      if (s.status === 'done' && onPullAllComplete) await onPullAllComplete();
    }
  }

  async function pullAll() {
    if (!onPullAll || pulling) return;
    pulling = true;
    pullState = 'running';
    await onPullAll();
    await poll();
    if (pullState === 'running') {
      pollTimer = setInterval(poll, 600);
    }
  }

  $effect(() => {
    return () => {
      if (pollTimer) clearInterval(pollTimer);
    };
  });

  async function flowPoll() {
    if (!onPullFlowStatus) return;
    const s = await onPullFlowStatus(currentFlow);
    if (!s) return;
    flowPullState = s.status;
    if (s.status !== 'running') {
      clearInterval(flowPollTimer);
      flowPollTimer = null;
      flowPulling = false;
      if (s.status === 'done' && onPullFlowComplete) await onPullFlowComplete();
    }
  }

  async function pullFlow() {
    if (!onPullFlow || flowPulling || !currentFlow) return;
    flowPulling = true;
    flowPullState = 'running';
    await onPullFlow(currentFlow);
    await flowPoll();
    if (flowPullState === 'running') {
      flowPollTimer = setInterval(flowPoll, 600);
    }
  }

  async function runFlow() {
    if (!onRunFlow || !currentFlow || runningFlow) return;
    runningFlow = true;
    try {
      // Idempotent server-side: only tops up whatever this flow still
      // needs, so clicking again when it's already running is a no-op.
      await onRunFlow(currentFlow);
    } finally {
      runningFlow = false;
    }
  }

  $effect(() => {
    return () => {
      if (flowPollTimer) clearInterval(flowPollTimer);
    };
  });

  function pick(flow) {
    menuOpen = false;
    onSelectFlow(flow);
  }

  function pickWorkspace(id) {
    wsMenuOpen = false;
    onSelectWorkspace(id);
  }

  function toggleWorkspaceMenu() {
    if (!wsMenuOpen && onOpenWorkspaces) onOpenWorkspaces();
    wsMenuOpen = !wsMenuOpen;
  }

  let currentWorkspaceLabel = $derived.by(() => {
    const w = (workspaces ?? []).find((w) => w.id === currentWorkspaceId);
    if (!w) return currentWorkspaceId ?? 'no workspace';
    const parts = String(w.workspace).split('/').filter(Boolean);
    return parts[parts.length - 1] ?? w.workspace;
  });
</script>

<div class="topbar">
  <div class="logo">
    <div class="logo-mark">f</div>
    <div class="stencil" style="font-size:15px;color:var(--ink)">fghj</div>
  </div>
  <div class="divider"></div>

  <div class="flow-picker">
    <button class="flow-btn" onclick={toggleWorkspaceMenu} title="switch workspace">
      <span>{currentWorkspaceLabel}</span>
    </button>
    {#if wsMenuOpen}
      <div class="flow-menu">
        {#each workspaces ?? [] as w}
          <div class="flow-row" onclick={() => pickWorkspace(w.id)}>{w.workspace}</div>
        {/each}
        {#if !(workspaces ?? []).length}
          <div class="flow-row" style="cursor:default;color:var(--ink-faint)">no workspaces yet</div>
        {/if}
      </div>
    {/if}
  </div>

  <div class="divider"></div>

  <div class="flow-picker">
    <button class="flow-btn" onclick={() => (menuOpen = !menuOpen)}>
      <span>{currentFlow ?? '…'}</span>
      <span style="width:6px;height:6px;background:{hasConflict ? 'var(--danger)' : 'var(--success)'}"></span>
    </button>
    {#if menuOpen}
      <div class="flow-menu">
        {#each flows as f}
          <div class="flow-row" onclick={() => pick(f)}>{f}</div>
        {/each}
      </div>
    {/if}
  </div>

  {#if activeTab === 'repos'}
    <button class="pull-btn" onclick={pullFlow} disabled={!currentFlow || flowPulling} title="clone every not-yet-downloaded repo in this flow">
      {#if flowPulling}<span class="spinner"></span> pulling flow…{:else}Pull flow{/if}
    </button>
    {#if flowPullState && flowPullState !== 'running'}
      <span class="pull-result" class:ok={flowPullState === 'done'} class:error={flowPullState === 'error'}
        >{flowPullState === 'done' ? '✓' : '✕'}</span
      >
    {/if}
  {:else if activeTab === 'containers'}
    <button class="pull-btn" onclick={runFlow} disabled={!currentFlow || runningFlow} title="ensure this flow's containers are running (leaves everything else untouched)">
      {#if runningFlow}<span class="spinner"></span> running…{:else}Run flow{/if}
    </button>
  {/if}

  <div class="divider"></div>
  <div class="pull-wrap">
    <button class="pull-btn" onclick={pullAll} disabled={pulling}>
      {#if pulling}<span class="spinner"></span> pulling…{:else}Pull all{/if}
    </button>
    {#if pullState && pullState !== 'running'}
      <span class="pull-result" class:ok={pullState === 'done'} class:error={pullState === 'error'}
        >{pullState === 'done' ? '✓' : '✕'}</span
      >
    {/if}
    <span class="ops-link" onclick={onOpenOperations} title="view pull/download operations queue">☰</span>
  </div>

  <div class="divider"></div>
  <div class="view-switch">
    {#each [['repos', 'Repos'], ['containers', 'Actual'], ['config', 'Config']] as [id, label]}
      <div class="view-tab" class:active={activeTab === id} onclick={() => onSelectTab(id)}>{label}</div>
    {/each}
  </div>

  <div class="divider"></div>
  <div class="help-btn" onclick={() => (helpOpen = true)}>?</div>
</div>

{#if helpOpen}
  <div class="modal-veil" onclick={() => (helpOpen = false)}>
    <div class="modal" onclick={(e) => e.stopPropagation()}>
      <h3>How to read the map</h3>
      <div class="line"><b style="color:var(--ink)">Solid line</b> — a <code>service</code> or <code>infra</code> dependency resolved from <code>fghj.yaml</code>. <b style="color:var(--ink)">Dashed</b> — a <code>shared-infra</code> reference to another service's resource.</div>
      <div class="line"><b style="color:var(--ink)">Tinted block</b> — infrastructure (postgres, redis): fixed image, no branch.</div>
      <div class="line">Click a node to see its resolved repo, branch and domain. Switch flows with the picker top-left — the selected flow's repos and edges highlight in accent color; every known repo still renders regardless of flow.</div>
      <div class="close" onclick={() => (helpOpen = false)}>Close</div>
    </div>
  </div>
{/if}

<style>
  .topbar {
    position: fixed; top: 16px; left: 16px; z-index: 50; display: flex; align-items: center; gap: 10px;
    background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 8px 10px;
    box-shadow: 0 12px 32px -12px rgba(0,0,0,0.6);
  }
  .logo { display: flex; align-items: center; gap: 6px; }
  .logo-mark { width: 18px; height: 18px; background: var(--accent); display: flex; align-items: center; justify-content: center; font: 800 11px var(--font-mono); color: var(--bg); }
  .divider { width: 1px; height: 18px; background: var(--line); }

  .flow-picker { position: relative; }
  .flow-btn {
    display: flex; align-items: center; gap: 7px; padding: 6px 10px; border: 1px solid var(--line-strong);
    border-radius: 4px; cursor: pointer; font: 600 12px var(--font-mono); color: var(--ink); background: none;
  }
  .flow-menu {
    position: absolute; top: calc(100% + 6px); left: 0; background: var(--panel-2); border: 1px solid var(--line-strong);
    border-radius: 6px; box-shadow: 0 12px 32px -8px rgba(0,0,0,0.5); z-index: 50; min-width: 200px; overflow: hidden;
  }
  .flow-row { padding: 9px 12px; cursor: pointer; font: 600 12px var(--font-mono); border-bottom: 1px solid var(--line); color: var(--ink); }
  .flow-row:hover { background: var(--panel); }

  .pull-wrap { position: relative; display: flex; align-items: center; gap: 6px; }
  .pull-btn {
    padding: 6px 10px; border: 1px solid var(--line-strong); border-radius: 4px; cursor: pointer;
    font: 700 11px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em; color: var(--ink); background: none;
  }
  .pull-btn:disabled { opacity: 0.6; cursor: default; }
  .spinner {
    display: inline-block; width: 9px; height: 9px; border-radius: 50%;
    border: 2px solid rgba(255,255,255,0.35); border-top-color: var(--ink);
    animation: spin 0.7s linear infinite; vertical-align: middle; margin-right: 2px;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  .pull-result { font: 700 13px var(--font-mono); }
  .pull-result.ok { color: var(--success); }
  .pull-result.error { color: var(--danger); }
  .ops-link { cursor: pointer; color: var(--ink-faint); font-size: 13px; }
  .ops-link:hover { color: var(--ink); }

  .view-switch { display: flex; padding: 2px; background: var(--panel-2); border-radius: 6px; gap: 2px; }
  .view-tab { padding: 6px 14px; border-radius: 4px; font: 700 11px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer; color: var(--ink-faint); }
  .view-tab.active { background: var(--accent); color: var(--bg); }

  .help-btn { width: 22px; height: 22px; border-radius: 50%; background: var(--panel-2); display: flex; align-items: center; justify-content: center; font: 700 12px var(--font-mono); color: var(--ink-dim); cursor: pointer; }

  .modal-veil { position: fixed; inset: 0; background: rgba(0,0,0,0.6); z-index: 80; display: flex; align-items: center; justify-content: center; }
  .modal { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 26px 28px; max-width: 460px; box-shadow: 0 24px 60px -20px rgba(0,0,0,0.7); }
  .modal h3 { font: 600 16px var(--font-body); color: var(--ink); margin: 0 0 14px; }
  .modal .line { font: 400 12.5px/1.5 var(--font-body); color: var(--ink-dim); margin-bottom: 10px; }
  .modal .close { margin-top: 18px; padding: 8px 0; text-align: center; border-radius: 4px; background: var(--panel-2); font: 700 11px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer; color: var(--ink); }
</style>
