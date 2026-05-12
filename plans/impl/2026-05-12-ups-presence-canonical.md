# Plan: drop `Ups.enable`, make presence canonical

## Context

The TUI has two near-mirror helpers, `fan_probe_effect` (`cli/src/tui/app.rs:36-44`) and `ups_probe_effect` (`cli/src/tui/app.rs:46-56`), that build a probe effect from a config block. The fan helper treats `Some(fc)` as "live"; the UPS helper additionally checks `u.enable`. The same divergence is duplicated inline in `Model::new` (`cli/src/tui/model.rs:276, 288-291`), in the layout decision (`cli/src/tui/view/mod.rs:942-943`), and in `doctor` (`cli/src/doctor.rs:635, 702`). The `ups status` short-circuit (`cli/src/ups.rs:121`) checks `.enable` too.

The diverging helpers are a symptom. The root cause is asymmetry in the two config types: `FanControl` (`cli/src/config.rs:24-29`) has no `enable` field, while `Ups` (`cli/src/config.rs:32-35`) carries `pub enable: bool`. The NixOS module already gates the entire JSON block: `modules/braid/cli.nix:30` emits `ups = { enable = cfg.ups.enable; ... }` only inside `lib.optionalAttrs cfg.ups.enable`, so in Nix-emitted configs `Ups.enable` is always `true` when the block is present. The field is structurally redundant.

There is also a pre-existing latent inconsistency the asymmetry hides: the preflight call sites (`add.rs:1287`, `remove.rs:421`, `replace.rs:887`, `remove_missing.rs:343`) pass `config.ups().map(|u| u.name.as_str())` and never check `.enable`. So today two interpretations of "UPS is live" coexist (`Some(u)` vs `Some(u) && u.enable`), masked only by the Nix gate ensuring `enable=false` is unreachable in production.

This change removes the asymmetry at the type level: drop `Ups.enable`. Presence of the `ups` block becomes the canonical "subsystem is live" signal, matching `FanControl` exactly. The two helpers collapse to identical shape, the preflight-vs-doctor divergence disappears, and the reviewer's hypothetical `is_enabled()` abstraction becomes unnecessary.

## Changes

### Config and Nix schema

- `cli/src/config.rs:32-35` -- delete `enable: bool` field from `Ups`. The struct now holds only `name: String`. **Add `#[serde(deny_unknown_fields)]` to the `Ups` struct.** Without strict rejection, a stale or hand-edited `{"ups":{"enable":false,"name":"ups"}}` would silently parse to `Ups { name: "ups" }` and flip from "skipped" to "live UPS" -- the exact safety regression the pivot is supposed to dissolve. The deny attribute makes the legacy-shape config fail loudly at parse time so the operator notices.
- `cli/src/config.rs:217-226` -- `parses_config_with_ups` test: drop `"enable": true` from the JSON literal and drop the `assert!(u.enable)` assertion. Keep `assert_eq!(u.name, "ups")`.
- `cli/src/config.rs` -- **add a new test** `rejects_config_with_legacy_ups_enable_field` that feeds `{"mount_point":"/mnt/storage","ups":{"enable":true,"name":"ups"}}` to `serde_json::from_str::<Config>` and asserts an error containing `enable` or `unknown field`. Pins the deny-unknown-fields contract so a future serde-attribute regression cannot quietly resurrect the legacy shape.
- `modules/braid/cli.nix:30-35` -- inside `lib.optionalAttrs cfg.ups.enable { ups = { ... } }`, drop the `enable = cfg.ups.enable;` line. The block only emits `name = cfg.ups.name;`. The Nix option `braid.ups.enable` and the outer `lib.optionalAttrs` gate are unchanged -- this is purely a JSON-schema simplification.

### Production callsites (drop `.enable` check; keep "Some = live")

- `cli/src/ups.rs:118-123` -- collapse to a single `let Some(ups_cfg) = config.ups() else { return print_not_enabled(json) };`. Delete the secondary `if !ups_cfg.enable` branch.
- `cli/src/tui/app.rs:46-56` -- `ups_probe_effect` becomes a structural twin of `fan_probe_effect`: `let u = model.ups_config.as_ref()?; Some(Effect::ProbeUps { name: u.name.clone() })`. Drop the `if !u.enable` early return. Keep the doc comment but trim the "AND enabled" claim.
- `cli/src/tui/app.rs:222-242` -- `RefreshUps` branch: replace `let enabled = model.ups_config.as_ref().is_some_and(|u| u.enable); if !enabled { return vec![]; }` with `if model.ups_config.is_none() { return vec![]; }`. This mirrors the existing `RefreshFan` branch at `app.rs:192-209`.
- `cli/src/tui/model.rs:276` -- `let fan_probe_inflight = fan_control.is_some();` is already correct; leave it.
- `cli/src/tui/model.rs:288-294` -- replace the UPS block with the symmetric form:
  ```rust
  let ups_probe_inflight = ups_config.is_some();
  if let Some(u) = ups_config.as_ref() {
      effects.push(Effect::ProbeUps { name: u.name.clone() });
  }
  ```
  (Mirror of the fan stanza directly above.)
