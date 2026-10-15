<script module>
  // Raw output (git's, a build's) as lines for the pane, with no clock.
  export function textLines(text) {
    return (text ?? '')
      .replace(/\n$/, '')
      .split(/\r?\n/)
      .map((text, key) => ({ key, text }));
  }
</script>

<script>
  // One look for every log in the UI: fghjd's own, a container's, a clone's.
  // `lines` are `{ key, ts, text, warn }`; `ts` is epoch ms, an ISO string,
  // or absent. Fills its parent's height unless `maxHeight` caps it.
  let { lines, loadingMore = false, onNearTop = null, maxHeight = null } = $props();

  let el = $state(null);

  // Follows the tail while you're at the bottom and leaves you alone while
  // you're reading further up. Lines added at the top (older history) keep
  // what you were looking at in place.
  let atBottom = true;
  let heightBefore = 0;
  let firstBefore = null;

  $effect.pre(() => {
    lines;
    if (!el) return;
    atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
    heightBefore = el.scrollHeight;
  });

  $effect(() => {
    lines;
    if (!el) return;
    const first = lines[0]?.key ?? null;
    if (atBottom) el.scrollTop = el.scrollHeight;
    else if (firstBefore !== null && first !== firstBefore) el.scrollTop += el.scrollHeight - heightBefore;
    firstBefore = first;
  });

  function onscroll() {
    if (onNearTop && el.scrollTop < 40) onNearTop();
  }

  // 24-hour, so every timestamp is exactly eight characters wide.
  function time(ts) {
    if (ts == null || ts === '') return '';
    const d = new Date(ts);
    return isNaN(d) ? String(ts) : d.toLocaleTimeString([], { hour12: false });
  }
</script>

<div class="pane" class:capped={maxHeight} style={maxHeight ? `max-height:${maxHeight}` : ''} bind:this={el} {onscroll}>
  {#if loadingMore}<div class="more">loading more…</div>{/if}
  {#each lines as l (l.key)}
    <!-- A grid, so a wrapped message hangs under the message column instead
         of running back under the clock. -->
    <div class="line" class:warn={l.warn} class:bare={!l.ts}>
      {#if l.ts}<span class="ts">{time(l.ts)}</span>{/if}
      <span class="msg">{l.text}</span>
    </div>
  {/each}
</div>

<style>
  .pane {
    flex: 1 1 auto; min-height: 120px; overflow: auto;
    background: var(--panel-2); border: 1px solid var(--line); border-radius: 6px;
    padding: 8px 10px;
  }
  .pane.capped { flex: 0 1 auto; min-height: 0; }
  .more { font: italic 500 11px var(--font-mono); color: var(--ink-faint); padding-bottom: 4px; }
  .line {
    display: grid; grid-template-columns: auto 1fr; gap: 0 10px;
    align-items: baseline; padding: 1.5px 0;
    font: 400 11px/1.55 var(--font-mono);
    overflow-wrap: anywhere; white-space: pre-wrap;
  }
  .line.bare { grid-template-columns: 1fr; }
  .ts { color: var(--ink-faint); font-variant-numeric: tabular-nums; }
  .msg { color: var(--ink); }
  /* Amber plus a rule down the left edge, so a warning is findable without
     reading. */
  .line.warn { background: var(--warning-bg); border-left: 2px solid var(--warning); margin-left: -10px; padding-left: 8px; }
  .line.warn .msg, .line.warn .ts { color: var(--warning); }
</style>
