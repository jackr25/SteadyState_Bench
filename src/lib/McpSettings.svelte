<script lang="ts">
  // Collapsible "MCP Settings": the agent-control switch ("Agentic power configuration") and
  // the port agents connect to. Collapsed until clicked.
  import { psu } from './psu.svelte'

  let open = $state(false)

  const on = $derived(psu.agentControl)
  const mcp = $derived(psu.mcp)

  // Port field: `draft` holds what's typed until it's applied or abandoned (Escape).
  let draft = $state<string | null>(null)
  let busy = $state(false)
  let applyError = $state<string | null>(null)

  const current = $derived(mcp?.port ?? null)
  const shown = $derived(draft ?? (current === null ? '' : String(current)))
  const parsed = $derived(/^\d{1,5}$/.test(shown.trim()) ? Number(shown.trim()) : NaN)
  const invalid = $derived(!(parsed >= 1 && parsed <= 65535))
  // A failed server can be retried on the same port (e.g. after freeing it).
  const canApply = $derived(!invalid && !busy && (parsed !== current || mcp?.kind === 'failed'))
  const error = $derived(applyError ?? (mcp?.kind === 'failed' ? mcp.error : null))

  async function apply(e: SubmitEvent) {
    e.preventDefault()
    if (!canApply) return
    busy = true
    applyError = await psu.setMcpPort(parsed)
    busy = false
    if (!applyError) draft = null
  }

  // In a short window the panel opens below the fold; bring it into view.
  function reveal(node: HTMLElement) {
    node.scrollIntoView({ block: 'nearest' })
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Escape' && draft !== null) {
      e.stopPropagation()
      draft = null
      applyError = null
    }
  }
</script>

<section class="settings" class:open>
  <button class="tab" aria-expanded={open} aria-controls="mcp-settings" onclick={() => (open = !open)}>
    <svg class="chevron" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
      <path d="M6 4l4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
    </svg>
    <span>MCP Settings</span>
  </button>

  {#if open}
    <div class="body" id="mcp-settings" use:reveal>
      <div class="row">
        <span class="label" class:on>{on ? 'Full Agent Controls' : 'Output On/Off Controls Only'}</span>
        <button
          class="switch"
          class:on
          role="switch"
          aria-checked={on}
          aria-label="Agentic power configuration"
          onclick={() => psu.setAgentControl(!on)}
        >
          <span class="pos" class:active={!on}>Off</span>
          <span class="pos" class:active={on}>On</span>
        </button>
      </div>

      <form class="address" onsubmit={apply}>
        <label class="label" for="mcp-port">Address</label>
        <div class="line">
          <div class="field" class:invalid={draft !== null && invalid}>
            <span class="fixed host">http://127.0.0.1:</span>
            <input
              id="mcp-port"
              inputmode="numeric"
              maxlength="5"
              autocomplete="off"
              spellcheck="false"
              value={shown}
              oninput={(e) => {
                draft = e.currentTarget.value
                applyError = null
              }}
              {onkeydown}
            />
            <span class="fixed">/mcp</span>
          </div>
          <button class="apply" type="submit" disabled={!canApply}>{busy ? 'Applying…' : 'Apply'}</button>
        </div>
        {#if error}
          <p class="error" role="alert">{error}</p>
        {/if}
      </form>
    </div>
  {/if}
</section>

<style>
  .settings {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 10px 14px;
    border: 0;
    border-radius: 6px;
    background: none;
    font-size: 12px;
    font-weight: 600;
    letter-spacing: 0.09em;
    text-transform: uppercase;
    color: var(--muted);
    cursor: pointer;
  }
  .tab:hover {
    color: var(--text);
  }
  .chevron {
    flex: none;
    transition: transform 120ms;
  }
  .open .chevron {
    transform: rotate(90deg);
  }
  .body {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 12px 14px 14px;
    border-top: 1px solid var(--border);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .label {
    flex: 1;
    min-width: 0;
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
  }
  .label.on {
    color: var(--focus);
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
  .switch.on .pos.active {
    background: var(--focus);
    color: #fff;
    box-shadow: none;
  }

  .address {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .line {
    display: flex;
    gap: 8px;
  }
  .field {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    padding: 0 8px;
    border: 1px solid var(--border-strong);
    border-radius: 5px;
    background: var(--surface-2);
    font-family: var(--mono);
    font-size: 12px;
    overflow: hidden;
  }
  .field:focus-within {
    border-color: var(--focus);
  }
  .field.invalid {
    border-color: var(--danger);
  }
  .fixed {
    flex: none;
    color: var(--faint);
    white-space: nowrap;
  }
  /* In a narrow window the fixed host gives way, not the port being edited. */
  .host {
    flex: 0 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  input {
    flex: none;
    width: 6ch;
    padding: 6px 1px;
    border: 0;
    background: none;
    font: inherit;
    color: var(--text);
    text-align: center;
    user-select: text;
  }
  input:focus {
    outline: none;
  }
  .apply {
    flex: none;
    padding: 0 12px;
    border: 1px solid var(--border-strong);
    border-radius: 5px;
    background: none;
    color: var(--muted);
    cursor: pointer;
  }
  .apply:hover:not(:disabled) {
    color: var(--text);
    background: var(--surface-2);
  }
  .apply:disabled {
    cursor: default;
    opacity: 0.5;
  }
  .error {
    margin: 0;
    font-size: 11px;
    color: var(--danger);
  }
</style>
