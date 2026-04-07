# Memory Index

## Project Context
- [project_nas_build.md](project_nas_build.md) — Dan's NAS build: IMB-X1231, i3-14100, ECC, 4x12TB Toshiba N300

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
