# Detect broken PC speaker via `braid doctor`

## Context

`braid-alert.service` (`modules/braid/monitor.nix:60-82`) runs an infinite
shell loop that beeps every 15 seconds until `braid ack` stops it. The beep
call swallows all errors with `|| true`, so a fundamentally broken PC speaker
(kmod blacklist not removed, evdev permissions wrong, no pcspkr device on the
motherboard) is invisible: the service stays `active`, no warning is logged,
and the user only finds out when a real disk failure produces no audible alert.

An earlier revision of this plan addressed this by adding a startup probe to
`braid-alert.service` itself, failing the unit when the speaker was broken.
That direction was rejected:

- **`braid-alert.service` `active` is a load-bearing external signal.** It
  cleanly means "braid currently has an active alert," and is consumable by
  `systemctl is-active braid-alert.service` from any shell, ssh probe, or
  monitoring tool. Overloading the same unit with *notifier-health* state
  would make `active` ambiguous (`active` could now mean "alert in progress"
  *or* "no alert and notifier is healthy" depending on whether anything has
  triggered the unit recently).
- **Proactive vs runtime concerns.** Verifying that the speaker is wired up
  is a *proactive diagnostic*, not a runtime alert state. The right home for
  proactive checks is `braid doctor`, which already covers config, pool
  health, profile consistency, and LUKS headers.

This revision moves broken-speaker detection into `braid doctor`. The alert
service's runtime semantics are left untouched. The `docs/decisions/alerts.md`
"silently swallowed" invariant at line 70 also stays accurate and is **not**
amended.

## Approach

Introduce a small braid-owned **canonical beep wrapper** in
`modules/braid/monitor.nix` and a runtime-readable **notifier config file**
that exposes whether beep is enabled and where the wrapper lives. Doctor
reads the config file and runs the wrapper as a subprocess.

This is the design the previous draft of this plan got wrong. That draft had
doctor scrape `systemctl cat braid-alert.service` and parse the rendered
shell script to discover the canonical setpriv+beep invocation. That made
doctor depend on the exact textual rendering of a systemd unit and a
generated shell script — a brittle internal-implementation contract. A
harmless refactor of `monitor.nix` (whitespace, function extraction, switch
to `User=` directive) could silently break doctor without changing actual
runtime behavior.

The replacement design has *one* canonical place where the beep argv lives:
the `pkgs.writeShellScriptBin "braid-beep-probe"` body inside `monitor.nix`.
Both the alert service script and `/etc/braid/notifier-config.json` reference
that same derivation by Nix store path. Doctor never inspects rendered unit
text — it reads the explicit, braid-owned config file.

Doctor's algorithm:

1. Read `/etc/braid/notifier-config.json`. Absent → `Skip`
   ("braid monitor not configured").
2. Parse it. Malformed → `Fail` (real defect — the module wrote junk).
3. If `beep_probe_path` is `null` (i.e. `monitor.beep = false`) → `Skip`
   ("beep monitoring disabled").