- `cli/src/tui/view/mod.rs:942-943` -- `let ups_enabled = model.ups_config.is_some();` (matches the line above for fan).
- `cli/src/doctor.rs:629-639` -- `check_ups_daemon_up`: replace the `match config.ups() { Some(u) if u.enable => u, _ => skip }` block with `let Some(ups_cfg) = config.ups() else { return CheckResult::skip(name, "skipped (braid.ups not enabled)"); };`. The user-facing skip message stays as-is (still accurate at the Nix-option level).
- `cli/src/doctor.rs:695-704` -- `check_braid_online_active_when_mounted`: replace `if !config.ups().is_some_and(|u| u.enable) { ... }` with `if config.ups().is_none() { ... }`.

### Hand-written UPS config fixtures (drop the legacy `enable` field)

These are not deletions -- they are required edits because `#[serde(deny_unknown_fields)]` will reject the legacy shape they currently produce.

- `cli/src/test_fixtures/ups.rs:14-22` -- `ups_write_config`: drop `"enable":true,` from the formatted JSON. New body: `format!(r#"{{"mount_point":"/mnt/storage","ups":{{"name":"{name}"}}}}"#)`.
- `cli/src/test_fixtures/doctor.rs:66-68` -- `config_with_ups_enabled`: drop `"enable":true,`. New body: `r#"{"mount_point":"/mnt/storage","ups":{"name":"ups"}}"#`.
- `cli/src/add.rs:6617-6620` -- the inline `serde_json::json!` literal in the UPS-on-battery add test: drop the `"enable": true,` entry. New shape: `"ups": { "name": "ups" }`.
- `cli/src/doctor.rs:1191-1209` -- `valid_json_bad_schema_skips_ups_as_config_unavailable`. The raw JSON at line 1196 currently reads `"ups": { "enable": true, "name": "ups" }`; under deny-unknown-fields this would fail with "unknown field `enable`" instead of reaching the empty-`mount_point` schema error the test is meant to assert. Drop `"enable": true,` from the literal AND update the scenario comment at line 1188 to say "hand-edited config sets an `ups` block but leaves `mount_point` empty".

### Tests to delete (the case becomes unreachable)

- `cli/src/test_fixtures/doctor.rs:74-76` -- delete `config_with_ups_disabled()`. Update the import line at `cli/src/doctor.rs:1037` and `cli/src/test_fixtures.rs:152` to remove the name.
- `cli/src/doctor.rs:2563-2587` -- delete `ups_daemon_check_skips_when_enable_false`. Coverage for the "skip when UPS not configured" path is retained by `ups_daemon_check_skips_when_config_absent` at line 2550.
- `cli/src/doctor.rs:2860-2876` -- delete `braid_online_check_skips_when_ups_disabled`. Coverage retained by `braid_online_check_skips_when_ups_absent` at line 2846.
- `cli/src/tui/app.rs:938-952` -- delete `refresh_ups_skips_when_not_enabled`. The "RefreshUps tears down when UPS absent" coverage is what's testable in the new schema; the existing `sample_ups_config` helper at `app.rs:275-280` loses its `enable: bool` parameter (it becomes `sample_ups_config() -> Ups { Ups { name: "ups".into() } }`).
- `cli/src/tui/app.rs:275-280` -- update `sample_ups_config` to drop the `enable` parameter. All other callers (`refresh_ups_emits_probe_when_idle` at 961, plus any other tests in the file) pass through unchanged after dropping the `(true)` / `(false)` arg.

### Comment / preamble cleanup pass

After the type-level axis is gone, the only "disabled" signal is absence of the `ups` block. Several comments still describe a second enable axis and need to align:

