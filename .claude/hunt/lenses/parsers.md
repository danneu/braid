# parsers

Parsers and fixtures for btrfs-progs, cryptsetup, NUT, smartmontools,
ethtool, and `lsblk --json`: formats the parser does not handle,
fixtures that no longer match the pinned tools, and golden tests that
pass on garbage. Lanes and refresh rules:
[parser-compatibility.md](../../../docs/dev/parser-compatibility.md).

Out of lane: what a command does with parsed data (its `command:`
slice). Evidence: the tool's output format from `reference/` or a
captured fixture, and the parser code that mishandles it.
