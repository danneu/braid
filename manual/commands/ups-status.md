[← Manual](../index.md)

# braid ups status

Query the UPS (NUT) daemon for the currently configured UPS and render
a curated human summary or the serialized parsed model as JSON.

Requires `braid.ups.enable = true`. With UPS disabled the command
prints an enable hint and exits 0 (not an error).

## Basic example

```sh
sudo braid ups status
```

Output:

```
UPS: ups
Status: OL
Battery: 100%
Runtime: 30:00
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

Emits the serialized `UpscOutput` model. Shape:

```json
{
  "status_flags": ["OL"],
  "battery": {
    "charge_pct": 100,
    "runtime_secs": 1800,
    "voltage": "27.0",
    "type_": "PbAc",
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
    "type_": "ups"
  },
  "extra": { "driver.name": "usbhid-ups" }
}
```

Distinct error sentinels cover the common non-OK cases:

| Condition | JSON | Exit code |
| --- | --- | --- |
| UPS reachable | serialized `UpscOutput` | 0 |
| UPS unreachable | `{"error": "daemon_down"}` | 1 |
| UPS not enabled | `{"error": "ups_not_enabled"}` | 0 |

## Flags

| Flag | Effect |
| --- | --- |
| `--json` | Emit parsed `upsc` model as JSON; stable shape for scripts |

## Related

- [UPS guide](../guides/ups.md) -- shutdown path, preflight refusal, v1 limitations
- [tui](tui.md) -- the TUI's Data tab shows the same live UPS state
- [doctor](doctor.md) -- UPS-adjacent configuration checks
