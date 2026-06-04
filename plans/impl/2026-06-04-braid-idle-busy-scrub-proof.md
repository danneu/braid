# Plan: deterministic live proof of `braid idle` busy-scrub path

## Context

`braid idle` is the autosuspend gate: a genuinely-running scrub must make it
exit 1 (`busy: scrub running`) so the NAS stays awake mid-scrub. This is the
single most operationally important `idle` outcome.

Today that exact path has **no live end-to-end proof**:

- The VM subtest (`tests/cli/braid-idle.py:123-140`) starts a scrub but, because
  scrub finishes instantly on small virtio disks, accepts `exit_code in [0, 1]`
  unconditionally and its `if exit_code == 1` branch only checks `"busy"` /
  `"scrub"` substrings. A regression that classified a running scrub into a
  terminal/idle state (exit 0) would pass unnoticed.
- The busy path is otherwise proven only by the Rust unit test
  `busy_when_scrub_running` (`cli/src/idle.rs`) against canned
  `idle_scrub_running()` stdout -- it never exercises the live
  `btrfs scrub status --raw` subprocess -> parser -> `IdleResult::Busy` -> exit 1
  chain.

The fix makes the scrub deterministically observable as `running` and asserts the
busy outcome unconditionally, closing the gap inside the parser-canary lane
(`just test-parsers`, which already runs `braid-idle`).

This is a **pivot** on the original finding: the finding's suggested mechanism
(per-device `scrub_speed_max` rate-limit, or a bare write-and-poll loop) is
rejected -- `scrub_speed_max` is documented as unreliable for this purpose in
`tests/repro/btrfs-replace-rejected-during-scrub.py` (it races the scrub daemon's
restore-limit loop and only throttles the first device). We instead reuse the
already-proven **dm-delay read throttle + poll-until-`Status: running`** technique.

## Approach

Throttle scrub *reads* with `dm-delay` so the scrub stays `running` long enough to
assert against, exactly as `tests/module/scrub-lifecycle.py` does. The status
queries themselves are not throttled (dm-delay only slows the data reads the
scrub performs), so the poll loop and `braid idle` stay responsive. Reuse the
same-lane wiring pattern from `tests/cli/braid-status-during-balance.nix`.

Scope is surgical: only the pool-creation block and the scrub subtest in
`braid-idle.py` change; the offline / config-error / non-root / idle /
probe-failure subtests are untouched (they run before the scrub and with
dm-delay at 0 delay, so behavior and speed are unchanged).

## Changes

### `tests/cli/braid-idle.nix`

Mirror `braid-status-during-balance.nix:35-50`:

- Add `pkgs.lvm2` to `environment.systemPackages` (provides `dmsetup`; `dm-delay`
  is a stock device-mapper target, `blockdev` comes from util-linux already on
  PATH).
- Prepend the helper to the test script:
  ```nix
  testScript =
    builtins.readFile ./../module/dm_delay_helpers.py + "\n\n"
    + builtins.readFile ./braid-idle.py;
  ```
- Keep `emptyDiskImages` at 512 MiB x2 and the existing `serial = "disk1"` /
  `"disk2"` opts -- `dm_delay_table` backs onto `/dev/disk/by-id/virtio-{name}`,
  which those serials already provide. Default memory is sufficient; only bump to
  `virtualisation.memorySize = 2048;` if a run OOMs.
- Refresh the `.nix` header comment to state the scrub busy path is now proven
  deterministically (no longer "start a scrub and check for busy").

### `tests/cli/braid-idle.py`

**Preamble (lines 1-15):** drop the "racy on small VM disks -- unit tests are
authoritative for the busy path" caveat from Intent/Scenario; state that the
busy-scrub exit-1 path is now deterministically proven via dm-delay read
throttling. Keep the three-section Intent / Why / Scenario form.

**Pool setup (lines 62-74):** create the LUKS+btrfs pool on dm-delay-backed
disks instead of raw virtio, following `scrub-lifecycle.py#setup_resume_pool`:

```python
for d in ["disk1", "disk2"]:
    dm_delay_create(machine, d)                       # 0 delay at creation
    by_id = f"/dev/disk/by-id/braid-test-{d}-delay"
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat "
                    f"--batch-mode --key-file=- --pbkdf pbkdf2 "
                    f"--pbkdf-force-iterations 1000 {by_id}")
    machine.succeed(f"echo -n '{passphrase}' | cryptsetup open "
                    f"--type luks --key-file=- {by_id} braid-{d}")
# mkfs.btrfs + mount lines unchanged (target /dev/mapper/braid-disk{1,2})
```

**Scrub subtest (replace lines 123-140):** deterministic busy assertion.

