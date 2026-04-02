# Plan: Simplify hw test runner CLI — drop `--from-config`

## Context

`--from-config /etc/braid/config.json` is misleading: the runner never reads `config.json`. It uses `dirname(config_path)` to locate `pool.json` alongside it, then falls back to `/var/lib/braid/pool.json` anyway. The actual test config (`/tmp/braid-hw-test/config.json` with mount_point) is generated separately by `write_config()` and is unrelated.

## Changes

### `tests/hw/runner.py`

1. Replace `--from-config PATH` with `--pool-json PATH` (default: `/var/lib/braid/pool.json`)
2. Simplify `devices_from_config(config_path)` → `devices_from_pool(pool_path)` — just read the path directly, no dirname dance or fallback
3. Update docstring/usage block at top of file

### `justfile`

Update `test-hw` comment from `--from-config /etc/braid/config.json` to `--pool-json /var/lib/braid/pool.json` (or show it without the flag since it's now the default).

### `AGENTS.md` / `README.md`

Update any `--from-config` references if present.

## Files

- `tests/hw/runner.py` — main changes
- `justfile` — comment update
- Scan `README.md`, `AGENTS.md`, `docs/` for references

## Verification

- `python3 tests/hw/runner.py --help` shows new `--pool-json` flag
- `python3 tests/hw/runner.py` (no args, no devices) defaults to `/var/lib/braid/pool.json`
- Positional device args still work as before
