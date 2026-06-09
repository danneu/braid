---
experimental: true
---
[← braid](../index.md)

# braid ups status

Query the UPS (NUT) daemon for the currently configured UPS and render
a curated human summary or the serialized parsed model as JSON.

Requires UPS support enabled (`braid.enable = true` and
`braid.ups.enable = true`). With UPS disabled the command prints an enable
hint and exits 0 (not an error).

## Basic example

```sh
sudo braid ups status
```

Output:

```
UPS: ups
Status: OL
Battery: 100%
Runtime: 30m 0s
Load: 17% (56 W estimated)
Input: 120.0 V (transfer 88-142 V)
Device: APC Back-UPS ES 550G
Battery manufactured: 2023/04/12
Last test: Done and passed
```

## JSON output

```sh
sudo braid ups status --json | jq .
```

Emits the serialized `UpscOutput` model. A success body (no top-level
`error`) is **trustworthy telemetry**: braid faithfully serialized
whatever `upsc` reported. It is **not** a claim that the UPS is online --
on-battery (`OB`), low-battery (`OB LB`), and all-unrecognized status sets
are all success bodies with no `error` and no `warning`.

To judge UPS state, read `status_flags`: utility power is proven only by
the presence of `OL` with no blocking flag (`OB`, `LB`, `TESTFAIL`,
`COMMBAD`, `FSD`) -- the same affirmative-`OL` criterion braid's own
mutation preflight uses (see
[the UPS guide](../guides/ups.md#mutation-refusal-when-utility-power-is-not-verified)).

Shape:

```json
{
  "status_flags": ["OL"],
  "battery": {
    "charge_pct": 100,
    "runtime_secs": 1800,
    "voltage": "27.0",
    "type": "PbAc",
    "mfr_date": "2023/04/12",
    "runtime_low_secs": 120
  },
  "load_pct": 17,
  "realpower_nominal_watts": 330,
  "input": {
    "voltage": "120.0",
    "transfer_low": "88",
    "transfer_high": "142",
    "sensitivity": "medium"
  },
  "test_result": "Done and passed",
  "device": {
    "model": "Back-UPS ES 550G",
    "mfr": "APC",
    "serial": "3B1234X56789",
    "type": "ups"
  },
  "extra": { "driver.name": "usbhid-ups", "battery.charge.low": "10" }
}
```

In a success body (the shape above -- a reachable UPS, no top-level `error`),
every typed field is always present: a scalar the driver did not report
serializes as `null` rather than being omitted, and the `battery`, `input`, and
`device` objects are always present even when all of their fields are `null`.
Test typed fields for a `null` value, not for a missing key -- a `has(...)`
check on any typed key always returns true. `status_flags` and `extra` are
always present but never `null` (`[]` and `{}` when empty). The only field
omitted when absent is the top-level `warning` (see the table below). Error
bodies are the exception -- they carry `error`/`detail` and none of the typed
keys, so a script must confirm there is no top-level `error` before relying on
the rule above.

`status_flags` lists flags in first-seen `ups.status` token order (whitespace normalized, duplicate tokens dropped); braid does not sort them, so the order is deterministic for a given UPS state.

`extra` is a string-keyed map of every `upsc` line that did not land in a typed field above. Its contents vary with the NUT driver and version (typically `driver.*` debug keys plus other untyped fields like `battery.charge.low` or `input.voltage.nominal`), and values are kept verbatim as strings.

Distinct sentinels cover the common non-OK cases:

| Condition | JSON | Exit code |
| --- | --- | --- |
| UPS reachable with populated `ups.status` | serialized `UpscOutput` | 0 |
| UPS reachable but `ups.status` empty | serialized `UpscOutput` plus `"warning": "ups_status_empty"` | 0 |
| UPS query failed | `{"error": "query_failed", "detail": "exit <code>: <stderr>"}` | 1 |
| UPS invocation failed (upsc could not run -- missing on PATH, killed by signal, or other runner-level failure) | `{"error": "invocation_failed", "detail": "command failed: upsc ups: <reason>"}` | 1 |
| UPS not enabled | `{"error": "ups_not_enabled"}` | 0 |

If `error` or `warning` is present, do not treat the typed body as
healthy UPS state. For these cases, `--json` writes only to stdout --
stderr stays silent so the JSON sentinel can be piped into a single
sink (`jq`, `tee`, CI logs) without a redundant human error line.
Other failure modes, such as malformed config, still print a human
error to stderr.

The converse does not hold: the absence of `error` and `warning` does
not by itself mean the UPS is online -- inspect `status_flags` as above.
`ups_status_empty` fires only when `ups.status` is empty or missing (no
flags to read), so it is not a general health signal.

## Flags

| Flag | Effect |
| --- | --- |
| `--json` | Emit parsed `upsc` model as JSON; stable shape for scripts |

## Related

- [UPS guide](../guides/ups.md) -- shutdown path, preflight refusal, v1 limitations
- [tui](tui.md) -- the TUI's Data tab shows the same live UPS state
- [doctor](doctor.md) -- UPS-adjacent configuration checks
