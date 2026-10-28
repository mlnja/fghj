<script>
  import SideDrawer from './SideDrawer.svelte';
  import LogPane, { textLines } from './LogPane.svelte';

  let {
    initialTab = 'logs',
    onClose,
    onFetchDaemonLogs,
    onFetchNetStatus,
    onFetchDoctor,
    onListJobs,
    workspaceId = null,
    workspaceName = null,
  } = $props();

  // Two halves, never mixed: what this workspace has been doing, and fghjd
  // itself, which every workspace shares. Every workspace's jobs go on
  // fghjd's side: a removed workspace's job outlives the workspace.
  const SECTIONS = {
    workspace: [
      ['downloads', 'Downloads'],
      ['containers', 'Containers'],
    ],
    fghjd: [
      ['logs', 'Logs'],
      ['activity', 'All activity'],
      ['doctor', 'Doctor'],
      ['help', 'Help'],
    ],
  };

  let activeTab = $state(initialTab);
  let section = $derived(SECTIONS.workspace.some(([id]) => id === activeTab) ? 'workspace' : 'fghjd');

  function select(tab) {
    activeTab = tab;
    if (tab === 'doctor' && doctorChecks === null) runDoctor();
  }

  // Logs tab: polling tail, same pattern as the pull queue below — daemon lifecycle/reconcile messages are low-volume enough that a
  // poll is just as responsive as SSE and much simpler.
  let logLines = $state([]);
  let lastSeq = $state(null);
  let logFilter = $state('');

  async function pollLogs() {
    if (!onFetchDaemonLogs) return;
    const entries = await onFetchDaemonLogs(lastSeq);
    if (!entries.length) return;
    logLines = [...logLines, ...entries].slice(-2000);
    lastSeq = entries[entries.length - 1].seq;
  }

  // Every message is written `fghjd: ...` at its call site, and that prefix
  // earns its place on stdout — `daemon_log::info` prints there too, into a
  // terminal or launchd's capture where fghjd's lines sit among everybody
  // else's. In here it is on all of them, so it says nothing and costs seven
  // columns of the width long resolver paths are already fighting for.
  // Stripped at render rather than at the source, so the stdout copy keeps it.
  function clean(message) {
    return message.startsWith('fghjd: ') ? message.slice(7) : message;
  }

  let visibleLines = $derived.by(() => {
    const needle = logFilter.trim().toLowerCase();
    const rows = needle
      ? logLines.filter((l) => l.message.toLowerCase().includes(needle))
      : logLines;
    return rows.map((l) => ({ key: l.seq, ts: l.ts_ms, warn: l.level === 'warn', text: clean(l.message) }));
  });

  // What fghjd has installed — /etc/hosts lines, /etc/resolver zones, the
  // raw zone's listeners — read alongside the doctor, and shown under the
  // check it backs.
  let netStatus = $state(null);
  let expanded = $state({});

  async function pollNetStatus() {
    if (!onFetchNetStatus) return;
    netStatus = await onFetchNetStatus();
  }

  // Doctor tab: fetched once when the tab is opened, and after that only when
  // asked. Unlike the other two this is deliberately not polled — every check
  // shells out to the OS (ifconfig, getaddrinfo, `security verify-cert`, TCP
  // connects) and none of what it measures changes on its own, so a timer
  // would spend real work re-proving the same thing. A failed check is
  // something you go fix and then re-run, which is a button.
  //
  // Loaded from the tab's own click rather than from the `$effect` below:
  // `runDoctor` writes the same state it reads to guard itself, and doing
  // that inside a tracked effect makes the effect re-run on its own writes.
  // Opening straight on `doctor` (the Docker banner does) runs it once below.
  let doctorChecks = $state(null);
  let doctorBusy = $state(false);
  let doctorAt = $state(null);

  async function runDoctor() {
    if (!onFetchDoctor || doctorBusy) return;
    doctorBusy = true;
    try {
      const [report] = await Promise.all([onFetchDoctor(), pollNetStatus()]);
      doctorChecks = report?.checks ?? [];
      doctorAt = Date.now();
    } finally {
      doctorBusy = false;
    }
  }

  // The job tabs: background jobs, each with its own log. This workspace's
  // downloads, or its switches, starts and stops; `null` is every
  // workspace's, removals included.
  const JOB_KINDS = {
    downloads: ['pull'],
    containers: ['switch', 'start', 'stop', 'delete'],
    activity: null,
  };
  let jobTab = $derived(activeTab in JOB_KINDS);
  let activities = $state([]);
  let selectedJob = $state(null);
  // One of the selected job's children, whose part of the log to show.
  let selectedChild = $state(null);

  let jobs = $derived.by(() => {
    if (!jobTab) return [];
    const kinds = JOB_KINDS[activeTab];
    return activities.filter(
      (a) => a.parent == null && (!kinds || (a.workspace_id === workspaceId && kinds.includes(a.kind))),
    );
  });

  async function pollJobs() {
    if (!onListJobs) return;
    activities = (await onListJobs()) ?? [];
  }

  // Newest job of the scope until one is picked; a pick that left the list
  // (replaced by a rerun) falls back the same way.
  $effect(() => {
    if (!jobs.some((j) => j.id === selectedJob)) {
      selectedJob = jobs[0]?.id ?? null;
      selectedChild = null;
    }
  });

  function pickJob(id) {
    selectedJob = id;
    selectedChild = null;
  }

  // A workspace id is its folder name plus a 16-hex-digit hash.
  function workspaceLabel(id) {
    return id ? id.replace(/-[0-9a-f]{16}$/, '') : 'fghjd';
  }

  function jobLabel(j) {
    if (j.kind === 'remove') return 'Remove workspace';
    if (j.kind === 'switch') return `Switch to ${j.key}`;
    if (j.kind === 'start') return `Start ${j.key}`;
    if (j.kind === 'stop') return `Stop ${j.key}`;
    if (j.kind === 'delete') return `Delete ${j.key}`;
    if (j.key === 'pull-all') return 'Pull all repos';
    if (j.key.startsWith('pull-flow:')) return `Pull ${j.key.slice('pull-flow:'.length)}`;
    if (j.key.startsWith('node:')) return `Download ${j.key.slice('node:'.length)}`;
    // A pull's child is one clone, keyed by its path.
    return j.key;
  }

  let job = $derived(jobs.find((j) => j.id === selectedJob) ?? null);
  // In the order they were planned or reached; a job with none is its own
  // single step.
  let steps = $derived.by(() => {
    if (!job) return [];
    const children = activities.filter((a) => a.parent === job.id).reverse();
    return children.length ? children : [job];
  });
  let shown = $derived(steps.find((c) => c.id === selectedChild) ?? job);

  const RUNNING = {
    start: 'starting',
    stop: 'stopping',
    delete: 'deleting',
    pull: 'downloading',
    remove: 'removing',
    switch: 'switching',
  };
  const DONE = { start: 'up', stop: 'stopped', delete: 'deleted', pull: 'downloaded', remove: 'removed' };

  function stepStatus(a) {
    if (a.status === 'queued') return 'waiting';
    if (a.status === 'running') return RUNNING[a.kind] ?? 'running';
    if (a.status === 'done') return `✓ ${DONE[a.kind] ?? 'done'}`;
    if (a.status === 'error') return '✕ failed';
    return 'cancelled';
  }

  // A step is named for what it acts on: the node, the clone, the resource.
  function stepLabel(a) {
    return a.id === job?.id ? jobLabel(a) : a.key;
  }

  function seconds(a) {
    const ms = (a.finished_ms ?? Date.now()) - a.started_ms;
    return a.status === 'queued' ? '' : `${(ms / 1000).toFixed(ms < 10000 ? 1 : 0)}s`;
  }

  if (initialTab === 'doctor') runDoctor();

  $effect(() => {
    if (activeTab === 'logs') {
      pollLogs();
      const timer = setInterval(pollLogs, 1500);
      return () => clearInterval(timer);
    }
    if (jobTab) {
      pollJobs();
      const timer = setInterval(pollJobs, 800);
      return () => clearInterval(timer);
    }
  });

  // The installed entries behind a check, as [name, target] rows; null when
  // the check has none to show.
  function netRows(name) {
    if (!netStatus) return null;
    if (name === 'dns')
      return netStatus.hosts.map((h) => [h, netStatus.proxy_ip ?? '127.0.0.1']);
    if (name === 'resolver-files')
      return netStatus.resolver_zones.map((z) => [`*.${z.zone}`, `127.0.0.1:${z.port}`]);
    if (name === 'raw-routes')
      return netStatus.raw_routes.map((r) => [
        `${r.raw_domain ?? r.virtual_ip}:${r.container_port}`,
        `${r.virtual_ip}:${r.container_port} → 127.0.0.1:${r.host_port}`,
      ]);
    return null;
  }

  function toggle(name) {
    expanded[name] = !expanded[name];
  }

  const MARKS = { pass: '\u2713', pending: '\u00b7', warn: '!', fail: '\u2717' };

  function formatAge(ms) {
    if (!ms) return 'never';
    const secs = Math.max(0, Math.round((Date.now() - ms) / 1000));
    if (secs < 2) return 'just now';
    if (secs < 60) return `${secs}s ago`;
    return `${Math.round(secs / 60)}m ago`;
  }
