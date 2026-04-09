[← Manual](../index.md)

# braid tui

Interactive terminal dashboard showing pool state, disk health, allocation, scrub status, and active alerts. Refreshes on demand.

## When to use it

- Quick visual overview of your NAS health.
- Checking disk-level detail (LUKS cipher, SMART health, error counts, transport).
- Monitoring during or after a scrub.

## Basic example

```
sudo braid tui
```

## Demo mode

Try the TUI without a real pool (no config or btrfs required, no root required):

```
braid tui --demo
```

Demo mode shows three fake disks with sample data, useful for exploring the interface.

## Flags

| Flag | Effect |
| --- | --- |
| `--demo` | Run with fake data (no config, btrfs, or root required) |

## Keybindings

| Key | Action |
| --- | --- |
| `q` | Quit |
| `r` | Reload pool data |
| `Tab` | Next tab |
| `Shift-Tab` | Previous tab |
| `j` / `k` | Select next/previous disk |
| `Enter` | Open disk detail popup |
| `Esc` | Close disk detail popup |
| `?` | Toggle help overlay |

## What it shows

**Main view** -- pool status, mount point, capacity bar, RAID profile, scrub state, balance state, and active alerts.

**Disk table** -- one row per disk showing name, size, allocated, unallocated, transport (sata/usb/nvme), SMART health, and error counts.

**Disk detail popup** (press Enter on a disk) -- LUKS cipher, key size, keyslot count, device errors breakdown (read/write/flush/corruption/generation), and SMART health.

**Tabs** -- the dashboard has a single view (no tab switching in the TUI; for tabbed raw btrfs output, see `braid browse`).

## Related commands

- [status](status.md) -- non-interactive pool health output
- [browse](browse.md) -- tabbed browser for raw btrfs command output
