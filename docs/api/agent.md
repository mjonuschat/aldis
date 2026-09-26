# `aldis agent` API (`api_version` 1)

`aldis agent` runs as a Moonraker agent extension named `aldis`. It exposes MCU firmware status
and updates over Moonraker's websocket API so web frontends (Fluidd, Mainsail, ...) can show and
trigger updates without shelling out to the CLI.

Clients detect aldis by finding the agent `aldis` in `server.extensions.list`, then checking
`status.api_version` for compatibility.

## Envelopes

A request from a frontend to Moonraker, relayed to the agent:

```jsonc
{"jsonrpc": "2.0", "id": 42, "method": "server.extensions.request",
 "params": {"agent": "aldis", "method": "update", "arguments": {"mcus": ["mcu", "can"]}}}
```

Moonraker relays the agent's result (or error) back as the response to that request. `status`
passes `"arguments": null`.

An event pushed from Moonraker to a connected frontend:

```jsonc
{"jsonrpc": "2.0", "method": "notify_agent_event",
 "params": [{"agent": "aldis", "event": "update_response", "data": { /* payload below */ }}]}
```

Names in requests (e.g. `"mcus": ["mcu", "can"]`) are the same display names `status` reports,
matched case-insensitively.

## `status`

No arguments; read-only.

```jsonc
{
  "api_version": 1,
  "host": {
    "klippy_state": "ready",            // ready | startup | error | shutdown | disconnected
    "klippy_message": "Printer is ready",
    "software_version": "v0.13.0-770-gce7002bed",
    "klipper_path": "/home/pi/klipper",
    "checkout_version": "v0.13.0-770-gce7002bed"
  },
  "blocker": null,                      // or {"reason", "message"}
  "mcus": [
    {"name": "mcu", "transport": {"type": "can", "interface": "can0", "uuid": "e7819ed8e7d3"},
     "running_version": "v0.13.0-770-gce7002bed", "state": "current",
     "message": "running v0.13.0-770-gce7002bed", "actions": []},
    {"name": "can", "transport": {"type": "can", "interface": "can0", "uuid": "e7819ed8e7d3"},
     "running_version": "v0.13.0-753-g8c29c0a8e", "state": "update_available",
     "message": "running v0.13.0-753-g8c29c0a8e, host is v0.13.0-770-gce7002bed",
     "actions": ["update"]},
    {"name": "xiao", "transport": null, "running_version": null, "state": "not_responding",
     "message": "not responding; run `aldis reboot` or power-cycle the board", "actions": []}
  ],
  "run": null                           // current or last run
}
```

`transport` is `null` for an MCU aldis has no configured host transport for. `running_version` is
`null` when the running firmware version is unknown (e.g. an unreported MCU).

When Klippy is `disconnected`, `host` carries only `klippy_state` and `klippy_message` (the other
fields are always omitted, not `null`), and `mcus` is empty. When Klippy is `startup`, `mcus` is
typically still empty (MCU discovery only succeeds once Klippy is `ready`), but the other `host`
fields may still be present if Moonraker already reports them.

### Blocker reasons

A blocker disables every MCU's `actions`. Reasons are listed in the order they are checked (the
first one that applies wins):

| Reason | Condition |
| --- | --- |
| `non_local_moonraker` | The `--moonraker` host is not loopback |
| `unsupported_instance` | Moonraker's paired Klipper unit is not `klipper` |
| `config_error` | Klippy could not load its config |
| `klippy_unavailable` | Klippy is `startup` or disconnected (including Klipper stopped) |
| `restart_pending` | The checkout at `klipper_path` does not match `software_version` (`revisions_match`) |
| `printing` | `print_stats` is not idle, or idleness could not be confirmed (not checked in `error`/`shutdown`) |

### MCU states

| State | Meaning | Actions |
| --- | --- | --- |
| `current` | Matches the running host | none |
| `update_available` | Differs from the running host | `update` |
| `indeterminate` | Revision cannot be compared | none |
| `externally_managed` | Non-Klipper application (e.g. Beacon) | none |
| `unsupported_legacy` | No embedded kconfig | none |
| `not_identified` | Empty object; fallback not attempted (CAN, host MCU, or port held) | none |
| `not_responding` | Empty object; no identify reply. Message points to `aldis reboot` or a power cycle | none |

The UI never re-implements eligibility: buttons follow `actions`, and "Update All" targets every
MCU whose actions include `update`. Force-updating a `current` MCU remains CLI-only.

### `run`

```jsonc
{"run_id": "…", "state": "running",   // running | finished
 "messages": [/* update_response payloads */], "result": null}
```

The agent keeps only the current or most recent run in memory, with `messages` capped at the
latest 500 payloads (the final payload is always kept). This history is lost when the agent
restarts; the run log on disk remains. It lets a reloaded page, or a client that reconnects
mid-run, repopulate its progress dialog from `status` alone.

