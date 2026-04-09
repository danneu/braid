[← Manual](../index.md)

# braid browse

Interactive read-only browser for raw btrfs command output. Organizes output into tabs and subtabs so you can inspect filesystem details without remembering btrfs subcommands.

## When to use it

- Exploring raw btrfs output without typing long commands.
- Checking scrub status, balance status, device stats, subvolume list, or filesystem usage.
- Quick verification that btrfs commands work against your pool (`--check` mode).

## Basic example

```
sudo braid browse
```

Opens the tabbed TUI using the mount point from your braid config.

## Common variations

Browse a specific mount point:

```
sudo braid browse --mount-point /mnt/storage
```

Non-interactive check (run key commands and exit 0/1):

```
sudo braid browse --check
```

Check mode runs `btrfs filesystem usage`, `btrfs subvolume list`, and `btrfs subvolume show` (on the first subvolume) and reports success/failure for each. Useful in scripts or tests.

## Flags

| Flag | Effect |
| --- | --- |
| `--mount-point <path>` | Mount point to inspect (defaults to config mount_point) |
| `--check` | Non-interactive: run key commands, print results, exit 0/1 |

## Tabs and subtabs

| Tab | Subtabs | btrfs command |
| --- | --- | --- |
| Filesystem | Usage, Show, Df | `btrfs filesystem usage`, `btrfs filesystem show`, `btrfs filesystem df` |
| Devices | Usage, Stats | `btrfs device usage`, `btrfs device stats` |
| Subvolumes | List | `btrfs subvolume list` |
| Scrub | Status | `btrfs scrub status` |
| Balance | Status | `btrfs balance status` |

## Keybindings

| Key | Action |
| --- | --- |
| `Tab` / `Shift-Tab` | Switch between tabs |
| `h` / `l` (or arrow keys) | Switch between subtabs within a tab |
| `j` / `k` (or arrow keys) | Scroll output / select subvolume |
| `Ctrl-D` / `Ctrl-U` | Page down / page up |
| `Enter` | Drill into subvolume detail (on Subvolumes tab) |
| `Esc` / `Backspace` | Back from subvolume detail |
| `r` | Reload current view |
| `q` | Quit |
| `?` | Toggle help overlay |

## Subvolume detail

On the Subvolumes tab, select a subvolume with `j`/`k` and press `Enter` to see `btrfs subvolume show` output for that subvolume. Press `Esc` to return to the list.

## Related commands

- [tui](tui.md) -- interactive dashboard (pool health, disk status, alerts)
- [status](status.md) -- non-interactive pool health output
