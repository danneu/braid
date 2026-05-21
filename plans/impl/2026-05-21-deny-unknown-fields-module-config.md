# Plan: enforce `deny_unknown_fields` on module-generated config JSON contracts (`NotifierConfig`, `FanControl`, `Pwm`)

## Scope

This plan covers **module-generated config JSON contracts** only -- files the NixOS module writes during activation and the CLI reads as input. Persisted runtime state managed by the CLI itself (e.g. `alert-latch.json`, `acked-stats.json` and their types in `cli/src/alert.rs`) is out of scope: those have a different change-management story (long-lived on-disk state, legacy-reload constraints; see `alert.rs:647-666`) and any tightening there needs its own analysis.

## Context

`NotifierConfig` at `cli/src/doctor.rs:15-18` is the Rust side of a bidirectional, braid-owned config contract -- `modules/braid/monitor.nix:82-84` writes `/etc/braid/notifier-config.json` and `doctor`'s `check_beep_path` reads it. The struct's doc comment claims:

> deserialize errors here are loud (Fail), so a stale parser cannot silently degrade.

But the struct derives only `Deserialize`. Without `#[serde(deny_unknown_fields)]`, an unknown field added to `monitor.nix` (e.g. a future `webhook_url` or `email_to`) would be silently dropped -- exactly the silent degradation the comment forbids.

Looking sideways at the other module-generated config types surfaces the same gap. `RawConfig` (`cli/src/config.rs:107`) is the top-level `/etc/braid/config.json` shape and has `deny_unknown_fields`, but per serde's container-attrs docs (https://serde.rs/container-attrs.html) the attribute does **not** propagate into nested struct types. `FanControl` (`cli/src/config.rs:24`) and `Pwm` (`cli/src/config.rs:16`) are braid-owned -- written by `modules/braid/cli.nix:20-29` (`fan_control`, nested `pwm` object) -- and carry no `deny_unknown_fields` of their own. An unknown sibling field inside the `fan_control` or `pwm` JSON object today silently parses, which is the same silent-skew failure mode as `NotifierConfig`. Among the module-generated config structs, `Ups` (`cli/src/config.rs:32`) is the only nested type that already carries the attribute; closing `NotifierConfig`, `FanControl`, and `Pwm` brings this surface to a uniform strict-schema posture.

The two `deny_unknown_fields` opt-outs in the codebase are deliberate and live in different categories from module-generated config: `parse/btrfs_device_stats.rs:19` keeps forward-compat with upstream btrfs-progs output (external-tool output), and `alert.rs:647-666` lets a legacy on-disk latch load post-refactor (CLI-managed persisted runtime state). Module-generated configs are neither -- they are regenerated on every nix activation, ship in lockstep with the CLI binary, and `AGENTS.md` explicitly forbids migration paths.

This fix closes the gap on all three structs and adds the missing tests that pin each invariant, so a future "helpful" refactor cannot quietly re-loosen any of these schemas.

## Changes

### 1. `cli/src/doctor.rs:15-18` -- tighten `NotifierConfig`

Add `#[serde(deny_unknown_fields)]`. While here, fold the new invariant into the existing doc comment so a future reader does not need to look at the attribute to recover it. The rewritten comment uses `--` per the project comment style; no other em-dashes in `cli/src/doctor.rs` are touched (the file has many pre-existing em-dashes in unrelated comments, and a file-wide sweep is out of scope here).

```rust
/// Schema of `/etc/braid/notifier-config.json`. Tracked in lockstep with the
/// `builtins.toJSON` writer in `modules/braid/monitor.nix`. A schema change
/// must update both sides -- deserialize errors here (including unknown
/// fields) are loud (Fail), so a stale parser cannot silently degrade.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifierConfig {
    beep_probe_path: Option<String>,
}
```

### 2. `cli/src/doctor.rs` (tests module, near line 3876) -- add invariant test

