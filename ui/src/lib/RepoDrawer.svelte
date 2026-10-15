<script>
  import SideDrawer from './SideDrawer.svelte';

  let {
    repo,
    graph,
    currentFlow,
    workspacePath,
    onClose,
    onDownload,
    onPullStatus,
    onDownloadComplete,
    onFetch,
    onOpenNode,
  } = $props();

  let g = $derived(repo.git);
  let absolutePath = $derived(
    repo.local_path && workspacePath ? `${workspacePath}/${repo.local_path}` : repo.local_path
  );

  // Repos this one needs, and repos that need it: the edges of `reposGraph`
  // are already cross-repo only.
  let requires = $derived(graph?.edges.filter((e) => e.from === repo.id).map((e) => e.to) ?? []);
  let requiredBy = $derived(graph?.edges.filter((e) => e.to === repo.id).map((e) => e.from) ?? []);

  let downloading = $state(false);
  let dlStatus = $state(null); // null | 'running' | 'done' | 'error'
  let dlLog = $state('');
  let pollTimer = null;

  async function poll() {
    const s = await onPullStatus?.(repo.id);
    if (!s) return;
    dlLog = s.log ?? '';
    dlStatus = s.status;
    if (s.status !== 'running') {
      clearInterval(pollTimer);
      pollTimer = null;
      downloading = false;
      if (s.status === 'done') await onDownloadComplete?.();
    }
  }

  async function download() {
    if (downloading) return;
    downloading = true;
    dlStatus = 'running';
    dlLog = '';
    await onDownload?.(repo.id);
    await poll();
    if (dlStatus === 'running') pollTimer = setInterval(poll, 600);
  }

  let fetching = $state(false);
  let fetchResult = $state(null); // { ok, output, hint }

  async function fetchRepo() {
    if (fetching || !repo.local_path) return;
    fetching = true;
    fetchResult = null;
    fetchResult = await onFetch?.(repo.local_path);
    fetching = false;
  }

  let copied = $state(false);
  let copiedTimer = null;
  function copyPath() {
    navigator.clipboard?.writeText(absolutePath);
    copied = true;
    clearTimeout(copiedTimer);
    copiedTimer = setTimeout(() => (copied = false), 1200);
  }

  // A different repo keeps the drawer mounted, so its results go.
  $effect(() => {
    repo.id;
    fetchResult = null;
  });

  $effect(() => () => {
    if (pollTimer) clearInterval(pollTimer);
    clearTimeout(copiedTimer);
  });

  function ago(ms) {
    if (!ms) return null;
    const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
    if (s < 60) return 'just now';
    if (s < 3600) return `${Math.round(s / 60)} min ago`;
    if (s < 86400) return `${Math.round(s / 3600)} h ago`;
    return `${Math.round(s / 86400)} d ago`;
  }

  // Git's XY code, said in words: index first, then the working tree.
  function changeLabel(code) {
    if (code === '??') return 'untracked';
    if (code.includes('U') || code === 'AA' || code === 'DD') return 'conflict';
    const c = code[1] !== '.' ? code[1] : code[0];
    return { M: 'modified', A: 'added', D: 'deleted', R: 'renamed', C: 'copied', T: 'type changed' }[c] ?? code;
  }
</script>

