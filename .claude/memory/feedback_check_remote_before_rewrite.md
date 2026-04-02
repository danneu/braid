---
name: Check remote before advising history rewrites
description: Always verify whether commits have been pushed before saying a rewrite is safe
type: feedback
---

Before advising that a history rewrite (squash, reset, rebase) is safe, check whether the commits have already been pushed to the remote. Don't assume commits are local-only.

**Why:** Told user a soft reset + squash was safe because commits were "local," but they turned out to be already pushed, requiring a force push to master.

**How to apply:** Run `git log --oneline origin/HEAD..HEAD` or similar to check if commits exist on the remote before recommending history rewrites.
