# Memory Index

## Project Context
- [project_nas_build.md](project_nas_build.md) — Dan's NAS build: IMB-X1231, i3-14100, ECC, 4x12TB Toshiba N300

## Reference
- [reference_just_test_repro_prefix.md](reference_just_test_repro_prefix.md) — `just test-repro <name>` requires the full `repro-` prefix; the justfile does not strip it
- [reference_nixos_test_set_e_pipefail.md](reference_nixos_test_set_e_pipefail.md) — NixOS test driver wraps every machine.succeed/execute command with `set -euo pipefail`; use `cmd || ec=$?` to capture expected non-zero exits without aborting the chain

## Feedback
- [feedback_test_isolation.md](feedback_test_isolation.md) — In eval-time tests, disable unrelated features rather than stubbing NixOS options
- [feedback_nixos_test_fstrings.md](feedback_nixos_test_fstrings.md) — NixOS VM test framework rejects Python f-strings without placeholders
- [feedback_invariants_at_right_layer.md](feedback_invariants_at_right_layer.md) — Put guards at the layer that owns the invariant, not downstream consumers
- [feedback_acquire_env_before_journal.md](feedback_acquire_env_before_journal.md) — Acquire environment-side resources (locks, inhibitors, dbus handshakes) before journal::write_journal, not after
- [feedback_no_diagnostic_refinements_in_mutation_paths.md](feedback_no_diagnostic_refinements_in_mutation_paths.md) — Don't widen probe-result enums into mutation paths if the new distinction only matters for diagnostics
- [feedback_doc_warnings_are_not_universal.md](feedback_doc_warnings_are_not_universal.md) — Doc warnings about new behavior describe new code paths, not universal changes — verify which path is affected before treating as comprehensive
- [feedback_no_local_luksheader_in_recovery_messages.md](feedback_no_local_luksheader_in_recovery_messages.md) — User-facing recovery messages must not reference local /var/lib/braid/luks-headers/ files; use generic off-system backup language
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
- [feedback_ascii_scope_whole_file.md](feedback_ascii_scope_whole_file.md) — ASCII cleanup slices cover every occurrence in the file, not just user-facing strings; verify with a zero-match grep
- [feedback_distinct_post_commit_variants.md](feedback_distinct_post_commit_variants.md) — Split post-commit error variants by remediation, not layer; each step's failure needs a message naming the specific on-disk consequence, plus a forced-failure test
- [feedback_tests_bind_to_real_mapping.md](feedback_tests_bind_to_real_mapping.md) — Error-classification tests must bind to the real mapping helper, not a hand-built variant; extract `.map_err` into a named helper and test that
- [feedback_fail_closed_by_downstream_blast_radius.md](feedback_fail_closed_by_downstream_blast_radius.md) — A new arm of a shared preflight helper inherits *shape*, not *error policy* — each arm's fail-closed stance is set by what its downstream mutation does on miss
- [feedback_no_fs_exists_gate_before_source_of_truth.md](feedback_no_fs_exists_gate_before_source_of_truth.md) — Don't gate a source-of-truth query behind fs.exists; query the authoritative source unconditionally
- [feedback_assert_typed_error_shape_not_substrings.md](feedback_assert_typed_error_shape_not_substrings.md) — Caller-boundary propagation tests assert on typed variant+payload, not message substrings
- [feedback_residual_invariant_checks_stay_hard.md](feedback_residual_invariant_checks_stay_hard.md) — When splitting a bidirectional runtime validation into type-encoded entry points, every entry point needs a symmetric hard runtime check on axes the types don't kill; never debug_assert!
- [feedback_docs_at_contract_level_not_impl_names.md](feedback_docs_at_contract_level_not_impl_names.md) — Architecture docs describe behavioral contracts, never internal helper names; check `exec` vs invoke before writing process-boundary claims
- [feedback_no_new_tests_requires_deterministic_gate.md](feedback_no_new_tests_requires_deterministic_gate.md) — Before claiming "no new tests needed," each cited test must be deterministic AND exercise the surviving path at the right layer
- [feedback_fold_validation_into_unsafe_primitive_owner.md](feedback_fold_validation_into_unsafe_primitive_owner.md) — Fold precondition checks into the helper that owns the unsafe primitive (silent `.remove`, partial write), not into a parallel guard
- [feedback_prefer_cli_vm_regression_over_formatter_helper.md](feedback_prefer_cli_vm_regression_over_formatter_helper.md) — For user-visible CLI output/control-flow bugs, add a behavioral VM test (like braid-remove-disk-busy.py) instead of extracting a single-use formatter helper
- [feedback_verify_cargo_package_name.md](feedback_verify_cargo_package_name.md) — Verify cargo package name from Cargo.toml before prescribing `cargo test -p <name>`; braid CLI's crate is `braid-cli`, not `braid`
- [feedback_new_vm_test_must_register_in_flake.md](feedback_new_vm_test_must_register_in_flake.md) — braid's `just test-vm` dispatches to `checks` entries in flake.nix; new VM test files must be registered there, not in the justfile
- [feedback_localized_shadowing_over_widening_mut.md](feedback_localized_shadowing_over_widening_mut.md) — When collapsing two bindings into one, shadow with `let mut x = x;` at the mutation seam instead of widening the top binding to `let mut`
- [feedback_replace_dead_test_dont_delete.md](feedback_replace_dead_test_dont_delete.md) — When a test is dead (tautology) but its name covers a real user-visible contract, default to replacing with a real regression, not deleting
- [feedback_plan_verification_grep_scope.md](feedback_plan_verification_grep_scope.md) — Scope plan verification greps to the code tree (cli/src), not repo-wide -- the plan file self-matches its own narrative
- [feedback_dont_run_just_test_all_autonomously.md](feedback_dont_run_just_test_all_autonomously.md) — Fix only the failing test + verify with `just test-vm <name>`; don't autonomously kick off `just test-all` -- let the user drive the full-suite re-run
- [feedback_exit_code_classifier_trace_command_path.md](feedback_exit_code_classifier_trace_command_path.md) — Exit-code classifier decisions: trace the specific subcommand's return-value path, not just the general errno translation table
- [feedback_behavior_lock_tool_contract_in_repro.md](feedback_behavior_lock_tool_contract_in_repro.md) — When code trusts a tool contract (exit code, wording), behavior-lock it in a live-tool repro test, not just mocks
- [feedback_no_false_compiler_backstop_for_pub.md](feedback_no_false_compiler_backstop_for_pub.md) — Don't justify "no new tests" by citing rustc's unused-variant/dead_code lint on a `pub` item; rustc's `dead_code` ignores public items
- [feedback_local_runner_over_shared_mock.md](feedback_local_runner_over_shared_mock.md) — Single-test needs for sequenced/stateful responses get a file-local runner (extend RecordingRunner or write a purpose-built one); don't widen MockRunner
- [feedback_prefer_injected_sleeper_over_cfg_test_const.md](feedback_prefer_injected_sleeper_over_cfg_test_const.md) — Inject a Sleeper trait rather than cfg(test)-gating a Duration const: avoids test-vs-prod build divergence and lets one deterministic unit test pin the prod delay
- [feedback_test_preamble_block_comment_literal.md](feedback_test_preamble_block_comment_literal.md) — New test preambles must be literal /* ... */ block comments with Intent/Why it exists/Scenario per AGENTS.md, not // line comments (even though many existing tests use //)
- [feedback_gate_tests_exhaustive_matrix_at_seam.md](feedback_gate_tests_exhaustive_matrix_at_seam.md) — Gate regressions need BOTH branches wired through one seam AND matrix cells that distinguish plausible wrong gates; sanity-check by reverting the gate
- [feedback_preview_boundary_tests_every_branch.md](feedback_preview_boundary_tests_every_branch.md) — When a helper is promoted to "the preview boundary" for a CLI contract, every branch (including no-op) needs an exact-output test, not just substrings
- [feedback_stream_routing_needs_cli_test.md](feedback_stream_routing_needs_cli_test.md) — Stdout-vs-stderr contracts aren't observable from unit tests on a render helper; a CLI subtest capturing `>stdout 2>stderr` is mandatory
- [feedback_dont_overclaim_cross_site_parity.md](feedback_dont_overclaim_cross_site_parity.md) — "Byte-for-byte parity" between two duplicated call sites requires tests on both sides; a one-sided test can't catch drift on the other
