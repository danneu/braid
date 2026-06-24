# Plan: document the longer systemd-shutdown umount retry budget in lock.md

## Context

`docs/commands/lock.md` describes `umount` as "retrying up to 3 times" (step 3,
line 40) and "after 3 retry attempts" (Error handling, bullet 2, line 62). That
count is accurate only for the user-typed `braid lock` (`LockMode::User` ->
`UMOUNT_RETRY_ATTEMPTS = 3`). The systemd-shutdown `ExecStop` path
(`braid lock --systemd-stop`, `LockMode::SystemdStop`) uses
`SYSTEMD_STOP_UMOUNT_RETRY_ATTEMPTS = 60` at a 500ms delay -- about 30 seconds of
busy retries (`cli/src/lock.rs:20-30`, selected at `cli/src/lock.rs:949-952`).

The divergence is intentional and load-bearing per
[ADR 018](../design/decisions/018-systemd-lifecycle.md#execstop-bounded-wait-pattern):
during shutdown a `btrfs balance` userspace process blocked in
`BTRFS_IOC_BALANCE_V2` can briefly outlive the Rust parent and hold the mount fd,
so the longer budget keeps a transient post-kill hold from stranding the pool with
LUKS still open. It is observable in shutdown logs.

An operator debugging a slow shutdown sees ~30s of umount-busy retries that the
cookbook says cannot happen, and the cookbook gives no way to reconcile the two.
The divergence is currently documented only in ADR 018 (design/authority) and the
in-code comment -- not in any user-facing guide or command page.

**Intended outcome:** `lock.md` scopes its "3 times" claim to the user-invoked
command and notes that the shutdown `ExecStop` path retries far longer, with a
cross-link to ADR 018. No code or behavior change. A future operator (or reviewer)
reading the cookbook can reconcile observed shutdown behavior and does not file the
same finding.

## Change

Single file: `docs/commands/lock.md`. Two edits.

### Edit 1 -- "What happens under the hood" step 3 (line 40)

Append a sentence scoping the count and noting the shutdown divergence + ADR link.

Before:

```
3. Unmounts the btrfs filesystem, retrying up to 3 times if the device is busy (covers the brief race after stopping SMB/NFS consumers, where the kernel has not yet released the last file descriptors)
```

After (finalize wording at implementation; ASCII only):

```
3. Unmounts the btrfs filesystem, retrying up to 3 times if the device is busy (covers the brief race after stopping SMB/NFS consumers, where the kernel has not yet released the last file descriptors). During systemd shutdown the `ExecStop` path retries far longer -- 60 attempts over roughly 30 seconds -- so a btrfs-progs process that still briefly holds the mount fd after its parent is killed does not strand the pool with LUKS open; see [ADR-018](../design/decisions/018-systemd-lifecycle.md#execstop-bounded-wait-pattern).
```

### Edit 2 -- "Error handling" bullet 2 (line 62)

Make the count mode-agnostic so step 3 stays the single source of truth for the
numbers and the bullet no longer contradicts the shutdown path. The consequence it
describes (skip `forget`, still attempt close, report) is the same in both modes --
only the retry count differs -- so the bullet only needs to drop the literal `3`.

Before:

```
- If unmount fails after 3 retry attempts (e.g. a process has files open on the mount), lock skips `btrfs device scan --forget` and still attempts to close the LUKS mappers, reporting the failure
```

After:

```
- If unmount fails after exhausting its umount busy-retry budget (e.g. a process has files open on the mount), lock skips `btrfs device scan --forget` and still attempts to close the LUKS mappers, reporting the failure
```

## Conventions / constraints

- ASCII only: `--`, straight quotes, `...`. `lock.md` is already ASCII (verified).
- Cross-link by `path#heading-slug`, never a line number (per
  `docs/dev/doc-citations.md`). Anchor `#execstop-bounded-wait-pattern` resolves to
  the `### ExecStop bounded-wait pattern` heading in ADR 018
  (`docs/design/decisions/018-systemd-lifecycle.md:236`).
- Link style matches the existing `lock.md` usage (`See [ADR-024](...#anchor)` at
  line 65): bare `ADR-NNN` text, relative path, trailing anchor.
- Do not present `--systemd-stop` as a user knob -- it is a hidden flag
  (`cli/src/main.rs:272`). Refer to "systemd shutdown" / the `ExecStop` path only.

## Out of scope (deliberately)

- **Step 5 mapper-close "3 times" (line 42)** -- left untouched. `CLOSE_RETRY_ATTEMPTS = 3`
  (`cli/src/mapper_close.rs:7`) is mode-independent; mapper close does not get the
  longer budget, so "3 times" is accurate in both modes. The divergence is umount-only.
- **README.md** -- no edit. Its `lock` mention (`README.md:124`) is high-level and
  carries no retry-budget detail; the count specifics belong in the command reference.
- **Sibling guides/command docs** -- no edits. Only `lock.md` carries the unqualified
  "3 times" claim; `nixos-configuration.md` documents the ExecStop *deadline*
  (correctly, a different knob), and ADR 018 already documents the umount divergence.
- **ADR 018 and the in-code comment** -- no edits. Both already state the correct
  model; this change brings the cookbook into line with them, not vice versa.
- **No test.** Pure user-facing doc prose; the behavior is already covered by Rust
  tests (`cli/src/lock.rs#systemd_stop_retries_busy_umount_beyond_user_attempts`)
  and the anchor is enforced by the docs link-check below.

## Verification

1. `just docs-build` -- builds the mdBook and runs `mdbook-linkcheck2`
   (`docs/book.toml:11-12`, `justfile:275-277`); a broken `#execstop-bounded-wait-pattern`
   anchor fails the build. This is the load-bearing check.
2. `rg "retrying up to 3 times" docs/commands/lock.md` -- still returns step 3 (the
   user-path count is preserved) and step 5 (mapper close, intentionally unchanged);
   `rg "after 3 retry attempts" docs/commands/lock.md` returns nothing (Edit 2 landed).
3. `rg "execstop-bounded-wait-pattern" docs/commands/lock.md` returns the new link.
4. `scripts/docs/check-output-ascii.py` is scoped to `cli/src/**/*.rs` and
   `modules/**/*.nix`, so it does not gate this file; still keep the prose ASCII by
   eye (no em-dash, curly quotes, or `x`-as-multiply introduced).
5. Eyeball render: step 3 reads as "3 for the user command, ~30s during shutdown,"
   and the Error-handling bullet no longer names a count.

## Commit

`docs(lock): note longer umount retry budget on systemd shutdown`

(Matches the established `docs(lock):` pattern for this file, e.g. `4bdc36b8`,
`d0e297dd`, `eb111b68`.)
