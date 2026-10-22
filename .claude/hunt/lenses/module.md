# module

The NixOS module (`modules/braid/`) and its systemd units: unit
ordering, hardening
([ADR 033](../../../docs/design/decisions/033-systemd-unit-hardening.md)),
lifecycle ([ADR 018](../../../docs/design/decisions/018-systemd-lifecycle.md)),
option defaults, and drift between the module and what the CLI expects.

Out of lane: a CLI command's own logic (its `command:` slice), process
spawning inside the CLI (subprocess). Evidence: the unit or option, the
ADR rule it breaks or the CLI code it disagrees with.
