<script>
  let {
    flows,
    recentFlows,
    singleRepoFlows,
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
    // Repos not on disk yet, workspace-wide and in `currentFlow`. `null`
    // until the graph has loaded.
    missingAll = null,
    missingInFlow = null,
    onSwitchFlow,
    envBusy,
    // Opens the drawer on a tab: 'downloads' or 'containers' for this
    // workspace's jobs, 'logs' for fghjd's.
    onOpenDrawer,
    workspaces,
    currentWorkspaceId,
    onOpenWorkspaces,
    onSelectWorkspace,
    // `null` when the workspace has no seeds, so there's nothing to show.
    showSeeds = null,
    onToggleSeeds,
  } = $props();
  let menuOpen = $state(false);
  let wsMenuOpen = $state(false);
  let actionsOpen = $state(false);
  let pulling = $state(false);
  let pullState = $state(null); // null | 'running' | 'done' | 'error'
  let pollTimer = null;

  // The flow-scoped action button: git ("Pull flow") on the Repos tab,
  // docker ("Switch to flow") on the Actual tab — same currentFlow, different verb
  // depending which half of fghj's model that tab represents. A switch (or a
  // first node start) keeps running long after its request returns, and every
  // node action is refused until it ends: `envBusy` is that.
  let flowPulling = $state(false);
  let flowPullState = $state(null); // null | 'running' | 'done' | 'error'
  let flowPollTimer = null;
  let switchingFlow = $state(false);
  // How the last finished download went, flow or all, shown by the menu.
  let pullResult = $derived(
    [flowPullState, pullState].find((s) => s === 'done' || s === 'error') ?? null,
  );

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

  async function switchFlow() {
    if (!onSwitchFlow || !currentFlow || switchingFlow) return;
    switchingFlow = true;
    try {
      // Idempotent server-side: once the environment is this flow,
      // clicking again changes nothing.
      await onSwitchFlow(currentFlow);
    } finally {
      switchingFlow = false;
    }
  }

  $effect(() => {
    return () => {
      if (flowPollTimer) clearInterval(flowPollTimer);
    };
  });

  // Each action closes the menu first, so its progress shows on the trigger.
  function act(fn) {
    actionsOpen = false;
    fn();
  }

  function pick(flow) {
    menuOpen = false;
    onSelectFlow(flow);
  }

  // Recent first, then flows spanning repos, then single-repo ones, which are
  // mostly parts of the others. Typing narrows all three.
  let flowFilter = $state('');
  let flowSections = $derived.by(() => {
    const needle = flowFilter.trim().toLowerCase();
    const shown = (f) => !needle || f.toLowerCase().includes(needle);
    const recent = (recentFlows ?? []).filter((f) => flows.includes(f));
    const rest = flows.filter((f) => !recent.includes(f));
    return [
      { title: 'recent', flows: recent.filter(shown) },
      { title: 'flows', flows: rest.filter((f) => !singleRepoFlows?.has(f) && shown(f)) },
      { title: 'single repo', flows: rest.filter((f) => singleRepoFlows?.has(f) && shown(f)) },
    ].filter((s) => s.flows.length);
  });

  function openFlowMenu() {
    flowFilter = '';
    menuOpen = !menuOpen;
  }

  function filterKey(e) {
    if (e.key === 'Escape') menuOpen = false;
    if (e.key === 'Enter') {
      const first = flowSections[0]?.flows[0];
      if (first) pick(first);
    }
  }

  function focus(el) {
    el.focus();
  }

  // Closes a menu on a press anywhere outside `node` (which holds its button
  // too, so the button still toggles) and on Escape.
  function dismiss(node, close) {
    const onPress = (e) => {
      if (!node.contains(e.target)) close();
    };
    const onKey = (e) => {
      if (e.key === 'Escape') close();
    };
    document.addEventListener('pointerdown', onPress, true);
    document.addEventListener('keydown', onKey);
    return {
      update(next) {
        close = next;
      },
      destroy() {
        document.removeEventListener('pointerdown', onPress, true);
        document.removeEventListener('keydown', onKey);
      },
    };
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

  <div class="flow-picker" use:dismiss={() => (wsMenuOpen = false)}>
    <button class="flow-btn ws-btn" onclick={toggleWorkspaceMenu} title="switch workspace: {currentWorkspaceLabel}">
      <span class="name">{currentWorkspaceLabel}</span>
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

  <!-- Flows, tabs and actions all act on a workspace; with none wired there is nothing for them to do. -->
  {#if currentWorkspaceId}
  <div class="divider"></div>

  <div class="flow-picker" use:dismiss={() => (menuOpen = false)}>
    <button class="flow-btn flow-name-btn" onclick={openFlowMenu} title={currentFlow}>
      <span class="name">{currentFlow ?? '…'}</span>
      <span style="width:6px;height:6px;background:{hasConflict ? 'var(--danger)' : 'var(--success)'}"></span>
    </button>
    {#if menuOpen}
      <div class="flow-menu flows">
        <input
          class="flow-filter"
          type="text"
          placeholder="filter flows…"
          bind:value={flowFilter}
          onkeydown={filterKey}
          spellcheck="false"
          use:focus
        />
        <div class="flow-list">
          {#each flowSections as section (section.title)}
            <div class="flow-section">{section.title}</div>
            {#each section.flows as f (f)}
              <div class="flow-row" class:current={f === currentFlow} onclick={() => pick(f)}>{f}</div>
            {/each}
          {:else}
            <div class="flow-empty">nothing matches</div>
          {/each}
        </div>
      </div>
    {/if}
  </div>

  <div class="divider"></div>
  <div class="view-switch">
    {#each [['repos', 'Repos'], ['containers', 'Actual'], ['config', 'Config']] as [id, label]}
      <div class="view-tab" class:active={activeTab === id} onclick={() => onSelectTab(id)}>{label}</div>
    {/each}
  </div>

  {/if}
  <div class="divider"></div>
  <div class="help-btn" onclick={() => onOpenDrawer('logs')} title="fghjd telemetry (logs, pull queue, doctor, help)">⚡</div>

  <!-- Per-tab actions go last, so switching tabs never moves the rest. -->
  {#if currentWorkspaceId && (activeTab === 'repos' || activeTab === 'containers')}
    <div class="divider"></div>
    <div class="flow-picker pull-wrap" use:dismiss={() => (actionsOpen = false)}>
      <button class="pull-btn actions-btn" onclick={() => (actionsOpen = !actionsOpen)} aria-expanded={actionsOpen}>
        {#if activeTab === 'repos' && (pulling || flowPulling)}<span class="spinner"></span> downloading…
        {:else if activeTab === 'containers' && (switchingFlow || envBusy)}<span class="spinner"></span> switching…
        {:else}Actions ▾{/if}
      </button>
      {#if activeTab === 'repos'}
        {#if pullResult}
          <span
            class="pull-result"
            class:ok={pullResult === 'done'}
            class:error={pullResult === 'error'}
            onclick={() => onOpenDrawer('downloads')}
            title="show downloads">{pullResult === 'done' ? '✓' : '✕'}</span
          >
        {/if}
      {/if}
      {#if actionsOpen}
        <div class="flow-menu actions-menu">
          {#if activeTab === 'repos'}
            <button class="flow-row" onclick={() => act(pullFlow)} disabled={!currentFlow || flowPulling || missingInFlow === 0} title="clone every not-yet-downloaded repo in this flow">
              {missingInFlow === 0 ? 'Flow downloaded' : `Download flow${missingInFlow ? ` (${missingInFlow})` : ''}`}
            </button>
            <button class="flow-row" onclick={() => act(pullAll)} disabled={pulling || missingAll === 0} title="clone every not-yet-downloaded repo">
              {missingAll === 0 ? 'All downloaded' : `Download all${missingAll ? ` (${missingAll})` : ''}`}
            </button>
            <button class="flow-row secondary" onclick={() => act(() => onOpenDrawer('downloads'))}>Show downloads</button>
          {:else}
            <button
              class="flow-row"
              onclick={() => act(switchFlow)}
              disabled={!currentFlow || switchingFlow || envBusy}
              title="start this flow's containers and stop (not remove) every other running one"
            >
              Switch to flow
            </button>
            <button class="flow-row secondary" onclick={() => act(() => onOpenDrawer('containers'))}>Show container activity</button>
          {/if}
        </div>
      {/if}
    </div>
  {/if}
  {#if currentWorkspaceId && activeTab === 'containers'}
    {#if showSeeds != null}
      <button
        class="pull-btn seeds-btn"
        class:on={showSeeds}
        aria-pressed={showSeeds}
        onclick={onToggleSeeds}
        title={showSeeds
          ? 'hide seeds: they fill a store with data once, and nothing waits on them'
          : "show seeds: they fill a store with data once, and nothing waits on them, so they're hidden by default"}
      >
        Seeds
      </button>
    {/if}
  {/if}
</div>


<style>
  .topbar {
    position: fixed; top: 16px; left: 16px; z-index: 50; display: flex; align-items: center; gap: 10px;
    background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 8px 10px;
    box-shadow: 0 12px 32px -12px rgba(19,27,46,0.18);
  }
  .logo { display: flex; align-items: center; gap: 6px; }
  .logo-mark { width: 18px; height: 18px; background: var(--accent); display: flex; align-items: center; justify-content: center; font: 800 11px var(--font-mono); color: var(--bg); }
  .divider { width: 1px; height: 18px; background: var(--line); }

  .flow-picker { position: relative; }
  .flow-btn {
    display: flex; align-items: center; gap: 7px; padding: 6px 10px; border: 1px solid var(--line-strong);
    border-radius: 4px; cursor: pointer; font: 600 12px var(--font-mono); color: var(--ink); background: none;
  }
  /* Fixed widths, so picking a longer or shorter name moves nothing after it. */
  .ws-btn { width: 150px; }
  .flow-name-btn { width: 240px; justify-content: space-between; }
  .flow-btn .name { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .flow-btn > span:not(.name) { flex: 0 0 auto; }
  .flow-menu {
    position: absolute; top: calc(100% + 6px); left: 0; background: var(--panel-2); border: 1px solid var(--line-strong);
    border-radius: 6px; box-shadow: 0 12px 32px -8px rgba(19,27,46,0.16); z-index: 50; min-width: 200px; overflow: hidden;
  }
  .flow-row { padding: 9px 12px; cursor: pointer; font: 600 12px var(--font-mono); border-bottom: 1px solid var(--line); color: var(--ink); }
  .flow-row:hover { background: var(--panel); }
  .flow-row.current { color: var(--accent); }
  .flow-menu.flows { min-width: 260px; }
  .flow-filter {
    width: 100%; box-sizing: border-box; padding: 8px 12px; border: none; border-bottom: 1px solid var(--line-strong);
    background: var(--panel); font: 500 12px var(--font-mono); color: var(--ink);
  }
  .flow-filter:focus { outline: none; }
  .flow-list { max-height: 60vh; overflow-y: auto; }
  .flow-section {
    padding: 8px 12px 4px; font: 700 9.5px var(--font-mono); text-transform: uppercase;
    letter-spacing: 0.08em; color: var(--ink-faint); border-bottom: 1px solid var(--line);
  }
  .flow-empty { padding: 9px 12px; font: 500 12px var(--font-mono); color: var(--ink-faint); font-style: italic; }

  .pull-wrap { position: relative; display: flex; align-items: center; gap: 6px; }
  .pull-btn {
    padding: 6px 10px; border: 1px solid var(--line-strong); border-radius: 4px; cursor: pointer;
    font: 700 11px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em; color: var(--ink); background: none;
  }
  .pull-btn:disabled { opacity: 0.6; cursor: default; }
  .seeds-btn { color: var(--ink-faint); }
  .seeds-btn.on { color: var(--success); border-color: var(--success); background: var(--success-bg); }
  .spinner {
    display: inline-block; width: 9px; height: 9px; border-radius: 50%;
    border: 2px solid var(--line-strong); border-top-color: var(--ink);
    animation: spin 0.7s linear infinite; vertical-align: middle; margin-right: 2px;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  .pull-result { font: 700 13px var(--font-mono); cursor: pointer; }
  .pull-result.ok { color: var(--success); }
  .pull-result.error { color: var(--danger); }
  .actions-btn { min-width: 120px; }
  .actions-menu { left: auto; right: 0; min-width: 240px; }
  .actions-menu .flow-row { display: block; width: 100%; text-align: left; background: none; border: none; border-bottom: 1px solid var(--line); }
  .actions-menu .flow-row:last-child { border-bottom: none; }
  .actions-menu .flow-row:disabled { color: var(--ink-faint); cursor: default; background: none; }
  .actions-menu .secondary { color: var(--ink-dim); font-weight: 500; }

  .view-switch { display: flex; padding: 2px; background: var(--panel-2); border-radius: 6px; gap: 2px; }
  .view-tab { padding: 6px 14px; border-radius: 4px; font: 700 11px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer; color: var(--ink-faint); }
  .view-tab.active { background: var(--accent); color: var(--bg); }

  .help-btn { width: 22px; height: 22px; border-radius: 50%; background: var(--panel-2); display: flex; align-items: center; justify-content: center; font: 700 12px var(--font-mono); color: var(--ink-dim); cursor: pointer; }

</style>