```python
with subtest("braid idle reports busy while a scrub is genuinely running"):
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/data bs=1M count=32")
    machine.succeed("sync")
    # Throttle scrub READS so it stays running through the assertion. Status
    # queries are not on the delayed path, so the poll + braid idle stay fast.
    dm_delay_activate(machine, ["disk1", "disk2"], read_delay_ms=500)
    # Redirect the scrub daemon's stdio off the driver pipe (see
    # tests/repro/btrfs-replace-rejected-during-scrub.py phase 3 note).
    machine.succeed("btrfs scrub start /mnt/storage > /dev/null 2>&1")
    # Poll until the kernel reports the scrub running (reuse scrub-lifecycle.py
    # loop; 400 x 0.05s = 20s budget).
    machine.succeed(
        "for i in $(seq 1 400); do "
        "out=\"$(btrfs scrub status --raw /mnt/storage 2>&1 || true)\"; "
        "if printf '%s\\n' \"$out\" | grep -Eq 'Status:[[:space:]]+running'; "
        "then exit 0; fi; sleep 0.05; done; "
        "printf '%s\\n' \"$out\"; exit 1"
    )

    status, output = machine.execute("braid idle")
    output = output.strip()
    assert status == 1, f"running scrub must make braid idle exit 1, got {status}: {output}"
    # startswith tolerates the optional ` (N%)` suffix from BusyReason::ScrubRunning.
    assert output.startswith("busy: scrub running"), (
        f"expected 'busy: scrub running', got: {output}"
    )

    # Teardown: cancel while the throttle still holds the scrub running, so the
    # cancel ioctl lands before the tiny (32 MiB) scrub can finish on its own.
    # `btrfs scrub cancel` on a no-longer-running scrub returns exit 2 (ENOTCONN
    # "not running") -- reference/btrfs-progs/cmds/scrub.c#cmd_scrub_cancel --
    # which would fail machine.succeed even though the assertion already passed.
    # Accept aborted OR interrupted: after a direct cancel btrfs persists
    # `aborted` (canceled=1) or `interrupted` (canceled=0/finished=0) per the
    # status-word logic in scrub.c (in_progress?running : canceled?aborted :
    # finished?finished : interrupted), exactly as the scrub-lifecycle
    # concurrency node documents and waits for. Deactivate only after a terminal
    # saved state is confirmed (mirrors that node: cancel with delay active ->
    # wait aborted|interrupted -> deactivate).
    machine.succeed("btrfs scrub cancel /mnt/storage")
    machine.wait_until_succeeds(
        "btrfs scrub status --raw /mnt/storage | "
        "grep -Eq 'Status:[[:space:]]+(aborted|interrupted)'",
        timeout=30,
    )
    dm_delay_deactivate(machine, ["disk1", "disk2"])
```

**Optional (nice-to-have, keep core tight):** the teardown above already leaves
the scrub in a terminal saved state (`aborted` or `interrupted` -- `cmd_idle`
maps both to Idle); add one `braid idle` call after `dm_delay_deactivate` and
assert exit 0 to prove the running->idle transition end-to-end. Skip if it adds
noticeable flake; the idle-exit-0 path is already covered by the "exits 0 when
pool is idle" subtest.

## Reused patterns (do not reinvent)

- `tests/module/dm_delay_helpers.py` -- `dm_delay_create` / `dm_delay_activate` /
  `dm_delay_deactivate`; backing device is `/dev/disk/by-id/virtio-{name}`,
  exposes `/dev/disk/by-id/braid-test-{name}-delay`.
- `tests/module/scrub-lifecycle.py` -- dm-delay scrub-hold + poll-until-running
  loop (`setup_resume_pool`, the `seq 1 400` loop).
- `tests/cli/braid-status-during-balance.nix` -- the `dm_delay_helpers.py`
  concat + `pkgs.lvm2` wiring for a `cli/` test in this exact lane.
- `tests/repro/btrfs-replace-rejected-during-scrub.py` -- scrub-daemon stdio
  redirect rationale; record of why `scrub_speed_max` is unsuitable.
- Assertion target: `cli/src/idle.rs#cmd_idle` (`ScrubState::Running` ->
  `BusyReason::ScrubRunning`) and `cli/src/main.rs` Idle dispatch
  (`println!("busy: {reason}")` + exit 1).

## Verification

1. `just test-vm braid-idle` -- focused run; the new subtest must pass
   deterministically (no `exit_code in [0,1]` tolerance left).
2. **Coverage proof (manual, revert after):** temporarily change the
   `ScrubState::Running { .. } => IdleResult::Busy(...)` arm in
   `cli/src/idle.rs#cmd_idle` to return `IdleResult::Idle`, rerun
   `just test-vm braid-idle`, and confirm the new subtest now FAILS (exit 0 / no
   `busy: scrub running`). Revert. This proves the test exercises the live
   busy-scrub wiring, not just a fixture.
3. `just test-parsers` -- confirm the full parser-canary lane still passes
   (broader blast radius since the pool-setup block changed).

## Out of scope

- No production code change (`cli/src/idle.rs`, parser, `cmd.rs` unchanged).
- No fixture changes (`btrfs-scrub-running.txt` stays as captured by
  `progress-monitoring.py`; the unit test `busy_when_scrub_running` stays).
- No `scrub_speed_max` rate-limiting (rejected -- unreliable per the repro).
