<script>
  import SideDrawer from './SideDrawer.svelte';

  let {
    node,
    onClose,
    liveInfo,
    actionLog = [],
    // Workspace-wide (not per-node) in-flight flag, owned by App.svelte so
    // it survives this component being destroyed/recreated on drawer
    // close/reopen — see App.svelte's actionQueue for why a locally-scoped
    // busy flag here couldn't be trusted across that remount.
    busy = false,
    runId,
    onLogStreamUrl,
    onFetchLogGenerations,
    onFetchLogHistory,
    onFetchEvents,
    onDownload,
    onPullStatus,
    onDownloadComplete,
    onStartNode,
    onStopNode,
    onDeleteNode,
    onResetNode,
  } = $props();
  let activeTab = $state('general');

  function formatActionTime(ms) {
    return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  }

  function startNode() {
    onStartNode?.(node.id);
  }

  function stopNode() {
    onStopNode?.(node.id);
  }

  function deleteNode() {
    onDeleteNode?.(node.id);
  }

  // Same backend action as "Start" (`runs::Runs::restart_container` always
  // stops+removes the existing container, then rebuilds/re-creates it) —
  // this button just exposes it while the container is already running,
  // which "Start" is hidden for. Container-only: never touches volumes.
  function resetNode() {
    onResetNode?.(node.id);
  }

  // Single source of truth for "what can this node's controls do right
  // now" — replaces a scatter of ad hoc `liveInfo?.observed.status === 'running'`
  // checks per button with one explicit state, so adding/auditing a
  // transition means touching one place. `liveInfo` is undefined once
  // `ContainerActionSettled` removes a deleted node's entry (or before it's
  // ever been started), which is exactly "absent"; a live `pending_action`
  // always wins over `status` since Docker hasn't settled yet.
  //   absent    -> only Start (create + start fresh)
  //   stopped   -> Start or Delete
  //   running   -> Stop, Delete, or Reset (force a fresh container)
  //   starting/stopping/removing -> nothing; show the in-flight action instead
  function nodeLifecycle(info) {
    if (!info) return 'absent';
    if (info.pending_action) return info.pending_action;
    return info.observed.status === 'running' ? 'running' : 'stopped';
  }
  // Persisted log history (see `store::WorkspaceDb`'s `logs` table): a
  // generation picker (current vs. previous, per the two-generation
  // retention policy) backing a scroll-up-for-more-history view, so a
  // crashed container's last lines stay readable even after it's gone —
  // the point of persisting logs at all rather than just reading Docker's
  // own (destroyed-on-remove) log buffer.
  let generations = $state([]);
  let selectedGeneration = $state(null);
  let historyLines = $state([]);
  let loadingHistory = $state(false);
  let hasMoreHistory = $state(true);
  let logsScrollEl = $state(null);
  let streaming = $state(false);

  // ArgoCD-style "events": what `fghjd` itself did during the last start or
  // stop of this node (image build, container creation, healthcheck wait,
  // ...), as opposed to the log-history state above (the container's own
  // stdout/stderr). Only the current cycle of `eventsAction` is ever
  // returned by the backend — see `store::WorkspaceDb::begin_event_cycle` —
  // so there's no generation picker here, just a start/stop toggle.
  let eventsAction = $state('start');
  let events = $state([]);
  let loadingEvents = $state(false);
  let eventsPollTimer = null;
  let downloading = $state(false);
  let dlStatus = $state(null); // null | 'running' | 'done' | 'error'
  let dlLog = $state('');
  let pollTimer = null;
  let copiedKey = $state(null);
  let copiedTimer = null;

  function copyText(text, key) {
    navigator.clipboard?.writeText(text);
    copiedKey = key;
    clearTimeout(copiedTimer);
    copiedTimer = setTimeout(() => (copiedKey = null), 1200);
  }

  function scrollLogsToBottom() {
    requestAnimationFrame(() => {
      if (logsScrollEl) logsScrollEl.scrollTop = logsScrollEl.scrollHeight;
    });
  }

  async function loadInitialHistory(generation) {
    historyLines = [];
    hasMoreHistory = true;
    if (!onFetchLogHistory) return;
    loadingHistory = true;
    historyLines = await onFetchLogHistory(node.id, generation);
    loadingHistory = false;
    hasMoreHistory = historyLines.length >= 500;
    scrollLogsToBottom();
  }

  async function loadLogGenerations(nodeId) {
    if (!onFetchLogGenerations) return;
    generations = await onFetchLogGenerations(nodeId);
    selectedGeneration = generations[0]?.generation ?? null;
    if (selectedGeneration != null) await loadInitialHistory(selectedGeneration);
  }

  function selectGeneration(generation) {
    if (generation === selectedGeneration) return;
    selectedGeneration = generation;
    loadInitialHistory(generation);
  }

  async function loadOlderHistory() {
    if (loadingHistory || !hasMoreHistory || selectedGeneration == null || !onFetchLogHistory) return;
    const oldestSeq = historyLines[0]?.seq;
    if (oldestSeq == null) {
      hasMoreHistory = false;
      return;
    }
    loadingHistory = true;
    const older = await onFetchLogHistory(node.id, selectedGeneration, oldestSeq);
    if (!older.length) {
      hasMoreHistory = false;
    } else {
      const prevHeight = logsScrollEl?.scrollHeight ?? 0;
      historyLines = [...older, ...historyLines];
      requestAnimationFrame(() => {
        if (logsScrollEl) logsScrollEl.scrollTop = logsScrollEl.scrollHeight - prevHeight;
      });
    }
    loadingHistory = false;
  }

  function onLogsScroll() {
    if (logsScrollEl && logsScrollEl.scrollTop < 40) loadOlderHistory();
  }

  async function loadEvents(nodeId, action) {
    if (!onFetchEvents) return;
    loadingEvents = true;
    events = await onFetchEvents(nodeId, action);
    loadingEvents = false;
  }

  function selectEventsAction(action) {
    if (action === eventsAction) return;
    eventsAction = action;
    loadEvents(node.id, action);
  }

  async function poll() {
    if (!onPullStatus) return;
    const s = await onPullStatus(node.id);
    if (!s) return;
    dlLog = s.log ?? '';
    dlStatus = s.status;
    if (s.status !== 'running') {
      clearInterval(pollTimer);
      pollTimer = null;
      downloading = false;
      if (s.status === 'done' && onDownloadComplete) await onDownloadComplete(node.id);
    }
  }

  async function download() {
    if (!onDownload || downloading) return;
    downloading = true;
    dlStatus = 'running';
    dlLog = '';
    await onDownload(node.id);
    await poll();
    if (dlStatus === 'running') {
      pollTimer = setInterval(poll, 600);
    }
  }

  $effect(() => {
    return () => {
      if (pollTimer) clearInterval(pollTimer);
      if (copiedTimer) clearTimeout(copiedTimer);
      if (eventsPollTimer) clearInterval(eventsPollTimer);
    };
  });

  // Loads the current cycle's events whenever the Events tab is opened (or
  // the node/action toggle changes), then polls while it stays open so
  // steps of an in-flight start/stop show up without a manual refresh —
  // there are only ever a handful of rows, so a short poll interval is
  // cheap. Stops as soon as the tab is left or the drawer closes.
  $effect(() => {
    if (activeTab !== 'events') {
      if (eventsPollTimer) {
        clearInterval(eventsPollTimer);
        eventsPollTimer = null;
      }
      return;
    }
    loadEvents(node.id, eventsAction);
    eventsPollTimer = setInterval(() => loadEvents(node.id, eventsAction), 1000);
    return () => {
      clearInterval(eventsPollTimer);
      eventsPollTimer = null;
    };
  });

  // Loads the retained generations (current/previous) and the selected
  // one's history whenever the Logs tab is opened or the node changes.
  $effect(() => {
    if (activeTab !== 'logs') return;
    loadLogGenerations(node.id);
  });

  // Live-follows the container's log output over SSE whenever the Logs tab
  // is open against a running container, appending straight onto the
  // current generation's view so new lines show up without polling. Only
  // applied while viewing the current generation — switching to "previous"
  // to read a crash's history shouldn't have unrelated live lines land in
  // it. Re-runs (tearing down the previous connection first) whenever the
  // tab, node, or live container identity changes.
  $effect(() => {
    if (activeTab !== 'logs' || !liveInfo || !onLogStreamUrl) return;
    const url = onLogStreamUrl(node.id);
    if (!url) return;

    const source = new EventSource(url);
    streaming = true;
    source.onmessage = (e) => {
      if (generations.length && selectedGeneration !== generations[0].generation) return;
      const lastSeq = historyLines.length ? historyLines[historyLines.length - 1].seq : -1;
      historyLines = [...historyLines, { seq: lastSeq + 1, stream: 'stdout', ts: '', line: e.data }];
      scrollLogsToBottom();
    };
    source.onerror = () => {
      streaming = false;
    };
    return () => {
      streaming = false;
      source.close();
    };
  });
