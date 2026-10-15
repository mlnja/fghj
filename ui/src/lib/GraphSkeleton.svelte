<script>
  // Stands in for the graph until there is one to draw: while it resolves,
  // while fghjd can't be reached, and while the workspace fails to resolve.
  // The card shapes match GraphView's nodes so the page doesn't jump when
  // the real graph replaces it.
  let { tone = 'loading', title, detail = null, onRetry = null } = $props();

  // Three columns, like a small dependency graph laid out left to right.
  const CARDS = [
    { x: 40, y: 20 }, { x: 40, y: 206 }, { x: 40, y: 392 },
    { x: 420, y: 113 }, { x: 420, y: 299 },
    { x: 800, y: 206 },
  ];
</script>

<div class="skeleton" class:still={tone === 'error'}>
  {#each CARDS as c}
    <div class="card" style="left:{c.x}px;top:{c.y}px">
      <div class="bar w60"></div>
      <div class="bar w80"></div>
      <div class="bar w40"></div>
      <div class="foot"></div>
    </div>
  {/each}

  <div class="message {tone}" role={tone === 'loading' ? 'status' : 'alert'}>
    <div class="title">
      {#if tone !== 'error'}<span class="spinner"></span>{/if}
      {title}
    </div>
    {#if detail}<pre class="detail">{detail}</pre>{/if}
    {#if onRetry}<button class="retry" onclick={onRetry}>Retry now</button>{/if}
  </div>
</div>

<style>
  .skeleton { position: relative; height: 620px; margin: 0 0 40px; overflow: hidden; }
  .card {
    position: absolute; width: 320px; height: 146px; padding: 14px; border-radius: 6px;
    background: var(--panel-2); border: 1px solid var(--line); display: flex; flex-direction: column; gap: 10px;
  }
  .bar, .foot {
    border-radius: 4px; height: 10px;
    background: linear-gradient(90deg, var(--line) 25%, var(--panel) 50%, var(--line) 75%);
    background-size: 200% 100%; animation: shimmer 1.4s linear infinite;
  }
  .still .bar, .still .foot { animation: none; background: var(--line); }
  .w60 { width: 60%; height: 13px; }
  .w80 { width: 80%; }
  .w40 { width: 40%; }
  .foot { margin-top: auto; height: 22px; width: 100%; }
  @keyframes shimmer { from { background-position: 200% 0; } to { background-position: -200% 0; } }

  .message {
    position: absolute; left: 50%; top: 200px; transform: translateX(-50%);
    max-width: min(640px, calc(100% - 80px)); display: flex; flex-direction: column; gap: 10px; align-items: flex-start;
    padding: 16px 20px; border-radius: 8px; background: var(--panel); border: 1px solid var(--line-strong);
    box-shadow: 0 8px 24px rgba(19, 27, 46, 0.12); font: 500 12px var(--font-mono); color: var(--ink);
  }
  .message.down { border-color: var(--warning); }
  .message.error { border-color: var(--danger); }
  .title { display: flex; align-items: center; gap: 10px; font-weight: 700; }
  .message.down .title { color: var(--warning); }
  .message.error .title { color: var(--danger); }
  .detail {
    margin: 0; white-space: pre-wrap; word-break: break-word; color: var(--ink-dim);
    font: 400 11.5px/1.5 var(--font-mono); max-height: 240px; overflow: auto;
  }
  .spinner {
    width: 10px; height: 10px; border-radius: 50%; flex: 0 0 auto;
    border: 2px solid var(--line-strong); border-top-color: currentColor; animation: spin 0.7s linear infinite;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  .retry {
    padding: 5px 10px; border-radius: 4px; background: var(--panel-2); border: 1px solid var(--line-strong);
    color: var(--ink); font: 700 10px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em; cursor: pointer;
  }
</style>
