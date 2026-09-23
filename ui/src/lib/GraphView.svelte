<script>
  let { graph, currentFlow, mode, runContainers, onSelectNode } = $props();

  const NODE_W = 260, NODE_H = 132, LEVEL_GAP = 340, ROW_GAP = 168;

  function shortRepo(url) {
    if (!url) return '';
    const m = url.match(/[:/]([^/:]+\/[^/]+?)(\.git)?$/);
    return m ? m[1] : url;
  }

  // Kind is a git-repo'd, independently deployable service vs. a "backing"
  // dependency (a database, cache, ...) declared inline in some service's
  // config with no repo of its own — shown as an icon before the name
  // instead of a text pill so it reads at a glance without eating a row.
  const SERVICE_ICON = `<svg viewBox="0 0 16 16" width="11" height="11" fill="none" stroke="currentColor" stroke-width="1.3"><path d="M8 1.6 13.8 5v6L8 14.4 2.2 11V5Z"/><path d="M2.2 5 8 8.2 13.8 5M8 8.2v6.2"/></svg>`;
  const BACKING_ICON = `<svg viewBox="0 0 16 16" width="11" height="11" fill="none" stroke="currentColor" stroke-width="1.3"><ellipse cx="8" cy="3.4" rx="5.4" ry="1.9"/><path d="M2.6 3.4v9.2c0 1.05 2.42 1.9 5.4 1.9s5.4-.85 5.4-1.9V3.4"/><path d="M2.6 8c0 1.05 2.42 1.9 5.4 1.9s5.4-.85 5.4-1.9"/></svg>`;

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
    {@const containerState = mode === 'containers' && n.kind !== 'flow' ? (live ? (live.pending_action ?? (live.observed.status === 'running' ? 'running' : 'stopped')) : 'none') : null}
    {@const synced = live && live.observed.sync !== 'unknown' ? live.observed.sync === 'synced' : null}
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
          {:else if n.kind === 'backing'}
            <span class="kind-icon backing" title="backing dependency">{@html BACKING_ICON}</span>
          {:else if n.kind === 'service'}
            <span class="kind-icon service" title="service">{@html SERVICE_ICON}</span>
          {/if}
          <span class="node-id">{n.label}</span>
        </div>
        <span class="crate-tag">{l.codeOf.get(n.id)}</span>
      </div>

      <!-- Docker half: only ever populated in containers mode, since
           there's nothing runtime-related to show for a plain repo view. -->
      {#if synced !== null}
        <div class="node-meta live-row">
          <span class="pill" class:drifted={!synced} class:synced={synced} title="{synced ? 'running container matches .fghj.yaml' : 'running container config no longer matches .fghj.yaml — reset to pick up the change'}">
            {synced ? 'SYNCED' : 'DRIFTED'}
          </span>
        </div>
      {/if}

      <!-- The literal boundary between the two halves: container status is
           the one fact that's neither a git nor a repo fact, so it gets the
           dividing line instead of living inside either half. -->
      {#if containerState}
        <div class="status-bar state-{containerState}" title="container: {containerState}">
          {containerState === 'none' ? 'absent' : containerState}
        </div>
      {/if}

      <!-- Git half: always shown, mode-independent. -->
      {#if mode === 'repos'}
        <div class="node-domain">{shortRepo(n.repo)}</div>
      {:else if n.repo}
        <div class="node-meta">{shortRepo(n.repo)}</div>
      {/if}
      {#if n.services?.length > 1}
        <div class="node-meta">{n.services.join(', ')}</div>
      {/if}
      {#if n.branch}
        <div class="node-meta branch-row">
          <span>{n.branch}</span>
          {#if n.downloaded !== false}
            <span class="pill" class:dirty={n.dirty} class:clean={!n.dirty} title="git working tree">{n.dirty ? 'DIRTY' : 'CLEAN'}</span>
          {/if}
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
  /* Normal document flow, not absolutely pinned to the card's bottom edge —
     an absolutely-positioned bar sat at a fixed height regardless of how
     much text was above it, overlapping whatever content was there. It's
     also the literal dividing line between the docker half (above: live
     port/drift) and the git half (below: repo/branch/dirty) of the card,
     bled out to the card's left/right edges past its own padding. */
  .status-bar {
    margin: 10px -14px; height: 26px;
    display: flex; align-items: center; justify-content: center;
    font: 700 12px var(--font-mono); text-transform: uppercase; letter-spacing: 0.06em;
  }
  .status-bar.state-running { background: var(--success); color: #ffffff; }
  .status-bar.state-stopped { background: var(--warning); color: #ffffff; }
  .status-bar.state-none { background: var(--line-strong); color: var(--ink-faint); }
  .status-bar.state-starting, .status-bar.state-stopping, .status-bar.state-removing {
    background: var(--accent); color: #ffffff; animation: status-pulse 1s ease-in-out infinite;
  }
  @keyframes status-pulse { 50% { opacity: 0.55; } }
  .node-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 6px; gap: 8px; }
  .node-id-wrap { display: flex; align-items: center; gap: 6px; min-width: 0; }
  .node-id { font: 600 13.5px var(--font-mono); color: var(--ink); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  /* service (git-backed, independently deployable) vs. backing (an inline
     dependency declared by some service, e.g. a database — no repo of its
     own) — see resolver::Node::kind's doc for the exact two values. */
  .kind-icon { display: inline-flex; flex: 0 0 auto; }
  .kind-icon.service { color: var(--accent); }
  .kind-icon.backing { color: var(--ink-faint); }
  .node-domain { font: 500 11px var(--font-mono); color: var(--ink-faint); word-break: break-all; }
  .node-meta { font: 500 10px var(--font-mono); color: var(--ink-faint); margin-top: 6px; }
  .live-row { display: flex; align-items: center; justify-content: flex-end; gap: 6px; }
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
  .pill.drifted { background: var(--warning-bg); color: var(--warning); }
  .pill.synced { background: var(--success-bg); color: var(--success); }
</style>
