<script lang="ts">
  // Edits are a local draft until Enter. Escape or leaving the field discards them, so a
  // stray click never sends a half-typed value to the supply.
  import { untrack } from 'svelte'

  let {
    value,
    decimals,
    max,
    step,
    unit,
    disabled = false,
    onapply,
  }: {
    value: number | null
    decimals: number
    max: number | null
    step: number
    unit: string
    disabled?: boolean
    onapply: (v: number) => Promise<boolean>
  } = $props()

  // After a successful apply the draft keeps showing the sent value until the device's
  // reading changes, or this long at most, so the field never lies about the device.
  const ECHO_MS = 1000

  let draft = $state<string | null>(null)
  let busy = $state(false)
  let focused = false
  let keepOnBlur = false

  // A new reading from the device replaces any draft that isn't being edited.
  $effect(() => {
    void value
    if (!focused && !untrack(() => busy)) draft = null
  })

  const shown = $derived(draft ?? (value === null ? '' : value.toFixed(decimals)))
  const parsed = $derived(draft === null || draft.trim() === '' ? NaN : Number(draft))
  const invalid = $derived(
    draft !== null && (!Number.isFinite(parsed) || parsed < 0 || (max !== null && parsed > max)),
  )
  const dirty = $derived(
    draft !== null && !invalid && (value === null || parsed.toFixed(decimals) !== value.toFixed(decimals)),
  )

  async function apply(input: HTMLInputElement) {
    if (!dirty || busy) return
    const v = Number(parsed.toFixed(decimals))
    busy = true
    const ok = await onapply(v)
    busy = false
    if (!ok) {
      if (!focused) draft = null
      return
    }
    draft = v.toFixed(decimals)
    setTimeout(() => {
      if (!focused) draft = null
    }, ECHO_MS)
    keepOnBlur = true
    input.blur()
    keepOnBlur = false
  }

  function nudge(dir: 1 | -1, e: KeyboardEvent) {
    const base = Number.isFinite(parsed) ? parsed : (value ?? 0)
    const s = e.shiftKey ? step * 10 : e.altKey ? step / 10 : step
    let next = Math.max(0, base + dir * s)
    if (max !== null) next = Math.min(max, next)
    draft = next.toFixed(decimals)
  }

  function onkeydown(e: KeyboardEvent) {
    const input = e.currentTarget as HTMLInputElement
    if (e.key === 'Enter') apply(input)
    else if (e.key === 'Escape') {
      draft = null
      input.blur()
      e.stopPropagation()
    } else if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
      e.preventDefault()
      nudge(e.key === 'ArrowUp' ? 1 : -1, e)
    }
  }
</script>

<label class="setpoint" class:dirty class:invalid class:busy class:disabled>
  <span class="tag">set</span>
  <input
    type="text"
    inputmode="decimal"
    spellcheck="false"
    autocomplete="off"
    value={shown}
    placeholder="—"
    {disabled}
    oninput={(e) => (draft = e.currentTarget.value)}
    onfocus={(e) => {
      focused = true
      e.currentTarget.select()
    }}
    onblur={() => {
      focused = false
      if (!busy && !keepOnBlur) draft = null
    }}
    {onkeydown}
    title={max === null ? '' : `0 – ${max.toFixed(decimals)} ${unit} · Enter to apply · ↑↓ to step (Shift ×10, Alt ÷10)`}
  />
  <span class="unit">{unit}</span>
  <span class="hint" class:show={dirty && !busy}>↵</span>
</label>

<style>
  .setpoint {
    display: inline-flex;
    align-items: baseline;
    gap: 6px;
    padding: 3px 8px;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--surface-2);
    font-family: var(--mono);
    font-variant-numeric: tabular-nums;
    transition: border-color 120ms;
    cursor: text;
  }
  .setpoint:hover:not(.disabled) {
    border-color: var(--border-strong);
  }
  .setpoint:focus-within {
    border-color: var(--focus);
  }
  .dirty {
    border-color: var(--warn) !important;
  }
  .invalid {
    border-color: var(--danger) !important;
  }
  .busy {
    opacity: 0.6;
  }
  .disabled {
    opacity: 0.5;
    cursor: default;
  }
  .tag {
    font-family: var(--sans);
    font-size: 10px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--faint);
  }
  input {
    width: 7ch;
    padding: 0;
    border: 0;
    outline: 0;
    background: transparent;
    font-size: 14px;
    text-align: right;
    user-select: text;
  }
  .unit {
    color: var(--muted);
    font-size: 12px;
  }
  .hint {
    width: 1ch;
    color: var(--warn);
    font-size: 11px;
    visibility: hidden;
  }
  .hint.show {
    visibility: visible;
  }
</style>
