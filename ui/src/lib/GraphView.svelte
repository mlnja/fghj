<script>
  let { graph, currentFlow, mode, runContainers, onSelectNode } = $props();

  // Widened from 260 so two state lanes fit side by side without truncating
  // a branch name on every card. The gaps keep their old slack (80px across,
  // 36px down), so the graph reads at the same density per card, just wider.
  const NODE_W = 320, NODE_H = 146, LEVEL_GAP = 400, ROW_GAP = 182;

  function shortRepo(url) {
    if (!url) return '';
    const m = url.match(/[:/]([^/:]+\/[^/]+?)(\.git)?$/);
    return m ? m[1] : url;
  }

  // Kind is a git-repo'd, independently deployable service vs. a "backing"
  // dependency (a database, cache, ...) declared inline in some service's
  // config with no repo of its own vs. a "task" (a seed, a migration —
  // inline like a backing dependency, but it exits) — shown as an icon
  // before the name instead of a text pill so it reads at a glance without
  // eating a row.
  const SERVICE_ICON = `<svg viewBox="0 0 16 16" width="11" height="11" fill="none" stroke="currentColor" stroke-width="1.3"><path d="M8 1.6 13.8 5v6L8 14.4 2.2 11V5Z"/><path d="M2.2 5 8 8.2 13.8 5M8 8.2v6.2"/></svg>`;
  const BACKING_ICON = `<svg viewBox="0 0 16 16" width="11" height="11" fill="none" stroke="currentColor" stroke-width="1.3"><ellipse cx="8" cy="3.4" rx="5.4" ry="1.9"/><path d="M2.6 3.4v9.2c0 1.05 2.42 1.9 5.4 1.9s5.4-.85 5.4-1.9V3.4"/><path d="M2.6 8c0 1.05 2.42 1.9 5.4 1.9s5.4-.85 5.4-1.9"/></svg>`;
  // A terminating node (a seed, a migration, any one-shot job): declared
  // inline by a service like a backing dependency, but it runs to completion
  // instead of staying up. A checkmark rather than a box — what matters
  // about it is that it finished.
  const TASK_ICON = `<svg viewBox="0 0 16 16" width="11" height="11" fill="none" stroke="currentColor" stroke-width="1.3"><circle cx="8" cy="8" r="6.2"/><path d="m5.2 8.2 2 2 3.6-4"/></svg>`;

  // What the status bar says about a node's container. Two layers, and the
  // order matters: a live `pending_action` always wins, because Docker
  // hasn't settled yet and the settled reading would be stale. Otherwise
  // it's `condition` — a derived field on `ContainerInfo` (Rust
  // `state::NodeCondition`), *not* re-derived here, so the rule for what a
  // desired/observed pair means lives in exactly one place. That rule is
  // more than "is it running": a terminating node is read on its exit code,
  // and a container fghj was asked to keep up but which Docker says is down
  // is `crashed`, which is a different situation from one the user stopped.
  function containerStateOf(live) {
    if (!live) return 'none';
    return live.pending_action ?? live.condition;
  }

  const STATE_TITLE = {
    completed: 'task finished successfully — it is meant to exit, not stay up',
    failed: 'task exited non-zero; everything that depends on it was not started',
    finishing: 'task has exited but has not been re-inspected yet — no exit code read, so no verdict',
    stopped: 'stopped, and fghj was not asked to keep it up — expected',
    crashed: 'fghj was asked to keep this up and Docker says it is down. Nothing will restart it on its own — press Start',
    restarting: "Docker is bouncing it under this node's own restart policy — not reachable while it does",
    paused: 'paused from outside fghj (docker pause) — not reachable, and Start will not help; docker unpause will',
  };

  // Keyed by `ContainerObserved::sync` (Rust `SyncStatus`, snake_case).
  // `unknown` is absent on purpose: it never renders a pill, so it never
  // needs a tooltip.
  const SYNC_TITLE = {
    synced: 'running container matches .fghj.yaml',
    drifted: 'running container config no longer matches .fghj.yaml — reset to pick up the change',
    orphaned: 'this node is no longer declared in the workspace (usually a branch switch) — the container is still running and nothing will reconcile it',
  };

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

    return { width, height, pos };
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
    <!-- Still gated on `mode`, and it has to be: a repos-mode card is a
         *group* of services keyed by checkout path (`App.svelte`'s
         `reposGraph`), not a node, so there is no single container behind it
         to report on — `runContainers` is keyed by node id and is not even
         passed for that view. Within containers mode the footer is
         unconditional, so a node with nothing running reads as ABSENT rather
         than as a differently-shaped card. A flow node never has a container
         of its own. -->
    {@const showDocker = mode === 'containers' && n.kind !== 'flow'}
    {@const containerState = showDocker ? containerStateOf(live) : null}
    {@const sha = n.head ? n.head.slice(0, 7) : null}
    <!-- The checkout the running container was *built* from, recorded at
         start time (`ContainerDesired::source`) and never refreshed. `null`
         for a node fghj doesn't build, and also for a container started by an
         fghjd from before the field existed — both render nothing, because
         neither is a finding. -->
    {@const built = live?.desired?.source ?? null}
    {@const builtSha = built?.head ? built.head.slice(0, 7) : null}
    <!-- Deliberately only shown when it *differs* from the checkout. On a
         synced node the built commit is the current commit, so printing it
         would restate the sha in the git lane one column to the left. The
         interesting case is the asymmetry, which is exactly what the user of
         this card is scanning for: the code moved and the container didn't. -->
    {@const movedOff = builtSha && sha && builtSha !== sha}
    {@const editedSince = !movedOff && built !== null && n.dirty && !built.dirty}
    <!-- `unknown` is the one verdict with nothing to say (no drift check has
         run yet, or the last one failed to re-resolve), so it renders no pill
         at all. `orphaned` does have something to say — the node is gone from
         the graph — so it gets its own. -->
    {@const sync = live && live.observed.sync !== 'unknown' ? live.observed.sync : null}
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
          {:else if n.kind === 'task'}
            <span class="kind-icon task" title="task (runs once and exits)">{@html TASK_ICON}</span>
          {:else if n.kind === 'service'}
            <span class="kind-icon service" title="service">{@html SERVICE_ICON}</span>
          {/if}
          <span class="node-id">{n.label}</span>
        </div>
        <!-- The corner used to hold a generated `AIK-01` code: the label's
             first three letters plus an index into id-sorted order. It
             restated the name beside it, renumbered whenever a node sorted
             ahead of it appeared, and existed in no API response, CLI output
             or drawer, so there was nothing to cross-reference it against.
             `domain_scope` is the fact worth that corner instead — a stable
             node drops the run id from its domain, so exactly one run can
             own that name at a time. It's opt-in and rare, which is what
             makes it worth marking. -->
        {#if n.domain_scope === 'stable'}
          <span class="scope-tag" title="domain_scope: stable — this node's domain has no run id in it, so only one run can hold this name at a time">STABLE</span>
        {/if}
      </div>

      <!-- Identity, full width. Which repo a node came from is neither a git
           *state* nor a docker one, and a whole line keeps `owner/name`
           readable instead of truncating it into a half-width lane. -->
      {#if n.repo}
        <div class="node-repo" title={n.repo}>{shortRepo(n.repo)}</div>
      {/if}
      {#if n.services?.length > 1}
        <div class="node-repo" title={n.services.join(', ')}>{n.services.join(', ')}</div>
      {/if}

      <!-- State, in two lanes: left is what git says about the checkout,
           right is what Docker says about the container. Both lanes always
           render, placeholder and all, so the same fact sits in the same
           place on every card and a missing one reads as absence rather
           than as a different layout.

           Both lanes are populated for every node kind that has a checkout
           behind it, not just services: a backing dependency or a task
           inherits its owning service's repo/branch/dirty/head
           (`resolver/visit_dependency.rs`), so a database card still says
           which branch produced it. -->
      <div class="halves" class:solo={!showDocker}>
        <div class="half git">
          {#if n.branch}
            <div class="lane-line" title={n.branch}>{n.branch}</div>
            <div class="lane-line pills">
              {#if n.downloaded !== false}
                <span class="pill" class:dirty={n.dirty} class:clean={!n.dirty} title="git working tree">{n.dirty ? 'DIRTY' : 'CLEAN'}</span>
              {/if}
              {#if sha}
                <span class="sha" title={n.head}>{sha}</span>
              {/if}
            </div>
          {:else if n.downloaded === false}
            <div class="lane-line empty" title="not pulled yet, so there is no checkout to read a branch from">not pulled</div>
          {:else}
            <!-- A downloaded node always sits in *some* working tree — a
                 service in its own, a backing dependency or task in its
                 owner's — so a missing branch here is a failed read, not an
                 absence. Saying so beats a dash that reads as "nothing to
                 report": the usual cause is `git` refusing a checkout it
                 considers foreign-owned, which also forces `dirty` to its
                 can't-vouch-for-it default of true. -->
            <div class="lane-line unreadable" title="fghjd could not read git state for this checkout. Its branch, commit and clean/dirty status are all unknown; the DIRTY mark below is a fallback, not a finding.">branch unreadable</div>
          {/if}
        </div>
        {#if showDocker}
        <div class="half docker">
          {#if sync !== null}
            <div class="lane-line pills">
              <span class="pill" class:drifted={sync === 'drifted'} class:synced={sync === 'synced'} class:orphaned={sync === 'orphaned'} title={SYNC_TITLE[sync]}>
                {sync.toUpperCase()}
              </span>
            </div>
          {/if}
          <!-- What the container was built from, when that is no longer what
               the checkout says. `DRIFTED` above is the verdict; this is the
               evidence for it, and it is the difference between "reset this"
               and knowing why. -->
          {#if movedOff}
            <div class="lane-line built" title="This container was built from {built.head}. The checkout is now on {n.head}, so the running code is behind by at least one commit. Reset the node to rebuild it.">
              <span class="sha stale">{builtSha}</span>&nbsp;&rarr;&nbsp;<span class="sha">{sha}</span>
            </div>
          {:else if editedSince}
            <div class="lane-line built" title="The checkout was clean when this container was built ({built.head ?? 'commit unknown'}) and has uncommitted changes now, so the container is serving code the working tree no longer contains. Reset the node to rebuild it.">
              edited since build
            </div>
          {/if}
          <!-- The image, not the published host port. A `5432->54321`
               mapping is the thing the raw zone exists to abolish (every raw
               node answers on its *declared* port at its own address), it is
               something you copy rather than scan, and the drawer already
               offers it with `raw_domain` preferred and a copy button. What
               the stripe and the sync pill between them never say is what
               this container actually is — which for a backing dependency
               labelled `db` is the whole question. -->
          {#if n.image}
            <div class="lane-line" title={n.image}>{n.image}</div>
          {:else if sync === null}
            <div class="lane-line empty" title="no container for this node">&mdash;</div>
          {/if}
        </div>
        {/if}
      </div>

      <!-- The verdict, and the last thing read. It used to be the divider
           between the two halves; the lanes now divide themselves, so this
           is the footer, pushed to the bottom edge and bled past the card's
           own padding on three sides. -->
      {#if containerState}
        <div class="status-bar state-{containerState}" title={STATE_TITLE[containerState] ?? `container: ${containerState}`}>
          {containerState === 'none' ? 'absent' : containerState}
        </div>
      {/if}
    </div>
  {/each}
</div>

<style>
  .graph-area { position: relative; }
  .node {
    position: absolute; width: 320px; min-height: 146px; padding: 14px; border-radius: 6px;
    background: var(--panel-2); border: 1px solid var(--line-strong); cursor: pointer;
    overflow: hidden; transition: opacity 0.15s ease;
    /* A flex column purely so the status bar can take `margin-top: auto`
       and sit on the bottom edge however much content is above it. */
    display: flex; flex-direction: column;
  }
  .node.in-flow { border-color: var(--accent); box-shadow: 0 0 0 1px var(--accent); }
  .node.not-downloaded { border-style: dashed; opacity: 0.7; background: transparent; }
  .node.dimmed { opacity: 0.35; }
  /* The card's footer. Still in normal flow rather than absolutely pinned —
     an absolutely-positioned bar sat at a fixed height regardless of how
     much text was above it and overlapped whatever was there. `margin-top:
     auto` is what puts it on the bottom edge instead, which works at any
     content height; the negative margins bleed it past the card's own
     padding on the two sides and the bottom. */
  .status-bar {
    margin: auto -14px -14px; height: 26px; flex: 0 0 auto;
    display: flex; align-items: center; justify-content: center;
    font: 700 12px var(--font-mono); text-transform: uppercase; letter-spacing: 0.06em;
  }
  .status-bar.state-running { background: var(--success); color: #ffffff; }
  .status-bar.state-stopped { background: var(--warning); color: #ffffff; }
  /* Deliberately louder than `stopped`, and the same weight as a failed
     task: both mean "this will not fix itself and you have to do
     something". A stopped container is a state the user chose; this one
     nobody chose. */
  .status-bar.state-crashed { background: var(--danger); color: #ffffff; }
  /* Pulsing like an in-flight action, because that is what it is — Docker's
     own restart loop rather than one of fghj's, but equally "wait". */
  .status-bar.state-restarting {
    background: var(--warning); color: #ffffff; animation: status-pulse 1s ease-in-out infinite;
  }
  .status-bar.state-paused { background: var(--line-strong); color: var(--ink); }
  .status-bar.state-finishing { background: var(--success-bg); color: var(--ink-faint); }
  /* A finished task is a success, but a quieter one than a running service:
     there is nothing there to reach, so it shouldn't read as "live". */
  .status-bar.state-completed { background: var(--success-bg); color: var(--success); }
  .status-bar.state-failed { background: var(--danger); color: #ffffff; }
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
     own) vs. task (likewise inline, but runs to completion instead of
     staying up) — see resolver::Node::kind's doc for the exact values. */
  .kind-icon { display: inline-flex; flex: 0 0 auto; }
  .kind-icon.service { color: var(--accent); }
  .kind-icon.backing { color: var(--ink-faint); }
  .kind-icon.task { color: var(--ink-faint); }
  /* Truncates rather than wrapping (the old `.node-domain` used
     `word-break: break-all`): a card's height is part of the graph layout,
     so a long `owner/name` must not be able to push the lanes down. */
  .node-repo {
    font: 500 10.5px var(--font-mono); color: var(--ink-faint); margin-top: 4px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }

  /* Two fixed-width lanes rather than `auto` columns: equal halves mean a
     given fact lands at the same x on every card in the graph, which is
     what makes a column of cards scannable down its left or right edge. */
  .halves { display: grid; grid-template-columns: 1fr 1fr; margin-top: 10px; }
  /* Repos mode has no docker side to show, so the git lane takes the whole
     card rather than sitting next to a permanently empty column. Every card
     in a given view still matches every other one; it's the two views that
     differ, which is honest — they are drawing different things. */
  .halves.solo { grid-template-columns: 1fr; }
  .halves.solo .half.git { padding-right: 0; }
  .half { min-width: 0; display: flex; flex-direction: column; gap: 5px; }
  .half.git { padding-right: 10px; }
  .half.docker { padding-left: 10px; border-left: 1px solid var(--line); }
  .lane-line {
    font: 500 10px var(--font-mono); color: var(--ink-faint);
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .lane-line.pills { display: flex; align-items: center; gap: 6px; overflow: visible; }
  /* An empty lane says "nothing to report here", which is a different thing
     from "this card is laid out differently" — hence a mark rather than a
     collapsed column. */
  .lane-line.empty { color: var(--line-strong); }
  .lane-line.unreadable { color: var(--warning); }
  .sha { font: 500 9.5px var(--font-mono); color: var(--ink-faint); opacity: 0.7; }
  /* The built-from line. Warning-coloured because it only ever renders when
     the container is behind the checkout, which is a thing to act on — and
     full opacity on the arrow so the pair reads as one statement rather than
     as two faded shas. */
  .lane-line.built {
    display: flex; align-items: center; color: var(--warning);
    font-size: 9.5px; opacity: 1;
  }
  /* The commit that is no longer current, struck through: the strike is what
     makes the pair legible at a glance without reading the arrow. */
  .lane-line.built .sha { color: var(--warning); opacity: 1; }
  .lane-line.built .sha.stale { text-decoration: line-through; opacity: 0.75; }
  /* Accent rather than the old code tag's grey: this one appears on few
     cards and means something when it does, so it should read as a mark
     rather than as furniture every card happens to carry. */
  .scope-tag {
    font: 700 8px var(--font-mono); text-transform: uppercase; letter-spacing: 0.05em;
    color: var(--accent); border: 1px solid var(--accent-bg); background: var(--accent-bg);
    border-radius: 3px; padding: 2px 5px; flex: 0 0 auto; white-space: nowrap;
  }
  .badge {
    font: 700 8.5px var(--font-mono); text-transform: uppercase; letter-spacing: 0.04em; color: var(--ink-faint);
    border: 1px dashed var(--line-strong); border-radius: 3px; padding: 2px 5px; flex: 0 0 auto;
  }
  .pill {
    font: 700 8px var(--font-mono); text-transform: uppercase; letter-spacing: 0.05em;
    border-radius: 999px; padding: 2px 7px; flex: 0 0 auto;
  }
  .pill.dirty { background: var(--warning-bg); color: var(--warning); }
  .pill.clean { background: var(--success-bg); color: var(--success); }
  .pill.drifted { background: var(--warning-bg); color: var(--warning); }
  .pill.synced { background: var(--success-bg); color: var(--success); }
  .pill.orphaned { background: var(--danger-bg); color: var(--danger); }
</style>
