# Fix: document the standalone-CLI skip for the `braid_online_active` doctor check

## Context

The `braid doctor` check `braid_online_active` (`cli/src/doctor.rs#check_braid_online_active_when_mounted`)
skips with `"skipped (systemd_lifecycle not configured -- braid-online is not Rust-managed)"`
on **standalone CLI installs** -- i.e. braid run without the NixOS module, where the
generated `systemd_lifecycle = true` flag (`modules/braid/cli.nix`) is absent and there is no
braid-managed `braid-online.service` to verify. (The module hardcodes the flag on, so
module-managed installs never hit this skip; only a standalone CLI / hand-written `--config`
JSON can.)

The authoritative command reference at `docs/commands/doctor.md:84` describes the check as
running "With UPS enabled and the pool mounted" -- omitting that third gate. A standalone-CLI
operator who meets both stated conditions sees a Skip and cannot reconcile it with the doc.

The fix is a one-sentence addition to that row, reusing vocabulary the project already uses
in its sibling command references.

## The fix

**File:** `docs/commands/doctor.md`, the `braid_online_active` row in the "What it checks" table (line ~84).

Append a standalone-CLI skip clause, mirroring the existing phrasing in `docs/commands/lock.md`
("Standalone CLI installs (no NixOS module) skip all three -- there is no `braid-online.service`
or scrub unit to stop.") and `docs/commands/unlock.md` ("Standalone CLI installs (no NixOS module)
skip this -- there is no `braid-online.service` to activate.").

- **Before:**
  `` | `braid_online_active` | With UPS enabled and the pool mounted, `braid-online.service` is active so shutdown unmounts the pool | ``
- **After:**
  `` | `braid_online_active` | With UPS enabled and the pool mounted, `braid-online.service` is active so shutdown unmounts the pool. Standalone CLI installs (no NixOS module) skip this -- there is no `braid-online.service` to verify. | ``

Constraints:
- Use `--` (not an em-dash), matching the existing doc prose style.
- Do **not** name the internal `systemd_lifecycle` flag -- it is ADR-only vocabulary
  (`docs/design/decisions/018-systemd-lifecycle.md`, `026-pool-lock-rust-owned.md`) and not a
  user-settable NixOS option. "Standalone CLI installs (no NixOS module)" is the established
  end-user term.
- Keep it on one table-cell line (no literal `|` inside the cell).

## Why this scope and not wider

- **doctor.md is the deployment-agnostic reference** read by both module and standalone-CLI
  users, and it is where the inaccurate positive claim lives. Fixing it follows the exact pattern
  already set by the `lock.md` / `unlock.md` command references.
- **`docs/guides/ups.md` is intentionally left unchanged.** It is a module-scoped task guide
  (premised on `braid.ups.enable`); its "braid-online" bullet (`ups.md:160-164`) states the
  *fail* condition accurately and makes no precondition claim. The project pattern is that command
  references carry the standalone-CLI distinction and task guides do not -- adding it here would
  introduce a deployment mode the guide otherwise never discusses.
- **`docs/commands/doctor.md:107`** (the "under the hood" flow summary) is generic prose with no
  per-check precondition claim -- left as-is.

## Out of scope (flagged, not fixed here)

`docs/guides/ups.md:166` ("Both checks skip when UPS support is disabled") is also incomplete for
`braid-online`: for the guide's module audience the missing skip is **pool-not-mounted**, not
standalone-CLI. This is a separate, pre-existing, milder gap that the finding did not raise; fold
it in only if you want a comprehensive skip-condition pass on the UPS guide as a follow-up.

## Verification

1. **Behavior already matches the new doc** -- no code change. The skip is locked by the existing
   Rust test `cli/src/doctor.rs#braid_online_check_skips_when_lifecycle_disabled`
   (scenario: "hand-written config.json has ups but omits systemd_lifecycle"). No test edit needed;
   confirm it still passes with `just test-rust` if desired.
2. **Docs build / linkcheck:** run `mdbook build docs` -- the edit adds no links, so this only
   confirms nothing else broke.
3. **Consistency grep:** `rg "Standalone CLI installs \(no NixOS module\)" docs/` should now list
   `doctor.md` alongside `lock.md` and `unlock.md`, with parallel phrasing.
4. **Visual scan:** confirm the `braid_online_active` row renders as a single table cell and reads
   cleanly next to the unchanged `ups_daemon` row above it.

## Follow Up

- `docs/guides/ups.md:166` ("Both checks skip when UPS support is disabled") is incomplete for the
  `braid_online_active` check: for that guide's module audience the additional skip is
  pool-not-mounted, not standalone-CLI. Pre-existing, milder gap left untouched here (the finding
  did not raise it); fold in only as part of a comprehensive skip-condition pass on the UPS guide.
