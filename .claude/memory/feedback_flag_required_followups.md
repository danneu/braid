---
name: Flag required follow-up actions prominently
description: When implementation leaves required follow-up steps (like fixture capture), call them out explicitly as next steps, don't bury them
type: feedback
---

When a change requires a follow-up action to be fully verified (like running `just capture-fixtures` after switching a parser to JSON), proactively tell the user what needs to happen and why — don't wait for them to ask.

**Why:** After switching the device_stats parser to JSON, the synthetic fixture needed to be validated against real VM output. This was easy to forget because all tests passed locally. The user had to ask twice before getting a clear answer.

**How to apply:** At the end of implementation, if there are required follow-up steps, state them clearly and prominently. Don't just say "later" — say "you need to do X before this is safe to ship, because Y."