Add `beep_path_fail_on_unknown_field` immediately after `beep_path_fail_on_malformed_config`. It is a near-clone with one input difference -- a known-good `beep_probe_path` plus an unknown sibling field -- and the same `Fail` + `"malformed"` assertion (the existing arm at `doctor.rs:1100` formats unknown-field rejections through the same "malformed" message).

Reuse the existing test scaffolding from `cli/src/test_fixtures/doctor.rs`: `isolated_paths`, `write_temp`, `beep_ctx`, `MockRunner`. No new helpers.

Preamble per `AGENTS.md` "Test Conventions":

- **Intent:** an unknown field in `notifier-config.json` produces Fail via the existing "malformed" arm, not silent Skip / Ok.
- **Why:** `NotifierConfig`'s doc-comment promises stale parsers cannot silently degrade. Without this test, a refactor that drops `#[serde(deny_unknown_fields)]` would not surface until the module side actually added a field, by which point the silent skew is already in production.
- **Scenario:** a future `modules/braid/monitor.nix` adds a `webhook_url` field to `notifier-config.json` against a CLI binary that predates the addition.

### 3. `cli/src/config.rs:16,24` -- tighten `Pwm` and `FanControl`

Add `#[serde(deny_unknown_fields)]` to both nested structs. `RawConfig`'s container-level attribute (line 107) does not propagate into nested types, so `FanControl` and `Pwm` must carry their own. Add a one-line doc comment per `AGENTS.md` "Doc Comments" rule justifying the boundary -- e.g.:

```rust
/// braid-owned config schema written by `modules/braid/cli.nix`; nested under
/// `RawConfig` whose `deny_unknown_fields` does not propagate, so this struct
/// must enforce the same bidirectional contract independently.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pwm { ... }
```

The doc comment on `FanControl` follows the same pattern.

### 4. `cli/src/config.rs` (tests module, near line 369) -- add nested invariant tests

Add two tests immediately after `rejects_malformed_pwm`, following the same parse-and-`expect_err` pattern as that test:

- `rejects_unknown_field_in_fan_control` -- valid `fan_control` body plus one unknown sibling key (e.g. `"future_key": 1`). Expect deserialize error mentioning the unknown field.
- `rejects_unknown_field_in_pwm` -- valid `pwm` object with all known fields plus one unknown sibling key. Expect deserialize error mentioning the unknown field.

Preamble per `AGENTS.md` "Test Conventions": Intent / Why / Scenario, with Why pinning that the parent `RawConfig`'s `deny_unknown_fields` does not propagate, so this test guards the nested boundary explicitly.

## Out of scope

- Adding any JSON fixture file -- all test inputs are inline string literals.
- Touching `Ups` -- already carries `deny_unknown_fields`.
- Auditing or tightening the CLI's persisted runtime-state types in `cli/src/alert.rs` (`AlertState`, `AlertCause`, `AckedStats`, `AckedDisk`, `AckedDeviceCounters`) or anything else outside the module-generated config surface. Some of those deliberately omit `deny_unknown_fields` (latch legacy-reload, `alert.rs:647-666`); the rest need their own scoping pass.
- Tightening the documented opt-outs in `parse/btrfs_device_stats.rs` and `alert.rs` -- their rationale is on the record and still correct.
- Sweeping pre-existing em-dashes in `cli/src/doctor.rs` (or elsewhere) beyond the one comment being rewritten here.
- Backward-compatibility shims -- the project forbids them.

## Verification

1. `just test-rust` -- runs the full Rust unit-test suite, including the existing `beep_path_*` and config tests plus the new `beep_path_fail_on_unknown_field`, `rejects_unknown_field_in_fan_control`, and `rejects_unknown_field_in_pwm`. Expected: all green; each new test fails without its corresponding `deny_unknown_fields` attribute and passes with it (sanity-check by temporarily removing each attribute and re-running).
2. No VM test changes -- this is a Rust-only typing change. The CLI parser canary (`just test-parsers`) is not affected because it exercises parsers against live tool output, not braid-owned config files.
