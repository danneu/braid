---
name: Check vendored reference source for upstream behavior
description: Always check vendored btrfs-progs/upstream source in reference/ before assuming command output schemas or feature availability
type: feedback
---

Don't invent JSON schemas or assume command behavior — check the vendored upstream source in `reference/` first.

**Why:** Plan v1 invented a JSON schema for `btrfs device stats` that got the missing-disk representation wrong (`<missing disk>` vs actual `devid:<n>`) and assumed `subvolume list --format json` was available when it's gated behind `#if EXPERIMENTAL`. Both could have been caught by reading the vendored source.

**How to apply:** Before writing serde structs or assuming a btrfs-progs feature is available, read the relevant source file in `reference/btrfs-progs/`. Check for `#if EXPERIMENTAL` guards, `fmt_print` calls (to understand JSON field types), and sentinel values.
