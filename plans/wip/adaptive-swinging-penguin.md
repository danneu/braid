# Remove duplicate systemPackages from storage.nix

## Context

`storage.nix:33` and `cli.nix:21` both add the wrapped braid CLI to `environment.systemPackages`. Since the assertion in `options.nix:62` guarantees `cfg.package != null` whenever `enable = true`, both paths always fire. Nix deduplicates store paths so there's no runtime effect, but having two owners is confusing. `cli.nix` is the natural owner (it handles CLI config, completions, etc.).

## Change

**`modules/braid/storage.nix`** — remove lines 32-33:

```nix
    # Wrapped CLI available on PATH
    environment.systemPackages = [ braidWrapped ];
```

Also remove `braidWrapped` from the `let` binding (line 9) since it's no longer used directly in this file... actually, `braidWrapped` is still used on lines 51 (ExecStop) and 66 (braid-unlock path). Keep the binding.

That's it. Single two-line deletion.

## Verification

- `just test` — all VM tests pass (braid CLI is still on PATH via cli.nix)
- `just test-rust` — unaffected (Rust unit tests don't touch NixOS config)
