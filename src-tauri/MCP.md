# Driving the bench from an agent (MCP)

While SteadyState is running, it serves an MCP server at:

```
http://127.0.0.1:47310/mcp
```

The **MCP** badge in the app's footer shows the address; click it to copy. The badge turns
blue while an agent is acting, and each action appears in the footer as it happens.

## Allowing changes: "Agentic power configuration"

The app has an **Agentic power configuration** switch below the Output button. It is **off
every time the app starts**. It only governs voltage and current: seeing the supply and
switching its output are always available, so an agent can power the device under test on,
off or through a reset as part of a larger task.

| | Off | On |
|---|---|---|
| `get_status`, `rescan` | yes | yes |
| `set_output`, `power_cycle` | yes | yes |
| `set_voltage`, `set_current`, `apply` | not offered | yes |

While off, the setpoint tools are left out of the tool list entirely. An agent that calls
one anyway (from an older list) is refused and told to ask you, and the footer shows
"Agent blocked: …". Flipping the switch sends connected agents `tools/list_changed`, so
clients that support it update their tools straight away.

Only you can flip it; there is no tool for it. `get_status` reports it as
`setpoint_tools_enabled`.

## Connecting an agent

**Claude Code**

```sh
claude mcp add --transport http steadystate http://127.0.0.1:47310/mcp
```

or in a project's `.mcp.json`:

```json
{
  "mcpServers": {
    "steadystate": { "type": "http", "url": "http://127.0.0.1:47310/mcp" }
  }
}
```

**Clients that only launch stdio servers** (e.g. Claude Desktop's config file) can use a
bridge such as [`mcp-remote`](https://www.npmjs.com/package/mcp-remote):

```json
{
  "mcpServers": {
    "steadystate": { "command": "npx", "args": ["mcp-remote", "http://127.0.0.1:47310/mcp"] }
  }
}
```

## Tools

| Tool | What it does |
|---|---|
| `get_status` | Connection state, connected supply and its limits, live readings, whether setpoint tools are on. Read-only. |
| `set_output` | Output on/off, at the current setpoints. |
| `power_cycle` | Output off for `off_seconds` (default 2, 0.1–60), then back on. Only while the output is on. |
| `rescan` | Look for supplies again while disconnected. |
| `set_voltage` | Voltage setpoint (V). Switch on only. |
| `set_current` | Current limit (A). Switch on only. |
| `apply` | Voltage and current limit together; both are checked before either is sent. Switch on only. |

Results are structured JSON (with a text copy), and control tools return the updated status.
Out-of-range values are refused as tool errors without touching the supply.

## What agents can't do

- **Change voltage or current while the switch is off** (see above), or flip the switch
  themselves.
- **Pick which supply to connect to.** When the app finds an unfamiliar supply, the person at
  the bench confirms it in the app; `get_status` reports `"choosing"` until they do.
- **Reach it from another machine or a web page.** It listens on loopback only, accepts only
  loopback `Host` names, and refuses any request carrying a browser `Origin` header (so
  browser-based tools such as the MCP Inspector web UI are refused too).

## Settings

| `STEADYSTATE_MCP_PORT` | Effect |
|---|---|
| unset | Listen on 47310 |
| a port number | Listen on that port |
| `off` | No MCP server |

If the port is taken (for example by a second instance), the app runs normally and the badge
shows the error, struck through.
