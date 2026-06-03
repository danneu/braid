# Plan: clarify confirm-prompt hardware fields are best-effort

## Context

`docs/commands/remove.md` step 3 lists the confirmation prompt's fields as
"name, model, size, serial, devid" as if all five always render. They do not.
The hardware fragment (model/size/serial) is probed live from the backing
device via lsblk and is **conditional**: `format_hw_info_line`
(`cli/src/confirm.rs#format_hw_info_line`) returns `None` when lsblk yields no
model/size/serial, and `format_remove_confirm`
(`cli/src/remove.rs#format_remove_confirm`) then prints only the disk name. An
operator who sees a bare name line (e.g. a drifted/odd backing path) can wonder
whether braid lost track of the disk. Only the disk **name** and **devid** are
unconditional.

`docs/commands/add.md` step 2 has the same root cause: it says "disk model,
serial, and size", but `format_add_confirm` (`cli/src/add.rs#format_add_confirm`)
always prints `name  by-id-path` and gates the hardware line behind the same
`format_hw_info_line`. So add.md (a) never mentions the always-shown name +
by-id anchors and (b) overstates hardware presence. Same helper, same gap --
fix both in one pass for consistency.

Goal: make the two command-reference docs state that model/size/serial are
best-effort from the live device and may be omitted, while naming the fields
that always appear.

## Edits

### 1. `docs/commands/remove.md` (step 3 of "What happens under the hood")

Before:
```
3. Shows a confirmation prompt with the disk's name, model, size, serial, devid, and the resulting disk count (e.g. `Pool: 3 disks -> 2 disks`)
```

After:
```
3. Shows a confirmation prompt with the disk's name and devid, its model/size/serial (best-effort from the live backing device via lsblk -- omitted if unavailable), and the resulting disk count (e.g. `Pool: 3 disks -> 2 disks`)
```

### 2. `docs/commands/add.md` (step 2 of "What happens under the hood")

Before:
```
2. Shows a confirmation prompt with disk model, serial, and size
```

After:
```
2. Shows a confirmation prompt with the disk's name and by-id path, plus its model/size/serial (best-effort from the live device via lsblk -- omitted if unavailable)
```

## Wording rationale

- **Parallel structure.** Both lines share the clause
  `(best-effort from the live ... device via lsblk -- omitted if unavailable)`.
  remove says "live backing device" (the block device behind the LUKS mapper,
  `work_plan.target_underlying`); add says "live device" (the raw by-id device
  the operator named). The distinction is accurate to each call site.
- **Names the always-shown anchors.** remove -> name + devid; add -> name +
  by-id path. This is what stays on screen when lsblk returns nothing.
- **Incidental fixes in add.md.** The reword also (a) adds the missing name +
  by-id anchors add.md never documented and (b) corrects field order to
  `model/size/serial`, matching the order `format_hw_info_line` emits (add.md
  currently says "model, serial, and size").
- **ASCII only:** literal `--`, no em-dash, per repo CLI/writing style.

## Out of scope (considered, excluded)

- `docs/design/decisions/012-intent-cli.md` (Active) already credits the block
  as "(model, size, serial via lsblk)". It states design intent at ADR altitude
  and does not claim the fields always render; the best-effort nuance belongs in
  the command reference, not the decision doc. Leave unchanged.
- `README.md` confirm example (the `ironwolf ... | serial ZL2A1B2C` block) is a
  cookbook example of the populated common case -- correct as shown. README
  style is deliberately brief, not reference-grade. Leave unchanged.
- `docs/commands/status.md` model/serial listing is a different command and
  render path (status output, not the confirm prompt) -- not this root cause.
- `docs/commands/remove-missing.md` step 4 already correctly omits hardware
  fields (a missing disk has no live device to probe) -- no change; it is the
  model the two edited docs are being brought in line with.

## Files

- `docs/commands/remove.md` -- 1 line (step 3)
- `docs/commands/add.md` -- 1 line (step 2)

No code changes; behavior is already correct. This aligns the docs to the code.

## Verification

- `mdbook build docs` succeeds. Edits are prose-only with no link
  additions/renames, so `mdbook-linkcheck2` is unaffected, but the build is the
  canonical doc check and confirms nothing broke.
- Re-read both edited lines against the actual emitters
  (`cli/src/remove.rs#format_remove_confirm`,
  `cli/src/add.rs#format_add_confirm`) to confirm the always-shown vs.
  best-effort split matches: name + devid (remove) / name + by-id (add) are
  unconditional; the `format_hw_info_line` fragment is the only optional part.
- No Rust/VM tests apply (prose-only change).
