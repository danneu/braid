# Serialize `braid discover` under the wrapper pool lock

## Context

`braid discover --write` is documented as the recovery path for a
missing or corrupt `pool.json` (decision 017, manual entry
`manual/commands/discover.md`). `cli/src/main.rs:712-719` enforces the
"refuses if pool.json already exists" contract via `pool_json.exists()`,
but the actual write (`save_membership` -> `state_io::atomic_write`,
`cli/src/main.rs:743` and `cli/src/state_io.rs:53-75`) only runs after
`discover_pool_members` finishes -- a delay of several `cryptsetup`
spawns (one `isLuks` + one `luksDump` per `/dev/disk/by-id/` entry).

`atomic_write` uses `OpenOptions::create(true).truncate(true)` on the
tmp file followed by `fs::rename`, which on POSIX silently replaces any
existing target. With no lock held across the exists-check -> scan ->
write sequence, two concurrent `discover --write` invocations (or one
discover racing an operator's `rm pool.json && braid discover --write`)
can both pass the `exists()` gate, both finish scanning, and the second
writer silently clobbers the first.

`discover --write` is the only `pool.json` membership writer (decision
017 enumerates six: `add`, `remove`, `replace`, `remove-missing`,
`discover --write`, `recover`) that the wrapper does not serialize.
The other five all sit in `modules/braid/braid-wrapper.sh`'s lock case
(`unlock|add|recover|remove|remove-missing|replace`). The pool lock is
wrapper-side per decision 018 line 144 ("the operation lock is acquired
in `modules/braid/braid-wrapper.sh` **before** the wrapper invokes the
Rust CLI") -- there is no flock code anywhere in `cli/src/`. The two
most recent additions to the wrapper case statement are commit
`ac219e4` (`replace`) and `3ee1674` (alert mutators); both follow the
same shape this plan uses.

Damage from the race is bounded -- pool.json content can be wrong but
not corrupt, LUKS labels remain authoritative for re-discovery -- but
the fix is small, the documented contract is clear, and the work fits
the established pattern. Severity: Low.

## Approach

Add `discover` to the wrapper's non-blocking fail-fast lock case
alongside the other pool mutators. Lock all `discover` invocations (not
just `--write`) -- the wrapper currently keys on subcommand name only,
and `discover` already calls `check_no_pending_operation` regardless of
`--write`, so the "you shouldn't be running this during another op"
semantic is already half-applied. Diagnostic `discover` during a long
pool op fails fast with the contention message; operators have
`braid status` for read-only inspection.

No changes to `cli/src/main.rs`, `cli/src/discover.rs`, or
`cli/src/state_io.rs`. Putting a lock in main.rs would duplicate the
wrapper's lock layer and contradict decision 018.

## Files to modify

1. **`modules/braid/braid-wrapper.sh`** (line 51 and comment on line 42)
   - Add `discover` to the `unlock|add|recover|remove|remove-missing|replace)` case so the wrapper acquires `/run/braid-pool.lock` non-blocking and fails fast with the existing "another braid operation is already in progress" message.
   - Update the per-command contention comment (around line 42-43) to list `discover` in the non-blocking fail-fast group.
   - The `skip_fixup` path (line 28) is unaffected: `discover` does not accept `--dry-run`, but the existing `--help`/`-h`/`--version`/`-V`/`--dry-run` guard still applies correctly if `discover --help` is ever invoked.

2. **`docs/principles.md`** (line 67)
   - Reword the Principle 12 opening so it no longer mislabels read-only `discover` as a mutator. Replace "Pool and alert-state mutators (`unlock`, `add`, ...)" with "Pool mutators, alert-state mutators, and `discover` (`unlock`, `add`, `recover`, `remove`, `remove-missing`, `replace`, `discover`, `ack`, `monitor`) ...". Add `discover` to the non-blocking fail-fast subset later in the sentence: "`unlock`, `add`, `recover`, `remove`, `remove-missing`, `replace`, and `discover`". Add a clause noting `discover` participates "because its scan -> `pool.json` write window must be serialized against pool-state mutators even though `discover` itself does not mutate pool state."

3. **`docs/decisions/018-systemd-lifecycle.md`** -- update **both** pool-lock paragraphs.
   - **Line 140 ("Pool lock mutual exclusion")**: same wording change as Principle 12 -- replace "Pool and alert-state mutators" with "Pool mutators, alert-state mutators, and `discover`", add `discover` to the enumerated list and to the non-blocking fail-fast subset, and note the read-only-but-serialized rationale once.
   - **Line 144 ("Lock acquisition site")**: replace "For non-dry-run pool and alert-state mutators, the operation lock is acquired in ..." with wording that also covers `discover` -- e.g. "For non-dry-run pool mutators, alert-state mutators, and `discover`, the operation lock is acquired in ...". Keep the rest of the paragraph (fd 9 / `flock` mechanics, "config load, pool.json load, journal read, ...") unchanged.

4. **`manual/commands/discover.md`** ("Safety checks" section, around line 59)
   - Add a bullet: "Refuses if another braid operation is in progress (`/run/braid-pool.lock` is held by another wrapper)." Mirror the wording used for the other "refuses if..." bullets already present.

5. **`tests/module/pool-lock-discover-contention.nix`** (new file)
   - Copy `tests/module/pool-lock-replace-contention.nix` as a template, but provision **one discoverable `braid-*` LUKS2 disk** via `tests/module/lib/initrd-fixture.nix` (which `luksFormat --label braid-<name>`s the empty virtio disk during initrd). Without a discoverable disk the test is degenerate: an unguarded `discover --write` would also exit non-zero with "no braid-labeled LUKS devices found" and leave no `pool.json`, so the contention assertion would pass even if the wrapper case were removed.
   - Fixture shape: `imports = [ ../../modules/braid (import ./lib/initrd-fixture.nix { passphrase = "testpassphrase"; diskNames = [ "disk1" ]; }) ];`, one `virtualisation.emptyDiskImages` entry with `serial = "disk1"`, `virtualisation.memorySize = 1024;`. The fixture leaves the disk LUKS-formatted and closed, exactly the post-initrd state `discover` expects to find under `/dev/disk/by-id/`.

6. **`tests/module/pool-lock-discover-contention.py`** (new file)
   - Copy `tests/module/pool-lock-replace-contention.py` as a template; replace the replace-specific setup and assertions with `braid discover` / `braid discover --write` invocations. Test body:
     1. `start_all()`, `wait_for_unit("multi-user.target")`.
     2. (Sanity precondition) `machine.succeed("test ! -e /var/lib/braid/pool.json")` to pin the "no prior pool.json" starting state; also `machine.succeed("test -L /dev/disk/by-id/virtio-disk1")` to confirm the fixture left the disk visible.
     3. **Spawn the background lock holder with `flock -x -o`**, not bare `flock -x`. `flock` forks the COMMAND; without `-o, --close` the child inherits the lock fd (`reference/util-linux/sys-utils/flock.c:430` -- the close-fd-in-child branch is gated by `do_close`, the manpage `flock.1.adoc:71-72` says "Close the file descriptor on which the lock is held before executing _command_. This is useful if _command_ spawns a child process which should not be holding the lock"). Without `-o`, `kill $holder_pid` leaves the orphaned `sh`/`sleep` child still owning the lock fd, and the positive-control discover at step 8 would falsely fail. Holder command: `nohup flock -x -o /run/braid-pool.lock sh -c 'touch /tmp/holder.ready; sleep 60' >/dev/null 2>&1 & echo $!`. Wait for `/tmp/holder.ready`, assert `FLOCK` appears in `/proc/locks`.
     4. **(`--write` contention)** `timeout 5 braid discover --write` -- capture rc + output. Assert `rc != 0`, `rc != 124`, `"another braid operation is already in progress" in out`, and `machine.fail("test -e /var/lib/braid/pool.json")`. With a real `braid-disk1` LUKS2 device present, the no-pool.json assertion is load-bearing: a missing/regressed wrapper case would let `discover --write` succeed and write `pool.json`.
     5. **(bare-`discover` contention)** `timeout 5 braid discover` (no `--write`) -- capture rc + output. Assert `rc != 0`, `rc != 124`, `"another braid operation is already in progress" in out`. Pins the agreed scope ("lock all `discover` invocations"); without this, an implementation that adds only a `discover-with-write` case would pass step 4 while contradicting the docs and the diagnostic-discover UX.
     6. Kill the holder: `machine.execute(f"kill {holder_pid} 2>/dev/null || true")`.
     7. **Wait for lock release** before the positive control: `machine.wait_until_succeeds("flock -n /run/braid-pool.lock true", timeout=10)`. This guards against any residual fd reference and gives a clean error if the holder process somehow didn't release.
     8. **(Positive control)** Run `braid discover --write` again and assert `machine.succeed("test -e /var/lib/braid/pool.json")`. Proves the disk was actually discoverable -- so a future regression that breaks the fixture (e.g. wrong label, missing initrd module) fails noisily here instead of producing a falsely-passing contention test in step 4.
   - Preamble: Intent / Why it exists / Scenario, in line with `tests/module/pool-lock-replace-contention.py` and the AGENTS.md test-conventions section. The "Why it exists" line should explicitly call out that both the `--write` and bare-`discover` cases are tested to pin the all-`discover`-invocations scope.

7. **`flake.nix`** (after the `pool-lock-replace-contention` entry around line 604-608)
   - Register `pool-lock-discover-contention` as a parallel `pkgs.testers.nixosTest` check, passing `braid = linuxCrane.braid-cli-unwrapped;` exactly like the replace entry.

## Reuse

- `state_io::atomic_write` (cli/src/state_io.rs:53) -- unchanged, behaves correctly under a lock.
- Wrapper's existing `flock -n 9` block and the "another braid operation is already in progress" string (modules/braid/braid-wrapper.sh:54-57) -- reused by adding `discover` to the case label.
- Test template `tests/module/pool-lock-replace-contention.{nix,py}` -- copied and trimmed.
- `tests/module/lib/initrd-fixture.nix` -- the shared LUKS-format-and-label fixture used by `raid1`, `degraded-raid1`, `single-disk`, and `no-silent-degraded`. Reused to seed exactly one `braid-disk1` LUKS2 disk for the contention test, so the post-contention "no `pool.json`" assertion is load-bearing.
- Decision 017 already lists `discover --write` among pool.json writers; no update needed there.

## Verification

1. `just test-rust` -- should still pass; no Rust source changes.
2. `nix flake check` or targeted: `just test-vm pool-lock-discover-contention` -- the new test must pass.
3. `just test-vm pool-lock-replace-contention braid-module-raid1 braid-module-degraded-raid1 braid-module-single-disk no-silent-degraded` -- existing discover-using tests (`raid1.py:27`, `degraded-raid1.py`, `single-disk.py`, `no-silent-degraded.py`) and the sibling lock-contention test must still pass, confirming the new wrapper case does not regress the success path.
4. Manual smoke (optional): boot a test VM, run `flock -x /run/braid-pool.lock sleep 30 &`, then `braid discover --write` -- expect exit 1 with `another braid operation is already in progress (pool lock /run/braid-pool.lock is held); retry once it finishes`; expect `/var/lib/braid/pool.json` absent.

## Out of scope

- Rust-side flock acquisition (would duplicate the wrapper layer, contradicts decision 018).
- Adding `O_EXCL` to `atomic_write` (this primitive's overwrite semantic is intentional for `pending-op.json` and other state files; defense-in-depth here would require a separate `atomic_create` and still wouldn't gate the multi-second scan).
- Subcommand-flag-aware locking in the wrapper (lock all `discover` is the agreed scope -- see open-question resolution).
- Concurrency hardening for `enroll-key-file` (LUKS-keyslot mutator, not currently in the lock list either -- separate concern, separate plan).
