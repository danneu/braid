Golden-file fixtures captured from a nixos-unstable VM by
`just capture-all-fixtures-unstable`. Non-authoritative; they exist so
upstream output changes are visible in git history.

**No smartctl fixtures by design.** VM virtio disks do not emit useful
SMART data. The smartctl parsers are exercised only against
`cli/tests/fixtures/nixos-26.05/smartctl-*.json` (a physical-drive SATA
capture plus hand-authored NVMe and self-test fixtures). The `tool-versions` VM
test verifies `smartctl` provenance and configured-package version but
does not detect nixpkgs version moves -- on any smartmontools nixpkgs
bump, review and refresh the stable smartctl fixtures by hand.
