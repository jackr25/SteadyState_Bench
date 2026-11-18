<script lang="ts">
  import { psu } from './psu.svelte'

  const link = $derived(psu.link)
  // While searching or disconnected, show the device we expect so startup isn't blank.
  // While choosing, the picker dialog has the details.
  const device = $derived(
    link.kind === 'connected' ? link.device : link.kind === 'choosing' ? null : link.last,
  )
  const title = $derived(
    link.kind === 'connected'
      ? `${link.device.model} on ${link.device.port} (${link.device.driver})`
      : link.kind === 'searching'
        ? 'Looking for a supported supply…'
        : link.kind === 'choosing'
          ? 'Choose a supply to connect to'
          : link.reason,
  )
</script>

<div class="link {link.kind}" {title}>
  <span class="dot"></span>
  {#if device}
    <span class="model">{device.model}</span>
    <span class="port">{device.port}</span>
  {/if}
  {#if link.kind === 'searching'}
    <span class="note">{device ? 'connecting…' : 'searching…'}</span>
  {:else if link.kind === 'choosing'}
    <span class="note">
      {link.found.length === 1 ? 'new supply found' : `${link.found.length} supplies found`}
    </span>
  {:else if link.kind === 'disconnected'}
    <span class="note">{device ? 'offline' : link.reason}</span>
    <button class="rescan" onclick={psu.rescan}>rescan</button>
  {/if}
</div>

<style>
  .link {
    display: flex;
    align-items: center;
    gap: 7px;
    min-width: 0;
    font-size: 11px;
    color: var(--faint);
    white-space: nowrap;
  }
  .dot {
    flex: none;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--on);
    opacity: 0.8;
  }
  .searching .dot {
    background: var(--warn);
    animation: pulse 1.2s ease-in-out infinite;
  }
  .choosing .dot {
    background: var(--warn);
  }
  .disconnected .dot {
    background: transparent;
    box-shadow: inset 0 0 0 1px var(--faint);
  }
  .model {
    overflow: hidden;
    text-overflow: ellipsis;
    color: var(--muted);
  }
  .disconnected .model {
    color: var(--faint);
  }
  .port {
    flex: none;
    font-family: var(--mono);
  }
  .note {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .rescan {
    flex: none;
    padding: 0;
    border: 0;
    background: none;
    color: var(--muted);
    text-decoration: underline;
    text-underline-offset: 2px;
    cursor: pointer;
  }
  .rescan:hover {
    color: var(--text);
  }
  @keyframes pulse {
    50% {
      opacity: 0.25;
    }
  }
</style>