</script>

<SideDrawer onClose={onClose}>
    <div class="head">
      <span class="eyebrow">{node.kind}</span>
      <div class="close" onclick={onClose}>×</div>
    </div>
    <h3 class="stencil">{node.label}</h3>

    {#if node.downloaded !== false}
      {@const lifecycle = nodeLifecycle(liveInfo)}
      <div class="controls">
        {#if lifecycle === 'starting' || lifecycle === 'stopping' || lifecycle === 'removing'}
          <span class="ctrl-status"><span class="spinner"></span> {lifecycle}…</span>
        {:else}
          {#if lifecycle === 'absent' || lifecycle === 'stopped'}
            <button class="ctrl-btn" onclick={startNode} disabled={!runId || busy}>Start</button>
          {/if}
          {#if lifecycle === 'running'}
            <button class="ctrl-btn" onclick={stopNode} disabled={!runId || busy}>Stop</button>
          {/if}
          {#if lifecycle !== 'absent'}
            <button class="ctrl-btn danger" onclick={deleteNode} disabled={!runId || busy}>Delete</button>
          {/if}
          {#if lifecycle === 'running'}
            <button class="ctrl-btn" onclick={resetNode} disabled={!runId || busy} title="Stop and recreate this container fresh. Volumes are untouched.">Reset</button>
          {/if}
        {/if}
      </div>
    {/if}

    {#if actionLog.length}
      <div class="action-log">
        {#each actionLog as entry (entry.id)}
          <div class="action-entry" class:error={!entry.ok}>
            <span class="action-time">{formatActionTime(entry.time)}</span>
            <span class="action-verb">{entry.action}</span>
            <span class="action-message">{entry.message}</span>
          </div>
        {/each}
      </div>
    {/if}

    <div class="tabs">
      <div class="tab" class:active={activeTab === 'general'} onclick={() => (activeTab = 'general')}>General info</div>
      <div class="tab" class:active={activeTab === 'logs'} onclick={() => (activeTab = 'logs')}>Logs</div>
      {#if node.downloaded !== false}
        <div class="tab" class:active={activeTab === 'events'} onclick={() => (activeTab = 'events')}>Events</div>
      {/if}
    </div>

    {#if activeTab === 'general'}
      <div class="rows">
        {#if node.downloaded === false}
          <div class="row not-downloaded">
            <span class="k">status</span>
            <span class="v">
              {#if dlStatus === 'running'}<span class="spinner"></span> downloading…
              {:else if dlStatus === 'error'}download failed
              {:else}not downloaded yet{/if}
            </span>
          </div>
        {/if}
        <div class="row"><span class="k">kind</span><span class="v">{node.kind}</span></div>
        {#if node.repo}
          <div class="row"><span class="k">repo</span><span class="v">{node.repo}</span></div>
        {/if}
        {#if node.branch}
          <div class="row"><span class="k">branch</span><span class="v">{node.branch}</span></div>
        {/if}
        {#if node.downloaded !== false}
          <div class="row"><span class="k">status</span><span class="v">{node.dirty ? 'dirty' : 'clean'}</span></div>
        {/if}
        {#if node.local_path}
          <div class="row"><span class="k">local path</span><span class="v">{node.local_path}</span></div>
        {/if}
        {#if node.image}
          <div class="row"><span class="k">image</span><span class="v">{node.image}</span></div>
        {/if}
        {#each Object.entries(node.ports ?? {}) as [port, cfg]}
          {@const routeDomain = cfg.primary ? node.domain : cfg.name ? `${cfg.name}.${node.domain}` : null}
          {@const isLive = routeDomain && liveInfo?.desired.routes?.some((r) => r.domain === routeDomain)}
          {@const role = cfg.primary ? 'main' : cfg.name ? 'additional' : 'tcp'}
          {@const displayDomain = cfg.wildcard ? `*.${routeDomain}` : routeDomain}
          <div class="port-row">
            <span class="port-chip">{port}</span>
            <span class="port-badge role-{role}">{role}</span>
            {#if cfg.wildcard}
              <span class="port-badge wildcard">wildcard</span>
            {/if}
            <span class="port-target">
              {#if routeDomain}
                {#if isLive}
                  <a href="https://{routeDomain}" target="_blank" rel="noopener">{displayDomain} ↗</a>
                {:else}
                  <span class="muted">{displayDomain}</span>
                {/if}
              {:else if liveInfo?.observed.ports?.[port] && liveInfo?.desired.raw_domain}
                <button class="copy-btn" onclick={() => copyText(`${liveInfo.desired.raw_domain}:${port}`, port)}>
                  {liveInfo.desired.raw_domain}:{port} {copiedKey === port ? '· copied' : '⧉'}
                </button>
              {:else if liveInfo?.observed.ports?.[port]}
                <button class="copy-btn" onclick={() => copyText(`127.0.0.1:${liveInfo.observed.ports[port]}`, port)}>
                  127.0.0.1:{liveInfo.observed.ports[port]} {copiedKey === port ? '· copied' : '⧉'}
                </button>
              {:else}
                <span class="muted">not running</span>
              {/if}
            </span>
          </div>
        {/each}
        {#each node.additional_hosts ?? [] as host}
          {@const liveRoute = liveInfo?.desired.routes?.find((r) => r.domain === host)}
          <div class="row">
            <span class="k">additional</span>
            <span class="v">
              {#if liveRoute}<a href="{liveRoute.https ? 'https' : 'http'}://{host}" target="_blank" rel="noopener">{host}</a>{:else}{host}{/if}
            </span>
          </div>
        {/each}
        {#each node.wildcard_hosts ?? [] as host}
          {@const liveRoute = liveInfo?.desired.routes?.find((r) => r.domain === host)}
          <div class="row">
            <span class="k">additional (wildcard)</span>
            <span class="v">
              {#if liveRoute}<a href="{liveRoute.https ? 'https' : 'http'}://{host}" target="_blank" rel="noopener">*.{host}</a>{:else}*.{host}{/if}
            </span>
          </div>
        {/each}
        <div class="row"><span class="k">flows</span><span class="v">{node.flows?.join(', ') || '—'}</span></div>
        {#if liveInfo && node.downloaded !== false}
          <div class="row"><span class="k">container status</span><span class="v">{liveInfo.observed.status}{liveInfo.pending_action ? ` (${liveInfo.pending_action}…)` : ''}</span></div>
          <div class="row"><span class="k">container name</span><span class="v">{liveInfo.desired.container_name}</span></div>
          <div class="row">
            <span class="k">config sync</span>
            <span class="v">
              {#if liveInfo.observed.sync === 'drifted'}
                <span class="pill unsynced" title="the running container's config no longer matches .fghj.yaml — restart this node to pick up the change">desired ≠ actual</span>
              {:else if liveInfo.observed.sync === 'synced'}
                <span class="pill synced">up to date</span>
              {:else}
                <span class="muted">unknown</span>
              {/if}
            </span>
          </div>
        {/if}
      </div>
    {:else if node.downloaded === false}
      <div class="logs">
        <button class="btn" onclick={download} disabled={downloading}>
          {#if downloading}<span class="spinner"></span> downloading…{:else}Download{/if}
        </button>
        {#if dlStatus === 'error'}
          <div class="dl-status error">✕ download failed — see log below</div>
        {:else if dlStatus === 'done'}
          <div class="dl-status ok">✓ downloaded</div>
        {/if}
        {#if dlLog}
          <pre>{dlLog}</pre>
        {/if}
      </div>
    {:else if activeTab === 'events'}
      <div class="logs">
        <div class="logs-toolbar">
          <div class="gen-picker">
            <button class="gen-btn" class:active={eventsAction === 'start'} onclick={() => selectEventsAction('start')}>start</button>
            <button class="gen-btn" class:active={eventsAction === 'stop'} onclick={() => selectEventsAction('stop')}>stop</button>
          </div>
        </div>
        {#if events.length}
          <div class="event-list">
            {#each events as e (e.seq)}
              <div class="event-row" class:error={e.status === 'error'}>
                <span class="event-time">{new Date(e.ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })}</span>
                <span class="event-status status-{e.status}">
                  {#if e.status === 'running'}<span class="spinner"></span>{:else if e.status === 'ok'}✓{:else if e.status === 'error'}✕{/if}
                </span>
                <span class="event-step">{e.step}</span>
                {#if e.detail}<span class="event-detail">{e.detail}</span>{/if}
              </div>
            {/each}
          </div>
        {:else if loadingEvents}
          <div class="logs-empty">loading…</div>
        {:else}
          <div class="logs-empty">no {eventsAction} events recorded yet</div>
        {/if}
      </div>
    {:else}
      <div class="logs">
        <div class="logs-toolbar">
          {#if generations.length}
            <div class="gen-picker">
              {#each generations as g, i}
                <button
                  class="gen-btn"
                  class:active={g.generation === selectedGeneration}
                  onclick={() => selectGeneration(g.generation)}
                  title="{g.line_count} lines{g.first_ts ? `, ${g.first_ts} – ${g.last_ts}` : ''}"
                >
                  {i === 0 ? 'current' : i === 1 ? 'previous' : `gen ${g.generation}`}
                </button>
              {/each}
            </div>
          {/if}
          {#if streaming}<span class="live-badge"><span class="spinner"></span> live</span>{/if}
        </div>
        {#if historyLines.length}
          <pre class="log-pane" bind:this={logsScrollEl} onscroll={onLogsScroll}
            >{#if loadingHistory}<span class="loading-more">loading more…</span>
{/if}{#each historyLines as l (l.seq)}{l.ts} {l.line}
{/each}</pre>
        {:else if loadingHistory}
          <div class="logs-empty">loading…</div>
        {:else}
          <div class="logs-empty">
            {liveInfo ? 'no logs captured yet' : 'no logs — start this node to begin capturing'}
          </div>
        {/if}
      </div>
    {/if}
</SideDrawer>

<style>
  .head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 10px; }
  .close { width: 22px; height: 22px; border-radius: 50%; background: var(--panel-2); display: flex; align-items: center; justify-content: center; cursor: pointer; color: var(--ink-dim); font-size: 14px; }
  h3 { font-size: 20px; color: var(--ink); margin-bottom: 20px; word-break: break-all; }
  .controls { display: flex; gap: 8px; margin-bottom: 16px; }
  .ctrl-btn {
    padding: 6px 12px; border-radius: 4px; background: var(--panel-2);
    border: 1px solid var(--line-strong); color: var(--ink); font: 700 10px var(--font-mono);
    text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer;
  }
  .ctrl-btn:disabled { opacity: 0.4; cursor: default; }
  .ctrl-btn.danger:not(:disabled) { border-color: var(--danger); color: var(--danger); }
  .ctrl-status {
    display: flex; align-items: center; padding: 6px 12px; color: var(--ink-dim);
    font: 700 10px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em;
  }
  .action-log {
    display: flex; flex-direction: column; gap: 4px; margin-bottom: 16px; padding: 8px 10px;
    background: var(--panel-2); border-radius: 6px; max-height: 140px; overflow-y: auto;
  }
  .action-entry {
    display: flex; align-items: baseline; gap: 8px; font: 500 11px var(--font-mono); color: var(--success);
  }
  .action-entry.error { color: var(--danger); }
  .action-time { color: var(--ink-faint); flex: 0 0 auto; }
  .action-verb { text-transform: uppercase; font-weight: 700; flex: 0 0 auto; }
  .action-message { color: var(--ink); word-break: break-word; }
  .action-entry.error .action-message { color: var(--danger); }
  .tabs { display: flex; gap: 2px; padding: 2px; background: var(--panel-2); border-radius: 6px; margin-bottom: 20px; width: fit-content; }
  .tab {
    padding: 6px 14px; border-radius: 4px; font: 700 11px var(--font-mono); text-transform: uppercase;
    letter-spacing: 0.04em; cursor: pointer; color: var(--ink-faint);
  }
  .tab.active { background: var(--accent); color: var(--bg); }
  .logs-empty { font: 500 12px var(--font-mono); color: var(--ink-faint); font-style: italic; }
  .rows { display: flex; flex-direction: column; gap: 12px; }
  .row { display: flex; flex-direction: column; gap: 3px; font: 500 12px var(--font-mono); }
  .row .k { color: var(--ink-faint); text-transform: uppercase; font-size: 10px; letter-spacing: 0.06em; }
  .row .v { color: var(--ink); word-break: break-all; }
  .muted { color: var(--ink-faint); font-style: italic; }
  .port-row { display: flex; align-items: center; flex-wrap: wrap; gap: 6px; font: 500 12px var(--font-mono); }
  .port-chip {
    padding: 1px 6px; border-radius: 4px; background: var(--panel-2); border: 1px solid var(--line-strong);
    color: var(--ink-dim); font-weight: 700; font-size: 11px;
  }
  .port-badge {
    padding: 1px 6px; border-radius: 3px; font: 700 9px var(--font-mono); text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .port-badge.role-main { background: var(--accent-bg); color: var(--accent); }
  .port-badge.role-additional { background: var(--panel-2); color: var(--ink-dim); border: 1px solid var(--line-strong); }
  .port-badge.role-tcp { background: var(--panel-2); color: var(--ink-faint); border: 1px solid var(--line-strong); }
  .port-badge.wildcard { background: var(--warning-bg); color: var(--warning); }
  .pill {
    padding: 1px 7px; border-radius: 999px; font: 700 9px var(--font-mono); text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .pill.unsynced { background: var(--warning-bg); color: var(--warning); }
  .pill.synced { background: var(--success-bg); color: var(--success); }
  .port-target { color: var(--ink); word-break: break-all; }
  .copy-btn {
    background: none; border: none; padding: 0; margin: 0; font: 500 12px var(--font-mono);
    color: var(--ink); cursor: pointer; word-break: break-all; text-align: left;
  }
  .copy-btn:hover { color: var(--accent); }
  .row.not-downloaded .v { color: var(--ink-faint); font-style: italic; }
  .logs { margin-top: 20px; display: flex; flex-direction: column; gap: 10px; }
  .logs .btn {
    align-self: flex-start; padding: 6px 10px; border-radius: 4px; background: var(--panel-2);
    border: 1px solid var(--line-strong); color: var(--ink); font: 700 10px var(--font-mono);
    text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer;
  }
  .logs .btn:disabled { opacity: 0.6; cursor: default; }
  .logs-toolbar { display: flex; align-items: center; gap: 10px; }
  .gen-picker { display: flex; gap: 6px; }
  .gen-btn {
    padding: 5px 10px; border-radius: 4px; background: var(--panel-2);
    border: 1px solid var(--line-strong); color: var(--ink-dim); font: 700 10px var(--font-mono);
    text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer;
  }
  .gen-btn.active { color: var(--ink); border-color: var(--accent, #6fa8ff); }
  .loading-more { color: var(--ink-faint); font-style: italic; }
  .event-list { display: flex; flex-direction: column; gap: 2px; }
  .event-row {
    display: flex; align-items: baseline; gap: 8px; padding: 4px 8px; border-radius: 4px;
    font: 500 11px var(--font-mono); background: var(--panel-2);
  }
  .event-row.error { background: var(--danger-bg, rgba(255, 90, 90, 0.08)); }
  .event-time { color: var(--ink-faint); flex: 0 0 auto; }
  .event-status { flex: 0 0 auto; width: 12px; text-align: center; }
  .event-status.status-ok { color: var(--success, #6fdc8c); }
  .event-status.status-error { color: var(--danger); }
  .event-step { color: var(--ink); text-transform: uppercase; letter-spacing: 0.03em; flex: 0 0 auto; }
  .event-detail { color: var(--ink-faint); word-break: break-all; }
  .live-badge {
    display: flex; align-items: center; gap: 4px; font: 700 10px var(--font-mono); text-transform: uppercase;
    letter-spacing: 0.04em; color: var(--success, #6fdc8c);
  }
  .spinner {
    display: inline-block; width: 9px; height: 9px; border-radius: 50%;
    border: 2px solid var(--line-strong); border-top-color: var(--ink);
    animation: spin 0.7s linear infinite; vertical-align: middle; margin-right: 2px;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  .dl-status { font: 700 11px var(--font-mono); }
  .dl-status.ok { color: var(--ok, #6fdc8c); }
  .dl-status.error { color: var(--danger); }
  .logs pre {
    background: #000; color: #b8ffb8; padding: 12px; border-radius: 6px; font: 400 11px var(--font-mono);
    max-height: 300px; overflow: auto; white-space: pre-wrap; word-break: break-all;
  }
</style>
