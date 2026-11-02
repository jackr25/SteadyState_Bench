<script lang="ts">
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import { onMount } from 'svelte'
  import Channel from './lib/Channel.svelte'
  import LinkStatus from './lib/LinkStatus.svelte'
  import OutputButton from './lib/OutputButton.svelte'
  import SetpointInput from './lib/SetpointInput.svelte'
  import { psu } from './lib/psu.svelte'
  import { cycleTheme, theme } from './lib/theme.svelte'

  const win = getCurrentWindow()
  let fullscreen = $state(false)

  const s = $derived(psu.status)
  const limits = $derived(psu.limits)

  onMount(() => {
    const unlisten = win.onResized(async () => {
      fullscreen = await win.isFullscreen()
    })
    return () => {
      unlisten.then((f) => f())
    }
  })

  async function setFullscreen(on: boolean) {
    await win.setFullscreen(on)
    fullscreen = on
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'F11') {
      e.preventDefault()
      setFullscreen(!fullscreen)
    } else if (e.key === 'Escape' && fullscreen) {
      setFullscreen(false)
    }
  }
</script>

<svelte:window {onkeydown} />

<div class="app">
  <main>
    <div class="channels">
      <Channel label="Voltage" unit="V" color="volt" value={s?.v_meas ?? null} decimals={2}>
        <SetpointInput
          value={s?.v_set ?? null}
          decimals={2}
          step={0.1}
          max={limits?.voltage ?? null}
          unit="V"
          disabled={!s}
          onapply={psu.setVoltage}
        />
      </Channel>
      <Channel label="Current" unit="A" color="amp" value={s?.i_meas ?? null} decimals={3}>
        <SetpointInput
          value={s?.i_set ?? null}
          decimals={3}
          step={0.01}
          max={limits?.current ?? null}
          unit="A"
          disabled={!s}
          onapply={psu.setCurrent}
        />
      </Channel>
      <Channel label="Power" unit="W" color="watt" value={s?.p_meas ?? null} decimals={2} />
    </div>
    <OutputButton />
  </main>

  <footer>
    {#if psu.error}
      <span class="error" title={psu.error}>{psu.error}</span>
    {:else}
      <LinkStatus />
    {/if}
    <div class="tools">
      <button class="icon" onclick={cycleTheme} title="Theme: {theme.value}" aria-label="Theme: {theme.value}">
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          {#if theme.value === 'light'}
            <circle cx="8" cy="8" r="3" fill="currentColor" />
            <g stroke="currentColor" stroke-width="1.3" stroke-linecap="round">
              <path d="M8 1.5v1.6M8 12.9v1.6M1.5 8h1.6M12.9 8h1.6M3.4 3.4l1.1 1.1M11.5 11.5l1.1 1.1M3.4 12.6l1.1-1.1M11.5 4.5l1.1-1.1" />
            </g>
          {:else if theme.value === 'dark'}
            <path d="M13.5 9.6A5.6 5.6 0 0 1 6.4 2.5a5.6 5.6 0 1 0 7.1 7.1Z" fill="currentColor" />
          {:else}
            <circle cx="8" cy="8" r="5.6" fill="none" stroke="currentColor" stroke-width="1.3" />
            <path d="M8 2.4a5.6 5.6 0 0 1 0 11.2Z" fill="currentColor" />
          {/if}
        </svg>
      </button>
      <button
        class="icon"
        onclick={() => setFullscreen(!fullscreen)}
        title={fullscreen ? 'Exit fullscreen (F11)' : 'Fullscreen (F11)'}
        aria-label="Toggle fullscreen"
      >
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          <g fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round">
            {#if fullscreen}
              <path d="M6 2v4H2M10 2v4h4M6 14v-4H2M10 14v-4h4" />
            {:else}
              <path d="M2 6V2h4M14 6V2h-4M2 10v4h4M14 10v4h-4" />
            {/if}
          </g>
        </svg>
      </button>
    </div>
  </footer>
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100%;
    container: app / inline-size;
  }
  main {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px;
    overflow-y: auto;
  }
  .channels {
    display: grid;
    gap: 8px;
  }
  footer {
    flex: none;
    display: flex;
    align-items: center;
    gap: 10px;
    height: 28px;
    padding: 0 6px 0 12px;
    border-top: 1px solid var(--border);
    background: var(--surface);
  }
  footer > :first-child {
    flex: 1;
    min-width: 0;
  }
  .error {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 11px;
    color: var(--danger);
  }
  .tools {
    display: flex;
    gap: 2px;
  }
  .icon {
    display: grid;
    place-items: center;
    width: 24px;
    height: 22px;
    padding: 0;
    border: 0;
    border-radius: 4px;
    background: none;
    color: var(--faint);
    cursor: pointer;
  }
  .icon:hover {
    background: var(--surface-2);
    color: var(--muted);
  }

  @container app (min-width: 560px) {
    main {
      padding: 14px;
      gap: 12px;
    }
    .channels {
      grid-template-columns: repeat(3, 1fr);
      gap: 12px;
    }
  }
  /* Fullscreen / large tiles: let the readouts take the space. */
  @container app (min-width: 900px) {
    main {
      justify-content: center;
      padding: 24px;
    }
  }
</style>
