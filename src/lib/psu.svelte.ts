// Frontend mirror of the Rust link worker (src-tauri/src/link.rs). The backend is the
// source of truth: setters only send commands, and the UI updates when the next status
// event reports what the device actually did.

import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

export type KnownDevice = { port: string; driver: string; model: string }
export type Limits = { voltage: number; current: number; power: number }

export type Status = {
  output_on: boolean
  v_meas: number
  i_meas: number
  p_meas: number
  v_set: number
  i_set: number
  ovp: number | null
  ocp: number | null
}

export type LinkState =
  | { kind: 'searching'; last: KnownDevice | null }
  | { kind: 'connected'; device: KnownDevice; limits: Limits }
  | { kind: 'disconnected'; last: KnownDevice | null; reason: string }

type Snapshot = { link: LinkState; status: Status | null }

const ERROR_MS = 5000

class Psu {
  link = $state<LinkState>({ kind: 'searching', last: null })
  status = $state<Status | null>(null)
  error = $state<string | null>(null)

  #errorTimer: ReturnType<typeof setTimeout> | undefined

  get connected() {
    return this.link.kind === 'connected'
  }

  get limits(): Limits | null {
    return this.link.kind === 'connected' ? this.link.limits : null
  }

  async init() {
    // Subscribe before fetching the snapshot so nothing is missed in between; an event
    // that lands first is newer than the snapshot, so the snapshot must not overwrite it.
    let sawLink = false
    let sawStatus = false
    await listen<LinkState>('psu://link', (e) => {
      sawLink = true
      this.link = e.payload
      if (e.payload.kind !== 'connected') this.status = null
    })
    await listen<Status>('psu://status', (e) => {
      sawStatus = true
      this.status = e.payload
    })
    const snap = await invoke<Snapshot>('snapshot')
    if (!sawLink) this.link = snap.link
    if (!sawStatus) this.status = snap.status
  }

  setVoltage = (volts: number) => this.#run('set_voltage', { volts })
  setCurrent = (amps: number) => this.#run('set_current', { amps })
  setOutput = (on: boolean) => this.#run('set_output', { on })
  rescan = () => this.#run('rescan')

  async #run(cmd: string, args?: Record<string, unknown>): Promise<boolean> {
    try {
      await invoke(cmd, args)
      return true
    } catch (e) {
      this.error = String(e)
      clearTimeout(this.#errorTimer)
      this.#errorTimer = setTimeout(() => (this.error = null), ERROR_MS)
      return false
    }
  }
}

export const psu = new Psu()
