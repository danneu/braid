# subprocess

Process spawning and the child environment in the Rust CLI
([ADR 034](../../../docs/design/decisions/034-subprocess-environment-discipline.md)):
inherited env, PATH, locale, exit-status handling, and stderr parsing.

Out of lane: parsing a tool's stdout (parsers), systemd unit
environment (module). Evidence: the spawn site and the ADR rule it
breaks, or the input that makes it misbehave.
