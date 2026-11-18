// Frontend mirror of the Rust link worker (src-tauri/src/link.rs). The backend is the
// source of truth: setters only send commands, and the UI updates when the next status
// event reports what the device actually did.

import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

export type KnownDevice = { port: string; driver: string; model: string }
export type Limits = { voltage: number; current: number; power: number }
/** A supply found by a scan (Rust `psu_core::drivers::Detected`). */
export type Detected = { port: string; info: { driver: string; model: string; limits: Limits } }

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
  | { kind: 'choosing'; found: Detected[]; suggested: number; last: KnownDevice | null }
  | { kind: 'disconnected'; last: KnownDevice | null; reason: string }

type Snapshot = { link: LinkState; status: Status | null }

/** The MCP server agents connect to (src-tauri/src/mcp.rs). `port` is the configured one. */
export type McpStatus =
  | { kind: 'listening'; url: string; port: number }
  | { kind: 'failed'; error: string; port: number }

const ERROR_MS = 5000
const AGENT_NOTE_MS = 6000

class Psu {
  link = $state<LinkState>({ kind: 'searching', last: null })
  status = $state<Status | null>(null)
  error = $state<string | null>(null)
  /** The latest action an agent took over MCP, shown briefly in the footer. */
  agentNote = $state<string | null>(null)
  mcp = $state<McpStatus | null>(null)
  /** "Agentic power configuration": may agents change setpoints and the output? */
  agentControl = $state(false)

  #errorTimer: ReturnType<typeof setTimeout> | undefined
  #agentTimer: ReturnType<typeof setTimeout> | undefined

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
    await listen<string>('psu://agent', (e) => {
      this.agentNote = e.payload
      clearTimeout(this.#agentTimer)
      this.#agentTimer = setTimeout(() => (this.agentNote = null), AGENT_NOTE_MS)
    })
    this.mcp = await invoke<McpStatus>('mcp_info')
    this.agentControl = await invoke<boolean>('agent_control')
    const snap = await invoke<Snapshot>('snapshot')
    if (!sawLink) this.link = snap.link
    if (!sawStatus) this.status = snap.status
  }

  setVoltage = (volts: number) => this.#run('set_voltage', { volts })
  setCurrent = (amps: number) => this.#run('set_current', { amps })
  setOutput = (on: boolean) => this.#run('set_output', { on })
  rescan = () => this.#run('rescan')
  pick = (d: Detected) => this.#run('pick_device', { port: d.port, driver: d.info.driver })
  dismissChoice = () => this.#run('dismiss_choice')

  /** Moves the MCP server to `port`; returns the error, if any (the server then stays put). */
  async setMcpPort(port: number): Promise<string | null> {
    try {
      this.mcp = await invoke<McpStatus>('set_mcp_port', { port })
      return null
    } catch (e) {
      return String(e)
    }
  }

  async setAgentControl(on: boolean) {
    try {
      this.agentControl = await invoke<boolean>('set_agent_control', { on })
    } catch (e) {
      this.#fail(e)
    }
  }

  async #run(cmd: string, args?: Record<string, unknown>): Promise<boolean> {
    try {
      await invoke(cmd, args)
      return true
    } catch (e) {
      this.#fail(e)
      return false
    }
  }

  #fail(e: unknown) {
    this.error = String(e)
    clearTimeout(this.#errorTimer)
    this.#errorTimer = setTimeout(() => (this.error = null), ERROR_MS)
  }
}

export const psu = new Psu()