## `update`

Arguments: `{"mcus": ["mcu", "can"]}` or `{"all": true}`. The request is validated synchronously,
then the call returns `{"run_id": "…"}` and the run continues in the background; progress and
completion are reported through `update_response` events (see below) and through `status.run`.

A request is accepted or rejected whole; there are no partial acceptances.

### Errors

The agent rejects with a JSON-RPC error `{"code": …, "message": "…", "data": {"reason": …}}`. Most
rejections (everything but the two protocol-level reasons below) use `code` -32000:

```jsonc
{"jsonrpc": "2.0", "id": 42, "error": {"code": -32000, "message": "update already running",
  "data": {"reason": "busy"}}}
```

Moonraker (verified against v0.11.0's `common.py`) relays agent errors wrapped: the frontend
receives `code` 424, `message` "Agent aldis RPC error", and the agent's whole error object as
`data`. The reason is therefore at `error.data.data.reason`:

```jsonc
{"jsonrpc": "2.0", "id": 42, "error": {"code": 424, "message": "Agent aldis RPC error",
  "data": {"code": -32000, "message": "update already running",
           "data": {"reason": "busy"}}}}
```

| Reason | Code | Meaning |
| --- | --- | --- |
| `busy` | -32000 | An update is running (agent or CLI) |
| `blocked` | -32000 | A blocker applies; `data.blocker` carries it |
| `unknown_mcu` | -32000 | A named MCU was not discovered |
| `not_updatable` | -32000 | A named MCU lacks the `update` action; `data.mcus` gives per-MCU reasons |
| `nothing_to_update` | -32000 | `mcus` was empty, or `all` matched no MCU |
| `unavailable` | -32000 | Moonraker discovery failed |
| `invalid_request` | -32602 | `arguments` deserializes to neither `{"mcus": [...]}` nor `{"all": ...}`, or is `{"all": false}` |
| `unknown_method` | -32601 | `method` is neither `status` nor `update` |

```jsonc
{"jsonrpc": "2.0", "id": 42, "error": {"code": -32000, "message": "the printer is busy; update when it is idle",
  "data": {"reason": "blocked", "blocker": {"reason": "printing", "message": "the printer is busy; update when it is idle"}}}}
{"jsonrpc": "2.0", "id": 42, "error": {"code": -32000, "message": "no MCU named \"typo\"",
  "data": {"reason": "unknown_mcu", "mcu": "typo"}}}
{"jsonrpc": "2.0", "id": 42, "error": {"code": -32000, "message": "some requested MCUs cannot be updated",
  "data": {"reason": "not_updatable",
           "mcus": [{"name": "mcu", "state": "current", "message": "running v0.13.0-770-gce7002bed"}]}}}
```

`unknown_method` is a protocol-level error: it's raised by dispatch before `update`'s arguments are
even looked at, so it precedes any lock or blocker check. `invalid_request` has two sources: a
malformed `arguments` shape is also protocol-level (raised while parsing, before `update` runs),
but `{"all": false}` is only recognized as invalid once `update` runs, so it is raised *after* the
busy and blocker checks (request validation needs the discovered MCU inventory to interpret the
request at all). Both use the standard JSON-RPC code for malformed params.

## Events

Event name `update_response`. The payload mirrors Moonraker's own `notify_update_response` so
Fluidd can feed it into its existing `UpdatingDialog`:

```jsonc
{"run_id": "…", "mcu": "can", "phase": "flash", "message": "flashing firmware", "complete": false}
{"run_id": "…", "mcu": "can", "phase": "flash", "message": "wrote 49152 bytes", "complete": false}
{"run_id": "…", "mcu": null, "phase": "done", "message": "2 updated; log: …", "complete": true,
 "result": {"outcome": "success",       // success | failed
            "klippy_state": "ready",
            "klipper_left_stopped": false,
            "mcus": [{"name": "can", "outcome": "updated", "message": "…"}]}}   // updated | failed | not_attempted
```

`mcu` is `null` for host-level messages (e.g. `stop-klipper`, `start-klipper`, `done`). `result` is
present only on the final, `complete: true` payload.

Phases are the stable labels from `update-lifecycle.md`: `discover`, `stop-klipper`, `build`,
`enter-bootloader`, `flash`, `verify`, `start-klipper`, `reconnect`, `done`. Messages are one line
per step; raw build output goes only to the run log.

`klipper_left_stopped: true` means the run deliberately did not restart Klipper (see
`update-lifecycle.md`'s Error Handling); the final message says so and names the MCU to recover.

## Versioning

`api_version` is an integer bumped only on breaking changes. New fields, states, reasons, and
actions are additive. Clients must ignore unknown fields and treat unknown states as having no
actions.
