<script>
  let { graph, currentFlow, mode, runContainers, onSelectNode } = $props();

  const NODE_W = 260, NODE_H = 132, LEVEL_GAP = 340, ROW_GAP = 168;

  function shortRepo(url) {
    if (!url) return '';
    const m = url.match(/[:/]([^/:]+\/[^/]+?)(\.git)?$/);
    return m ? m[1] : url;
  }

  function layout(g) {
    const edges = g.edges.filter((e) => e.kind !== 'shared-infra').map((e) => [e.from, e.to]);

    // Dependencies can legitimately form a cycle across flows (e.g. a hard
    // dependency one way, a flow-scoped one the other way) — drop back-edges
    // via DFS so the longest-path depth pass below always terminates instead
    // of growing the layout without bound.
    const adj = {};
    edges.forEach(([a, b]) => (adj[a] = adj[a] || []).push(b));
    const dagEdges = [];
    const visitState = {}; // undefined = unvisited, 1 = in-progress, 2 = done
    function dfs(u) {
      visitState[u] = 1;
      for (const v of adj[u] || []) {
        if (visitState[v] === 1) continue; // back-edge: would reopen a cycle, drop it
        dagEdges.push([u, v]);
        if (!visitState[v]) dfs(v);
      }
      visitState[u] = 2;
    }
    g.nodes.forEach((n) => { if (!visitState[n.id]) dfs(n.id); });

    const depth = {};
    g.nodes.forEach((n) => (depth[n.id] = 0));
    let changed = true, guard = 0;
    while (changed && guard < g.nodes.length + 1) {
      changed = false; guard++;
      dagEdges.forEach(([a, b]) => {
        const d = (depth[a] || 0) + 1;
        if (d > (depth[b] || 0)) { depth[b] = d; changed = true; }
      });
    }

    // Layout depends only on the graph's structure (nodes/edges), never on
    // which flow is selected — sort by id so row order within a depth level
    // is stable regardless of the input array's order.
    const byDepth = {};
    const sortedNodes = [...g.nodes].sort((a, b) => a.id.localeCompare(b.id));
    sortedNodes.forEach((n) => (byDepth[depth[n.id]] = byDepth[depth[n.id]] || []).push(n));
    const maxDepth = Math.max(0, ...Object.values(depth));
    const maxPerLevel = Math.max(1, ...Object.values(byDepth).map((a) => a.length));
    const width = (maxDepth + 1) * LEVEL_GAP + NODE_W - 10;
    const height = Math.max(300, maxPerLevel * ROW_GAP + 40);

    const pos = {};
    Object.keys(byDepth).forEach((d) => {
      const arr = byDepth[d];
      const totalH = arr.length * ROW_GAP;
      const offY = (height - totalH) / 2;
      arr.forEach((n, i) => (pos[n.id] = { x: d * LEVEL_GAP + 20, y: offY + i * ROW_GAP + 10 }));
    });

    const sorted = [...g.nodes].sort((a, b) => a.id.localeCompare(b.id));
    const codeOf = new Map();
    let oi = 0;
    sorted.forEach((n) => {
      const code = (n.label.replace(/[^a-z0-9]/gi, '').slice(0, 3).toUpperCase() || 'NOD') + '-' + String(++oi).padStart(2, '0');
      codeOf.set(n.id, code);
    });

    return { width, height, pos, codeOf };
  }

  let l = $derived(layout(graph));

  function edgeLine(e) {
    const a = l.pos[e.from], b = l.pos[e.to];
    if (!a || !b) return null;
    return { x1: a.x + NODE_W, y1: a.y + NODE_H / 2, x2: b.x, y2: b.y + NODE_H / 2 };
  }
</script>

