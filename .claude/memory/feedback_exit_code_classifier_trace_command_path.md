---
name: exit-code classifier - trace the specific command's path
description: When evaluating whether a tool's exit code uniquely classifies an error, read the specific subcommand's return-value path, not just the general errno-to-exit translation table
type: feedback
originSessionId: e4c2feef-b6b5-481b-a648-f88718239625
---
When deciding whether an exit code is a reliable classifier for a specific tool invocation (e.g. "is cryptsetup exit 5 == busy?"), the general errno translation table (`translate_errno` in cryptsetup/src/utils_tools.c) only tells you which errnos *can* map to that exit -- it does NOT tell you which errnos the *specific subcommand* actually produces. A subcommand may only populate a subset of the table, making the exit code effectively single-valued for that path.

**Why:** I rejected exit-code classification for `cryptsetup close` busy detection because `translate_errno` maps both `-EEXIST` and `-EBUSY` to exit 5. But `cryptsetup close` -> `action_close` -> `crypt_deactivate_by_name` (reference/cryptsetup/lib/setup.c:5763-5811) has no `-EEXIST` branch at all -- its return codes are `-EBUSY`, `-EINVAL`, `-ENODEV`. So exit 5 from `cryptsetup close` is EBUSY-exclusive in practice, which makes exit-code the robust classifier (wording- and locale-independent) that stderr substring matching is not. The user caught this; my analysis had stopped at the translation table.

**How to apply:** For any "can I classify this tool failure by exit code?" question: after checking `translate_errno` (or equivalent), open the specific subcommand's action handler and follow its return statements through the library functions it actually calls. Enumerate the errnos that path can produce, then decide. A multi-purpose exit-code translation table does not prove a multi-purpose exit code *for a given command*. Related: feedback_check_vendored_source.md (read reference/ before assuming).
