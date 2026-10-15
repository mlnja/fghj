<script>
  // HTTP traffic the host proxy forwarded to this node, DevTools-style.
  // Recording lives in fghjd, not here: closing the drawer or reloading the
  // page doesn't stop it, only the Stop button or a fghjd restart does. This
  // component only follows it, over SSE, while the tab is open. See
  // `concepts/traffic-capture.md`.
  let { nodeId, trafficUrl } = $props();

  let mode = $state(null);
  let exchanges = $state(new Map());
  let connected = $state(false);
  let pickedMode = $state('text');
  let query = $state('');
  let hiddenSources = $state(new Set());
  let selectedId = $state(null);
  let detail = $state(null);
  let detailTab = $state('headers');
  let copied = $state(false);

  const MODES = [
    { value: 'text', label: 'Text bodies', hint: 'JSON, text, XML, forms. Images and other binaries by size only.' },
    { value: 'all', label: 'All bodies', hint: 'Binary bodies too, up to 8 MiB each.' },
    { value: 'none', label: 'Headers only', hint: 'No bodies at all.' },
  ];

  $effect(() => {
    const source = new EventSource(trafficUrl(nodeId, '/stream'));
    source.onopen = () => (connected = true);
    source.onerror = () => (connected = false);
    source.onmessage = (e) => {
      const msg = JSON.parse(e.data);
      if (msg.type === 'snapshot') {
        mode = msg.mode;
        exchanges = new Map(msg.exchanges.map((x) => [x.id, x]));
      } else if (msg.type === 'upsert') {
        const next = new Map(exchanges);
        next.set(msg.exchange.id, msg.exchange);
        exchanges = next;
      } else if (msg.type === 'mode') {
        mode = msg.mode;
      } else if (msg.type === 'cleared') {
        exchanges = new Map();
        selectedId = null;
      }
    };
    return () => {
      connected = false;
      source.close();
    };
  });

  let sources = $derived([...new Set([...exchanges.values()].map((x) => x.source))].sort());

  let rows = $derived.by(() => {
    const q = query.trim().toLowerCase();
    return [...exchanges.values()]
      .filter((x) => !hiddenSources.has(x.source))
      .filter((x) => !q || `${x.method} ${x.status ?? ''} ${x.url}`.toLowerCase().includes(q))
      .sort((a, b) => b.id - a.id);
  });

  let selected = $derived(selectedId == null ? null : exchanges.get(selectedId));

  // Refetched whenever the exchange moves on (response arrived, body
  // finished), so an open detail view fills in as the request completes.
  $effect(() => {
    if (!selected) {
      detail = null;
      return;
    }
    const key = `${selected.state}:${selected.response_size}`;
    void key;
    fetchDetail(selected.id);
  });

  async function fetchDetail(id) {
    const res = await fetch(trafficUrl(nodeId, `/${id}`));
    if (res.ok && selectedId === id) detail = await res.json();
  }

  async function setMode(next) {
    await fetch(trafficUrl(nodeId, '/recording'), {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ mode: next }),
    });
  }

  async function clear() {
    await fetch(trafficUrl(nodeId, '/clear'), { method: 'POST' });
  }

  function toggleSource(s) {
    const next = new Set(hiddenSources);
    next.has(s) ? next.delete(s) : next.add(s);
    hiddenSources = next;
  }

  function open(x) {
    detail = null;
    selectedId = x.id;
    detailTab = 'headers';
  }

  function formatSize(n) {
    if (n == null) return '';
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} kB`;
    return `${(n / 1024 / 1024).toFixed(1)} MB`;
  }

  function formatMs(ms) {
    if (ms == null) return '';
    return ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(2)} s`;
  }

  function formatTime(ms) {
    return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  }

  function statusLabel(x) {
    if (x.upgraded) return '101 ⇄';
    if (x.status == null) return '…';
    return String(x.status);
  }

  function isError(x) {
    return x.state === 'error' || x.state === 'aborted' || (x.status ?? 0) >= 400;
  }

  // Pretty-printed when it parses as JSON, verbatim otherwise.
  function pretty(body) {
    const ct = (body.content_type ?? '').toLowerCase();
    if (ct.includes('json') && !body.truncated) {
      try {
        return JSON.stringify(JSON.parse(body.text), null, 2);
      } catch {
        // not JSON after all
      }
    }
    return body.text;
  }

  function shellQuote(s) {
    return `'${s.replaceAll("'", `'\\''`)}'`;
  }

  function curlFor(d) {
    const skip = new Set(['host', 'content-length', 'connection', 'accept-encoding']);
    const parts = ['curl', shellQuote(`${d.scheme}://${d.host}${d.url}`)];
    if (d.method !== 'GET') parts.push('-X', d.method);
    for (const [k, v] of d.request.headers) {
      if (!skip.has(k)) parts.push('-H', shellQuote(`${k}: ${v}`));
    }
    if (d.request.body.text != null) parts.push('--data-raw', shellQuote(d.request.body.text));
    return parts.join(' ');
  }

  async function copyCurl() {
    await navigator.clipboard.writeText(curlFor(detail));
    copied = true;
    setTimeout(() => (copied = false), 1200);
  }

  function bodyUrl(side) {
    return trafficUrl(nodeId, `/${detail.id}/body/${side}`);
  }
