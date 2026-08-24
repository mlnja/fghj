<script>
  import SideDrawer from './SideDrawer.svelte';

  let { node, onClose, liveInfo, onFetchLogs, onLogStreamUrl, onDownload, onPullStatus, onDownloadComplete } = $props();
  let activeTab = $state('general');
  let logs = $state('');
  let loadingLogs = $state(false);
  let streaming = $state(false);
  let downloading = $state(false);
  let dlStatus = $state(null); // null | 'running' | 'done' | 'error'
  let dlLog = $state('');
  let pollTimer = null;

  async function loadLogs() {
    if (!onFetchLogs) return;
    loadingLogs = true;
    logs = await onFetchLogs(node.id);
    loadingLogs = false;
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
    };
  });

  // Live-follows the container's log output over SSE whenever the Logs tab
  // is open against a running container, so new lines show up without
  // re-clicking "load logs". Re-runs (tearing down the previous connection
  // first) whenever the tab, node, or live container identity changes.
  $effect(() => {
    if (activeTab !== 'logs' || !liveInfo || !onLogStreamUrl) return;
    const url = onLogStreamUrl(node.id);
    if (!url) return;

    const source = new EventSource(url);
    streaming = true;
    source.onmessage = (e) => {
      logs = logs ? `${logs}\n${e.data}` : e.data;
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

    <div class="tabs">
      <div class="tab" class:active={activeTab === 'general'} onclick={() => (activeTab = 'general')}>General info</div>
      <div class="tab" class:active={activeTab === 'logs'} onclick={() => (activeTab = 'logs')}>Logs</div>
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
        {#if node.domain}
          <div class="row"><span class="k">domain</span><span class="v">{node.domain}</span></div>
        {/if}
        {#if node.image}
          <div class="row"><span class="k">image</span><span class="v">{node.image}</span></div>
        {/if}
        {#if node.ports?.length}
          <div class="row"><span class="k">ports</span><span class="v">{node.ports.join(', ')}</span></div>
        {/if}
        <div class="row"><span class="k">flows</span><span class="v">{node.flows?.join(', ') || '—'}</span></div>
        {#if liveInfo && node.downloaded !== false}
          <div class="row"><span class="k">container status</span><span class="v">{liveInfo.status}</span></div>
          <div class="row"><span class="k">container name</span><span class="v">{liveInfo.container_name}</span></div>
          {#if liveInfo.published_port}
            <div class="row"><span class="k">open</span><span class="v"><a href="http://127.0.0.1:{liveInfo.published_port}" target="_blank">127.0.0.1:{liveInfo.published_port}</a></span></div>
          {/if}
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
    {:else if liveInfo}
      <div class="logs">
        <div class="logs-toolbar">
          <button class="btn" onclick={loadLogs}>{loadingLogs ? 'loading…' : 'load logs'}</button>
          {#if streaming}<span class="live-badge"><span class="spinner"></span> live</span>{/if}
        </div>
        {#if logs}
          <pre>{logs}</pre>
        {/if}
      </div>
    {:else}
      <div class="logs-empty">no live container — start a run to see logs</div>
    {/if}
</SideDrawer>

<style>
  .head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 10px; }
  .close { width: 22px; height: 22px; border-radius: 50%; background: var(--panel-2); display: flex; align-items: center; justify-content: center; cursor: pointer; color: var(--ink-dim); font-size: 14px; }
  h3 { font-size: 20px; color: var(--ink); margin-bottom: 20px; word-break: break-all; }
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
  .row.not-downloaded .v { color: var(--ink-faint); font-style: italic; }
  .logs { margin-top: 20px; display: flex; flex-direction: column; gap: 10px; }
  .logs .btn {
    align-self: flex-start; padding: 6px 10px; border-radius: 4px; background: var(--panel-2);
    border: 1px solid var(--line-strong); color: var(--ink); font: 700 10px var(--font-mono);
    text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer;
  }
  .logs .btn:disabled { opacity: 0.6; cursor: default; }
  .logs-toolbar { display: flex; align-items: center; gap: 10px; }
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
