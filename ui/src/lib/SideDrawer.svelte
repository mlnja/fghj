<script>
  let { onClose, width = '70vw', children } = $props();
</script>

<div class="veil" onclick={onClose}>
  <div class="drawer" style="width:{width}" onclick={(e) => e.stopPropagation()}>
    {@render children()}
  </div>
</div>

<style>
  .veil { position: fixed; inset: 0; background: rgba(0,0,0,0.5); z-index: 90; display: flex; justify-content: flex-end; }
  .drawer {
    height: 100%; background: var(--panel); border-left: 1px solid var(--line-strong);
    padding: 24px; box-shadow: -24px 0 60px -20px rgba(19,27,46,0.22); overflow-y: auto;
  }
</style>
