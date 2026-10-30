<script>
  // Shown while fghjd has no workspace wired at all. App keeps polling, so
  // the graph replaces this as soon as `fghj wire` registers one.
  const DOCS = 'https://fghj.mlops.ninja';

  const WAYS = [
    {
      title: 'Try the example',
      text: 'A small Dropbox in four repos, the one the tutorial builds.',
      command: 'mkdir files && cd files\nfghj wire https://github.com/fghj-examples/files-web.git',
    },
    {
      title: 'Wire your own repo',
      text: 'From a checkout that has a .fghj.yaml. Its parent folder becomes the workspace.',
      command: 'cd path/to/your-repo\nfghj wire .',
    },
  ];

  let copied = $state(null);
  async function copy(i, command) {
    await navigator.clipboard.writeText(command);
    copied = i;
    setTimeout(() => {
      if (copied === i) copied = null;
    }, 1500);
  }
</script>

<div class="welcome">
  <h1>Welcome to fghj</h1>
  <p class="lead">No workspace is wired yet. Wire one from a terminal and it shows up here.</p>

  <div class="ways">
    {#each WAYS as way, i}
      <section class="way">
        <h2>{way.title}</h2>
        <p>{way.text}</p>
        <div class="command">
          <pre>{way.command}</pre>
          <button onclick={() => copy(i, way.command)}>{copied === i ? 'Copied' : 'Copy'}</button>
        </div>
      </section>
    {/each}
  </div>

  <p class="waiting"><span class="spinner"></span> Waiting for a workspace…</p>

  <p class="links">
    New here? <a href="{DOCS}/getting-started/quickstart/" target="_blank" rel="noreferrer">Quickstart</a>,
    or the <a href="{DOCS}/tutorial/" target="_blank" rel="noreferrer">tutorial</a>, which starts from the
    example above.
  </p>
</div>

<style>
  .welcome { max-width: 760px; margin: 40px auto 60px; padding: 0 40px; color: var(--ink); }
  h1 { font: 700 34px/1.15 var(--font-display); margin: 0 0 10px; }
  .lead { font: 400 15px/1.5 var(--font-body); color: var(--ink-dim); margin: 0 0 28px; }
  .ways { display: flex; flex-direction: column; gap: 16px; }
  .way {
    background: var(--panel); border: 1px solid var(--line); border-radius: var(--radius-md);
    padding: 18px 20px; display: flex; flex-direction: column; gap: 8px;
  }
  h2 { font: 700 16px var(--font-body); margin: 0; }
  .way p { font: 400 13px/1.5 var(--font-body); color: var(--ink-dim); margin: 0; }
  .command {
    position: relative; background: var(--panel-2); border: 1px solid var(--line);
    border-radius: var(--radius-sm); margin-top: 6px;
  }
  pre { margin: 0; padding: 12px 70px 12px 12px; font: 400 12px/1.6 var(--font-mono); white-space: pre; overflow-x: auto; }
  .command button {
    position: absolute; top: 8px; right: 8px; padding: 3px 8px; border-radius: 4px; cursor: pointer;
    background: var(--panel); border: 1px solid var(--line-strong); color: var(--ink); font: 600 11px var(--font-body);
  }
  .command button:focus-visible, a:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  .waiting { display: flex; align-items: center; gap: 10px; margin: 24px 0 8px; font: 500 12px var(--font-mono); color: var(--ink-faint); }
  .spinner {
    width: 10px; height: 10px; border-radius: 50%; flex: 0 0 auto;
    border: 2px solid var(--line-strong); border-top-color: var(--ink-faint); animation: spin 0.7s linear infinite;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  @media (prefers-reduced-motion: reduce) { .spinner { animation: none; } }
  .links { font: 400 13px/1.5 var(--font-body); color: var(--ink-dim); margin: 0; }
  a { color: var(--accent); }
</style>
