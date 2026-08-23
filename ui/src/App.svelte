<script>
  import Header from './lib/Header.svelte';
  import GraphView from './lib/GraphView.svelte';
  import Drawer from './lib/Drawer.svelte';
  import Placeholder from './lib/Placeholder.svelte';
  import RunControls from './lib/RunControls.svelte';

  let universe = $state(null);
  let error = $state(null);
  let activeTab = $state('repos');
  let currentFlow = $state(null);
  let selectedNode = $state(null);
  let runs = $state([]);
  let selectedRunId = $state(null);
  let runsPoll = null;

  let workspaces = $state([]);
  let currentWorkspaceId = $state(
    new URLSearchParams(location.search).get('workspace') || localStorage.getItem('fghj:workspace') || null,
  );

  function withWs(path) {
    if (!currentWorkspaceId) return path;
    const sep = path.includes('?') ? '&' : '?';
    return `${path}${sep}workspace=${encodeURIComponent(currentWorkspaceId)}`;
  }

  async function loadWorkspaces() {
    try {
      const res = await fetch('/workspaces');
      workspaces = await res.json();
    } catch (e) {
      // best-effort; picker just stays empty
    }
  }
  loadWorkspaces();

  function selectWorkspace(id) {
    currentWorkspaceId = id;
    localStorage.setItem('fghj:workspace', id);
    universe = null;
    error = null;
    selectedNode = null;
    runs = [];
    selectedRunId = null;
    load();
  }

  async function load() {
    try {
      const res = await fetch(withWs('/universe.json'));
      const data = await res.json();
      if (data.error) throw new Error(data.error);
      universe = data;
      const flows = [...new Set(data.nodes.flatMap((n) => n.flows))].sort();
      if (!currentFlow || !flows.includes(currentFlow)) currentFlow = flows[0] ?? null;
    } catch (e) {
      error = String(e);
    }
  }
  load();

  async function loadRuns() {
    try {
      const res = await fetch(withWs('/runs'));
      runs = await res.json();
      if (!selectedRunId && runs.length) selectedRunId = runs[0].run_id;
      if (selectedRunId && !runs.find((r) => r.run_id === selectedRunId)) {
        selectedRunId = runs.length ? runs[0].run_id : null;
      }
    } catch (e) {
      // best-effort polling; ignore transient failures
    }
  }

  async function startRun(spec) {
    const res = await fetch(withWs('/runs'), { method: 'POST', body: JSON.stringify(spec) });
    const state = await res.json();
    if (!state.error) selectedRunId = state.run_id;
    await loadRuns();
  }

  async function stopRun(runId) {
    await fetch(withWs(`/runs/${runId}/stop`), { method: 'POST' });
    await loadRuns();
  }

  async function pullAll() {
    await fetch(withWs('/pull-all'), { method: 'POST' });
  }

  async function pullAllStatus() {
    const res = await fetch(withWs('/pull-all/status'));
    if (!res.ok) return null;
    return await res.json();
  }

  async function onPullAllComplete() {
    await load();
  }

  async function downloadNode(nodeId) {
    await fetch(withWs(`/pull/${nodeId}`), { method: 'POST' });
  }

  async function pullStatus(nodeId) {
    const res = await fetch(withWs(`/pull/${nodeId}/status`));
    if (!res.ok) return null;
    return await res.json();
  }

  async function onDownloadComplete(nodeId) {
    await load();
    selectedNode = universe.nodes.find((n) => n.id === nodeId) ?? null;
  }

  async function fetchLogs(nodeId) {
    if (!selectedRunId) return '';
    const res = await fetch(withWs(`/runs/${selectedRunId}/nodes/${nodeId}/logs`));
    const data = await res.json();
    return data.logs ?? data.error ?? '';
  }

  $effect(() => {
    if (activeTab === 'containers') {
      loadRuns();
      runsPoll = setInterval(loadRuns, 1000);
      return () => clearInterval(runsPoll);
    }
  });

  let graphPoll = null;
  $effect(() => {
    if (activeTab === 'repos') {
      graphPoll = setInterval(load, 3000);
      return () => clearInterval(graphPoll);
    }
  });

  let selectedRun = $derived(runs.find((r) => r.run_id === selectedRunId) ?? null);
  let runContainers = $derived.by(() => {
    if (!selectedRun) return {};
    const map = {};
    for (const c of selectedRun.containers) map[c.node_id] = c;
    return map;
  });
  let liveInfo = $derived(selectedNode ? runContainers[selectedNode.id] : null);

  let flowNames = $derived(universe ? [...new Set(universe.nodes.flatMap((n) => n.flows))].sort() : []);

  // Repos tab: which repo requires which other repo. The currently selected
  // flow is highlighted (border/edge color), not filtered — every known repo
  // always renders, per the fog-of-war model.
  let reposGraph = $derived.by(() => {
    if (!universe) return null;
    const nodes = universe.nodes.filter((n) => n.kind === 'service');
    const ids = new Set(nodes.map((n) => n.id));
    const edges = universe.edges.filter((e) => e.kind === 'depends-on' && ids.has(e.from) && ids.has(e.to));
    return { nodes, edges };
  });

  // Actual tab: services + infra, everything the daemon would eventually run.
  let containersGraph = $derived.by(() => {
    if (!universe) return null;
    const nodes = universe.nodes;
    const ids = new Set(nodes.map((n) => n.id));
    const edges = universe.edges.filter((e) => e.kind !== 'shared-infra' && ids.has(e.from) && ids.has(e.to));
    return { nodes, edges };
  });

  let hasWarnings = $derived(universe ? universe.warnings.length > 0 : false);
