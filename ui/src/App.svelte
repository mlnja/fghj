<script>
  import Header from './lib/Header.svelte';
  import GraphView from './lib/GraphView.svelte';
  import Drawer from './lib/Drawer.svelte';
  import RepoDrawer from './lib/RepoDrawer.svelte';
  import WorkspaceConfig from './lib/WorkspaceConfig.svelte';
  import TelemetryDrawer from './lib/TelemetryDrawer.svelte';
  import GraphSkeleton from './lib/GraphSkeleton.svelte';
  import Welcome from './lib/Welcome.svelte';

  let universe = $state(null);
  // fghjd answered, but with an error: the workspace failed to resolve.
  let error = $state(null);
  // fghjd didn't answer at all: restarting, upgrading, or stopped. Kept
  // apart from `error` because the cure is waiting, not editing a file.
  let backendDown = $state(false);
  let activeTab = $state('repos');
  let currentFlow = $state(null);
  let selectedNode = $state(null);
  // A repo card is a group keyed by checkout, not a node, so it gets its own
  // drawer. Held by id so the drawer follows the graph as it reloads.
  let selectedRepoId = $state(null);
  // `GET /repos`: `local_path` to git status, read only while the Repos tab
  // is open.
  let repoStatus = $state({});
  // `null` when closed, else the tab it opens on.
  let telemetryTab = $state(null);
  // The workspace's one environment (`GET /environment`), or null before
  // anything has been started.
  let env = $state(null);
  let envPoll = null;

  let workspaces = $state([]);
  // Whether `GET /workspaces` has answered once, so an empty list means
  // "none wired" rather than "not asked yet".
  let workspacesLoaded = $state(false);
  const WORKSPACE_KEY = 'fghj:workspace';
  let currentWorkspaceId = $state(
    new URLSearchParams(location.search).get('workspace') || localStorage.getItem(WORKSPACE_KEY) || null,
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
      workspacesLoaded = true;
      return true;
    } catch (e) {
      // best-effort; picker just stays empty
      return false;
    }
  }

  // Keeps the current workspace while it's still wired. Otherwise: the one
  // used last, if it still is, else the first. None wired leaves `null`, and
  // the welcome screen.
  function chooseWorkspace() {
    const ids = workspaces.map((w) => w.id);
    if (currentWorkspaceId && ids.includes(currentWorkspaceId)) {
      localStorage.setItem(WORKSPACE_KEY, currentWorkspaceId);
      setUrlWorkspace(currentWorkspaceId);
      return;
    }
    const stored = localStorage.getItem(WORKSPACE_KEY);
    const next = (stored && ids.includes(stored) ? stored : ids[0]) ?? null;
    if (next) {
      selectWorkspace(next);
    } else {
      currentWorkspaceId = null;
      setUrlWorkspace(null);
      universe = null;
      error = null;
      env = null;
    }
  }

  // The URL names the workspace on screen, so a reload comes back to it.
  function setUrlWorkspace(id) {
    const url = new URL(location.href);
    if (id) url.searchParams.set('workspace', id);
    else url.searchParams.delete('workspace');
    history.replaceState(null, '', url);
  }

  function selectWorkspace(id) {
    currentWorkspaceId = id;
    localStorage.setItem(WORKSPACE_KEY, id);
    setUrlWorkspace(id);
    universe = null;
    error = null;
    selectedNode = null;
    selectedRepoId = null;
    repoStatus = {};
    env = null;
    load();
  }

  // fghjd forgets the workspace at once and tears its containers and network
  // down in the background, volumes included. We move to whichever workspace is
  // left and open the Activity tab, where the teardown's log is.
  async function removeWorkspace(id) {
    const res = await fetch('/workspaces/remove', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id }),
    });
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error ?? `HTTP ${res.status}`);
    telemetryTab = 'activity';
    // A reload must not come back to the id that's gone.
    const url = new URL(location.href);
    url.searchParams.delete('workspace');
    history.replaceState(null, '', url);
    await loadWorkspaces();
    const next = workspaces[0]?.id;
    if (next) {
      activeTab = 'repos';
      selectWorkspace(next);
    } else {
      currentWorkspaceId = null;
      localStorage.removeItem(WORKSPACE_KEY);
      universe = null;
      env = null;
      load();
    }
  }

  // Polled (see the effect below), so every outcome has to be able to
  // undo the last one: a success clears both the error and the outage.
  async function load() {
    // Nothing to resolve yet: watch for a workspace to be wired instead.
    if (!currentWorkspaceId) {
      if (await loadWorkspaces()) {
        backendDown = false;
        if (workspaces.length) chooseWorkspace();
      } else {
        backendDown = true;
      }
      return;
    }
    let data;
    try {
      const res = await fetch(withWs('/universe.json'));
      // Not JSON means something other than fghjd's API answered.
      data = await res.json();
    } catch (e) {
      backendDown = true;
      return;
    }
    if (backendDown) {
      backendDown = false;
      // It may have failed while fghjd was away, leaving the picker empty.
      loadWorkspaces();
    }
    if (data.error) {
      // Maybe it was removed under us (`fghj workspaces rm`, another tab): then
      // move on to another one rather than show its error.
      if (await loadWorkspaces() && !workspaces.some((w) => w.id === currentWorkspaceId)) {
        chooseWorkspace();
        return;
      }
      error = data.error;
      return;
    }
    error = null;
    universe = data;
    const flows = data.flows ?? [];
    const recent = loadRecentFlows();
    if (!currentFlow || !flows.includes(currentFlow))
      currentFlow = recent.find((f) => flows.includes(f)) ?? flows[0] ?? null;
  }

  (async () => {
    if (await loadWorkspaces()) chooseWorkspace();
    if (currentWorkspaceId) load();
  })();

  // A poll skips its turn while its last request is still out. Otherwise
  // requests slower than the interval pile up, and fghjd works on every one.
  // Called directly, after an action, they always fetch.
  function polled(fn) {
    let inFlight = false;
    return async () => {
      if (inFlight) return;
      inFlight = true;
      try {
        await fn();
      } finally {
        inFlight = false;
      }
    };
  }
  const pollLoad = polled(load);
  const pollEnv = polled(loadEnv);
  const pollRepos = polled(loadRepos);

  async function loadEnv() {
    if (!currentWorkspaceId) return;
    try {
      const [envRes, recRes] = await Promise.all([
        fetch(withWs('/environment')),
        fetch(withWs('/traffic/recording')),
      ]);
      env = await envRes.json();
      if (recRes.ok) recordingNodes = (await recRes.json()).nodes ?? {};
    } catch (e) {
      // best-effort polling; ignore transient failures
    }
  }

  // Node id -> body mode, for every node whose traffic fghjd is recording.
  // Polled with `/environment` for the dot on the graph; recording itself
  // is held by fghjd and outlives the drawer.
  let recordingNodes = $state({});

  function trafficUrl(nodeId, rest) {
    return withWs(`/nodes/${encodeURIComponent(nodeId)}/traffic${rest}`);
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
    await loadRepos();
  }

  async function loadRepos() {
    if (!currentWorkspaceId) return;
    try {
      const res = await fetch(withWs('/repos'));
      if (res.ok) repoStatus = await res.json();
    } catch {
      // fghjd unreachable: the connection banner already says so.
    }
  }

  // `action` is `fetch` or `pull`.
  async function gitRepo(path, action) {
    try {
      const res = await fetch(withWs(`/repos/${encodeURIComponent(path)}/${action}`), { method: 'POST' });
      const out = await res.json();
      if (!res.ok) return { ok: false, output: out.error ?? `HTTP ${res.status}` };
      return out;
    } catch (e) {
      return { ok: false, output: String(e) };
    } finally {
      await loadRepos();
    }
  }

  async function pullFlow(flow) {
    await fetch(withWs(`/pull-all?flow=${encodeURIComponent(flow)}`), { method: 'POST' });
  }

  async function pullFlowStatus(flow) {
    const res = await fetch(withWs(`/pull-all/status?flow=${encodeURIComponent(flow)}`));
    if (!res.ok) return null;
    return await res.json();
  }

  // Starts the flow's containers and stops (not removes) everything else.
  // The flows switched to last, newest first, per workspace and in this
  // browser only: what the user actually runs is the best sign of which flows
  // matter, and nothing in the config has to say so.
  const RECENT_FLOWS = 5;
  let recentFlows = $state([]);

  function loadRecentFlows() {
    try {
      recentFlows = JSON.parse(localStorage.getItem(`fghj:recent-flows:${currentWorkspaceId}`)) ?? [];
    } catch {
      recentFlows = [];
    }
    return recentFlows;
  }

  function rememberFlow(flow) {
    recentFlows = [flow, ...recentFlows.filter((f) => f !== flow)].slice(0, RECENT_FLOWS);
    localStorage.setItem(`fghj:recent-flows:${currentWorkspaceId}`, JSON.stringify(recentFlows));
  }

  async function switchFlow(flow) {
    rememberFlow(flow);
    await fetch(withWs('/switch'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ flow }),
    });
    await loadEnv();
  }

  // Daemon-global, children included: the drawer scopes and nests them.
  async function listActivities() {
    const res = await fetch('/activities');
    if (!res.ok) return [];
    return await res.json();
  }

  // Daemon-global — never workspace-scoped, so no withWs().
  async function fetchDaemonLogs(afterSeq) {
    let path = '/daemon/logs?limit=500';
    if (afterSeq != null) path += `&after_seq=${afterSeq}`;
    const res = await fetch(path);
    if (!res.ok) return [];
    const data = await res.json();
    return data.entries ?? [];
  }

  async function fetchDaemonNetStatus() {
    const res = await fetch('/daemon/net-status');
    if (!res.ok) return null;
    return await res.json();
  }

  async function fetchDaemonDoctor() {
    const res = await fetch('/daemon/doctor');
    if (!res.ok) return null;
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
    const res = await fetch(withWs(`/nodes/${encodeURIComponent(nodeId)}/logs`));
    const data = await res.json();
    return data.logs ?? data.error ?? '';
  }

  function logStreamUrl(nodeId) {
    return withWs(`/nodes/${encodeURIComponent(nodeId)}/logs/stream`);
  }

  // Persisted-log-history support (see `store::WorkspaceDb`'s `logs` table):
  // `fetchLogGenerations` lists the current/previous generation for the
  // Drawer's picker; `fetchLogHistory` pages through one generation,
  // oldest-first, for both the initial load and scroll-triggered
  // infinite-scroll-back.
  async function fetchLogGenerations(nodeId) {
    const res = await fetch(
      withWs(`/nodes/${encodeURIComponent(nodeId)}/logs/generations`),
    );
    if (!res.ok) return [];
    const data = await res.json();
    return data.generations ?? [];
  }

  async function fetchLogHistory(nodeId, generation, beforeSeq) {
    let path = `/nodes/${encodeURIComponent(nodeId)}/logs/history?generation=${generation}`;
    if (beforeSeq != null) path += `&before_seq=${beforeSeq}`;
    const res = await fetch(withWs(path));
    if (!res.ok) return [];
    const data = await res.json();
    return data.lines ?? [];
  }

  // ArgoCD-style "events": what `fghjd` itself did during the last start or
  // stop of this node (image build, container creation, healthcheck wait,
  // ...), as opposed to `fetchLogs`/`fetchLogHistory`'s raw container
  // stdout/stderr. Only the current cycle of `action` is ever returned —
  // see `store::WorkspaceDb::begin_event_cycle`.
  async function fetchEvents(nodeId, action) {
    const res = await fetch(
      withWs(
        `/nodes/${encodeURIComponent(nodeId)}/events?action=${action}`,
      ),
    );
    if (!res.ok) return [];
    const data = await res.json();
    return data.events ?? [];
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
      const res = await fetch(withWs(job.path), {
        method: 'POST',
        // Only the debug-wait toggle sends a body; every other node action
        // is a bare POST and must stay one (the handlers don't read a body,
        // and a stray Content-Type would be noise in the access log).
        ...(job.body
          ? { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(job.body) }
          : {}),
      });
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
    await loadEnv();
    actionQueue = actionQueue.slice(1);
    activeAction = null;
    processQueue();
  }

  function enqueueNodeAction(nodeId, action, path, body) {
    actionQueue = [...actionQueue, { id: `${Date.now()}-${Math.random()}`, nodeId, action, path, body }];
    processQueue();
  }

  // `ContainerInfo.pending_action` is the actual source of truth for "what
  // is this node doing right now" — the reducer sets it when it accepts a
  // lifecycle request and clears it when the Docker call settles, and it's
  // what the reducer itself checks before rejecting a duplicate call, so it
  // can never disagree with reality the way a frontend-only guess could
  // (e.g. after a page reload, or from a second browser tab).
  // `activeAction`/`actionQueue` above stay only as an optimistic fill-in
  // for the ~1s gap between firing a request and the next `/environment` poll
  // picking up the backend's own flag.
  let backendPending = $derived(Object.values(env?.containers ?? {}).filter((c) => c.pending_action));
  const ACTION_GERUNDS = { start: 'starting', stop: 'stopping', delete: 'removing', reset: 'resetting' };
  let displayPending = $derived.by(() => {
    if (backendPending.length) return backendPending.map((c) => ({ nodeId: c.node_id, action: c.pending_action }));
    if (activeAction) return [{ nodeId: activeAction.nodeId, action: ACTION_GERUNDS[activeAction.action] ?? activeAction.action }];
    return [];
  });

  function startNode(nodeId) {
    enqueueNodeAction(nodeId, 'start', `/nodes/${encodeURIComponent(nodeId)}/start`);
  }

  function stopNode(nodeId) {
    enqueueNodeAction(nodeId, 'stop', `/nodes/${encodeURIComponent(nodeId)}/stop`);
  }

  function deleteNode(nodeId) {
    enqueueNodeAction(nodeId, 'delete', `/nodes/${encodeURIComponent(nodeId)}/delete`);
  }

  // Container-only "Reset": force a fresh container in one click while the
  // node is already Running, without touching its volumes. Same endpoint as
  // `startNode` — `restart_container` on the backend always stops+removes
  // the existing container and rebuilds/re-creates it regardless of whether
  // one was already running — this button just exposes that while Running,
  // which the Start control is deliberately hidden for (see
  // `nodeLifecycle` in Drawer.svelte for the full state/action legality
  // table).
  function resetNode(nodeId) {
    enqueueNodeAction(nodeId, 'reset', `/nodes/${encodeURIComponent(nodeId)}/start`);
  }

  // The per-container debug switch (`FGHJ_DEBUG_WAIT`). Goes through the
  // same queue as Start/Stop because it *is* a lifecycle action: the backend
  // records the new desire and then recreates the container to apply it, so
  // letting it race a concurrent Stop would be the same mistake.
  //
  // Deliberately not declarable in `.fghj.yaml` — that file is committed and
  // shared, and "halt before line 0 until a human attaches" pinned there
  // would block every teammate's start of this node. See
  // `concepts/debugging-in-containers.md`.
  function setDebugWait(nodeId, wait) {
    enqueueNodeAction(
      nodeId,
      wait ? 'debug-wait on' : 'debug-wait off',
      `/nodes/${encodeURIComponent(nodeId)}/debug-wait`,
      { wait },
    );
  }

  $effect(() => {
    if (activeTab === 'containers') {
      loadEnv();
      envPoll = setInterval(pollEnv, 1000);
      return () => clearInterval(envPoll);
    }
    // Slower: here it only feeds the stale-container mark.
    if (activeTab === 'repos') {
      loadEnv();
      envPoll = setInterval(pollEnv, 5000);
      return () => clearInterval(envPoll);
    }
  });

  // Git status is three git calls per repo, so not every second.
  $effect(() => {
    if (activeTab !== 'repos' || !currentWorkspaceId) return;
    loadRepos();
    const id = setInterval(pollRepos, 10000);
    return () => clearInterval(id);
  });

  // Kept running regardless of which tab is active — not just 'repos' — so
  // a node whose id changes shape after being resolved for real (a
  // not-yet-downloaded dependency's placeholder id, once its repo is
  // actually cloned and its own .fghj.yaml parsed, becomes a different,
  // proper `service.repo` id) doesn't leave the Containers tab pointing at
  // a graph that no longer has that node, silently 404ing every Start/Stop
  // click against it.
  // Faster while fghjd is unreachable, so the page comes back about as soon
  // as fghjd does after a restart.
  $effect(() => {
    const id = setInterval(pollLoad, backendDown ? 1000 : 3000);
    return () => clearInterval(id);
  });

  // `RunState.containers` is already keyed by node id on the backend, so
  // this is a rename, not an index build.
  let runContainers = $derived(env?.containers ?? {});
  // fghjd keeps each container's last reading when Docker stops answering
  // and marks it unseen, so one unseen container means Docker is out of reach.
  let dockerDown = $derived(Object.values(runContainers).some((c) => c.observed?.unseen));
  let liveInfo = $derived(selectedNode ? runContainers[selectedNode.id] : null);
  // Scoped to the selected node only, not the whole workspace: the backend
  // already serializes lifecycle calls *execution-order-wise* per workspace
  // (`RunRegistry::action_lock`, to avoid racing Docker calls against the
  // same container name), but that's invisible latency, not a reason to
  // stop the user from even queuing an action on an unrelated container.
  // `liveInfo?.pending_action` is the backend's own per-node truth, except
  // that a start can be called off by a Stop or Delete;
  // `activeAction`/`actionQueue` fill the ~1s gap before the next `/environment`
  // poll would otherwise reflect a just-fired request for *this* node.
  let selectedNodeBusy = $derived(
    selectedNode
      ? env?.pending_create != null ||
          (liveInfo?.pending_action != null && liveInfo.pending_action !== 'starting') ||
          activeAction?.nodeId === selectedNode.id ||
          actionQueue.some((j) => j.nodeId === selectedNode.id)
      : false
  );

  // `repo/flow` ids, every flow any repo publishes — including an empty
  // one, which no node would carry.
  let flowNames = $derived(universe?.flows ?? []);

  // Flows whose members all come from one repo: usually a building block
  // another flow is made of, so the picker ranks them below the rest.
  let singleRepoFlows = $derived.by(() => {
    const repos = new Map();
    for (const n of universe?.nodes ?? []) {
      for (const f of n.flows ?? []) {
        if (!repos.has(f)) repos.set(f, new Set());
        repos.get(f).add(n.repo ?? n.local_path);
      }
    }
    return new Set(flowNames.filter((f) => (repos.get(f)?.size ?? 0) <= 1));
  });

  // Repos tab: which repo requires which other repo. The currently selected
  // flow is highlighted (border/edge color), not filtered — every known repo
  // always renders, per the fog-of-war model.
  //
  // One box per *repo*, not per service: everything declared in one repo
  // (e.g. aikido-core's `php`, `vite` and its postgres) shares one checkout,
  // one git branch/dirty state, and one clone/pull lifecycle, so it
  // collapses into a single node here. `local_path` (present once a repo is
  // actually on disk) is the grouping key, since it's the checkout identity
  // — stub (not-yet-downloaded) nodes have no `local_path` yet, so they fall
  // back to their own `id`, which is the repo's folder name.
  let reposGraph = $derived.by(() => {
    if (!universe) return null;
    const services = universe.nodes;
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
      // Containers built from an older state of this checkout than it's in
      // now: the same comparison the Containers tab makes per card.
      const stale = [];
      for (const m of members) {
        const built = runContainers[m.id]?.desired?.source;
        if (!built) continue;
        if (built.head && m.head && built.head !== m.head) {
          stale.push({ id: m.id, label: m.label, built: built.head, now: m.head });
        } else if (m.dirty && !built.dirty) {
          stale.push({ id: m.id, label: m.label, built: built.head, now: null });
        }
      }
      nodes.push({
        id: key,
        label: key,
        kind: 'repo',
        repo: repr.repo,
        branch: repr.branch,
        head: repr.head,
        dirty: members.some((m) => m.dirty),
        downloaded: members.every((m) => m.downloaded),
        local_path: repr.local_path,
        domain: repr.domain,
        flows: [...new Set(members.flatMap((m) => m.flows))],
        services: members.filter((m) => m.kind === 'service').map((m) => m.label).sort(),
        members: members.map((m) => ({ id: m.id, label: m.label, kind: m.kind })),
        git: repr.local_path ? (repoStatus[repr.local_path] ?? null) : null,
        stale,
      });
    }

    // Cross-repo edges only — an edge between two services in the same
    // group (e.g. vite -> php) is internal to that repo and has nothing to
    // do with which *other* repos this one depends on. A repo that needs
    // another both to start and at runtime is drawn as needing it to
    // start: that's the stronger of the two.
    const edgeMap = new Map();
    for (const e of universe.edges) {
      const from = nodeToGroup.get(e.from);
      const to = nodeToGroup.get(e.to);
      if (!from || !to || from === to) continue;
      const key = `${from}|${to}`;
      if (!edgeMap.has(key)) edgeMap.set(key, { from, to, kind: e.kind, required: false, flows: new Set() });
      const merged = edgeMap.get(key);
      if (e.required !== false) merged.required = true;
      e.flows.forEach((f) => merged.flows.add(f));
    }
    const edges = [...edgeMap.values()].map((e) => ({ ...e, flows: [...e.flows] }));

    return { nodes, edges };
  });

  let selectedRepo = $derived(
    selectedRepoId ? (reposGraph?.nodes.find((n) => n.id === selectedRepoId) ?? null) : null
  );
  let missingAll = $derived(reposGraph ? reposGraph.nodes.filter((n) => !n.downloaded).length : null);
  let missingInFlow = $derived(
    reposGraph && currentFlow
      ? reposGraph.nodes.filter((n) => !n.downloaded && n.flows.includes(currentFlow)).length
      : null
  );

  // Actual tab: services + infra, everything the daemon would eventually run.
  //
  // Plus one class of node the resolver can't produce: a container that is
  // still running for a node the freshly-resolved graph no longer contains
  // (`SyncStatus::Orphaned` — almost always a `git switch` to a branch where
  // the service isn't declared). The backend deliberately doesn't reconcile
  // those away, so if this tab only drew `universe.nodes` the container would
  // be running, holding its ports and its domain, and invisible. Synthesizing
  // a node for it is what makes it selectable — and therefore stoppable —
  // from the drawer.
  let showSeeds = $state(localStorage.getItem('fghj:show-seeds') === '1');
  function setShowSeeds(on) {
    showSeeds = on;
    localStorage.setItem('fghj:show-seeds', on ? '1' : '0');
  }

  let containersGraph = $derived.by(() => {
    if (!universe) return null;
    const declared = new Set(universe.nodes.map((n) => n.id));
    const orphans = Object.values(runContainers)
      .filter((c) => c.observed.sync === 'orphaned' && !declared.has(c.node_id))
      .map((c) => ({
        id: c.node_id,
        // The id's leaf is the service name; the rest is the owner path,
        // which is what the card's repo line would show if this still had a
        // repo to point at.
        label: c.node_id.split('.')[0],
        kind: 'service',
        domain: c.desired.domain,
        downloaded: true,
        dirty: false,
        flows: [],
        ports: {},
        environment: [],
        command: [],
        volumes: [],
        additional_hosts: [],
        wildcard_hosts: [],
        orphaned: true,
      }));
    const nodes = [...universe.nodes, ...orphans];
    const ids = new Set(nodes.map((n) => n.id));
    const edges = universe.edges.filter((e) => ids.has(e.from) && ids.has(e.to));
    return { nodes, edges };
  });

  // Only blocking warnings light the header's conflict indicator. An
  // advisory ("this repo declares no services") is information; a blocking
  // one is the reason the next start attempt will be refused, and the two
  // shouldn't look the same.
  let hasWarnings = $derived(
    universe ? universe.warnings.some((w) => w.severity === 'blocking') : false,
  );