4. If `geteuid() != 0` → `Skip` ("requires root to play the alert test
   tone"). Checked *before* the JSON gate so non-root gets the actionable
   "use sudo" message regardless of output mode.
5. If `json_output` is `true` → `Skip` ("suppressed in --json mode — run
   without --json to play the alert test tone"). `braid doctor --json` is
   designed for programmatic/scripted consumption; emitting an audible
   side effect from a data-output command is wrong. The check is still
   *present* in the JSON report (with `status: "skip"`) so scripts that
   audit doctor output can see it, but the tone is never played.
6. Run the wrapper via the typed `CmdRequest::BraidBeepProbe { path }` —
   mockable in unit tests via `MockRunner`.
7. Exit 0 → `Ok` (alert test tone played). Non-zero → `Fail` (could not
   play alert test tone, with the wrapper's stderr in the message). Spawn
   error → `Fail`.

### The audible tone is intentional product behavior

When `monitor.beep = true`, the doctor check **plays a short alert test
tone** through the canonical wrapper. This is not an incidental side
effect of "checking the path is invokable" — it is the entire point of
the check, and serves two simultaneous purposes:

1. **Notifier health check.** Confirms the PC speaker is reachable
   end-to-end, exercising the kmod blacklist removal, the pcspkr module,
   the udev rule that grants the beep group write access to the evdev
   device, the privilege drop via setpriv, and the `beep` binary itself.
2. **Alert preview.** Lets the operator hear *exactly* what a real disk
   alert will sound like when it fires. Because the wrapper is the same
   code path the alert service uses, hearing the tone from `braid doctor`
   is a positive guarantee that future alerts will produce the same
   sound — there is no separate "doctor tone" or "alert tone" to keep in
   sync.

User-facing copy (result messages, the human label, the README line) must
say so explicitly: doctor *plays a short alert test tone*, not "verifies
the beep path is invokable."

### Architecture preserved

`braid-alert.service`'s runtime semantics are **preserved exactly**. The
only change to `monitor.nix` is replacing the inlined `setpriv … beep …` in
the loop body with `${braidBeepProbe}/bin/braid-beep-probe` — a
behavior-preserving refactor. The unit's `active` state still means "alert
in progress," nothing more.

## Files to modify

### 1. `modules/braid/monitor.nix` — extract canonical wrapper + emit notifier config

This is the only `monitor.nix` change in the plan, and it is purely
structural: the same syscalls in the same order with the same privilege
drop are still executed by the alert service.

#### 1a. Define the wrapper

In the existing `let` block at the top of the file (next to `braidWrapped`
at line 5), add:

```nix
braidBeepProbe = pkgs.writeShellScriptBin "braid-beep-probe" ''
  exec ${pkgs.util-linux}/bin/setpriv \
    --reuid=nobody --regid=beep --groups=beep -- \
    ${pkgs.beep}/bin/beep -f 1000 -l 500
'';
```

This is now the **single source of truth** for the canonical
privilege-dropped beep argv. It is referenced by both the alert service
script (1b) and the notifier config file (1c) by Nix store path, so they
cannot drift.

#### 1b. Use the wrapper from the alert service script

Replace the two inlined `setpriv … beep …` invocations in the rendered
script (`modules/braid/monitor.nix:75-79`) with calls to the wrapper:

```nix
${lib.optionalString beepEnabled ''
  while true; do
    ${braidBeepProbe}/bin/braid-beep-probe 2>/dev/null || true
    sleep 15
  done
''}
```

This is a textually-shorter, behavior-identical refactor: the wrapper is
just `exec setpriv … -- beep …`, so the unit performs the same syscalls in
the same order. The `2>/dev/null || true` guard, the 15 s loop, the
`modprobe pcspkr || true`, and `alertCommand || true` all stay unchanged.

#### 1c. Write `/etc/braid/notifier-config.json`

Inside `config = lib.mkIf (cfg.enable && cfg.monitor.enable) { … };`, add:

```nix
environment.etc."braid/notifier-config.json".text = builtins.toJSON {
  beep_probe_path =
    if beepEnabled
    then "${braidBeepProbe}/bin/braid-beep-probe"
    else null;
};
```

Schema:

```json
{ "beep_probe_path": "/nix/store/.../bin/braid-beep-probe" }   // beep on
{ "beep_probe_path": null }                                    // beep off
```

This file is the explicit braid-owned contract doctor consumes. The schema
is small enough to be stable; if it changes, doctor's deserializer changes
in lockstep, and the breakage is loud (deserialize error) rather than silent
(stale text scrape).

#### 1d. What does NOT change

- `braid-alert.service` runtime semantics: unchanged. `active` continues to
  mean exactly "an alert is in progress," nothing else.
- `|| true` in the loop body: stays. Transient in-loop failures still cannot
  silence an in-progress alert.
- `modprobe pcspkr || true` and the optional `alertCommand || true`: stay.
- The 15 s sleep, the loop structure, the `serviceConfig` Type: all stay.
- The smartd hook script and `braid-monitor.service` start callsites
  (`monitor.nix:9, 94`): stay.

### 2. `cli/src/cmd.rs` — typed wrapper-probe command

Doctor invokes the wrapper through the typed `CmdRequest` enum so it's
mockable via `MockRunner`. Add one new variant near the other read-only
queries (`cli/src/cmd.rs:21-258`):

```rust
// in pub enum CmdRequest { … }
BraidBeepProbe {
    /// Absolute path to the wrapper script, read from
    /// /etc/braid/notifier-config.json by the doctor check.
    path: String,
},
```

`to_argv` arm (`cli/src/cmd.rs:260`):

```rust
CmdRequest::BraidBeepProbe { path } => CmdArgs {
    program: path.clone(),
    args: vec![],
},
```

`requires_stdin`: no arm needed (defaults to false).

**Type widening (mechanical, every existing variant touched):** the
existing `CmdArgs::program` field is `&'static str`, which cannot hold a
runtime-resolved Nix store path. Widen it to `String`:

```rust
pub struct CmdArgs {
    pub program: String,
    pub args: Vec<String>,
}
```

This is a one-shot mechanical change touching every `CmdRequest` variant
(~45 sites across `to_argv`): `program: "btrfs",` becomes
`program: "btrfs".to_owned(),`. The two `RealRunner::exec*` callers also
flip from `Command::new(cmd.program)` to `Command::new(&cmd.program)`,
and `to_shell_string` switches `iter::once(self.program)` to
`iter::once(self.program.as_str())`. Existing test assertions like
`assert_eq!(cmd.program, "btrfs")` keep working because `String == &str`
is supported.

The widening is the cleanest option: `Cow<'static, str>` would require
the same per-callsite churn (`.into()` instead of `.to_owned()`), and
`Box::leak` for the dynamic path would leak memory per `CmdRequest`
construction. Splitting into separate static/dynamic fields would fork
the rendering paths and break uniformity.

### 3. `cli/src/doctor.rs` — new check + unit tests

#### 3a. Notifier-config deserializer

At the top of the checks section, add a small struct:

```rust
#[derive(Debug, Clone, Deserialize)]
struct NotifierConfig {
    beep_probe_path: Option<String>,
}
```

#### 3b. New check function

Two layers: a thin public wrapper that reads the real filesystem,
the real `geteuid()`, and the `json_output` flag from context; and a
fully-deterministic inner helper that takes all three as explicit
parameters. **Unit tests call only the inner helper** with any combination
of `is_root` and `json_output` they need — no OS calls, no context mutation.

`run_doctor` gains a `json: bool` parameter that flows into
`check_beep_path`. Existing unit tests calling `run_doctor` add `false`
as the last argument; the call in `cmd_doctor` passes the existing `json`
flag it already has.

```rust
const NOTIFIER_CONFIG_PATH: &str = "/etc/braid/notifier-config.json";

pub fn run_doctor<R: CommandRunner>(
    config_path: &Path,
    runner: &R,
    paths: &StatePaths,
    json: bool,          // ← new parameter
) -> DoctorReport { … }

fn check_beep_path<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>, json: bool) -> CheckResult {
    let is_root = unsafe { libc::geteuid() } == 0;
    check_beep_path_inner(ctx, Path::new(NOTIFIER_CONFIG_PATH), is_root, json)
}

fn check_beep_path_inner<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
    notifier_path: &Path,
    is_root: bool,
    json_output: bool,
) -> CheckResult {
    let name = "beep_path".to_string();

    // 1. Read the notifier config the NixOS module wrote.
    let raw = match std::fs::read_to_string(notifier_path) {
        Ok(s) => s,
        Err(_) => {
            return CheckResult {
                name,
                status: CheckStatus::Skip,
                message: "skipped (braid monitor not configured)".into(),
            };
        }
    };

    // 2. Parse. Malformed = real defect.
    let cfg: NotifierConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                name,
                status: CheckStatus::Fail,
                message: format!("{}: malformed: {e}", notifier_path.display()),
            };
        }
    };

    // 3. Beep disabled is a clean Skip.
    let probe_path = match cfg.beep_probe_path {
        Some(p) => p,
        None => {
            return CheckResult {
                name,
                status: CheckStatus::Skip,
                message: "skipped (beep monitoring disabled)".into(),
            };
        }
    };

    // 4. Lack of root is an INVOCATION CONTEXT issue, not a SPEAKER HEALTH
    //    issue. Checked BEFORE the JSON gate so non-root always gets the
    //    actionable "use sudo" message regardless of output mode.
    if !is_root {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped (requires root to play the alert test tone)".into(),
        };
    }

    // 5. JSON mode is for programmatic consumption; emitting audible side
    //    effects from a data-output command is wrong. The check still
    //    appears in the JSON report (as Skip) so scripts can see it.
    if json_output {
        return CheckResult {
            name,
            status: CheckStatus::Skip,
            message: "skipped in --json mode — run without --json to play \
                      the alert test tone".into(),
        };
    }

    // 6. Run the canonical wrapper. This PLAYS the real short alert tone
    //    (1 kHz, 500 ms) — same code path the alert service uses. Hearing
    //    the tone is both the success signal AND a preview of what real
    //    disk alerts will sound like.
    match ctx.runner.run(&CmdRequest::BraidBeepProbe { path: probe_path }) {
        Ok(out) if out.exit_status == 0 => CheckResult {
            name,
            status: CheckStatus::Ok,
            message: "alert test tone played (1 kHz, 500 ms) — \
                      same tone braid will use for real disk alerts".into(),
        },
        Ok(out) => CheckResult {
            name,
            status: CheckStatus::Fail,
            message: format!(
                "could not play alert test tone (braid-beep-probe exited {}) \
                 — speaker likely broken: missing pcspkr device, evdev \
                 permissions wrong, or kmod blacklist still active: {}",
                out.exit_status,
                out.stderr.trim()
            ),
        },
        Err(e) => CheckResult {
            name,
            status: CheckStatus::Fail,
            message: format!("could not play alert test tone (braid-beep-probe failed to spawn): {e}"),
        },
    }
}
```

#### 3c. Wiring

- Append `check_beep_path(&mut ctx)` to the `checks` vec
  (`cli/src/doctor.rs:586-594`).
- Add `"beep_path" => "alert tone"` to the human-format label map
  (`cli/src/doctor.rs:614-622`). The internal check identifier stays
  `beep_path` for stability of the JSON schema; the *human* label
  reflects the product framing — what the operator hears, not what the
  code does.

#### 3d. Unit tests

Tests live in the existing `mod tests` block (`cli/src/doctor.rs:665+`).
**All branch coverage uses `check_beep_path_inner` directly**, passing an
explicit `notifier_path: &Path` (a `tempfile::NamedTempFile`) and an
explicit `is_root: bool`. This makes every test deterministic regardless
of which UID `cargo test` runs under — there is no `geteuid()` syscall in
the test path. The runner side is mocked via `MockRunner::with_output`
(see `cli/src/doctor.rs:704+` for the existing pattern).

Tests to add:

- `beep_path_skips_when_notifier_config_absent` — `is_root=true`,
  nonexistent notifier path → `Skip`
- `beep_path_fail_on_malformed_config` — `is_root=true`, temp file with
  `not json {` → `Fail`
- `beep_path_skips_when_beep_disabled` — `is_root=true`, temp file with
  `{"beep_probe_path": null}` → `Skip` ("beep monitoring disabled")
- `beep_path_skips_when_not_root` — `is_root=false`, `json_output=false`,
  temp file with a real probe path, MockRunner with **no** `BraidBeepProbe`
  output configured → `Skip` ("requires root"). Asserting that the runner
  was *not* invoked (e.g. by leaving the output map empty and confirming
  no panic for an unmatched call) is the strongest form.

  **Why this is unit-tested only, not VM-tested.** `cli/src/main.rs:248`
  applies a blanket root gate to every braid command except `tui --demo`:
  non-root `braid doctor` is rejected with "error: braid must be run as
  root" before command dispatch even runs. The non-root skip branch in
  `check_beep_path_inner` is therefore unreachable in production; it exists
  as defense-in-depth (future refactors, library re-use, test isolation).
  The unit test is the authoritative and complete coverage for this branch.
  No VM subtest is needed or meaningful.
- `beep_path_skips_in_json_mode` — `is_root=true`, `json_output=true`,
  temp file with a real probe path, MockRunner with no `BraidBeepProbe`
  output → `Skip` ("--json mode" in message). Proves the runner is never
  invoked in JSON mode even when the caller is root and the wrapper exists.
- `beep_path_ok_on_zero_exit` — `is_root=true`, `json_output=false`, temp file with a real
  probe path; mock runner returns `exit_status: 0` → `Ok`
- `beep_path_fail_on_nonzero_exit` — `is_root=true`, `json_output=false`,
  mock returns `exit_status: 1` with stderr "mock failure" → `Fail`; assert
  message contains "speaker likely broken"

`run_doctor` now takes `json: bool` as a final param. Existing
`run_doctor(path, &mock, &paths)` calls in the test module become
`run_doctor(path, &mock, &paths, false)`.

The existing `valid_config_parses_ok_disks_warn` (`cli/src/doctor.rs:704`)
asserts `report.checks.len() == 7`. Bump to **8** and add an assertion
that the new `beep_path` check is present. Its status in that test will
depend on whether `/etc/braid/notifier-config.json` exists in the test
environment (it does not), so it will be `Skip` with the
"monitor not configured" message — assert that explicitly.

### 4. `tests/cli/braid-doctor-beep.nix` + `.py` — new VM test

The VM imports the braid module so `monitor.nix` actually writes
`/etc/braid/notifier-config.json` and renders the wrapper. The mock for
`pkgs.beep` is gated on a flag file, so the same VM exercises the Ok, Fail,
and recovery branches without rebuilding:

```nix
# tests/cli/braid-doctor-beep.nix
# Test: braid doctor — PC speaker probe (beep_path check)
#
# What: Validates that the doctor `beep_path` check plays the alert test
# tone in human mode (Ok when the wrapper works, Fail when it does not,
# recovery when the underlying issue is resolved) and skips silently in
# `--json` mode regardless of speaker state.
#
# Why: Without an active alert, a broken PC speaker is invisible — the alert
# service's `|| true` swallows beep failures and the user only discovers the
# problem when a real disk alert produces no sound. doctor exists precisely
# to surface this kind of latent breakage. `--json` mode must never produce
# audible side effects so scripts piping doctor output stay silent.
{ braid }:
{
  name = "braid-doctor-beep";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    # Flag-file-controlled mock so one VM can exercise both healthy and broken
    # branches by touching/removing the flag, instead of needing two VMs with
    # two different overlay-pinned beep packages.
    nixpkgs.overlays = [
      (final: prev: {
        beep = prev.writeShellScriptBin "beep" ''
          if [ -f /tmp/beep-broken ]; then
            echo "mock beep: failing per /tmp/beep-broken" >&2
            exit 1
          fi
          exit 0
        '';
      })
    ];

    braid = {
      enable = true;
      package = braid;
      monitor.enable = true;
      monitor.beep = true;
    };

    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];

    # /etc/braid/config.json is written by the braid NixOS module from the
    # default `braid.mountPoint` (/mnt/storage). No explicit override needed —
    # adding `environment.etc."braid/config.json"` here would conflict with
    # the module's own writer (cli.nix:19) and fail to evaluate.
  };

  testScript = builtins.readFile ./braid-doctor-beep.py;
}
```

```python
# tests/cli/braid-doctor-beep.py
#
# Intent: Verify the new doctor `beep_path` check across its live branches:
#   human-mode Ok (working wrapper plays the tone), human-mode Fail (broken
#   wrapper), recovery, and JSON-mode Skip (the tone is suppressed for
#   programmatic consumption regardless of speaker state).
#
# Why it exists: Doctor is the proactive diagnostic surface for "is braid
#   wired up correctly?" Without this check, a broken PC speaker is invisible
#   until a real alert silently fails to make a sound. This test pins the
#   check's behavior so a regression that re-introduces silent best-effort
#   beeping, OR a regression that lets `braid doctor --json` produce audible
#   side effects (which would surprise scripts piping it into a monitoring
#   system), is caught.
#
# Scenario: NixOS machine with braid.monitor.beep = true and pkgs.beep
#   replaced by a flag-file-gated mock. /etc/braid/notifier-config.json is
#   written by the module. Subtests touch/remove /tmp/beep-broken to drive
#   the healthy/broken branches.

import json

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("Notifier config was written by the module"):
    cfg = json.loads(machine.succeed("cat /etc/braid/notifier-config.json"))
    assert cfg["beep_probe_path"] is not None
    assert "braid-beep-probe" in cfg["beep_probe_path"]

# Human-mode subtests: these actually invoke the wrapper (the mock beep).
# `braid doctor` without --json is the operator-facing path, where playing
# the tone is the entire point of the check.

with subtest("Healthy beep (human mode): doctor exits 0"):
    machine.succeed("rm -f /tmp/beep-broken")
    machine.succeed("braid doctor")

with subtest("Broken beep (human mode): doctor exits 1"):
    machine.succeed("touch /tmp/beep-broken")
    rc, _ = machine.execute("braid doctor")
    assert rc == 1, f"expected exit 1 for broken beep in human mode, got {rc}"

with subtest("Recovery (human mode): clearing the flag returns to exit 0"):
    machine.succeed("rm -f /tmp/beep-broken")
    machine.succeed("braid doctor")

# JSON-mode subtest: must NEVER play the tone regardless of speaker state.

with subtest("JSON mode: beep_path is always Skip (tone never played)"):
    # Use broken beep to prove that the wrapper is not invoked even when it
    # would fail — the Skip must happen before any subprocess is spawned.
    # Broken speaker in human mode → exit 1, but JSON mode → exit 0 (Skip ≠ Fail).
    machine.succeed("touch /tmp/beep-broken")
    out = machine.succeed("braid doctor --json")
    report = json.loads(out)
    beep = next(c for c in report["checks"] if c["name"] == "beep_path")
    assert beep["status"] == "skip", f"expected skip in json mode, got {beep}"
    assert "json" in beep["message"].lower(), (
        f"Skip message must explain suppression in --json mode: {beep['message']}"
    )
    # Clean up so the next VM test (if any) starts from the healthy state.
    machine.succeed("rm -f /tmp/beep-broken")

# NOTE on the missing non-root subtest:
#
# The Skip-when-not-root branch of `check_beep_path_inner` is covered
# deterministically by `doctor::tests::beep_path_skips_when_not_root`, which
# constructs the context directly and asserts the runner is never invoked.
# It is intentionally NOT exercised here. `sudo -u tester braid doctor` would
# exit 1 with "braid must be run as root" because `cli/src/main.rs:244-251`
# rejects every command except `tui --demo` for non-root users — the request
# never reaches `cmd_doctor`. Reaching the inner branch from a VM would
# require relaxing that top-level CLI gate, which is intentionally out of
# scope for this plan.

machine.shutdown()
```

### 5. `tests/cli/braid-doctor.py` — assert skip on the no-monitor VM

The existing `tests/cli/braid-doctor.nix` does not import the braid module,
so `/etc/braid/notifier-config.json` does not exist. The new check skips
naturally there. Add one assertion (in any existing subtest that already
parses doctor JSON output) that the `beep_path` check is present and is
`Skip` with the "monitor not configured" message. This pins the
"no-notifier-config" branch without spinning up another VM.

### 6. `flake.nix` — register the new test

Add a `braid-doctor-beep` check next to the existing `braid-doctor`
registration (look for `braid-doctor = pkgs.testers.nixosTest …` near
`flake.nix:480-500`):

```nix
braid-doctor-beep = pkgs.testers.nixosTest (
  import ./tests/cli/braid-doctor-beep.nix {
    braid = linuxCrane.braid;
  }
);
```

### 7. `README.md:288` — extend doctor description with the test-tone behavior

> **Status: NOT applied in this implementation.** A first pass of this
> change was made and then reverted (either by the user or a linter).
> The README line at `README.md:288` remains unchanged from the
> pre-existing form. Restore the update below in a follow-up if/when the
> README copy decision is settled.

Originally proposed change:

Current line:

```
sudo braid doctor           # check config, pool health, profile consistency, LUKS headers
```

Updated:

```
sudo braid doctor           # check config, pool health, profiles, LUKS headers; plays a short alert test tone if beep is enabled
```

The "plays a short alert test tone" phrasing is the user-facing surface
of the dual-purpose framing in the Approach section. Operators reading
the README should understand *before* running `braid doctor` that it will
make a sound on systems with `monitor.beep = true`, both as a health
check and as a preview of the real alert tone.

## Files explicitly **not** touched (vs the previous revision of this plan)

These changes were in the previous direction and are now reverted:

- `tests/module/braid-alert.nix` — no overlay mock added; the existing test
  config is unchanged.
- `tests/module/braid-alert.py` — no comment-block update, no `is-failed`
  assertion. The previous draft was going to mock `pkgs.beep` and update
  the test's narrative around "the VM has no PC speaker hardware"; this
  draft does neither. (A small grep update *is* required for the wrapper
  extraction — see verification §3 — but that is a follow-on from 1b, not
  the previous draft's planned changes.)
- `tests/module/braid-alert-broken-speaker.nix` / `.py` — **not created**.
- `docs/decisions/alerts.md:70` — the "beep/pcspkr failures are silently
  swallowed" line stays accurate as a description of the alert *unit*'s
  runtime behavior. Doctor coverage is a separate, proactive surface and
  does not invalidate the runtime invariant.

## What is intentionally **not** changing

- **Alert service runtime semantics.** `braid-alert.service` `active`
  continues to mean exactly "an alert is in progress," nothing else. The
  wrapper extraction in 1b is purely structural.
- **The exponential-backoff plan**
  (`plans/wip/purring-coalescing-reddy.md`). It modifies the loop's sleep
  duration only and is unaffected by the wrapper extraction.
- **Speaker-degradation case** (beep writes to evdev fine but produces no
  audible sound). No software-only approach can detect this. Out of scope.
- **`braid status` and the shared `AlertState` model.** Notifier health is
  *not* an `AlertCause`. The status schema in `cli/src/status.rs:44-73` is
  unchanged. Per `docs/decisions/alerts.md`, alert causes are about disk
  health; notifier health belongs in doctor.

## Out of scope — follow-up TODO

**Dedicated on-demand alert tone replay command.** `braid doctor` plays
the alert test tone as part of running the *full* doctor suite, but
operators may want to hear the tone on demand without paying for the
other checks (config, pool, profiles, LUKS headers). A future PR should
add a small dedicated surface for this. Possible shapes:

- `braid alert --test` — runs the canonical wrapper directly
- `braid doctor --only=alert_tone` — runs only the alert-tone check
- A standalone `braid beep` command

Whichever shape is chosen, it should reuse the same `braid-beep-probe`
wrapper introduced in §1a so there is exactly one place where the
canonical setpriv+beep argv lives. This is **out of scope** for the
present plan: the user-facing convenience is small, the change adds CLI
surface area, and the value of the doctor-integrated tone (as both a
health check and an alert preview) is unchanged regardless of whether
the on-demand command exists.

## Verification

1. **Rust unit tests** for the new check:

   ```sh
   just test-rust -- doctor::tests::beep_path
   ```

   Expected: 7 tests pass — `notifier-config-absent`,
   `fail-on-malformed-config`, `beep-disabled`, `not-root`, `in-json-mode`,
   `ok-on-zero-exit`, `fail-on-nonzero-exit`. The `not-root` and
   `in-json-mode` tests both leave the MockRunner with no `BraidBeepProbe`
   output configured to pin the runner-not-invoked invariant for those
   short-circuit branches.

2. **Existing doctor unit tests still pass** with the new check counted:

   ```sh
   just test-rust -- doctor::tests
   ```

   The `report.checks.len()` assertion in `valid_config_parses_ok_disks_warn`
   (`cli/src/doctor.rs:708`) must be bumped from 7 to 8.

3. **`braid-alert` test still passes** (sanity check that the wrapper
   extraction was behavior-preserving):

   ```sh
   just test-vm braid-alert braid-alert-no-beep monitor-lifecycle smartd-hook
   ```

   `tests/module/braid-alert.py:36-39` currently asserts that the rendered
   alert script contains `setpriv`, `reuid=nobody`, and `regid=beep`. After
   1b, those tokens move into the wrapper script (referenced by Nix store
   path) and are no longer in the rendered alert script body. The test
   **must** be updated to:

   1. Assert the rendered alert script references `braid-beep-probe`.
   2. Resolve the wrapper's store path and `cat` it. The implementation
      uses `grep -oE '/nix/store/[^[:space:]]*braid-beep-probe' …` against
      the rendered alert script to extract the absolute path. The `command
      -v braid-beep-probe` alternative does **not** work because the
      wrapper is referenced by Nix store path from inside the alert
      service script and is not added to `environment.systemPackages`, so
      it is not on the system PATH.
   3. Assert the wrapper script contains `setpriv`, `reuid=nobody`,
      `regid=beep` — the same invariants the existing test pins, just one
      indirection deeper.

   This grep update is the only follow-on change in the existing alert tests.

4. **Existing doctor VM test still passes** with the new skip assertion:

   ```sh
   just test-vm braid-doctor
   ```

5. **New doctor-beep VM test passes**:

   ```sh
   just test-vm braid-doctor-beep
   ```

6. **New doctor-beep VM test FAILS against current code** (proves it pins
   the new behavior). Stash the `cli/` and `monitor.nix` changes, run the
   new test alone, confirm it fails because the `beep_path` check is missing
   from the report and `/etc/braid/notifier-config.json` doesn't exist.
   Unstash.

7. **Manual NAS verification** on a real machine:

   ```sh
   sudo braid doctor                    # listen — should hear the short alert test tone
   sudo braid doctor --json | jq '.checks[] | select(.name=="beep_path")'
   #   ^ expect status="ok", message mentions "alert test tone played"
   cat /etc/braid/notifier-config.json  # verify the module wrote it
   braid doctor --json | jq '.checks[] | select(.name=="beep_path")'
   #   ^ unprivileged: expect status="skip", "requires root to play the alert test tone"
   ```

   To simulate breakage on a real machine, use the same overlay technique as
   the VM test:

   ```nix
   nixpkgs.overlays = [
     (final: prev: {
       beep = prev.writeShellScriptBin "beep" "exit 1";
     })
   ];
   ```

   `sudo nixos-rebuild switch && sudo braid doctor` → expect `beep_path` to
   report `Fail` with stderr from the wrapper. Revert the overlay and
   re-rebuild to confirm recovery.

## Critical files

- `modules/braid/monitor.nix:1-11` (let block) — define `braidBeepProbe`
  derivation
- `modules/braid/monitor.nix:75-79` — replace inlined setpriv+beep with
  wrapper invocation
- `modules/braid/monitor.nix` (inside `config = lib.mkIf …`) — add
  `environment.etc."braid/notifier-config.json"`
- `cli/src/cmd.rs:21` — `CmdRequest` enum (add `BraidBeepProbe` variant)
- `cli/src/cmd.rs:260` — `to_argv` (add matching arm)
- `cli/src/doctor.rs:60-557` — checks section (insert `NotifierConfig`
  struct, `check_beep_path(ctx, json)` public wrapper,
  `check_beep_path_inner(ctx, notifier_path, is_root, json_output)`
  testable helper)
- `cli/src/doctor.rs:572` — `run_doctor` signature: add `json: bool` param;
  pass it to `check_beep_path`
- `cli/src/doctor.rs:641` — `cmd_doctor`: update `run_doctor` call to pass
  the existing `json` local it already has
- `cli/src/doctor.rs:586-594` — `run_doctor` checks vec (append new check)
- `cli/src/doctor.rs:614-622` — human-format label map (add `"beep_path"`)
- `cli/src/doctor.rs:665+` — `mod tests` (add new unit tests; bump `len() == 7`
  to 8 in `valid_config_parses_ok_disks_warn`)
- `tests/cli/braid-doctor.nix` — read-only; the new VM test mirrors its shape
- `tests/cli/braid-doctor.py` — add a single `beep_path` skip assertion
- `tests/cli/braid-doctor-beep.nix` — new file
- `tests/cli/braid-doctor-beep.py` — new file
- `tests/module/braid-alert.py:32-49` — grep update for the wrapper
  rename (extracts the wrapper path from the rendered alert script via
  `grep -oE '/nix/store/…braid-beep-probe'` and asserts the wrapper body
  contains `setpriv`, `reuid=nobody`, `regid=beep`); see verification §3
- `flake.nix` (`braid-doctor` registration site, ~line 187) — register
  the new test
- `README.md:288` — read-only in this implementation. The originally
  proposed extension was reverted; see §7 for the deferred copy.
- `docs/decisions/alerts.md:70` — read-only; unchanged
- `cli/src/status.rs:44-73` — read-only; documents that notifier health is
  intentionally absent from the `StatusReport` schema and out of scope
- `plans/wip/purring-coalescing-reddy.md` — read-only; verifies no conflict
