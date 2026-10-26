// Top-to-bottom order of the cards in each column, plugged into dagre as its
// `customOrder`. dagre still picks the columns, places the cards and routes
// the edges; only the order within a column is ours.
//
// dagre's own ordering is a greedy sweep that stops at the first local
// optimum it finds, and which optimum that is depends on the order edges
// were added in. It also knows nothing about repos, so two services from one
// checkout can end up on either side of an unrelated one. Here its result is
// only the starting point:
//
//  - Cards from one repo form a block that's never split. Edge dummies (the
//    slots a column-skipping edge passes through) are blocks of their own.
//  - Sweeps sort blocks, then cards within a block, by where their neighbours
//    sit in the column next door.
//  - Swapping neighbouring blocks, and neighbouring cards within a block,
//    keeps any swap that cuts crossings.
//
// Crossings are weighted the way dagre weighs them, by the product of the two
// edges' weights, so a needed-to-start edge is worth avoiding more than a
// needed-at-runtime one. Every step only looks at a card's own edges and
// the passes are capped, so a big column costs passes, not combinations.
const MAX_SWEEPS = 12, MAX_SWAP_PASSES = 8;

export function columnOrder(groupOf) {
  return (graph, dagreOrder) => {
    dagreOrder(graph);

    const ranks = new Map();
    graph.nodes().forEach((v) => {
      const { rank } = graph.node(v);
      if (!ranks.has(rank)) ranks.set(rank, []);
      ranks.get(rank).push(v);
    });
    const all = [...ranks.keys()].sort((a, b) => a - b).map((r) =>
      ranks.get(r).sort((a, b) => graph.node(a).order - graph.node(b).order),
    );
    // dagre doubles its ranks internally, so between two columns of cards
    // there's a column of nothing but dummies, one per edge. Ordering those
    // as well would mean a card can only swap with its neighbour if their
    // dummies swap too, which no single step does. So they're skipped here,
    // each card linked straight through to the next real column, and lined
    // up behind the result at the end.
    const passing = all.filter((layer) => layer.every((v) => graph.node(v).dummy));
    const skipped = new Set(passing.flat());
    let layers = all.filter((layer) => !passing.includes(layer));

    const group = (v) => groupOf(v) ?? v;
    // dagre's edges all point to the next rank by now (longer ones are
    // chains through dummies), so in = left and out = right.
    const through = (v, edgesOf, end) =>
      edgesOf.call(graph, v).map((e) => {
        let n = e[end];
        while (skipped.has(n)) n = edgesOf.call(graph, n)[0][end];
        return { v: n, w: graph.edge(e).weight ?? 1 };
      });
    const left = {}, right = {};
    graph.nodes().forEach((v) => {
      left[v] = through(v, graph.inEdges, 'v');
      right[v] = through(v, graph.outEdges, 'w');
    });

    const pos = {};
    const index = (layer) => layer.forEach((v, i) => (pos[v] = i));

    // Blocks as they currently stand, in order: one run of cards per group.
    // Joining a group's cards into one block happens on the first sort.
    const blocksOf = (layer) => {
      const blocks = new Map();
      layer.forEach((v) => {
        const g = group(v);
        if (!blocks.has(g)) blocks.set(g, []);
        blocks.get(g).push(v);
      });
      return [...blocks.values()];
    };

    // Sort one column by its neighbours in the column `fixed` points into.
    // A card with no neighbour there takes its block's value; a block with
    // none keeps its current place. Positions are scaled to 0..1 so a
    // neighbour's place and a card's own are comparable.
    const sortLayer = (layer, fixed, fixedSize) => {
      const scale = (i, n) => (n > 1 ? i / (n - 1) : 0.5);
      const own = (v) => {
        let sum = 0, weight = 0;
        fixed[v].forEach((n) => ((sum += n.w * scale(pos[n.v], fixedSize)), (weight += n.w)));
        return weight ? { sum, weight } : null;
      };
      const keyed = blocksOf(layer).map((members) => {
        const scores = members.map(own);
        let sum = 0, weight = 0;
        scores.forEach((s) => s && ((sum += s.sum), (weight += s.weight)));
        const first = Math.min(...members.map((v) => pos[v]));
        const at = weight ? sum / weight : scale(first, layer.length);
        const cards = members
          .map((v, i) => ({ v, at: scores[i] ? scores[i].sum / scores[i].weight : at }))
          .sort((a, b) => a.at - b.at || pos[a.v] - pos[b.v])
          .map((c) => c.v);
        return { cards, at, first };
      });
      keyed.sort((a, b) => a.at - b.at || a.first - b.first);
      return keyed.flatMap((b) => b.cards);
    };

    // Crossings between u's edges and v's edges, on both sides, if u is
    // above v. Only the cards' own edges move when they swap.
    const pairCost = (u, v) => {
      let c = 0;
      for (const edges of [left, right])
        for (const a of edges[u]) for (const b of edges[v]) if (pos[a.v] > pos[b.v]) c += a.w * b.w;
      return c;
    };
    const blockCost = (a, b) => a.reduce((c, u) => b.reduce((c, v) => c + pairCost(u, v), c), 0);

    const swapPass = (layer) => {
      let improved = false;
      const blocks = blocksOf(layer);
      for (let i = 0; i + 1 < blocks.length; i++) {
        if (blockCost(blocks[i + 1], blocks[i]) < blockCost(blocks[i], blocks[i + 1])) {
          [blocks[i], blocks[i + 1]] = [blocks[i + 1], blocks[i]];
          index(blocks.flat());
          improved = true;
        }
      }
      for (const cards of blocks) {
        for (let i = 0; i + 1 < cards.length; i++) {
          if (pairCost(cards[i + 1], cards[i]) < pairCost(cards[i], cards[i + 1])) {
            [cards[i], cards[i + 1]] = [cards[i + 1], cards[i]];
            index(blocks.flat());
            improved = true;
          }
        }
      }
      return { layer: blocks.flat(), improved };
    };

    const crossings = () => {
      let c = 0;
      layers.forEach((layer) => {
        const edges = layer.flatMap((u) => right[u].map((n) => ({ a: pos[u], b: pos[n.v], w: n.w })));
        for (let i = 0; i < edges.length; i++)
          for (let j = i + 1; j < edges.length; j++)
            if ((edges[i].a - edges[j].a) * (edges[i].b - edges[j].b) < 0) c += edges[i].w * edges[j].w;
      });
      return c;
    };

    layers.forEach(index);
    // Join each group into one block before anything is scored: until then
    // the order may split a repo, and the best one kept has to be grouped.
    layers = layers.map((layer) => {
      const joined = blocksOf(layer).flat();
      index(joined);
      return joined;
    });

    let best = layers.map((l) => [...l]), bestCrossings = crossings();
    for (let i = 0, stale = 0; i < MAX_SWEEPS && stale < 2; i++) {
      // Alternate left-to-right (sort by the column on the left) and back.
      const down = i % 2 === 0;
      const order = down ? layers.map((_, r) => r) : layers.map((_, r) => layers.length - 1 - r);
      for (const r of order.slice(1)) {
        const fixed = down ? layers[r - 1] : layers[r + 1];
        layers[r] = sortLayer(layers[r], down ? left : right, fixed.length);
        index(layers[r]);
      }
      for (let pass = 0, again = true; again && pass < MAX_SWAP_PASSES; pass++) {
        again = false;
        layers = layers.map((layer) => {
          const result = swapPass(layer);
          again ||= result.improved;
          return result.layer;
        });
      }
      const c = crossings();
      if (c < bestCrossings) {
        best = layers.map((l) => [...l]);
        bestCrossings = c;
        stale = 0;
      } else {
        stale++;
      }
    }

    best.forEach((layer) => layer.forEach((v, i) => ((graph.node(v).order = i), (pos[v] = i))));
    // A skipped dummy sits on one edge: in its column, edges go in the
    // order of where they start, then of where they end.
    passing.forEach((layer) =>
      layer
        .map((v) => ({ v, from: pos[left[v][0].v], to: pos[right[v][0].v] }))
        .sort((a, b) => a.from - b.from || a.to - b.to)
        .forEach((d, i) => (graph.node(d.v).order = i)),
    );
  };
}