</script>

{#snippet netList(rows)}
  <div class="net-list">
    {#each rows as [k, v]}
      <div class="row"><span class="k">{k}</span><span class="v">{v}</span></div>
    {:else}
      <div class="empty">nothing installed</div>
    {/each}
  </div>
{/snippet}

<SideDrawer onClose={onClose} width="55vw">
  <div class="head">
    <div class="sections">
      <button class="section-btn" class:active={section === 'workspace'} onclick={() => select(SECTIONS.workspace[0][0])}
        >{workspaceName ?? 'workspace'}</button
      >
      <button class="section-btn" class:active={section === 'fghjd'} onclick={() => select(SECTIONS.fghjd[0][0])}>fghjd</button>
    </div>
    <div class="close" onclick={onClose}>×</div>
  </div>
  <h3 class="stencil">{section === 'workspace' ? (workspaceName ?? 'This workspace') : 'fghjd'}</h3>
  <p class="scope-note">
    {section === 'workspace'
      ? 'What fghj has done in this workspace.'
      : 'The daemon every workspace shares.'}
  </p>

  <div class="tabs">
    {#each SECTIONS[section] as [id, label] (id)}
      <div class="tab" class:active={activeTab === id} onclick={() => select(id)}>{label}</div>
    {/each}
  </div>

  {#if activeTab === 'logs'}
    <div class="logs">
      <div class="log-bar">
        <input
          class="log-filter"
          type="text"
          placeholder="filter lines…"
          bind:value={logFilter}
          spellcheck="false"
        />
        <span class="log-count">
          {#if logFilter.trim()}
            {visibleLines.length} of {logLines.length}
          {:else}
            {logLines.length} {logLines.length === 1 ? 'line' : 'lines'}
          {/if}
        </span>
      </div>
      {#if !logLines.length}
        <div class="empty">no log lines yet</div>
      {:else if !visibleLines.length}
        <div class="empty">nothing matches “{logFilter.trim()}”</div>
      {:else}
        <LogPane lines={visibleLines} />
      {/if}
    </div>
  {:else if jobTab}
    <div class="ops">
      <div class="ops-list">
        {#if !jobs.length}
          <div class="empty">nothing has run yet</div>
        {/if}
        {#each jobs as j (j.id)}
          <div class="ops-row" class:active={j.id === selectedJob} onclick={() => pickJob(j.id)}>
            <span class="dot" class:running={j.status === 'running'} class:done={j.status === 'done'} class:error={j.status === 'error'}></span>
            <span class="ops-label"
              >{jobLabel(j)}{#if activeTab === 'activity'}<span class="ops-ws">{workspaceLabel(j.workspace_id)}</span>{/if}</span
            >
          </div>
        {/each}
      </div>
      <div class="ops-log">
        {#if job}
          <div class="ops-steps">
            {#each steps as c (c.id)}
              <button
                class="row step-{c.status}"
                class:picked={c.id === selectedChild}
                disabled={c.id === job.id}
                onclick={() => (selectedChild = selectedChild === c.id ? null : c.id)}
                title={c.id === job.id ? null : selectedChild === c.id ? 'back to the whole log' : 'only this step’s log'}
              >
                <span class="k">{stepLabel(c)}</span>
                <span class="v">
                  {#if c.status === 'running'}<span class="dot running"></span>{/if}
                  {stepStatus(c)}<span class="secs">{seconds(c)}</span>
                </span>
              </button>
            {/each}
          </div>
        {/if}
        {#if shown}
          <LogPane lines={textLines(shown.log || '(no output yet)')} />
        {:else}
          <div class="empty">select a job to see its log</div>
        {/if}
      </div>
    </div>
  {:else if activeTab === 'help'}
    <div class="net help">
      <div class="section-title">How to read the map</div>
      <p><b>Solid line</b> — <code>depends_on</code>: the left node needs the right one to start. It starts after it, waits for it, and is blocked if it fails. <b>Dashed</b> — <code>required: false</code>: the left node needs the right one at runtime. Nothing waits, and it starts only if a flow lists it.</p>
      <p><b>Tinted block</b> — infrastructure (postgres, redis): fixed image, no branch.</p>
      <p>Click a node to see its resolved repo, branch and domain. Switch flows with the picker top-left — a flow is <code>repo/flow</code>, and what starting it would start highlights in accent color; every known repo still renders regardless of flow.</p>
    </div>
  {:else}
    <div class="net">
      <div class="doc-head">
        <div class="net-meta">
          read from the machine itself{#if doctorAt}, <b>{formatAge(doctorAt)}</b>{/if}
        </div>
        <button class="rerun" onclick={runDoctor} disabled={doctorBusy}>
          {doctorBusy ? 'checking…' : 're-run'}
        </button>
      </div>

      {#if doctorChecks === null}
        <div class="empty">checking…</div>
      {:else if !doctorChecks.length}
        <div class="empty">fghjd returned no checks</div>
      {:else}
        <div class="section">
          {#each doctorChecks as c}
            {@const rows = netRows(c.name)}
            <div class="check {c.verdict}" class:expandable={rows} onclick={() => rows && toggle(c.name)}>
              <div class="check-head">
                <span class="mark">{MARKS[c.verdict] ?? '?'}</span>
                <span class="check-title">{c.title}</span>
                {#if rows}<span class="chevron">{expanded[c.name] ? '▾' : '▸'} {rows.length}</span>{/if}
              </div>
              <div class="check-detail">{c.detail}</div>
              {#if c.hint}
                <div class="check-hint">→ {c.hint}</div>
              {/if}
              {#if rows && expanded[c.name]}
                {@render netList(rows)}
              {/if}
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
</SideDrawer>

<style>
  .head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 10px; }
  .sections { display: flex; gap: 2px; padding: 2px; border: 1px solid var(--line); border-radius: 6px; }
  .section-btn {
    padding: 4px 10px; border: none; border-radius: 4px; background: none; cursor: pointer;
    font: 600 11px var(--font-mono); color: var(--ink-faint);
  }
  .section-btn.active { background: var(--panel-2); color: var(--ink); }
  .scope-note { font: 500 12px var(--font-mono); color: var(--ink-faint); margin: -14px 0 16px; }
  .close { width: 22px; height: 22px; border-radius: 50%; background: var(--panel-2); display: flex; align-items: center; justify-content: center; cursor: pointer; color: var(--ink-dim); font-size: 14px; }
  h3 { font-size: 20px; color: var(--ink); margin-bottom: 20px; }

  .tabs { display: flex; gap: 2px; padding: 2px; background: var(--panel-2); border-radius: 6px; margin-bottom: 20px; width: fit-content; }
  .tab {
    padding: 6px 14px; border-radius: 4px; font: 700 11px var(--font-mono); text-transform: uppercase;
    letter-spacing: 0.04em; cursor: pointer; color: var(--ink-faint);
  }
  .tab.active { background: var(--accent); color: var(--bg); }

  .empty { font: 500 12px var(--font-mono); color: var(--ink-faint); font-style: italic; padding: 10px 0; }

  /* Every tab body takes the rest of the drawer's height. */
  .logs, .ops, .net { flex: 1 1 auto; min-height: 0; }
  .logs { display: flex; flex-direction: column; gap: 8px; }

  .log-bar { display: flex; align-items: center; gap: 10px; }
  .log-filter {
    flex: 1; min-width: 0; padding: 5px 9px; border-radius: 5px;
    border: 1px solid var(--line-strong); background: var(--panel);
    font: 400 11.5px var(--font-mono); color: var(--ink);
  }
  .log-filter:focus { outline: none; border-color: var(--accent); }
  .log-count { font: 500 10.5px var(--font-mono); color: var(--ink-faint); white-space: nowrap; }

  .ops { display: flex; gap: 16px; }
  .ops-list { flex: 0 0 200px; display: flex; flex-direction: column; gap: 4px; overflow-y: auto; }
  .ops-row {
    display: flex; align-items: center; gap: 8px; padding: 8px 10px; border-radius: 5px; cursor: pointer;
    font: 500 11.5px var(--font-mono); color: var(--ink-dim); border: 1px solid transparent;
  }
  .ops-row:hover { background: var(--panel-2); }
  .ops-row.active { background: var(--panel-2); border-color: var(--line-strong); color: var(--ink); }
  .ops-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; display: flex; flex-direction: column; }
  .ops-steps { display: flex; flex-direction: column; margin-bottom: 10px; max-height: 40%; overflow-y: auto; flex: 0 0 auto; }
  .ops-steps .row { border: 1px solid transparent; width: 100%; text-align: left; cursor: pointer; }
  .ops-steps .row:disabled { cursor: default; }
  .ops-steps .row:not(:disabled):hover, .ops-steps .row.picked { border-color: var(--line-strong); }
  .ops-steps .row .k { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .ops-steps .row .v { white-space: nowrap; }
  .secs { color: var(--ink-faint); margin-left: 8px; font-size: 0.9em; }
  .ops-ws { color: var(--ink-faint); font-size: 0.9em; overflow: hidden; text-overflow: ellipsis; }
  .dot { width: 8px; height: 8px; border-radius: 50%; flex: 0 0 auto; background: var(--ink-faint); }
  .dot.running { background: var(--accent); animation: pulse 1s ease-in-out infinite; }
  .dot.done { background: var(--success, #6fdc8c); }
  .dot.error { background: var(--danger); }
  @keyframes pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.4; } }
  .ops-log { flex: 1; min-width: 0; display: flex; flex-direction: column; }

  .net { display: flex; flex-direction: column; gap: 20px; overflow-y: auto; }
  .help { gap: 12px; }
  .help p { margin: 0; font: 400 12.5px/1.55 var(--font-body); color: var(--ink-dim); }
  .help b { color: var(--ink); }
  .net-meta { font: 500 11.5px var(--font-mono); color: var(--ink-dim); }
  .net-meta b { color: var(--ink); }
  .section-title { font: 700 10px var(--font-mono); text-transform: uppercase; letter-spacing: 0.06em; color: var(--ink-faint); margin-bottom: 8px; }
  .row { display: flex; justify-content: space-between; gap: 12px; padding: 5px 8px; border-radius: 4px; font: 500 11.5px var(--font-mono); background: var(--panel-2); margin-bottom: 3px; }
  .row .k { color: var(--ink); }
  .row .v { color: var(--ink-dim); word-break: break-all; text-align: right; }

  .row .v .dot { display: inline-block; margin-right: 6px; }
  .row.step-running .v { color: var(--accent); }
  .row.step-done .v { color: #4ac97e; }
  .row.step-error .v { color: var(--danger); }
  .row.step-queued .v, .row.step-cancelled .v { color: var(--ink-faint); }

  .doc-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  .rerun {
    border: 1px solid var(--line-strong); background: var(--panel-2); color: var(--ink);
    border-radius: 4px; padding: 5px 12px; cursor: pointer;
    font: 700 10px var(--font-mono); text-transform: uppercase; letter-spacing: 0.06em;
  }
  .rerun:disabled { color: var(--ink-faint); cursor: default; }

  .check {
    padding: 8px 10px; border-radius: 4px; background: var(--panel-2); margin-bottom: 4px;
    border-left: 3px solid var(--line-strong);
  }
  /* Verdict carries a shape as well as a color — the mark glyph — so the
     states stay distinguishable without relying on hue. `pending` is "not set
     up yet, nothing to do": it keeps the neutral border. */
  .check.pass { border-left-color: #4ac97e; }
  .check.warn { border-left-color: #d9a73c; }
  .check.fail { border-left-color: #e05c5c; }
  .check-head { display: flex; align-items: baseline; gap: 8px; }
  .mark { font: 700 12px var(--font-mono); }
  .check.pass .mark { color: #4ac97e; }
  .check.pending .mark { color: var(--ink-faint); }
  .check.warn .mark { color: #d9a73c; }
  .check.fail .mark { color: #e05c5c; }
  .check-title { font: 700 11.5px var(--font-mono); color: var(--ink); }
  .check.expandable { cursor: pointer; }
  .chevron { margin-left: auto; font: 600 10.5px var(--font-mono); color: var(--ink-faint); }
  .net-list { margin: 8px 0 2px 20px; }
  .net-list .row { background: var(--panel); }
  .check-detail { font: 500 11.5px var(--font-mono); color: var(--ink-dim); margin: 3px 0 0 20px; word-break: break-word; }
  .check-hint { font: 500 11.5px var(--font-mono); color: var(--ink); margin: 4px 0 0 20px; word-break: break-word; }
</style>
