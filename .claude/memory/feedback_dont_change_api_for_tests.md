---
name: Don't change command APIs purely for testability
description: Don't warp command signatures (return types, parameters) just to make tests easier — test the helper directly instead
type: feedback
---

Don't change a command's return type or API boundary purely to make a test easier to write.

**Why:** It weakens the command boundary by mixing presentation concerns (warning text) into what should be a success/failure contract. It also causes unrelated ripple effects at the call site (e.g. main.rs).

**How to apply:** When a command needs testable side-effect behavior (like a warning message), extract the logic into a helper that returns the testable value. Test the helper directly. The command's integration test just verifies the command succeeds/fails as expected — the helper test covers the output content.