</script>

<div style="position:relative;height:100vh;width:100vw;overflow:auto;background:var(--bg);color:var(--ink)">
  <Header
    flows={flowNames}
    currentFlow={currentFlow}
    hasConflict={hasWarnings}
    activeTab={activeTab}
    onSelectFlow={(f) => (currentFlow = f)}
    onSelectTab={(t) => (activeTab = t)}
    onPullAll={pullAll}
    onPullAllStatus={pullAllStatus}
    onPullAllComplete={onPullAllComplete}
    workspaces={workspaces}
    currentWorkspaceId={currentWorkspaceId}
    onOpenWorkspaces={loadWorkspaces}
    onSelectWorkspace={selectWorkspace}
  />

  <div style="padding-top:90px">
    {#if error}
      <pre style="color:var(--danger);padding:20px;font-family:var(--font-mono)">{error}</pre>
    {:else if !universe}
      <div class="body-sm" style="padding:20px">resolving graph…</div>
    {:else}
      {#if universe.warnings.length}
        <div style="padding:0 40px;display:flex;flex-direction:column;gap:6px;margin-bottom:4px">
          {#each universe.warnings as w}
            <div class="warning-banner"><span class="tag">warning</span><span>{w}</span></div>
          {/each}
        </div>
      {/if}

      {#if activeTab === 'repos'}
        <GraphView graph={reposGraph} {currentFlow} mode="repos" onSelectNode={(n) => (selectedNode = n)} />
      {:else if activeTab === 'containers'}
        <Placeholder
          eyebrow="Actual — live container state"
          text="Start the default environment to build and run every service/infra as real Docker containers on an isolated workspace network, or start a named review run that overrides one service to a different branch alongside the rest running normally. Domain-based access from the browser still requires the future fghj daemon — for now, open a running service via its published localhost port below."
        >
          <RunControls
            {runs}
            serviceIds={universe.nodes.filter((n) => n.kind === 'service').map((n) => n.id)}
            onStart={startRun}
            onStop={stopRun}
          />
          <GraphView graph={containersGraph} {currentFlow} mode="containers" {runContainers} onSelectNode={(n) => (selectedNode = n)} />
        </Placeholder>
      {:else}
        <Placeholder
          eyebrow="Config — secrets, split-DNS, root CA"
          text="Not available yet. This view will surface per-service env vars, the split-DNS table, and issued local TLS certs — once the superdaemon subsystems from the spec are implemented. None of that exists yet, so there is nothing real to show here."
        />
      {/if}
    {/if}
  </div>

  {#if selectedNode}
    <Drawer
      node={selectedNode}
      onClose={() => (selectedNode = null)}
      {liveInfo}
      onFetchLogs={fetchLogs}
      onDownload={downloadNode}
      onPullStatus={pullStatus}
      onDownloadComplete={onDownloadComplete}
    />
  {/if}
</div>

<style>
  :global(.warning-banner) {
    display: flex; align-items: center; gap: 10px; background: var(--warning-bg); border: 1px solid var(--warning);
    color: var(--warning); padding: 8px 12px; border-radius: 6px; font: 500 11.5px var(--font-mono); max-width: 760px;
  }
  :global(.warning-banner .tag) {
    font: 700 9.5px/1 var(--font-mono); text-transform: uppercase; letter-spacing: 0.06em; background: var(--danger);
    color: var(--bg); padding: 3px 6px; border-radius: 3px; flex: 0 0 auto;
  }
</style>
