# Brainstorm: `braid recover --abort`

## Context

Today, `braid recover` assumes there is a real pool to reconcile back into
`pool.json`. That breaks down for one specific bootstrap failure:

- first-ever `braid add`
- LUKS format completed
- crash happened before `mkfs.btrfs`
- `pending-op.json` exists
- there is no btrfs filesystem yet

In that state, `braid recover` cannot succeed because there is nothing to
mount. The current improvement path is to detect this case and print a much
better error. That is useful, but it still leaves cleanup as a manual sequence
of `rm` + wipe commands.

## UX idea

Add an explicit abort path:

```text
braid recover --abort
```

Intent:

- `braid recover` = finish / reconcile an interrupted operation
- `braid recover --abort` = discard an interrupted operation and return to the
  pre-operation state when braid can prove that is safe

This is a better UX than embedding manual cleanup steps in an error message.

## Safest initial scope

Limit `--abort` to the bootstrap interrupted-add case only:

- `journal.pre_membership` is empty
- `journal.op` is `Add`
- target disks were newly initialized for this aborted operation
- recovery failed because the filesystem does not exist yet

Why this narrow scope first:

- the rollback target is unambiguous: "no pool exists"
- there is no live btrfs state to reconcile
- braid can explain exactly what destructive action it will take
- it avoids inventing generic rollback semantics for partially completed
  remove/replace operations

## Proposed user flow

1. User runs `braid recover`
2. braid detects: bootstrap add, no existing pool, mount step failed
3. braid prints a focused message like:

   ```text
   bootstrap add was interrupted before the filesystem was created.
   no pool exists yet, so normal recovery cannot continue.

   to discard this interrupted add, run:
     braid recover --abort
   ```

4. User runs `braid recover --abort`
5. braid prints what it will destroy and asks for confirmation
6. After confirmation, braid wipes the affected disk(s), clears
   `pending-op.json`, and exits back to a clean pre-bootstrap state

## What `--abort` should probably do

For bootstrap interrupted add only:

- load and validate `pending-op.json`
- verify this is an allowed abort case
- enumerate only the disks introduced by the interrupted add
- print a destructive confirmation prompt that names those by-id paths
- wipe the partially initialized disk state
- clear `pending-op.json`
- do not write `pool.json`

Likely wipe target:

- remove braid-created signatures so the next `braid add` starts from a clean
  disk
- in practice this probably means `wipefs -a` on the added disk(s), but the
  exact wipe contract should be designed deliberately rather than copied from an
  error-message example

## Important safety constraints

- `--abort` must never be a generic "delete the journal" escape hatch
- `--abort` must refuse if braid cannot prove the rollback is safe
- non-bootstrap interrupted operations should continue to use normal recovery
  unless explicit abort semantics are separately designed
- the confirmation must name exact disks and be clearly destructive
- braid should prefer refusing over guessing

## Why not broaden it immediately

Generic abort semantics get complicated quickly:

- interrupted add on an existing pool may have already changed live btrfs state
- interrupted remove / remove-missing / replace may need pool inspection to know
  what is reversible
- "clear the journal and hope" would violate the journal/recovery safety model

So the better path is:

- phase 1: bootstrap-only `recover --abort`
- phase 2: reconsider broader abort support only with explicit semantics per
  operation kind

## Open design questions

- Should `braid recover` detect the bootstrap case before or after attempting
  the mount?
- Should `--abort` require a dedicated `--yes` flag in addition to an
  interactive confirmation?
- What exact wipe operation best matches braid's safety expectations:
  `wipefs -a`, explicit LUKS header erase, or something more constrained?
- Should `--abort` leave any breadcrumb in stderr/journalctl explaining what was
  discarded?
- Should the command be `braid recover --abort`, or is a separate top-level
  command like `braid abort` clearer?

## Recommended next implementation plan

If this is picked up later, the simplest robust sequence is:

1. Land the current bootstrap detection and improved `recover` error first
2. Add a small follow-up plan for bootstrap-only `recover --abort`
3. Define the exact abort invariants and destructive confirmation contract
4. Add tests for:
   - allowed bootstrap abort
   - refusal outside bootstrap
   - refusal when disks do not match the journal
   - journal cleared only after successful cleanup
