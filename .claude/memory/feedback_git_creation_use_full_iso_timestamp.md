---
name: When ordering by git creation, use %aI not --date=short
description: Always use full ISO author timestamps (%aI) when sorting files by git creation date — --date=short truncates to day-only and silently loses sub-day ordering
type: feedback
---

When deriving file order from git creation history, use `git log --diff-filter=A --follow --format=%aI -- <file>` (full ISO author timestamp), NOT `git log ... --date=short` (truncates to YYYY-MM-DD).

**Why:** I planned an ADR rename that numbered files chronologically by git creation. I used `--date=short`, which collapsed 8 same-day files into a single date, then I alphabetically tiebroke all of them. But git preserved distinct sub-day timestamps for several of those files — `sane-defaults.md` was authored 1.5 hours before `nix-native.md`, which was 30 minutes before `disk-pool-management.md`. My alphabetical tiebreak baked the wrong order into the ADR IDs, defeating the rename's whole purpose of capturing real chronology. The user caught it on review.

**How to apply:**
- Use `%aI` (author ISO) for "when was this written," `%cI` (committer ISO) for "when was this applied." Author date is usually what you want for ordering.
- Sort by full timestamp first; only alphabetically tiebreak files that share a literal commit (same `%aI` to the second).
- Apply this to any task that orders, dedupes, or groups files by git creation: ADR numbering, changelog generation, "first introduced in" annotations.
- `--date=short` is fine for *display* but never for *sort keys*.
