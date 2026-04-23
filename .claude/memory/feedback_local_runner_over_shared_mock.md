---
name: Prefer local purpose-built test runner over widening shared MockRunner
description: For single-test needs (e.g. sequenced/stateful responses), extend the file-local RecordingRunner-style helper or add a purpose-built runner; do not add fields/methods to the shared MockRunner in cli/src/cmd.rs
type: feedback
originSessionId: eef91a8d-c8c0-464b-a01a-67592f653402
---
When a single test in a `cli/src/<cmd>.rs` module needs response behavior the
shared `MockRunner` can't express (e.g. a sequence of different responses for
the same `CmdRequest` key, stateful side effects, conditional failures),
**default to a file-local runner** rather than widening `MockRunner` in
`cli/src/cmd.rs`.

Patterns the repo already uses for this:
- `cli/src/lock.rs` -- `RecordingRunner { inner: MockRunner, close_calls: Mutex<...> }` delegates to `MockRunner` for everything except the recording it adds.
- `cli/src/remove.rs:527` -- `RecordingRunner` is fully purpose-built; matches `CmdRequest` variants directly with hard-coded outputs and per-instance config flags (e.g. `fail_device_remove`).
- `cli/src/recover.rs` -- `MapperClosingRunner` mutates a `StatefulMockFs` on `CryptsetupClose` success.

**Why:** Shared `MockRunner` is consumed by every test in the workspace, so
adding fields/methods (especially ones requiring interior mutability like
`Mutex<...>`) widens the API surface and adds maintenance cost for a need
confined to one test. The new helper is also only covered indirectly by the
test that motivated it. Local runners keep the blast radius small and the
intent visible at the callsite.

**How to apply:** Before proposing a `with_*` builder on `MockRunner`, check
whether the file already has a `RecordingRunner` or similar wrapper. If yes,
extend that. If no, write a small purpose-built one in the test module.
Reserve `MockRunner` changes for behavior every test needs (e.g. the existing
`with_mapper_open`, `with_luks_dump_text_luks2` helpers that are clearly
broadly applicable).