<SideDrawer {onClose} width="min(640px, 70vw)">
  <div class="head">
    <span class="eyebrow">repo</span>
    <div class="close" onclick={onClose}>×</div>
  </div>
  <h3 class="stencil">{repo.label}</h3>

  <div class="controls">
    {#if repo.downloaded === false}
      <button class="ctrl-btn" onclick={download} disabled={downloading}>
        {#if downloading}<span class="spinner"></span> downloading…{:else}Download{/if}
      </button>
    {:else}
      <button class="ctrl-btn" onclick={fetchRepo} disabled={fetching} title="git fetch as you: updates what the remote has, never your working tree">
        {#if fetching}<span class="spinner"></span> fetching…{:else}Fetch{/if}
      </button>
      {#if absolutePath}
        <button class="ctrl-btn" onclick={copyPath} title={absolutePath}>{copied ? 'Copied' : 'Copy path'}</button>
      {/if}
    {/if}
  </div>

  {#if dlStatus === 'error' || (dlStatus === 'running' && dlLog)}
    <pre class="log">{dlLog}</pre>
  {/if}
  {#if fetchResult && !fetchResult.ok}
    <pre class="log">{fetchResult.output}{fetchResult.hint ? `\nhint: ${fetchResult.hint}` : ''}</pre>
  {/if}

  <div class="rows">
    {#if repo.repo}
      <div class="row"><span class="k">remote</span><span class="v">{repo.repo}</span></div>
    {/if}

    {#if repo.downloaded === false}
      <div class="row"><span class="k">status</span><span class="v muted">not downloaded yet</span></div>
    {:else}
      {#if absolutePath}
        <div class="row"><span class="k">local path</span><span class="v">{absolutePath}</span></div>
      {/if}
      <div class="row">
        <span class="k">branch</span>
        <span class="v">
          {g?.branch ?? repo.branch ?? 'detached or unreadable'}
          {#if g?.upstream}
            <span class="muted-inline">→ {g.upstream}</span>
            {#if g.ahead || g.behind}
              <span class="ab">{g.ahead ? `↑${g.ahead}` : ''} {g.behind ? `↓${g.behind}` : ''}</span>
            {:else}
              <span class="muted-inline">up to date</span>
            {/if}
          {:else if g}
            <span class="muted-inline">tracks no upstream</span>
          {/if}
        </span>
      </div>
      {#if g}
        <div class="row">
          <span class="k">last fetched</span>
          <span class="v">{g.fetched_at_ms ? ago(g.fetched_at_ms) : 'never'}{fetchResult?.ok ? ' · fetched' : ''}</span>
        </div>
      {/if}
      {#if g?.last_commit}
        {@const c = g.last_commit}
        <div class="row">
          <span class="k">last commit</span>
          <span class="v"><span class="mono">{c.sha.slice(0, 7)}</span> {c.subject}</span>
          <span class="v sub">{c.author} · {ago(c.time_ms)}</span>
        </div>
      {/if}

      <div class="row">
        <span class="k">working tree</span>
        {#if !g}
          <span class="v">{repo.dirty ? 'dirty' : 'clean'}</span>
        {:else if g.changes_total === 0}
          <span class="v">clean</span>
        {:else}
          <span class="v">{g.changes_total} changed file{g.changes_total === 1 ? '' : 's'}</span>
          <div class="changes">
            {#each g.changes as ch}
              <div class="change"><span class="code code-{changeLabel(ch.code)}">{changeLabel(ch.code)}</span><span class="path">{ch.path}</span></div>
            {/each}
            {#if g.changes_total > g.changes.length}
              <div class="muted">and {g.changes_total - g.changes.length} more</div>
            {/if}
          </div>
        {/if}
      </div>
    {/if}

    {#if repo.stale?.length}
      <div class="row">
        <span class="k">containers on older code</span>
        {#each repo.stale as s}
          <button class="link" onclick={() => onOpenNode?.(s.id)} title="open it on the Containers tab to reset it">
            {s.label}
            <span class="muted-inline">
              {#if s.now}built from {s.built?.slice(0, 7)}, checkout is on {s.now.slice(0, 7)}{:else}edited since it was built{/if}
            </span>
          </button>
        {/each}
      </div>
    {/if}

    {#if repo.members?.length}
      <div class="row">
        <span class="k">declares</span>
        <div class="chips">
          {#each repo.members as m}
            <button class="chip" onclick={() => onOpenNode?.(m.id)} title="open on the Containers tab">{m.label}<span class="chip-kind">{m.kind}</span></button>
          {/each}
        </div>
      </div>
    {/if}

    {#if repo.flows?.length}
      <div class="row">
        <span class="k">in flows</span>
        <div class="chips">
          {#each repo.flows as f}
            <span class="chip" class:current={f === currentFlow}>{f}</span>
          {/each}
        </div>
      </div>
    {/if}

    {#if requires.length}
      <div class="row"><span class="k">needs</span><span class="v">{requires.join(', ')}</span></div>
    {/if}
    {#if requiredBy.length}
      <div class="row"><span class="k">needed by</span><span class="v">{requiredBy.join(', ')}</span></div>
    {/if}
  </div>
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
  .rows { display: flex; flex-direction: column; gap: 12px; }
  .row { display: flex; flex-direction: column; gap: 3px; font: 500 12px var(--font-mono); }
  .row .k { color: var(--ink-faint); text-transform: uppercase; font-size: 10px; letter-spacing: 0.06em; }
  .row .v { color: var(--ink); word-break: break-all; }
  .row .v.sub { color: var(--ink-dim); font-size: 11px; }
  .muted { color: var(--ink-faint); font-style: italic; }
  .muted-inline { color: var(--ink-faint); margin-left: 6px; }
  .mono { font-family: var(--font-mono); font-size: 11px; color: var(--ink-dim); }
  .ab { color: var(--warning); font-weight: 700; margin-left: 6px; }
  .changes { display: flex; flex-direction: column; gap: 2px; max-height: 260px; overflow-y: auto; margin-top: 4px; }
  .change { display: flex; gap: 8px; font-size: 11px; }
  .change .code { flex: 0 0 76px; color: var(--ink-dim); }
  .change .code-untracked { color: var(--ink-faint); }
  .change .code-added { color: var(--success); }
  .change .code-deleted, .change .code-conflict { color: var(--danger); }
  .change .code-modified, .change .code-renamed { color: var(--warning); }
  .change .path { color: var(--ink); word-break: break-all; }
  .chips { display: flex; flex-wrap: wrap; gap: 6px; }
  .chip {
    display: inline-flex; align-items: center; gap: 6px; padding: 2px 8px; border-radius: 999px;
    background: var(--panel-2); border: 1px solid var(--line-strong); color: var(--ink);
    font: 500 11px var(--font-mono);
  }
  button.chip { cursor: pointer; }
  button.chip:hover { border-color: var(--accent); }
  .chip.current { background: var(--accent-bg); border-color: var(--accent); color: var(--accent); }
  .chip-kind { color: var(--ink-faint); font-size: 9px; text-transform: uppercase; }
  .link {
    background: none; border: none; padding: 0; text-align: left; cursor: pointer;
    font: 500 12px var(--font-mono); color: var(--warning);
  }
  .link:hover { text-decoration: underline; }
  .log {
    background: #000; color: #b8ffb8; padding: 12px; border-radius: 6px; font: 400 11px var(--font-mono);
    max-height: 300px; overflow: auto; white-space: pre-wrap; word-break: break-all; margin-bottom: 16px;
  }
  .spinner {
    display: inline-block; width: 9px; height: 9px; border-radius: 50%;
    border: 2px solid var(--line-strong); border-top-color: var(--ink);
    animation: spin 0.7s linear infinite; vertical-align: middle; margin-right: 2px;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
</style>
