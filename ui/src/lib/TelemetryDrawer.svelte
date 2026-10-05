<script>
  import SideDrawer from './SideDrawer.svelte';

  let { onClose, onFetchDaemonLogs, onFetchNetStatus, onFetchDoctor } = $props();

  let activeTab = $state('logs');

  // Logs tab: polling tail, same pattern as OperationsDrawer's pull-queue
  // log — daemon lifecycle/reconcile messages are low-volume enough that a
  // poll is just as responsive as SSE and much simpler.
  let logLines = $state([]);
  let lastSeq = $state(null);
  let logsPoll = null;
  let logsPre = $state(null);

  async function pollLogs() {
    if (!onFetchDaemonLogs) return;
    const entries = await onFetchDaemonLogs(lastSeq);
    if (!entries.length) return;
    logLines = [...logLines, ...entries].slice(-2000);
    lastSeq = entries[entries.length - 1].seq;
  }

  $effect(() => {
    logLines;
    if (logsPre) logsPre.scrollTop = logsPre.scrollHeight;
  });

  // Network tab: which /etc/hosts lines, /etc/resolver zones, and pf NAT
  // routes fghjd currently has installed, plus when it last reconciled them.
  let netStatus = $state(null);

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
  // The drawer always opens on `logs`, so a click is the only way in here.
  let doctorChecks = $state(null);
  let doctorBusy = $state(false);
  let doctorAt = $state(null);

  async function runDoctor() {
    if (!onFetchDoctor || doctorBusy) return;
    doctorBusy = true;
    try {
      const report = await onFetchDoctor();
      doctorChecks = report?.checks ?? [];
      doctorAt = Date.now();
    } finally {
      doctorBusy = false;
    }
  }

  function openDoctor() {
    activeTab = 'doctor';
    if (doctorChecks === null) runDoctor();
  }

  $effect(() => {
    if (activeTab === 'logs') {
      pollLogs();
      const timer = setInterval(pollLogs, 1500);
      return () => clearInterval(timer);
    }
    if (activeTab === 'network') {
      pollNetStatus();
      const timer = setInterval(pollNetStatus, 2000);
      return () => clearInterval(timer);
    }
  });

  const MARKS = { pass: '\u2713', warn: '!', fail: '\u2717' };

  function formatTime(ms) {
    return new Date(ms).toLocaleTimeString();
  }

  function formatAge(ms) {
    if (!ms) return 'never';
    const secs = Math.max(0, Math.round((Date.now() - ms) / 1000));
    if (secs < 2) return 'just now';
    if (secs < 60) return `${secs}s ago`;
    return `${Math.round(secs / 60)}m ago`;
  }
</script>

<SideDrawer onClose={onClose} width="55vw">
  <div class="head">
    <span class="eyebrow">fghjd telemetry</span>
    <div class="close" onclick={onClose}>×</div>
  </div>
  <h3 class="stencil">Daemon</h3>

  <div class="tabs">
    <div class="tab" class:active={activeTab === 'logs'} onclick={() => (activeTab = 'logs')}>Logs</div>
    <div class="tab" class:active={activeTab === 'network'} onclick={() => (activeTab = 'network')}>DNS / DNAT</div>
    <div class="tab" class:active={activeTab === 'doctor'} onclick={openDoctor}>Doctor</div>
  </div>

  {#if activeTab === 'logs'}
    <div class="logs">
      {#if !logLines.length}
        <div class="empty">no log lines yet</div>
      {:else}
        <pre bind:this={logsPre}>{logLines
            .map((l) => `[${formatTime(l.ts_ms)}]${l.level === 'warn' ? ' !' : ''} ${l.message}`)
            .join('\n')}</pre>
      {/if}
    </div>
  {:else if activeTab === 'network'}
    <div class="net">
      {#if !netStatus}
        <div class="empty">loading…</div>
      {:else}
        <div class="net-meta">last reconciled: <b>{formatAge(netStatus.last_reconcile_ms)}</b></div>

        <div class="section">
          <div class="section-title">/etc/hosts</div>
          {#if !netStatus.hosts.length}
            <div class="empty">no managed entries</div>
          {:else}
            {#each netStatus.hosts as h}
              <div class="row"><span class="k">{h}</span><span class="v">{netStatus.proxy_ip ?? '127.0.0.1'}</span></div>
            {/each}
          {/if}
        </div>

        <div class="section">
          <div class="section-title">/etc/resolver zones</div>
          {#if !netStatus.resolver_zones.length}
            <div class="empty">no managed zones</div>
          {:else}
            {#each netStatus.resolver_zones as z}
              <div class="row"><span class="k">*.{z.zone}</span><span class="v">127.0.0.1:{z.port}</span></div>
            {/each}
          {/if}
        </div>

        <div class="section">
          <div class="section-title">raw-zone NAT routes (pf)</div>
          {#if !netStatus.raw_routes.length}
            <div class="empty">no active routes</div>
          {:else}
            {#each netStatus.raw_routes as r}
              <div class="row">
                <span class="k">{r.raw_domain ?? r.virtual_ip}:{r.container_port}</span>
                <span class="v">{r.virtual_ip}:{r.container_port} → 127.0.0.1:{r.host_port}</span>
              </div>
            {/each}
          {/if}
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
            <div class="check {c.verdict}">
              <div class="check-head">
                <span class="mark">{MARKS[c.verdict] ?? '?'}</span>
                <span class="check-title">{c.title}</span>
              </div>
              <div class="check-detail">{c.detail}</div>
              {#if c.hint}
                <div class="check-hint">→ {c.hint}</div>
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

  .logs pre {
    height: calc(100vh - 220px); margin: 0; background: #000; color: #b8ffb8; padding: 12px; border-radius: 6px;
    font: 400 11px var(--font-mono); overflow: auto; white-space: pre-wrap; word-break: break-all;
  }

  .net { display: flex; flex-direction: column; gap: 20px; overflow-y: auto; height: calc(100vh - 220px); }
  .net-meta { font: 500 11.5px var(--font-mono); color: var(--ink-dim); }
  .net-meta b { color: var(--ink); }
  .section-title { font: 700 10px var(--font-mono); text-transform: uppercase; letter-spacing: 0.06em; color: var(--ink-faint); margin-bottom: 8px; }
  .row { display: flex; justify-content: space-between; gap: 12px; padding: 5px 8px; border-radius: 4px; font: 500 11.5px var(--font-mono); background: var(--panel-2); margin-bottom: 3px; }
  .row .k { color: var(--ink); }
  .row .v { color: var(--ink-dim); word-break: break-all; text-align: right; }

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
  .check-detail { font: 500 11.5px var(--font-mono); color: var(--ink-dim); margin: 3px 0 0 20px; word-break: break-word; }
  .check-hint { font: 500 11.5px var(--font-mono); color: var(--ink); margin: 4px 0 0 20px; word-break: break-word; }
</style>
