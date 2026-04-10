# Memory Index

## Project Context
- [project_nas_build.md](project_nas_build.md) — Dan's NAS build: IMB-X1231, i3-14100, ECC, 4x12TB Toshiba N300

## Reference
- [reference_just_test_repro_prefix.md](reference_just_test_repro_prefix.md) — `just test-repro <name>` requires the full `repro-` prefix; the justfile does not strip it
- [reference_nixos_test_set_e_pipefail.md](reference_nixos_test_set_e_pipefail.md) — NixOS test driver wraps every machine.succeed/execute command with `set -euo pipefail`; use `cmd || ec=$?` to capture expected non-zero exits without aborting the chain

## Feedback
- [feedback_check_remote_before_rewrite.md](feedback_check_remote_before_rewrite.md) — Always check if commits are pushed before advising history rewrites
- [feedback_test_isolation.md](feedback_test_isolation.md) — In eval-time tests, disable unrelated features rather than stubbing NixOS options
- [feedback_nixos_test_fstrings.md](feedback_nixos_test_fstrings.md) — NixOS VM test framework rejects Python f-strings without placeholders
- [feedback_invariants_at_right_layer.md](feedback_invariants_at_right_layer.md) — Put guards at the layer that owns the invariant, not downstream consumers
- [feedback_acquire_env_before_journal.md](feedback_acquire_env_before_journal.md) — Acquire environment-side resources (locks, inhibitors, dbus handshakes) before journal::write_journal, not after
- [feedback_no_diagnostic_refinements_in_mutation_paths.md](feedback_no_diagnostic_refinements_in_mutation_paths.md) — Don't widen probe-result enums into mutation paths if the new distinction only matters for diagnostics
- [feedback_dont_change_api_for_tests.md](feedback_dont_change_api_for_tests.md) — Don't warp command signatures just for testability; test the helper directly
- [feedback_check_vendored_source.md](feedback_check_vendored_source.md) — Check vendored reference/ source before assuming JSON schemas or feature availability
- [feedback_consult_principles_md.md](feedback_consult_principles_md.md) — Read docs/principles.md before plans touching mounts, balance/replace, recovery, or safety model
- [feedback_doc_warnings_are_not_universal.md](feedback_doc_warnings_are_not_universal.md) — Doc warnings about new behavior describe new code paths, not universal changes — verify which path is affected before treating as comprehensive
- [feedback_no_local_luksheader_in_recovery_messages.md](feedback_no_local_luksheader_in_recovery_messages.md) — User-facing recovery messages must not reference local /var/lib/braid/luks-headers/ files; use generic off-system backup language
- [feedback_flag_required_followups.md](feedback_flag_required_followups.md) — Proactively flag required follow-up actions (like fixture capture) at end of implementation
- [feedback_recipe_tests_use_user_facing_commands.md](feedback_recipe_tests_use_user_facing_commands.md) — Verify each recipe step against current code (cmd_*/plan_*) before committing to a recovery/cleanup plan
- [feedback_focused_repro_tests_no_bundling.md](feedback_focused_repro_tests_no_bundling.md) — Don't bundle tangential concerns into focused repro tests, even if the source issue lists them; point at existing dedicated coverage instead
- [feedback_vm_verify_kernel_async_assumptions.md](feedback_vm_verify_kernel_async_assumptions.md) — Always run the VM repro test before declaring kernel-async/mount-state fixes done; unit tests can pass while the kernel state machine still loses
- [feedback_dont_overclaim_refactor_benefits.md](feedback_dont_overclaim_refactor_benefits.md) — Only claim invariant enforcement a refactor actually provides; `pub` field newtypes don't enforce anything
- [feedback_caller_specific_gating_belongs_at_callsites.md](feedback_caller_specific_gating_belongs_at_callsites.md) — When a "should we do X now?" rule depends on caller context, don't bake it into a shared helper — keep helpers pure and gate at each callsite
- [feedback_split_test_tree_mutations_from_docs.md](feedback_split_test_tree_mutations_from_docs.md) — Split test-tree mutations (file deletions, moves) out of docs-only patches and gate them on a mandatory `just test-vm` run, not optional verification
- [feedback_test_at_failure_layer.md](feedback_test_at_failure_layer.md) — A bug fix's primary test must FAIL when the bug is reintroduced — parser canaries don't catch wiring bugs
- [feedback_reuse_proven_vm_patterns.md](feedback_reuse_proven_vm_patterns.md) — Reuse existing canonical VM test setup patterns (missing disk, degraded mount, ENOSPC) instead of inventing new ones in a new test
- [feedback_audit_narrative_after_dropping_assumption.md](feedback_audit_narrative_after_dropping_assumption.md) — When a mid-edit drops a test assumption, audit the entire file's narrative for stale references, not just the lines being edited
- [feedback_callsite_sweep_via_grep.md](feedback_callsite_sweep_via_grep.md) — Callsite inventories for renames/refactors must come from `git ls-files`+grep, never from a hand-curated list
- [feedback_git_creation_use_full_iso_timestamp.md](feedback_git_creation_use_full_iso_timestamp.md) — Order files by git creation using `%aI` (full ISO author timestamp), never `--date=short`
- [feedback_check_local_before_web.md](feedback_check_local_before_web.md) — Clone repos or check local source before web searching for tool behavior
- [feedback_no_emdash.md](feedback_no_emdash.md) — Use double hyphens (`--`) instead of em dash characters
