<script>
  import Header from './lib/Header.svelte';
  import GraphView from './lib/GraphView.svelte';
  import Drawer from './lib/Drawer.svelte';
  import Placeholder from './lib/Placeholder.svelte';
  import RunControls from './lib/RunControls.svelte';
  import OperationsDrawer from './lib/OperationsDrawer.svelte';

  let universe = $state(null);
  let error = $state(null);
  let activeTab = $state('repos');
  let currentFlow = $state(null);
  let selectedNode = $state(null);
  let opsOpen = $state(false);
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

  async function pullFlow(flow) {
    await fetch(withWs(`/pull-all?flow=${encodeURIComponent(flow)}`), { method: 'POST' });
  }

  async function pullFlowStatus(flow) {
    const res = await fetch(withWs(`/pull-all/status?flow=${encodeURIComponent(flow)}`));
    if (!res.ok) return null;
    return await res.json();
  }

  async function runFlow(flow) {
    await startRun({ run_id: null, overrides: {}, flow });
  }

  async function listPullJobs() {
    const res = await fetch(withWs('/pull-jobs'));
    if (!res.ok) return [];
    return await res.json();
  }

  async function downloadNode(nodeId) {
    await fetch(withWs(`/pull/${encodeURIComponent(nodeId)}`), { method: 'POST' });
  }

  async function pullStatus(nodeId) {
    const res = await fetch(withWs(`/pull/${encodeURIComponent(nodeId)}/status`));
    if (!res.ok) return null;
    return await res.json();
  }

  async function onDownloadComplete(nodeId) {
    await load();
    selectedNode = universe.nodes.find((n) => n.id === nodeId) ?? null;
  }

  async function fetchLogs(nodeId) {
    if (!selectedRunId) return '';
    const res = await fetch(withWs(`/runs/${selectedRunId}/nodes/${encodeURIComponent(nodeId)}/logs`));
    const data = await res.json();
    return data.logs ?? data.error ?? '';
  }

  function logStreamUrl(nodeId) {
    if (!selectedRunId) return null;
    return withWs(`/runs/${selectedRunId}/nodes/${encodeURIComponent(nodeId)}/logs/stream`);
  }

  // Recent Start/Stop/Delete outcomes, newest first — surfaced in the
  // Drawer so a failed action (e.g. a stale node id from a graph that
  // changed shape after the click, or a real Docker error) is visible
  // instead of a silent no-op. Capped rather than persisted: this is
  // session-scoped operator feedback, not an audit log.
  let actionLog = $state([]);

  function logAction(nodeId, action, ok, message) {
    actionLog = [
      { id: `${Date.now()}-${Math.random()}`, time: Date.now(), nodeId, action, ok, message },
      ...actionLog,
    ].slice(0, 50);
  }

  // Start/Stop/Delete run through one workspace-wide queue, not a per-node
  // busy flag: the Drawer is a fresh component instance every time it's
  // opened (Svelte destroys it on close), so any "am I busy" state that
  // lived inside Drawer itself was lost the moment you closed and reopened
  // it, silently un-disabling the buttons mid-request. Living here instead
  // means it survives the Drawer's own lifecycle. One job in flight at a
  // time (rather than one per node) also means two actions can never race
  // each other's Docker/network side effects on the same run.
  let actionQueue = $state([]);
  let activeAction = $state(null);

  async function processQueue() {
    if (activeAction || !actionQueue.length) return;
    const job = actionQueue[0];
    activeAction = job;
    let ok = false;
    let message = '';
    try {
      const res = await fetch(withWs(job.path), { method: 'POST' });
      let body = null;
      try {
        body = await res.json();
      } catch (e) {
        // non-JSON (or empty) body is fine on success; nothing to parse
      }
      ok = res.ok && !body?.error;
      message = body?.error || (ok ? `${job.action} succeeded` : `HTTP ${res.status}`);
    } catch (e) {
      message = String(e);
    }
    logAction(job.nodeId, job.action, ok, message);
    await loadRuns();
    actionQueue = actionQueue.slice(1);
    activeAction = null;
    processQueue();
  }

  function enqueueNodeAction(nodeId, action, path) {
    actionQueue = [...actionQueue, { id: `${Date.now()}-${Math.random()}`, nodeId, action, path }];
    processQueue();
  }

  let workspaceBusy = $derived(activeAction !== null || actionQueue.length > 0);

  function startNode(nodeId) {
    if (!selectedRunId) return;
    enqueueNodeAction(nodeId, 'start', `/runs/${selectedRunId}/nodes/${encodeURIComponent(nodeId)}/start`);
  }

  function stopNode(nodeId) {
    if (!selectedRunId) return;
    enqueueNodeAction(nodeId, 'stop', `/runs/${selectedRunId}/nodes/${encodeURIComponent(nodeId)}/stop`);
  }

  function deleteNode(nodeId) {
    if (!selectedRunId) return;
    enqueueNodeAction(nodeId, 'delete', `/runs/${selectedRunId}/nodes/${encodeURIComponent(nodeId)}/delete`);
  }

  $effect(() => {
    if (activeTab === 'containers') {
      loadRuns();
      runsPoll = setInterval(loadRuns, 1000);
      return () => clearInterval(runsPoll);
    }
  });

  // Kept running regardless of which tab is active — not just 'repos' — so
  // a node whose id changes shape after being resolved for real (a
  // not-yet-downloaded dependency's placeholder id, once its repo is
  // actually cloned and its own .fghj.yaml parsed, becomes a different,
  // proper `service.repo` id) doesn't leave the Containers tab pointing at
  // a graph that no longer has that node, silently 404ing every Start/Stop
  // click against it.
  $effect(() => {
    const id = setInterval(load, 3000);
    return () => clearInterval(id);
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
  //
  // One box per *repo*, not per service: two services declared in the same
  // repo (e.g. aikido-core's `php` and `vite`) share one checkout, one git
  // branch/dirty state, and one clone/pull lifecycle, so they collapse into
  // a single node here. `local_path` (present once a repo is actually on
  // disk) is the grouping key, since it's the checkout identity — `repo`
  // alone doesn't group already-downloaded siblings any better and stub
  // (not-yet-downloaded) nodes have no `local_path` yet, so they fall back
  // to their own `id` and stay their own single-member group.
  let reposGraph = $derived.by(() => {
    if (!universe) return null;
    const services = universe.nodes.filter((n) => n.kind === 'service');
    const groupKey = (n) => n.local_path ?? n.id;

    const groups = new Map();
    for (const n of services) {
      const key = groupKey(n);
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(n);
    }

    const nodeToGroup = new Map();
    const nodes = [];
    for (const [key, members] of groups) {
      for (const m of members) nodeToGroup.set(m.id, key);
      const repr = members[0];
      nodes.push({
        id: key,
        label: key,
        kind: 'service',
        repo: repr.repo,
        branch: repr.branch,
        dirty: members.some((m) => m.dirty),
        downloaded: members.every((m) => m.downloaded),
        domain_scope: repr.domain_scope,
        local_path: repr.local_path,
        domain: repr.domain,
        flows: [...new Set(members.flatMap((m) => m.flows))],
        services: members.map((m) => m.label).sort(),
      });
    }

    // Cross-repo edges only — an edge between two services in the same
    // group (e.g. vite -> php) is internal to that repo and has nothing to
    // do with which *other* repos this one depends on.
    const edgeMap = new Map();
    for (const e of universe.edges) {
      if (e.kind !== 'depends-on') continue;
      const from = nodeToGroup.get(e.from);
      const to = nodeToGroup.get(e.to);
      if (!from || !to || from === to) continue;
      const key = `${from}|${to}`;
      if (!edgeMap.has(key)) edgeMap.set(key, { from, to, kind: 'depends-on', flows: new Set() });
      e.flows.forEach((f) => edgeMap.get(key).flows.add(f));
    }
    const edges = [...edgeMap.values()].map((e) => ({ ...e, flows: [...e.flows] }));

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
    onPullFlow={pullFlow}
    onPullFlowStatus={pullFlowStatus}
    onPullFlowComplete={onPullAllComplete}
    onRunFlow={runFlow}
    onOpenOperations={() => (opsOpen = true)}
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
            onOpenOperations={() => (opsOpen = true)}
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

  {#if activeAction || actionQueue.length}
    <div class="action-banner">
      <span class="spinner"></span>
      {#if activeAction}
        <span>{activeAction.action}ing <b>{activeAction.nodeId}</b>…</span>
      {/if}
      {#if actionQueue.length > 1}
        <span class="queue-count">+{actionQueue.length - 1} queued</span>
      {/if}
    </div>
  {/if}

  {#if selectedNode}
    <Drawer
      node={selectedNode}
      onClose={() => (selectedNode = null)}
      {liveInfo}
      actionLog={actionLog.filter((a) => a.nodeId === selectedNode.id)}
      busy={workspaceBusy}
      runId={selectedRunId}
      onFetchLogs={fetchLogs}
      onLogStreamUrl={logStreamUrl}
      onDownload={downloadNode}
      onPullStatus={pullStatus}
      onDownloadComplete={onDownloadComplete}
      onStartNode={startNode}
      onStopNode={stopNode}
      onDeleteNode={deleteNode}
    />
  {/if}

  {#if opsOpen}
    <OperationsDrawer onClose={() => (opsOpen = false)} onListJobs={listPullJobs} />
  {/if}
</div>

<style>
  .action-banner {
    position: fixed; top: 96px; right: 24px; z-index: 50; display: flex; align-items: center; gap: 8px;
    background: var(--panel-2); border: 1px solid var(--line-strong); border-radius: 6px; padding: 8px 12px;
    font: 500 11.5px var(--font-mono); color: var(--ink); box-shadow: 0 4px 12px rgba(0, 0, 0, 0.25);
  }
  .action-banner b { color: var(--accent); }
  .action-banner .queue-count { color: var(--ink-faint); }
  .action-banner .spinner {
    display: inline-block; width: 9px; height: 9px; border-radius: 50%; flex: 0 0 auto;
    border: 2px solid var(--line-strong); border-top-color: var(--accent);
    animation: banner-spin 0.7s linear infinite;
  }
  @keyframes banner-spin { to { transform: rotate(360deg); } }
  :global(.warning-banner) {
    display: flex; align-items: center; gap: 10px; background: var(--warning-bg); border: 1px solid var(--warning);
    color: var(--warning); padding: 8px 12px; border-radius: 6px; font: 500 11.5px var(--font-mono); max-width: 760px;
  }
  :global(.warning-banner .tag) {
    font: 700 9.5px/1 var(--font-mono); text-transform: uppercase; letter-spacing: 0.06em; background: var(--danger);
    color: var(--bg); padding: 3px 6px; border-radius: 3px; flex: 0 0 auto;
  }
</style>
