# Plan: Unify Recover Non-Braid Mapper Skip Notes

## Summary

Fix the recover preview/execute mismatch by making non-`braid-` live-pool mapper skips a shared `PreviewNote` decision. Keep the pivot narrow: classify and render mapper skip notes consistently, without moving recovered-membership construction or by-id resolution into dry-run planning.

## Key Changes

- Add recover-local helpers near the live-pool helpers in `cli/src/recover.rs`:
  - `non_braid_mapper_skip_note(&MapperName) -> PreviewNote`
  - `non_braid_mapper_skip_notes(&PoolState) -> Vec<PreviewNote>`
  - `emit_non_braid_mapper_skip_note(&MapperName)`
- Use existing `PreviewNote::PerDisk { level: NoteLevel::Skip }` rather than adding a new preview enum variant.
  - Exact note shape: `name = mapper.0`, `message = "no braid- prefix"`.
  - Rendered dry-run and execute line: `[skip] disk <mapper>: no braid- prefix`.
- Make `emit_non_braid_mapper_skip_note` render a one-note slice through `preview::render_notes_for_stderr_with(..., RecoverPlan::STDERR_STYLE, color_enabled_for_stderr())`, then write the resulting string through `status_tag::emit_status`.
  - This keeps the real stderr wording on the shared preview renderer while making unit tests use the existing `status_tag::testing` capture seam.
- In the already-mounted dry-run path of `plan_recover`, append `non_braid_mapper_skip_notes(&pool)` immediately after successful `probe_pool`, before mount read-only checks and validation.
  - This preserves the notes on `PlanFailure` if a later dry-run check fails.
- Replace the two direct `eprintln!("  skip: device ...")` branches in the recovery membership builders with `emit_non_braid_mapper_skip_note`.
  - Keep membership behavior unchanged: non-braid mappers are skipped, not adopted.
- Do not change `live_member_names` or `validate_live_members_allowed`.
  - This avoids duplicate skip output from repeated validation calls and keeps topology matching semantics stable.
- Add `///` comments for the new top-level helper functions per the repo doc-comment rule.

## Test Plan

- Add a dry-run unit test for an already-mounted pool whose probed live pool includes expected braid mappers plus `/dev/mapper/luks-foreign`.
  - Assert the preview contains `[skip] disk luks-foreign: no braid- prefix`.
  - Assert normal recover preview steps such as `write recovered pool.json` and `clear pending-op.json` still render.
- Add a dry-run already-mounted `PlanFailure` unit test using a probed live pool that includes `/dev/mapper/luks-foreign`, followed by a read-only mountinfo refusal.
  - Use the existing read-only mountinfo test shape (`MockFs::with_mounted_pool_ro_fs` or equivalent).
  - Render `failure.notes` with `preview::render_notes_for_stderr_with(..., RecoverPlan::STDERR_STYLE, false)` and assert it includes `[skip] disk luks-foreign: no braid- prefix`.
- Add focused stderr-capture tests for both recovery membership builders:
  - `build_membership_from_live_pool` emits the shared skip note and excludes the foreign mapper.
  - `recover_membership_matching_expected` emits the shared skip note and excludes the foreign mapper.
  - Capture output with `status_tag::testing::capture_with_color(false, ...)`; do not use process-wide stderr capture.
- Run `just test-rust`.

## Assumptions

- The output change from `  skip: device <mapper> has no braid- prefix` to `[skip] disk <mapper>: no braid- prefix` is acceptable because recover already routes plan notes through bracketed `PreviewNote` rendering.
- No README or decision-doc update is needed; this aligns with the existing dry-run preview model and does not change recovery semantics.
