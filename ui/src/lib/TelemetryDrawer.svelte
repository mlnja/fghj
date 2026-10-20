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
    env,
    nodes,
  } = $props();

  let activeTab = $state(initialTab);

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

  // What fghjd has installed — /etc/hosts lines, /etc/resolver zones, pf NAT
  // routes — read alongside the doctor, and shown under the check it backs.
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

  // Pull queue tab: clones and downloads, each with its own log.
  let jobs = $state([]);
  let selectedJob = $state(null);

  async function pollJobs() {
    if (!onListJobs) return;
    jobs = (await onListJobs()) ?? [];
    if (!selectedJob && jobs.length) selectedJob = jobs[0].key;
  }

  function jobLabel(key) {
    if (key === 'pull-all') return 'Pull all repos';
    if (key.startsWith('node:')) return `Download ${key.slice('node:'.length)}`;
    return key;
  }

  let job = $derived(jobs.find((j) => j.key === selectedJob) ?? null);

  // Switch tab: where the switch in flight stands, node by node. Derived from
  // the same live `env` the graph draws, so it needs no poll of its own: the
  // flow's members come up, everything else running goes down.
  const STEPS = {
    starting: 'starting',
    stopping: 'stopping',
    waiting: 'to start',
    'to-stop': 'to stop',
    ok: 'up',
    done: 'done',
  };
  const STEP_ORDER = Object.keys(STEPS);

  let switchFlow = $derived(env?.pending_create?.flow ?? null);
  let switchPlan = $derived.by(() => {
    if (!switchFlow) return [];
    const containers = env?.containers ?? {};
    const inFlow = new Set((nodes ?? []).filter((n) => n.flows?.includes(switchFlow)).map((n) => n.id));
    const rows = [];
    for (const id of inFlow) {
      const c = containers[id];
      const step =
        c?.pending_action === 'starting' || c?.pending_action === 'restarting'
          ? 'starting'
          : c?.condition === 'running'
            ? 'ok'
            : c?.condition === 'completed'
              ? 'done'
              : 'waiting';
      rows.push({ id, step });
    }
    for (const [id, c] of Object.entries(containers)) {
      if (inFlow.has(id)) continue;
      if (c.pending_action === 'stopping') rows.push({ id, step: 'stopping' });
      else if (c.observed.status === 'running') rows.push({ id, step: 'to-stop' });
    }
    return rows.sort(
      (a, b) => STEP_ORDER.indexOf(a.step) - STEP_ORDER.indexOf(b.step) || a.id.localeCompare(b.id),
    );
  });
  let switchLeft = $derived(switchPlan.filter((r) => r.step !== 'ok' && r.step !== 'done').length);

  function openDoctor() {
    activeTab = 'doctor';
    if (doctorChecks === null) runDoctor();
  }

  if (initialTab === 'doctor') runDoctor();

  $effect(() => {
    if (activeTab === 'logs') {
      pollLogs();
      const timer = setInterval(pollLogs, 1500);
      return () => clearInterval(timer);
    }
    if (activeTab === 'pulls') {
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

  const MARKS = { pass: '\u2713', warn: '!', fail: '\u2717' };

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
    <span class="eyebrow">fghjd telemetry</span>
    <div class="close" onclick={onClose}>×</div>
  </div>
  <h3 class="stencil">Daemon</h3>

  <div class="tabs">
    <div class="tab" class:active={activeTab === 'logs'} onclick={() => (activeTab = 'logs')}>Logs</div>
    <div class="tab" class:active={activeTab === 'pulls'} onclick={() => (activeTab = 'pulls')}>Pull queue</div>
    <div class="tab" class:active={activeTab === 'doctor'} onclick={openDoctor}>Doctor</div>
    <div class="tab" class:active={activeTab === 'switch'} onclick={() => (activeTab = 'switch')}>Switch</div>
    <div class="tab" class:active={activeTab === 'help'} onclick={() => (activeTab = 'help')}>Help</div>
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
  {:else if activeTab === 'pulls'}
    <div class="ops">
      <div class="ops-list">
        {#if !jobs.length}
          <div class="empty">no pulls yet</div>
        {/if}
        {#each jobs as j (j.key)}
          <div class="ops-row" class:active={j.key === selectedJob} onclick={() => (selectedJob = j.key)}>
            <span class="dot" class:running={j.status === 'running'} class:done={j.status === 'done'} class:error={j.status === 'error'}></span>
            <span class="ops-label">{jobLabel(j.key)}</span>
          </div>
        {/each}
      </div>
      <div class="ops-log">
        {#if job}
          <LogPane lines={textLines(job.log || '(no output yet)')} />
        {:else}
          <div class="empty">select a pull to see its log</div>
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
  {:else if activeTab === 'switch'}
    <div class="net">
      {#if !switchFlow}
        <div class="empty">no switch in flight</div>
      {:else}
        <div class="net-meta">
          switching to <b>{switchFlow}</b>: {switchLeft ? `${switchLeft} left to do` : 'finishing up'}
        </div>
        <div class="section">
          {#each switchPlan as r (r.id)}
            <div class="row step-{r.step}">
              <span class="k">{r.id}</span>
              <span class="v">
                {#if r.step === 'starting' || r.step === 'stopping'}<span class="dot running"></span>{/if}
                {STEPS[r.step]}
              </span>
            </div>
          {/each}
        </div>
      {/if}
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
  .eyebrow { font: 700 10px var(--font-mono); text-transform: uppercase; letter-spacing: 0.08em; color: var(--ink-faint); }
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
  .ops-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
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
  .row.step-starting .v, .row.step-stopping .v { color: var(--accent); }
  .row.step-ok .v, .row.step-done .v { color: #4ac97e; }
  .row.step-waiting .v, .row.step-to-stop .v { color: var(--ink-faint); }

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
     three states stay distinguishable without relying on hue. */
  .check.pass { border-left-color: #4ac97e; }
  .check.warn { border-left-color: #d9a73c; }
  .check.fail { border-left-color: #e05c5c; }
  .check-head { display: flex; align-items: baseline; gap: 8px; }
  .mark { font: 700 12px var(--font-mono); }
  .check.pass .mark { color: #4ac97e; }
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
