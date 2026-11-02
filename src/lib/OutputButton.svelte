<script lang="ts">
  import { psu } from './psu.svelte'

  let busy = $state(false)
  // Shows what the device reports, not what we last asked for.
  const on = $derived(psu.status?.output_on ?? false)
  const known = $derived(psu.status !== null)

  async function toggle() {
    busy = true
    await psu.setOutput(!on)
    busy = false
  }
</script>

<button class="output" class:on disabled={!known || busy} onclick={toggle} aria-pressed={on}>
  <span class="led"></span>
  <span class="label">Output</span>
  <span class="state">{known ? (on ? 'On' : 'Off') : '—'}</span>
</button>

<style>
  .output {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 12px 16px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-size: 12px;
    font-weight: 600;
    letter-spacing: 0.09em;
    text-transform: uppercase;
    color: var(--muted);
    cursor: pointer;
    transition:
      background 120ms,
      border-color 120ms,
      color 120ms;
  }
  .output:hover:not(:disabled) {
    border-color: var(--border-strong);
  }
  .output:disabled {
    cursor: default;
    opacity: 0.55;
  }
  .led {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--faint);
    transition:
      background 120ms,
      box-shadow 120ms;
  }
  .state {
    margin-left: auto;
    font-family: var(--mono);
    letter-spacing: 0.04em;
  }
  .on {
    background: var(--on-bg);
    border-color: var(--on);
    color: var(--on);
  }
  .on:hover:not(:disabled) {
    border-color: var(--on);
  }
  .on .led {
    background: var(--on);
    box-shadow: 0 0 6px var(--on);
  }
</style>
