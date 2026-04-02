---
name: NixOS test f-string lint
description: NixOS VM test framework rejects Python f-strings without placeholders
type: feedback
---

NixOS VM test scripts are linted at build time. f-strings without `{placeholder}` variables (e.g., `f"Missing foo in config"`) cause a build failure: `f-string is missing placeholders`. Use plain strings or string concatenation instead when there's no interpolation needed.

**Why:** The NixOS test framework runs a linter pass before executing the Python test script. This catches accidental f-string prefixes that serve no purpose.

**How to apply:** In `tests/**/*.py` files, never use `f"..."` without at least one `{variable}` inside. Use `"literal" + variable` for assertion messages that include dynamic values.
