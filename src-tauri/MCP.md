# Driving the bench from an agent (MCP)

While SteadyState is running, it serves an MCP server at:

```
http://127.0.0.1:47310/mcp
```

The **MCP** badge in the app's footer shows the address; click it to copy. The badge turns
blue while an agent is acting, and each action appears in the footer as it happens.

## Allowing changes: "Agentic power configuration"

The app has an **Agentic power configuration** switch below the Output button. It is **off
every time the app starts**:

- **Off:** agents can call `get_status` and `rescan`; `set_voltage`, `set_current`, `apply`
  and `set_output` are refused (the agent is told to ask you), and the footer shows
  "Agent blocked: …".
- **On:** agents can change setpoints and switch the output.

Only you can flip it; there is no tool for it. `get_status` reports it as
`agent_control_enabled`.

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
| `get_status` | Connection state, connected supply and its limits, live readings, whether changes are allowed. Read-only. |
| `set_voltage` | Voltage setpoint (V). |
| `set_current` | Current limit (A). |
| `apply` | Voltage and current limit together; both are checked before either is sent. |
| `set_output` | Output on/off. |
| `rescan` | Look for supplies again while disconnected. |

Results are structured JSON (with a text copy), and control tools return the updated status.
Out-of-range values are refused as tool errors without touching the supply.

## What agents can't do

- **Change anything while the switch is off** (see above), or flip the switch themselves.
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
