<script lang="ts">
  // Asks which supply to connect to when a scan finds a new one, or several (the backend's
  // `LinkState::Choosing`). The supply used last time reconnects without asking.
  import { psu } from './psu.svelte'

  const choice = $derived(psu.link.kind === 'choosing' ? psu.link : null)
  const single = $derived(choice?.found.length === 1)

  let selected = $state(0)
  let busy = $state(false)

  // Each new question starts from the backend's suggestion.
  $effect.pre(() => {
    if (choice) {
      selected = choice.suggested
      busy = false
    }
  })

  const lastNote = $derived(
    choice?.last ? `Your last supply, ${choice.last.model}, wasn't found. ` : '',
  )

  async function connect() {
    if (!choice || busy) return
    busy = true
    // On success the link moves on and this dialog closes; on failure the error shows in
    // the footer and the choice stays open.
    if (!(await psu.pick(choice.found[selected]))) busy = false
  }

  function dismiss() {
    if (!busy) psu.dismissChoice()
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault()
      e.stopPropagation()
      dismiss()
    } else if (e.key === 'Enter' && !(e.target instanceof HTMLButtonElement)) {
      e.preventDefault()
      connect()
    }
  }

  function autofocus(node: HTMLElement) {
    node.focus()
  }
</script>

{#if choice}
  <div class="backdrop">
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      class="dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="picker-title"
      aria-describedby="picker-desc"
      tabindex="-1"
      {onkeydown}
    >
      <h2 id="picker-title">
        {single ? 'New power supply found' : `${choice.found.length} power supplies found`}
      </h2>
      <p id="picker-desc">
        {lastNote}{single ? 'Connect to it?' : 'Choose one to connect to.'}
        <span class="aside">Connecting only reads the supply; its output isn't changed.</span>
      </p>

      <div class="options" role="radiogroup" aria-label="Supplies found">
        {#each choice.found as d, i (d.port + d.info.driver)}
          <label class="option" class:selected={i === selected} class:single>
            <input type="radio" name="supply" value={i} bind:group={selected} disabled={busy} />
            <span class="model">{d.info.model}</span>
            <span class="port">{d.port}</span>
          </label>
        {/each}
      </div>

      <div class="actions">
        <button class="secondary" onclick={dismiss} disabled={busy}>Not now</button>
        <button class="primary" onclick={connect} disabled={busy} use:autofocus>
          {busy ? 'Connecting…' : 'Connect'}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 10;
    /* Flex, not grid: a grid item's 100% would resolve against its auto-sized track, so
       a long list would overflow the window instead of scrolling. */
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 16px;
    background: rgba(0, 0, 0, 0.35);
  }
  .dialog {
    width: min(360px, 100%);
    max-height: 100%;
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px;
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.25);
    outline: none;
  }
  h2 {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
  }
  p {
    margin: 0;
    color: var(--muted);
  }
  .aside {
    display: block;
    margin-top: 4px;
    font-size: 11px;
    color: var(--faint);
  }
  .options {
    display: flex;
    flex-direction: column;
    gap: 6px;
    min-height: 0;
    overflow-y: auto;
  }
  .option {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 9px 12px;
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: 6px;
    cursor: pointer;
  }
  .option.selected {
    border-color: var(--focus);
    box-shadow: inset 0 0 0 1px var(--focus);
  }
  .option.single {
    cursor: default;
  }
  .option:has(input:focus-visible) {
    outline: 2px solid var(--focus);
    outline-offset: 1px;
  }
  /* The row is the control; the native radio only carries focus and arrow keys. */
  input {
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }
  .model {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text);
  }
  .port {
    flex: none;
    font-family: var(--mono);
    font-size: 12px;
    color: var(--muted);
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .actions button {
    padding: 6px 14px;
    border-radius: 5px;
    border: 1px solid var(--border-strong);
    cursor: pointer;
  }
  .actions button:disabled {
    cursor: default;
    opacity: 0.6;
  }
  .secondary {
    background: none;
    color: var(--muted);
  }
  .secondary:hover:not(:disabled) {
    color: var(--text);
    background: var(--surface-2);
  }
  .primary {
    background: var(--focus);
    border-color: var(--focus);
    color: #fff;
    font-weight: 600;
  }
  .primary:hover:not(:disabled) {
    filter: brightness(1.08);
  }
</style>
