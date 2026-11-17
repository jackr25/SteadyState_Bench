<script lang="ts">
  // "Agentic power configuration": whether agents connected over MCP may change setpoints
  // and the output. Off at every launch; reading status is always allowed.
  import { psu } from './psu.svelte'

  const on = $derived(psu.agentControl)
  const unavailable = $derived(psu.mcp.kind !== 'listening')
  const note = $derived(
    psu.mcp.kind === 'failed'
      ? 'MCP server not running'
      : on
        ? 'Agents can change setpoints and output'
        : 'Agents can only read status',
  )
</script>

{#if psu.mcp.kind !== 'disabled'}
  <div class="agent" class:on>
    <div class="text">
      <span class="label" id="agent-switch-label">Agentic power configuration</span>
      <span class="note">{note}</span>
    </div>
    <button
      class="switch"
      role="switch"
      aria-checked={on}
      aria-labelledby="agent-switch-label"
      disabled={unavailable}
      onclick={() => psu.setAgentControl(!on)}
    >
      <span class="pos" class:active={!on}>Off</span>
      <span class="pos" class:active={on}>On</span>
    </button>
  </div>
{/if}

<style>
  .agent {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 12px 10px 16px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
    transition: border-color 120ms;
  }
  .agent.on {
    border-color: var(--focus);
  }
  .text {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .label {
    font-size: 12px;
    font-weight: 600;
    letter-spacing: 0.09em;
    text-transform: uppercase;
    color: var(--muted);
  }
  .on .label {
    color: var(--focus);
  }
  .note {
    font-size: 11px;
    color: var(--faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* Two-position switch: both positions labeled, the active one filled. */
  .switch {
    flex: none;
    display: flex;
    padding: 2px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--surface-2);
    cursor: pointer;
  }
  .switch:disabled {
    cursor: default;
    opacity: 0.55;
  }
  .pos {
    min-width: 34px;
    padding: 3px 8px;
    border-radius: 4px;
    font-family: var(--mono);
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--faint);
    transition:
      background 120ms,
      color 120ms;
  }
  .pos.active {
    background: var(--surface);
    color: var(--text);
    box-shadow: 0 0 0 1px var(--border-strong);
  }
  .on .pos.active {
    background: var(--focus);
    color: #fff;
    box-shadow: none;
  }
</style>
