# Salvage: document + guard the monitor's statx mount gate

## Context

An `/ultrareview` finding (Low / Project fit) claimed that
`unitConfig.ConditionPathIsMountPoint = cfg.mountPoint` on
`braid-monitor.service` (`modules/braid/monitor.nix:125`) "pre-empts the
fail-closed mountinfo path that ADR 014 specifies must beep," and asked for a
doc note recording that the service condition trades the fail-closed beep for
an offline-skip.

Verification showed the **headline claim is wrong**, so the finding's proposed
note must not be written verbatim (it would enshrine a false divergence):

- systemd's `ConditionPathIsMountPoint` resolves through
  `statx(STATX_ATTR_MOUNT_ROOT)` -> `name_to_handle_at(2)` -> `/proc/self/fdinfo`
  (`reference/systemd/src/shared/condition.c:953-958`,
  `reference/systemd/src/basic/mountpoint-util.c` `is_mount_point_at`). It is a
  kernel VFS query and **never parses `/proc/self/mountinfo` text**.
- braid's fail-closed path parses that text and latches `ComputationError` on a
  malformed line / duplicate target / read error
  (`cli/src/mount_check.rs:5,62-77`; contract at
  `docs/design/decisions/014-alerts.md:91`).
- The two checks have **disjoint failure modes**. On a genuinely-mounted pool,
  `statx` reports a mount root regardless of any mountinfo text anomaly, so the
  service runs and the fail-closed beep fires. The gate only short-circuits a
  `statx`-confirmed-offline pool; the sole beep it suppresses is braid's
  conservative `ComputationError` on an *offline* pool whose mountinfo text is
  anomalous -- not a disk-health alert. The protective beep is never gated away.

The gate is deliberate (commit `34c4849c` replaced `BindsTo + After
mnt-storage.mount`, which threw "Unit not found" every 5 min pre-unlock) and is
documented at `docs/design/decisions/018-systemd-lifecycle.md:96,98`.

**The real correctness exposure** is not a runtime bug -- it is that a future
maintainer could read ADR 014's fail-closed contract next to
`monitor.nix:125`, repeat the finding's reasoning, and *delete the gate*.
Today that deletion passes the whole suite silently: `monitor-lifecycle.py`
subtests 2 and 8 ("no alert side effects before/after mount") pass with or
without the gate, because an offline pool with well-formed mountinfo exits 0 ->
no beep regardless. The scrub service already has a tripwire for its own gate
(`tests/module/auto-scrub.py:101-108`); the monitor has none.

Intended outcome: explain why the gate is sound (so the misread doesn't
recur), guard the edit site, and make a future deletion fail CI instead of
passing silently.

## The fix (docs + code comment + test tripwire)

Three small, additive changes. No runtime behavior changes.

### 1. ADR 018 -- canonical rationale (`docs/design/decisions/018-systemd-lifecycle.md`)

Add a new bullet to the `braid-monitor.timer + braid-monitor.service` section,
immediately after the existing fail-closed bullet (currently line 98), so it
follows the fail-closed explanation it ties back to:

```markdown
- **The gate and the fail-closed path are independent mount checks, so the gate
  cannot mask a real alert.** `ConditionPathIsMountPoint` resolves through
  `statx(STATX_ATTR_MOUNT_ROOT)` (then `name_to_handle_at(2)`, then
  `/proc/self/fdinfo`) -- a kernel VFS query, never a parse of
  `/proc/self/mountinfo` text. The fail-closed path above instead parses that
  text and latches `ComputationError` on a malformed line, duplicate target, or
  read error. On a genuinely-mounted pool `statx` reports a mount root
  regardless of any text anomaly, so the service runs and the beep fires -- the
  protective beep is never gated away. The gate only short-circuits a
  `statx`-confirmed-offline pool; the sole beep it suppresses is braid's
  conservative `ComputationError` on an *offline* pool with anomalous mountinfo
  text, which is not a disk-health alert.
```

Leave the `status: Active` frontmatter unchanged (the decision is not being
revised). No new heading/anchor -> no `mdbook-linkcheck` exposure.

### 2. `modules/braid/monitor.nix` -- guard at the edit site

Add a short comment directly under the gate (line 125), matching the file's
existing explanatory-comment density:

```nix
unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
# statx-based gate (STATX_ATTR_MOUNT_ROOT), independent of the
# /proc/self/mountinfo parse `braid monitor` fails closed on -- skips
# only a confirmed-offline pool, never the mounted-but-anomalous beep.
# Keep it: removal means wasteful 5-min offline runs. See ADR 018.
```

### 3. `tests/module/monitor-lifecycle.py` -- regression tripwire

Add a subtest after subtest 1 ("Monitor timer is active at boot", line 27-28),
mirroring `auto-scrub.py:101-108` (which uses `systemctl cat`):

```python
with subtest("braid-monitor.service carries the statx mount-point gate"):
    # Regression tripwire. The gate is a statx(STATX_ATTR_MOUNT_ROOT) check,
    # independent of the /proc/self/mountinfo parse `braid monitor` fails
    # closed on, so it skips only a confirmed-offline pool and never masks the
    # mounted-but-anomalous beep (see ADR 018). Subtests 2/8 below pass with or
    # without the gate (offline -> exit 0 -> no beep), so without this
    # assertion the gate could be deleted silently. Mirrors auto-scrub.py.
    unit = machine.succeed("systemctl cat braid-monitor.service")
    assert "ConditionPathIsMountPoint=/mnt/storage" in unit, (
        "braid-monitor.service must carry ConditionPathIsMountPoint; got:\n"
        + unit
    )
```

Reuse, not new infrastructure: the assertion copies the established
`auto-scrub.py` unit-text pattern; no helper or fixture is added.

## Critical files

- `docs/design/decisions/018-systemd-lifecycle.md` -- add bullet (after ~line 98).
- `modules/braid/monitor.nix:125` -- add comment under the gate.
- `tests/module/monitor-lifecycle.py` -- add subtest after ~line 28.

`docs/design/decisions/014-alerts.md` is intentionally **not** touched: line 91
is correctly scoped to `cmd_monitor` (the command), line 97 is precise
("No mount condition on the *timer*"), and adding systemd-unit mechanics there
would blur the ADR-014 (alert model) / ADR-018 (systemd unit) boundary.

## Verification

- `just test-vm monitor-lifecycle` -- new subtest passes; sanity-check it fails
  if the gate line is temporarily removed from `monitor.nix` (confirms the
  tripwire actually guards the gate). This is a focused, localized change, so a
  single-test run is sufficient; no full-suite run needed.
- `mdbook build docs` -- ADR 018 edit renders and `mdbook-linkcheck` passes (no
  new cross-links were introduced).
- No Rust or parser changes -> no `just test-rust` / fixture refresh needed.

## Rejected alternatives

- **Remove the gate** (the finding's implied direction): regresses commit
  `34c4849c`, adds wasteful 5-min offline spawns, and reintroduces the
  conservative false-alarm beep on offline-but-anomalous mountinfo. The gate is
  the more authoritative offline check; keep it.
- **Write the finding's note verbatim** ("condition trades the fail-closed beep
  for offline-skip"): documents a divergence that does not exist and would
  mislead future readers into thinking malformed mountinfo can silently skip the
  monitor.
- **Behavioral VM test of mounted-but-anomalous mountinfo**: infeasible --
  `/proc/self/mountinfo` is kernel-generated and cannot be corrupted while
  genuinely mounted. The unit-text tripwire (with precedent) is the pragmatic
  guard.
- **ADR 014 cross-reference**: marginal discoverability, guards no additional
  regression vector, and blurs the ADR boundary (see above).
