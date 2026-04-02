---
name: test_isolation_preference
description: In eval-time tests, disable unrelated features rather than stubbing NixOS options
type: feedback
---

When an eval-time test (lib.evalModules in isolation) breaks because of a new NixOS option dependency, disable the unrelated feature in the test config rather than expanding the fake module surface with stubs.

**Why:** Stubbing options (e.g. adding `options.users`) makes the test less isolated and can mask future accidental dependencies on unrelated NixOS top-level options. Disabling the feature that introduced the dependency keeps the test focused on what it actually tests.

**How to apply:** When fixing eval-time test failures caused by new module dependencies, first check if the dependency comes from a feature the test doesn't need. If so, set that feature's config to its "off" value (e.g. `storageGroup = null`) instead of adding option stubs.
