<script>
  let { eyebrow, text, children } = $props();
</script>

<div class="placeholder">
  <div class="head">
    <span class="eyebrow">{eyebrow}</span>
    <p class="body-sm">{text}</p>
  </div>
  {#if children}
    <div class="body">
      {@render children()}
    </div>
  {/if}
</div>

<style>
  .placeholder { padding: 0 40px 40px; }
  .head { max-width: 720px; display: flex; flex-direction: column; gap: 8px; margin-bottom: 20px; }
  .body { opacity: 0.9; }
</style>
