# Memory Index

## Project Context
- [project_nas_build.md](project_nas_build.md) — Dan's NAS build: IMB-X1231, i3-14100, ECC, 4x12TB Toshiba N300

## Feedback
- [feedback_check_remote_before_rewrite.md](feedback_check_remote_before_rewrite.md) — Always check if commits are pushed before advising history rewrites
- [feedback_test_isolation.md](feedback_test_isolation.md) — In eval-time tests, disable unrelated features rather than stubbing NixOS options
- [feedback_nixos_test_fstrings.md](feedback_nixos_test_fstrings.md) — NixOS VM test framework rejects Python f-strings without placeholders
- [feedback_invariants_at_right_layer.md](feedback_invariants_at_right_layer.md) — Put guards at the layer that owns the invariant, not downstream consumers
- [feedback_dont_change_api_for_tests.md](feedback_dont_change_api_for_tests.md) — Don't warp command signatures just for testability; test the helper directly
- [feedback_check_vendored_source.md](feedback_check_vendored_source.md) — Check vendored reference/ source before assuming JSON schemas or feature availability
- [feedback_flag_required_followups.md](feedback_flag_required_followups.md) — Proactively flag required follow-up actions (like fixture capture) at end of implementation
- [feedback_plan_lifecycle.md](feedback_plan_lifecycle.md) — Move plans/wip → plans/impl as final implementation step, same commit