- `cli/src/tui/view/mod.rs:273` -- precondition comment for `ups_section` currently reads `model.ups_config is Some AND enable=true`. Update to `model.ups_config is Some`.
- `cli/src/ups.rs:1-10` -- module preamble currently says "Missing or disabled config prints a helpful enable-hint and exits 0". Update to phrase the only disabled signal as "Missing `ups` block in config.json prints a helpful enable-hint and exits 0". Keep the user-facing `print_not_enabled` message at line 147 as-is -- it still tells the operator to set `braid.ups.enable = true` in NixOS, which remains the correct user-facing knob.
- `cli/src/ups.rs:862-866` -- `snapshot_json_not_enabled` test preamble: simplify "host without `braid.ups.enable = true`" to a single phrasing that does not imply two axes; keep the sentinel-string assertion intact.
- `cli/src/ups.rs:477-481` -- `json_not_enabled_has_sentinel_error` scenario currently reads "triggered when `braid.ups.enable` is false or the config block is absent". The "or" is gone -- the only trigger is "the `ups` block is absent from `config.json`" (which itself corresponds to `braid.ups.enable = false` at the Nix-option level). Rewrite to that single condition.
- `cli/src/tui/app.rs:46` -- the doc comment on `ups_probe_effect` says "when the UPS block is present AND enabled". Trim the `AND enabled` clause (already noted under production-callsite edits, but call it out explicitly here too so the cleanup sweep does not skip it).
- `cli/src/config.rs:210-216` -- `parses_config_with_ups` preamble: drop the `{ enable, name }` shape note; the block now contains only `name`.
- Sweep the codebase for any other "enable = false" / "enable = true" prose about the JSON axis, "disabled config", "Some AND enable", or "block present AND enabled" phrasings. Use `rg -n 'enable\s*=\s*(true|false)|disabled config|Some AND enable|AND enabled|present AND enable' cli/src` -- `rg` honors `\s` whereas `git grep -E` does not, so this command actually matches. Pre-filter to keep only references that describe the JSON-level axis (the *Nix-option* `braid.ups.enable = true` mentions in user-facing strings and Nix-option-scoped comments are correct and stay). The pass is purely descriptive -- no behavior changes.

## Critical files

- `cli/src/config.rs` -- struct + parse test
- `modules/braid/cli.nix` -- JSON emission
- `cli/src/tui/app.rs` -- helpers, RefreshUps branch, sample fixture, one test
- `cli/src/tui/model.rs` -- Model::new initial probes
- `cli/src/tui/view/mod.rs` -- layout decision
- `cli/src/doctor.rs` -- two check functions, two test deletions, plus the `valid_json_bad_schema_skips_ups_as_config_unavailable` JSON-literal edit
- `cli/src/ups.rs` -- braid ups status short-circuit
- `cli/src/test_fixtures/doctor.rs` -- fixture deletion AND `config_with_ups_enabled` edit
- `cli/src/test_fixtures/ups.rs` -- `ups_write_config` edit
- `cli/src/test_fixtures.rs` -- re-export cleanup
- `cli/src/add.rs` -- UPS-on-battery test JSON literal

## Reused patterns

- The fan stanza in `Model::new` (`cli/src/tui/model.rs:276-284`) is the template for the new UPS stanza -- presence-only check, no extra axis.
- The `RefreshFan` arm at `cli/src/tui/app.rs:192-209` is the template for the simplified `RefreshUps` arm.
- The doctor skip-when-absent tests (`ups_daemon_check_skips_when_config_absent`, `braid_online_check_skips_when_ups_absent`) already provide the coverage shape; the to-be-deleted tests are duplicates of that shape against a now-unreachable JSON state.

## Verification

1. `just test-rust` -- exercises the parser tests (`parses_config_with_ups`, the new `rejects_config_with_legacy_ups_enable_field`), the doctor checks, the UPS-on-battery add test, and the TUI update tests. All updated/deleted assertions land here, and the new deny-unknown-fields contract is pinned.
2. `just test-vm` -- run the full VM suite to confirm the Nix emission still produces a parsable config and the UPS-enabled hosts still bring up upsd. The UPS-related VM tests (`tests/module/*ups*.nix`, `tests/capture-ups-fixtures.nix`) all set `braid.ups.enable = true`, so they exercise the only emission path that survives.
3. `just test-parsers` -- confirms the parser canary still parses `upsc` output against the live VM, which is unaffected by config-schema changes but worth running to lock the parser surface.
4. Manual JSON sanity-check: read `/etc/braid/config.json` after a VM boot with `braid.ups.enable = true` and confirm the `ups` block now contains only `name`.
5. No fixture-refresh obligation: the Rust golden fixtures in `cli/tests/fixtures/` are parser-output captures (btrfs/cryptsetup/upsc/etc.), not config-schema fixtures; none of them depend on `Ups.enable`.

## Out of scope

- Consolidating `fan_probe_effect` and `ups_probe_effect` into a single generic helper. Post-pivot they have identical shape but build different `Effect` variants with subsystem-specific fields; a generic abstraction would obscure intent for no real win.
- Adding `is_enabled()` methods (the original finding's proposal). The pivot makes them unnecessary -- there is no second axis to centralize. If a real "disabled_until" or similar concept lands later, introduce the method then.
- The preflight call sites at `add.rs:1287` et al. -- they already use `config.ups().map(|u| u.name.as_str())`, which is correct under the new "presence = live" semantics. No change needed; they were already on the right side of the new invariant.