<div class="graph-area" style="width:{l.width}px;height:{l.height}px">
  <svg width={l.width} height={l.height} style="position:absolute;top:0;left:0;overflow:visible">
    {#each graph.edges as e}
      {@const line = edgeLine(e)}
      {#if line}
        {@const inFlow = e.flows.includes(currentFlow)}
        <line
          x1={line.x1} y1={line.y1} x2={line.x2} y2={line.y2}
          stroke={inFlow ? 'var(--accent)' : 'var(--accent-dim)'}
          stroke-width={inFlow ? 2 : 1.5}
          stroke-dasharray="5,4"
        />
      {/if}
    {/each}
  </svg>

  {#each graph.nodes as n}
    {@const p = l.pos[n.id]}
    {@const inFlow = n.flows.includes(currentFlow)}
    {@const dimmed = currentFlow && !inFlow}
    {@const live = runContainers?.[n.id]}
    {@const dotColor = live ? (live.status === 'running' ? 'var(--success)' : 'var(--danger)') : 'var(--success)'}
    {@const containerState = mode === 'containers' && n.kind !== 'flow' ? (live ? (live.status === 'running' ? 'running' : 'stopped') : 'none') : null}
    <div
      class="node"
      class:not-downloaded={n.downloaded === false}
      class:in-flow={inFlow}
      class:dimmed={dimmed}
      style="left:{p.x}px;top:{p.y}px"
      onclick={() => onSelectNode(n)}
    >
      <div class="node-head">
        <div class="node-id-wrap">
          {#if n.downloaded === false}
            <span class="badge">not downloaded</span>
          {:else}
            <span class="dot" style="background:{dotColor}"></span>
          {/if}
          <span class="node-id">{n.label}</span>
        </div>
        <span class="crate-tag">{l.codeOf.get(n.id)}</span>
      </div>
      <div class="kind-row">
        <span class="kind-pill" class:infra={n.kind === 'infra'}>{n.kind}</span>
        {#if n.ports?.length}<span class="ports">{n.ports.join(', ')}</span>{/if}
      </div>
      <div class="node-domain">{mode === 'containers' ? (n.domain || n.image || '') : shortRepo(n.repo)}</div>
      {#if n.branch}
        <div class="node-meta branch-row">
          <span>{n.branch}</span>
          {#if n.downloaded !== false}
            <span class="pill" class:dirty={n.dirty} class:clean={!n.dirty}>{n.dirty ? 'DIRTY' : 'CLEAN'}</span>
          {/if}
        </div>
      {/if}
      {#if live}
        <div class="node-meta">{live.status}{#if live.published_port} · 127.0.0.1:{live.published_port}{/if}</div>
      {/if}
      {#if containerState}
        <div class="status-bar state-{containerState}" title="container: {containerState}">
          {containerState === 'none' ? 'absent' : containerState}
        </div>
      {/if}
    </div>
  {/each}
</div>

<style>
  .graph-area { position: relative; }
  .node {
    position: absolute; width: 260px; min-height: 132px; padding: 14px; border-radius: 6px;
    background: var(--panel-2); border: 1px solid var(--line-strong); cursor: pointer;
    overflow: hidden; transition: opacity 0.15s ease;
  }
  .node.in-flow { border-color: var(--accent); box-shadow: 0 0 0 1px var(--accent); }
  .node.not-downloaded { border-style: dashed; opacity: 0.7; background: transparent; }
  .node.dimmed { opacity: 0.35; }
  .status-bar {
    position: absolute; left: 0; right: 0; bottom: 0; height: 26px;
    display: flex; align-items: center; justify-content: center;
    font: 700 12px var(--font-mono); text-transform: uppercase; letter-spacing: 0.06em;
  }
  .status-bar.state-running { background: var(--success); color: #ffffff; }
  .status-bar.state-stopped { background: var(--warning); color: #ffffff; }
  .status-bar.state-none { background: var(--line-strong); color: var(--ink-faint); }
  .node-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 6px; gap: 8px; }
  .node-id-wrap { display: flex; align-items: center; gap: 6px; min-width: 0; }
  .node-id { font: 600 13.5px var(--font-mono); color: var(--ink); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .kind-row { display: flex; align-items: center; justify-content: space-between; gap: 8px; margin-bottom: 6px; }
  .kind-pill {
    font: 700 8.5px var(--font-mono); text-transform: uppercase; letter-spacing: 0.05em;
    color: var(--accent); border: 1px solid var(--accent-dim); border-radius: 3px; padding: 1px 5px; flex: 0 0 auto;
  }
  .kind-pill.infra { color: var(--ink-faint); border-color: var(--line-strong); }
  .ports { font: 500 10px var(--font-mono); color: var(--ink-faint); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .node-domain { font: 500 11px var(--font-mono); color: var(--ink-faint); word-break: break-all; }
  .node-meta { font: 500 10px var(--font-mono); color: var(--ink-faint); margin-top: 6px; }
  .badge {
    font: 700 8.5px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em; color: var(--ink-faint);
    border: 1px dashed var(--line-strong); border-radius: 3px; padding: 2px 5px; flex: 0 0 auto;
  }
  .branch-row { display: flex; align-items: center; justify-content: space-between; gap: 6px; }
  .pill {
    font: 700 8px var(--font-mono); text-transform: uppercase; letter-spacing: 0.05em;
    border-radius: 999px; padding: 2px 7px; flex: 0 0 auto;
  }
  .pill.dirty { background: var(--warning-bg); color: var(--warning); }
  .pill.clean { background: var(--success-bg); color: var(--success); }
</style>
