---
intent: Explain why the TUI has both curated tabs and a raw-output Browse tab. Read before moving command output into or out of Browse.
status: Active
---

# Decision: Browse Tab Is Raw Output, Curated Tabs Are First-Class UX


> Principles:
> - [Unified CLI](012-intent-cli.md)
> - [Pinned toolchain](010-toolchain-pinning.md)

## Context

The original standalone browse command exposed low-level btrfs command output
in its own TUI. The main `braid tui` later grew real top-level tabs for curated
pool views such as Data and Scrub. Keeping a separate browse runtime duplicated
input handling, event filtering, command-generation guards, and snapshot tests.

At the same time, not every useful operator view should become a polished TUI
panel immediately. Some data is most useful as complete raw command output while
braid is still learning which parts deserve a first-class workflow.

## Decision

`braid tui` owns the interactive UI surface. The standalone browse command is
removed, and its raw inspection workflow lives as the `Browse` top tab.

The top tabs are:

- **Data** -- curated pool and disk health UX.
- **Scrub** -- curated scrub status UX.
- **Browse** -- raw CLI output inspector.

Browse is intentionally low-level and pass-through. It may overlap curated tabs
because overlap is not duplication here: Browse answers "what did the underlying
tool say?" while curated tabs answer "what should the operator understand or do
next?"

Features graduate out of Browse only when they need dedicated interaction,
history, safety checks, progress semantics, or domain-specific summaries. Until
then, Browse is the holding area for complete command output.

## Consequences

- Raw command coverage and parser canaries must exercise Browse through
  `braid tui`, not a separate non-interactive browse command.
- The Browse tab can expose Btrfs and NUT commands even when related curated
  panels already exist.
- New Browse entries should be append-only within the Browse program/command
  menus unless they are promoted to a curated tab with a separate design reason.
