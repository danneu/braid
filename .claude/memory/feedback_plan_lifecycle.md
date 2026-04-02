---
name: Plan file lifecycle on implementation
description: When implementing a plan from plans/wip, rename and move it to plans/impl with a date-prefixed descriptive name, in the same commit as the implementation
type: feedback
---

When finishing implementation of a plan, rename it from its random wip codename to a `YYYY-MM-DD-description` name and move it to `plans/impl/`. Include the move in the same commit as the implementation.

**Why:** `plans/wip/` uses random codenames (e.g. `bubbly-toasting-cerf.md`). `plans/impl/` uses date-prefixed descriptive names so you can tell what a plan is about at a glance and when it shipped.

**How to apply:**

```
git mv plans/wip/bubbly-toasting-cerf.md plans/impl/2026-04-02-pool-unlock-retry.md
git mv plans/wip/compiled-munching-narwhal.md plans/impl/2026-04-02-autosuspend.md
```

Use today's date and a short kebab-case description of what the plan covers. Stage with the implementation changes.
