---
experimental: true
---
[← braid](../index.md)

# braid ups status

{{#include ../_includes/experimental-command-callout.md.inc}}

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
Status: OL  [ok] on utility power
Battery: 100%
Runtime: 30m 0s
Load: 17% (56 W estimated)
Input: 120.0 V (transfer 88-142 V)
Device: APC Back-UPS ES 550G
Battery manufactured: 2023/04/12
Last test: Done and passed
```

The human `Status:` line preserves the raw NUT tokens and adds a severity tag:
`[ok]`, `[warn]`, `[fail]`, or `[skip]`. Tags are colored on an interactive TTY
and plain under `NO_COLOR` or when output is piped.

## JSON output

```sh
sudo braid ups status --json | jq .
```

Emits the serialized `UpscOutput` model. A success body (no top-level
`error`) means the `upsc` query succeeded. The body is braid's parse of
that output plus braid's `severity` verdict. The typed fields are
braid's reading of the keys it knows, not copies of the raw lines, and
`extra` holds the other keys verbatim. So the body is **not** a full
copy of what `upsc` printed.

`severity` is braid's judgment of the UPS state, from the same
classifier braid's own mutation preflight uses (see
[the UPS guide](../guides/ups.md#mutation-refusal-when-utility-power-is-not-verified)).
Only `"severity": "online"` proves utility power. Any other value, or a
missing `severity` key (error bodies), means utility power is not
proven. Treat a value you do not recognize the same way, so a value
added later fails closed. The values are:

| `severity` | Meaning |
| --- | --- |
| `online` | On utility power with no blocking flag; preflight lets mutations start |
| `on_battery` | On battery, not yet critical |
| `critical` | A critical state, such as low battery or lost contact with the UPS |
| `indeterminate` | `ups.status` has flags, but none proves utility power |
| `status_missing` | `ups.status` is empty or missing, for example while the driver is still starting |

To learn which flag fired -- an advisory flag such as `RB`, or
`OL TESTFAIL` versus `OB LB` -- read `status_flags`.

Shape:

```json
{
  "severity": "online",
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
no field is ever omitted, and `severity` is always present and never `null`.
Every typed field is always present: a scalar the driver did not report
serializes as `null` rather than being omitted, and the `battery`, `input`, and
`device` objects are always present even when all of their fields are `null`.
Test typed fields for a `null` value, not for a missing key -- a `has(...)`
check on any typed key always returns true. `status_flags` and `extra` are
always present but never `null` (`[]` and `{}` when empty). Error bodies are
the exception -- they carry `error`/`detail` and none of the typed keys or
`severity`, so a script must confirm there is no top-level `error` before
relying on the rule above.

`status_flags` lists flags in first-seen `ups.status` token order (whitespace normalized, duplicate tokens dropped); braid does not sort them, so the order is deterministic for a given UPS state.

`extra` is a string-keyed map of every `upsc` line whose key braid does not parse into a typed field above. Its contents vary with the NUT driver and version (typically `driver.*` debug keys plus other untyped keys like `battery.charge.low` or `input.voltage.nominal`), and values are kept verbatim as strings.

A key braid parses never appears in `extra`, so `--json` does not keep every raw value. When braid rejects a typed key's value (out of range, or not a number), the typed field is `null` and the raw value is not kept. A `ups.*` device-identity key such as `ups.model` is dropped when its `device.*` twin has a value, because `device` prefers the `device.*` spelling. To see exactly what the driver sent, run `upsc <name>` or open the Browse tab's NUT > Variables view in the [TUI](tui.md).

Distinct sentinels cover the common non-OK cases:

| Condition | JSON | Exit code |
| --- | --- | --- |
| UPS reachable | serialized `UpscOutput` plus `severity` (see above) | 0 |
| UPS query failed | `{"error": "query_failed", "detail": "exit <code>: <stderr>"}`. If stderr is blank, `detail` is just `exit <code>`. | 1 |
| UPS invocation failed (upsc could not run -- missing on PATH, killed by signal, or other runner-level failure) | `{"error": "invocation_failed", "detail": "command failed: upsc ups: <reason>"}`. When upsc is missing, `detail` ends with ` -- is braid.packages.nut on PATH?`. Other failures, such as a signal kill, get no hint. | 1 |
| UPS not enabled | `{"error": "ups_not_enabled"}` | 0 |

If `error` is present, the body carries no UPS state. For these cases,
`--json` writes only to stdout -- stderr stays silent so the JSON
sentinel can be piped into a single sink (`jq`, `tee`, CI logs) without
a redundant human error line. Other failure modes, such as malformed
config, still print a human error to stderr.

If braid cannot write its output to stdout, for example on a full disk
under a redirect or after the reader closed the pipe, it prints a human
error to stderr and exits 1, in both modes and on every row above,
including the rows that exit 0. The body may be missing or cut short, so
exit 0 always means stdout got the whole body.

## Flags

| Flag | Effect |
| --- | --- |
| `--json` | Emit parsed `upsc` model and braid's `severity` verdict as JSON; stable shape for scripts |

## Related

- [UPS guide](../guides/ups.md) -- shutdown path, preflight refusal, v1 limitations
- [tui](tui.md) -- the TUI's Data tab shows the same live UPS state
- [doctor](doctor.md) -- UPS-adjacent configuration checks
