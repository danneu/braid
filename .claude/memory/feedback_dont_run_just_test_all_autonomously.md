---
name: Don't run `just test-all` autonomously
description: When a specific test from `just test-all` fails, fix only that test and verify with `just test-vm <name>`; let the user drive the re-run of the full suite
type: feedback
originSessionId: 799e8da1-b22f-4b5c-b367-d41c6b83d64d
---
When `just test-all` (or any comparable expensive full-suite run in this repo) surfaces a specific failing VM test, scope the fix and verification to that test. Use `just test-vm <failing-test>` (plus any sibling file touched) to confirm the fix. Do not launch `just test-all` yourself -- even in the background -- to "close the loop." Report that the targeted fix is ready and let the user re-run `just test-all` on their schedule.

**Why:** User explicitly stopped a `just test-all` I kicked off after targeted tests passed: "nah, i'll be the one who runs just test-all; just tell me when. your job is to fix the one test that failed." Full-suite runs on linux-builder are tens of minutes of builder time, and the user wants control over when that cost is spent.

**How to apply:** After fixing a specific failing VM test (`just test-vm <name>` + any sibling touched passes), stop and tell the user the fix is ready to re-run `just test-all`. Do not queue `just test-all` as an autonomous verification step, background or foreground.
