# dry-run

The planner/executor split
([ADR 022](../../../docs/design/decisions/022-dry-run-preview-model.md)):
plans that differ from execution, side effects during planning, and
previews that lie.

Out of lane: a bug in what the executor does when the plan is right
(the command's `command:` slice). Evidence: the plan, the execution
path that departs from it, and the input that shows the difference.
