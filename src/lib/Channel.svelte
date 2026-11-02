<script lang="ts">
  import type { Snippet } from 'svelte'

  let {
    label,
    unit,
    color,
    value,
    decimals,
    children,
  }: {
    label: string
    unit: string
    /** Accent token name from app.css, e.g. "volt". */
    color: string
    value: number | null
    decimals: number
    children?: Snippet
  } = $props()

  const placeholder = $derived('-.' + '-'.repeat(decimals))
</script>

<section class="channel" style:--accent="var(--{color})">
  <div class="label">{label}</div>
  <div class="body">
    <div class="reading" class:stale={value === null}>
      <span class="num">{value === null ? placeholder : value.toFixed(decimals)}</span>
      <span class="unit">{unit}</span>
    </div>
    {#if children}
      <div class="controls">{@render children()}</div>
    {/if}
  </div>
</section>

<style>
  .channel {
    position: relative;
    padding: 10px 14px 12px 16px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
    overflow: hidden;
  }
  .channel::before {
    content: '';
    position: absolute;
    inset: 0 auto 0 0;
    width: 3px;
    background: var(--accent);
  }
  .label {
    font-size: 10.5px;
    font-weight: 600;
    letter-spacing: 0.09em;
    text-transform: uppercase;
    color: var(--muted);
  }
  .body {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-end;
    justify-content: space-between;
    gap: 6px 12px;
    margin-top: 2px;
  }
  .reading {
    display: flex;
    align-items: baseline;
    gap: 6px;
    font-family: var(--mono);
    font-variant-numeric: tabular-nums;
    line-height: 1;
  }
  .num {
    font-size: clamp(28px, 12cqi, 52px);
    font-weight: 500;
    letter-spacing: -0.02em;
  }
  .unit {
    font-size: clamp(13px, 4cqi, 18px);
    color: var(--accent);
  }
  .stale .num {
    color: var(--faint);
  }
  .controls {
    padding-bottom: 2px;
  }

  @container app (min-width: 560px) {
    .body {
      flex-direction: column;
      align-items: flex-start;
      gap: 12px;
    }
    .num {
      font-size: clamp(32px, 6.5cqi, 112px);
    }
    .unit {
      font-size: clamp(14px, 2.2cqi, 32px);
    }
  }
</style>