</script>

<div style="position:relative;height:100vh;width:100vw;overflow:auto;background:var(--bg);color:var(--ink)">
  <Header
    flows={flowNames}
    {recentFlows}
    {singleRepoFlows}
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
    {missingAll}
    {missingInFlow}
    onSwitchFlow={switchFlow}
    envBusy={env?.pending_create != null}
    onOpenDrawer={(tab) => (telemetryTab = tab)}
    workspaces={workspaces}
    currentWorkspaceId={currentWorkspaceId}
    onOpenWorkspaces={loadWorkspaces}
    onSelectWorkspace={selectWorkspace}
    showSeeds={containersGraph?.nodes.some((n) => n.seeds?.length) ? showSeeds : null}
    onToggleSeeds={() => setShowSeeds(!showSeeds)}
  />

  <div style="padding-top:90px">
    {#if backendDown && !universe}
      <GraphSkeleton
        tone="down"
        title="fghjd isn't answering. Retrying the connection…"
        detail={"It may be restarting or upgrading. If it was stopped with `fghj daemon stop`, run `fghj daemon start`; otherwise `fghj doctor` shows what is wrong."}
        onRetry={load}
      />
    {:else if workspacesLoaded && !currentWorkspaceId}
      <Welcome />
    {:else if activeTab === 'config'}
      <!-- Ahead of `error`: a workspace that no longer resolves still has to be removable. -->
      <WorkspaceConfig
        workspace={workspaces.find((w) => w.id === currentWorkspaceId)}
        workspaceName={universe?.workspace_name}
        onRemove={removeWorkspace}
      />
    {:else if error}
      <GraphSkeleton
        tone="error"
        title="This workspace failed to resolve"
        detail={error}
        onRetry={load}
      />
    {:else if !universe}
      <GraphSkeleton title="Resolving graph…" />
    {:else}
      {#if backendDown}
        <div class="connection-banner" role="alert">
          <span class="spinner"></span>
          <span>fghjd isn't answering. Showing the last known graph and retrying the connection…</span>
          <button onclick={load}>Retry now</button>
        </div>
      {/if}
      {#if dockerDown && !backendDown}
        <div class="connection-banner" role="alert">
          <span class="spinner"></span>
          <span>Docker isn't answering, so every container's state is unknown until it's back. fghjd picks it up on its own.</span>
          <button onclick={() => (telemetryTab = 'doctor')}>Doctor</button>
        </div>
      {/if}
      {#if universe.warnings.length}
        <div style="padding:0 40px;display:flex;flex-direction:column;gap:6px;margin-bottom:4px">
          {#each universe.warnings as w}
            <div class="warning-banner" class:advisory={w.severity !== 'blocking'}>
              <span class="tag">{w.severity === 'blocking' ? 'blocks start' : 'advisory'}</span>
              <span>{w.message}</span>
            </div>
          {/each}
        </div>
      {/if}

      {#if activeTab === 'repos'}
        <GraphView graph={reposGraph} {currentFlow} mode="repos" onSelectNode={(n) => (selectedRepoId = n.id)} />
      {:else if activeTab === 'containers'}
        <GraphView graph={containersGraph} {currentFlow} mode="containers" {runContainers} {recordingNodes} {showSeeds} onSelectNode={(n) => (selectedNode = n)} />
      {/if}
    {/if}
  </div>

  {#if displayPending.length}
    <div class="action-banner">
      <span class="spinner"></span>
      <span>{displayPending[0].action} <b>{displayPending[0].nodeId}</b>…</span>
      {#if displayPending.length > 1}
        <span class="queue-count">+{displayPending.length - 1} more</span>
      {/if}
    </div>
  {/if}

  {#if selectedNode}
    <Drawer
      node={selectedNode}
      onClose={() => (selectedNode = null)}
      {liveInfo}
      actionLog={actionLog.filter((a) => a.nodeId === selectedNode.id)}
      busy={selectedNodeBusy}
      onFetchLogs={fetchLogs}
      onFetchLogGenerations={fetchLogGenerations}
      onFetchLogHistory={fetchLogHistory}
      onFetchEvents={fetchEvents}
      onLogStreamUrl={logStreamUrl}
      onTrafficUrl={trafficUrl}
      onDownload={downloadNode}
      onPullStatus={pullStatus}
      onDownloadComplete={onDownloadComplete}
      onStartNode={startNode}
      onStopNode={stopNode}
      onDeleteNode={deleteNode}
      onResetNode={resetNode}
      onSetDebugWait={setDebugWait}
    />
  {/if}

  {#if selectedRepo}
    <RepoDrawer
      repo={selectedRepo}
      graph={reposGraph}
      {currentFlow}
      workspacePath={workspaces.find((w) => w.id === currentWorkspaceId)?.workspace}
      onClose={() => (selectedRepoId = null)}
      onDownload={downloadNode}
      onPullStatus={pullStatus}
      onDownloadComplete={onPullAllComplete}
      onGit={gitRepo}
      onOpenNode={(id) => {
        selectedRepoId = null;
        activeTab = 'containers';
        selectedNode = universe?.nodes.find((n) => n.id === id) ?? null;
      }}
    />
  {/if}

  {#if telemetryTab}
    <TelemetryDrawer
      initialTab={telemetryTab}
      workspaceName={universe?.workspace_name}
      workspaceId={currentWorkspaceId}
      onClose={() => (telemetryTab = null)}
      onListJobs={listActivities}
      onFetchDaemonLogs={fetchDaemonLogs}
      onFetchNetStatus={fetchDaemonNetStatus}
      onFetchDoctor={fetchDaemonDoctor}
    />
  {/if}
</div>

<style>
  .connection-banner {
    display: flex; align-items: center; gap: 10px; margin: 0 40px 10px; padding: 8px 12px; border-radius: 6px;
    background: var(--warning-bg); border: 1px solid var(--warning); color: var(--warning); font: 500 11.5px var(--font-mono);
  }
  .connection-banner .spinner {
    width: 9px; height: 9px; border-radius: 50%; flex: 0 0 auto;
    border: 2px solid var(--line-strong); border-top-color: var(--warning); animation: banner-spin 0.7s linear infinite;
  }
  .connection-banner button {
    margin-left: auto; padding: 4px 9px; border-radius: 4px; background: var(--panel); border: 1px solid var(--warning);
    color: var(--warning); font: 700 10px var(--font-mono); text-transform: uppercase; cursor: pointer;
  }
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
  /* Advisories are worth reading, not worth alarming over — the loud
     treatment is reserved for the ones that will refuse a start. */
  :global(.warning-banner.advisory) {
    background: var(--panel); border-color: var(--line); color: var(--muted);
  }
  :global(.warning-banner.advisory .tag) { background: var(--muted); }
  :global(.warning-banner .tag) {
    font: 700 9.5px/1 var(--font-mono); text-transform: uppercase; letter-spacing: 0.06em; background: var(--danger);
    color: var(--bg); padding: 3px 6px; border-radius: 3px; flex: 0 0 auto;
  }
</style>
