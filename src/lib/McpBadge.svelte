<script lang="ts">
  // Shows that agents can drive the app over MCP, and where. Click copies the URL.
  import { psu } from './psu.svelte'

  const mcp = $derived(psu.mcp)
  let copied = $state(false)

  const title = $derived(
    mcp?.kind === 'listening'
      ? `Agents can control this supply over MCP at ${mcp.url} (click to copy)`
      : mcp?.kind === 'failed'
        ? `MCP server not running: ${mcp.error}`
        : '',
  )

  async function copy() {
    if (mcp?.kind !== 'listening') return
    try {
      await navigator.clipboard.writeText(mcp.url)
      copied = true
      setTimeout(() => (copied = false), 1200)
    } catch {
      // Clipboard unavailable: the URL is still in the tooltip.
    }
  }
</script>

{#if mcp}
  <button class="mcp {mcp.kind}" class:active={psu.agentNote !== null} onclick={copy} {title} aria-label={title}>
    {copied ? 'copied' : 'MCP'}
  </button>
{/if}

<style>
  .mcp {
    height: 22px;
    padding: 0 6px;
    border: 0;
    border-radius: 4px;
    background: none;
    font-family: var(--mono);
    font-size: 10px;
    letter-spacing: 0.04em;
    color: var(--faint);
    cursor: pointer;
  }
  .mcp:hover {
    background: var(--surface-2);
    color: var(--muted);
  }
  .failed {
    color: var(--danger);
    text-decoration: line-through;
  }
  .active {
    color: var(--focus);
  }
</style>