</script>

{#snippet bodyView(body, side)}
  {@const ct = body.content_type ?? 'no content type'}
  <div class="body-meta">
    {ct}{body.content_encoding ? ` · ${body.content_encoding}` : ''} · {formatSize(body.size)}
  </div>
  {#if body.truncated}
    <div class="note">Truncated: kept the first 8 MiB of {formatSize(body.size)}.</div>
  {/if}
  {#if body.state === 'aborted'}
    <div class="note">The connection closed before the body finished.</div>
  {/if}
  {#if body.decode_error}
    <div class="note">{body.decode_error}</div>
  {/if}
  {#if body.omitted === 'headers_only'}
    <div class="logs-empty">Not kept: this was recorded with headers only.</div>
  {:else if body.omitted === 'binary'}
    <div class="logs-empty">Binary body, not kept in Text mode. Record with All bodies to keep it.</div>
  {:else if body.parts}
    <div class="parts">
      {#each body.parts as p}
        <div class="part">
          <div class="part-head">
            <b>{p.name ?? '(unnamed)'}</b>
            {#if p.filename}<span>{p.filename}</span>{/if}
            {#if p.content_type}<span>{p.content_type}</span>{/if}
            <span>{formatSize(p.size)}</span>
          </div>
          {#if p.text != null}<pre>{p.text}</pre>{/if}
        </div>
      {/each}
    </div>
  {:else if body.text != null}
    <pre>{pretty(body)}</pre>
  {:else if body.binary}
    {#if ct.startsWith('image/')}
      <img class="preview" src={bodyUrl(side)} alt="response body" />
    {/if}
    <a class="download" href={bodyUrl(side)}>Download</a>
  {:else if body.size === 0 && body.state !== 'streaming'}
    <div class="logs-empty">Empty.</div>
  {:else}
    <div class="logs-empty">Waiting for the body…</div>
  {/if}
{/snippet}

<div class="traffic">
  <div class="toolbar">
    {#if mode}
      <span class="rec-on"><span class="rec-dot"></span>recording</span>
      <select value={mode} onchange={(e) => setMode(e.currentTarget.value)}>
        {#each MODES as m}<option value={m.value} title={m.hint}>{m.label}</option>{/each}
      </select>
      <button class="btn danger" onclick={() => setMode(null)}>Stop</button>
    {:else}
      <button class="btn" onclick={() => setMode(pickedMode)}>⏺ Record</button>
      <select bind:value={pickedMode}>
        {#each MODES as m}<option value={m.value} title={m.hint}>{m.label}</option>{/each}
      </select>
    {/if}
    <button class="btn" onclick={clear} disabled={!exchanges.size}>Clear</button>
    {#if !connected}<span class="muted">reconnecting…</span>{/if}
  </div>

  {#if selected && detail}
    <div class="detail">
      <div class="detail-head">
        <button class="btn" onclick={() => (selectedId = null)}>← Back</button>
        <span class="method">{detail.method}</span>
        <span class="status" class:error={isError(detail)}>{statusLabel(detail)}</span>
        <span class="url" title={`${detail.scheme}://${detail.host}${detail.url}`}>{detail.url}</span>
      </div>
      {#if detail.error}<div class="note error">{detail.error}</div>{/if}
      <div class="detail-tabs">
        {#each ['headers', 'request', 'response', 'timing'] as t}
          <button class="sub-tab" class:active={detailTab === t} onclick={() => (detailTab = t)}>{t}</button>
        {/each}
        <button class="btn copy" onclick={copyCurl}>{copied ? 'Copied' : 'Copy as curl'}</button>
      </div>
      {#if detailTab === 'headers'}
        <div class="section-title">General</div>
        <div class="kv"><span>URL</span><span>{detail.scheme}://{detail.host}{detail.url}</span></div>
        <div class="kv"><span>Method</span><span>{detail.method}</span></div>
        <div class="kv"><span>Status</span><span>{detail.status ?? '—'}</span></div>
        <div class="kv"><span>Source</span><span>{detail.source}</span></div>
        <div class="section-title">Response headers</div>
        {#each detail.response.headers as [k, v]}
          <div class="kv"><span>{k}</span><span>{v}</span></div>
        {/each}
        <div class="section-title">Request headers</div>
        {#each detail.request.headers as [k, v]}
          <div class="kv"><span>{k}</span><span>{v}</span></div>
        {/each}
      {:else if detailTab === 'request'}
        {@render bodyView(detail.request.body, 'request')}
      {:else if detailTab === 'response'}
        {@render bodyView(detail.response.body, 'response')}
      {:else}
        <div class="kv"><span>Started</span><span>{new Date(detail.started_at_ms).toLocaleString()}</span></div>
        <div class="kv"><span>Waiting (TTFB)</span><span>{formatMs(detail.ttfb_ms) || '—'}</span></div>
        <div class="kv"><span>{detail.upgraded ? 'Open for' : 'Total'}</span><span>{formatMs(detail.duration_ms) || 'in progress'}</span></div>
      {/if}
    </div>
  {:else}
    <div class="filters">
      <input class="filter" placeholder="Filter by method, status or path" bind:value={query} />
      {#each sources as s}
        <button class="chip" class:off={hiddenSources.has(s)} onclick={() => toggleSource(s)}>{s}</button>
      {/each}
    </div>
    {#if rows.length}
      <div class="list">
        {#each rows as x (x.id)}
          <button class="row" class:error={isError(x)} onclick={() => open(x)}>
            <span class="method">{x.method}</span>
            <span class="status">{statusLabel(x)}</span>
            <span class="path" title={x.url}>{x.url}</span>
            <span class="src">{x.source}</span>
            <span class="size">{formatSize(x.response_size)}</span>
            <span class="time" title={formatTime(x.started_at_ms)}>{formatMs(x.duration_ms)}</span>
          </button>
        {/each}
      </div>
    {:else if exchanges.size}
      <div class="logs-empty">Nothing matches the filter.</div>
    {:else if mode}
      <div class="logs-empty">Recording. Requests through https://… will show up here.</div>
    {:else}
      <div class="logs-empty">Not recording. Press Record to capture this service's HTTP traffic. It keeps recording with the drawer closed, until you press Stop.</div>
    {/if}
  {/if}
</div>

<style>
  .traffic { margin-top: 20px; display: flex; flex-direction: column; gap: 10px; }
  .toolbar, .filters, .detail-head, .detail-tabs { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .btn, select, .chip, .sub-tab {
    padding: 5px 10px; border-radius: 4px; background: var(--panel-2);
    border: 1px solid var(--line-strong); color: var(--ink); font: 700 10px var(--font-mono);
    text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer;
  }
  .btn:disabled { opacity: 0.4; cursor: default; }
  .btn.danger { border-color: var(--danger); color: var(--danger); }
  .btn.copy { margin-left: auto; }
  .chip.off { opacity: 0.45; text-decoration: line-through; }
  .sub-tab.active { background: var(--accent); color: var(--bg); border-color: var(--accent); }
  .rec-on { display: flex; align-items: center; gap: 6px; font: 700 10px var(--font-mono); text-transform: uppercase; color: var(--danger); }
  .rec-dot { width: 8px; height: 8px; border-radius: 50%; background: var(--danger); animation: rec-pulse 1.4s ease-in-out infinite; }
  @keyframes rec-pulse { 50% { opacity: 0.35; } }
  .muted { color: var(--ink-faint); font: 500 11px var(--font-mono); font-style: italic; }
  .logs-empty { font: 500 12px var(--font-mono); color: var(--ink-faint); font-style: italic; }
  .filter {
    flex: 1 1 200px; padding: 5px 8px; border-radius: 4px; border: 1px solid var(--line-strong);
    background: var(--panel); color: var(--ink); font: 500 11px var(--font-mono);
  }
  .list { display: flex; flex-direction: column; gap: 1px; max-height: 60vh; overflow-y: auto; }
  .row {
    display: grid; grid-template-columns: 52px 46px 1fr auto 60px 60px; gap: 8px; align-items: baseline;
    padding: 4px 8px; border-radius: 4px; cursor: pointer; font: 500 11px var(--font-mono); color: var(--ink);
    border: 0; background: none; text-align: left; width: 100%;
  }
  .row:hover { background: var(--panel-2); }
  .row.error { color: var(--danger); background: var(--danger-bg); }
  .method { font-weight: 700; }
  .path, .url { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
  .url { font: 500 12px var(--font-mono); flex: 1; }
  .src, .size, .time { color: var(--ink-faint); text-align: right; }
  .row.error .src, .row.error .size, .row.error .time { color: inherit; }
  .status.error { color: var(--danger); }
  .detail { display: flex; flex-direction: column; gap: 8px; font: 500 11px var(--font-mono); }
  .section-title { margin-top: 8px; color: var(--ink-faint); text-transform: uppercase; font-size: 10px; letter-spacing: 0.06em; }
  .kv { display: grid; grid-template-columns: minmax(110px, 30%) 1fr; gap: 8px; }
  .kv span:first-child { color: var(--ink-dim); font-weight: 600; word-break: break-all; }
  .kv span:last-child { color: var(--ink); word-break: break-all; }
  .body-meta { color: var(--ink-faint); }
  .note { color: var(--warning); }
  .note.error { color: var(--danger); white-space: pre-wrap; }
  pre {
    background: #000; color: #b8ffb8; padding: 12px; border-radius: 6px; font: 400 11px var(--font-mono);
    max-height: 60vh; overflow: auto; white-space: pre-wrap; word-break: break-all; margin: 0;
  }
  .parts { display: flex; flex-direction: column; gap: 8px; }
  .part-head { display: flex; gap: 8px; color: var(--ink-dim); margin-bottom: 4px; }
  .preview { max-width: 100%; max-height: 50vh; border: 1px solid var(--line); border-radius: 4px; align-self: flex-start; }
  .download { color: var(--accent); }
</style>
