# Credential follow-ups: shared helper + module-side audit

## Context

Three credential-handling fixes have already landed in braid:

1. LUKS passphrase buffers are zeroized (`Zeroizing<String>` / `Zeroizing<Vec<u8>>` in `cli/src/luks.rs`).
2. Generated keyfile bytes are zeroized in `cli/src/enroll_key_file.rs::generate_key_file`.
3. USB auto-unlock keyfile lifecycle hardened in commit `df706c44875f` (`modules/braid/storage.nix`).

Two follow-up todos remain:

- Decide whether to extract shared secret helpers across the CLI now that the passphrase/keyfile fixes have shaken out a pattern.
- Audit module-side file credentials, with USB auto-unlock keyfiles likely covered by `df706c44875f` and UPS/NUT password handling as the highest-priority unknown.

The intended outcome is to either land a small, contained piece of work or to record the audit conclusion and close each todo.

## Audit findings

### 1. CLI shared helper extraction

**No real duplication exists.** Concrete inventory:

- All passphrase reads route through three centralized helpers in `cli/src/luks.rs` (`read_passphrase_with_readers`, `read_line_into_zeroizing`, `read_file_into_zeroizing`). They already produce `Zeroizing<Vec<u8>>` / `Zeroizing<String>`.
- The "duplicated" call sites are trivial one-liners: three `key_file_path.display().to_string()` conversions (`luks.rs:806`, `luks.rs:827`, `luks.rs:842`) and three `passphrase.as_bytes()` calls into `run_with_stdin` (`luks.rs:399`, `luks.rs:504`, `luks.rs:839`).
- The kernel pipe buffer used by `RealRunner::exec_with_stdin` (`cli/src/cmd.rs:918`) is not a userspace gap -- the caller still owns the `Zeroizing<String>` whose drop wipes the source bytes.
- `cli/src/enroll_key_file.rs::generate_key_file` is a one-of-a-kind code path (random buffer -> file -> sync) with no peer to deduplicate against.

Wrapping these one-liners would obscure intent without removing risk. The Zeroizing-typed return values are already the contract.

### 2. Module-side credentials

Three file credentials cross the module boundary:

| Credential | Path | Owner / mode | Lifecycle |
|---|---|---|---|
| LUKS pool passphrase | stdin only | n/a | piped from `systemd-ask-password` to `braid unlock --passphrase-stdin`; never persisted |
| USB auto-unlock keyfile | `/run/braid-key/mnt/braid.key` | mounted vfat | mount-read-unmount under locked parent `/run/braid-key` (0700 root:root); EXIT trap installed before mount |
| Upsmon password | `/var/lib/braid/upsmon.pass` | root:root 0600 | generated at runtime by `braid-ups-secrets.service`; consumed by nixpkgs `power.ups` via `passwordFile` (decision 020) |

Verification per leak vector:

- **`/nix/store` leakage:** Braid passes `passwordFile = "/var/lib/braid/upsmon.pass"` (a path) to `power.ups`, not `builtins.readFile`. Nixpkgs renders `/run/nut/upsd.users` and `/run/nut/upsmon.conf` at runtime from the file path. The token never enters the store. Decision 020 documents this as load-bearing.
- **Generated config perms:** Verified by audit -- runtime configs land in `/run/nut/`, not committed Nix output. Exact mode/owner of `/run/nut/upsd.users` should be asserted by the new test below.
- **Argv exposure:** No braid systemd unit or wrapper passes a credential as an argv. `power.ups` reads from config files, not argv.
- **Logs:** systemd-ask-password and braid-ups-secrets shell are silent on the secret content.
- **USB auto-unlock:** Hardened by `df706c44875f`. `realpath` keeps the resolved keyfile under `/run/braid-key/mnt/`, EXIT trap covers every exit path, locked parent at 0700 prevents non-root traversal during the mount window. No follow-up needed.

Existing test coverage:

- `tests/module/ups-lb-clean-shutdown.py:64-66` already asserts `/var/lib/braid/upsmon.pass` is mode `0600 root:root`.
- No existing test asserts the absence of the secret in `/nix/store`, the perms of `/run/nut/upsd.users`/`/run/nut/upsmon.conf`, or the absence of the secret in `ps`/journal/`systemctl show`.

The single useful piece of follow-up work is locking decision 020's "never enters the Nix store" invariant behind a behavioral test.

## Recommended actions

### A. Close the shared-helper todo

No code change. Record the rationale (above) in the commit message that resolves the todo, and remove or strike the todo wherever it lives. The current centralization in `cli/src/luks.rs` is the right shape.

### B. Close the USB auto-unlock todo

No code change. `df706c44875f` covers it; `docs/luks-unlock.md:67-88` documents the contract; the existing `braid-auto-unlock.service` shell script enforces it.

### C. Add one UPS-credential leakage test

Lock in decision 020's invariant with a focused VM test.

**File to add:** `tests/module/ups-credential-lifecycle.nix` and `tests/module/ups-credential-lifecycle.py` (mirrors the layout of `ups-lb-clean-shutdown`).

**Reuse, do not duplicate:** Import `./lib/ups-fixture.nix` (located at `tests/module/lib/ups-fixture.nix`, the shared harness already used by every `ups-lb-*` test -- see `tests/module/ups-lb-clean-shutdown.nix:48`) so the test gets the dummy-ups driver and `testops` SET user without bespoke setup.

**Required module wiring** (the fixture only sets `braid.ups.*`; the braid UPS config in `modules/braid/ups.nix:59` is guarded by `cfg.enable && ups.enable`, so the test VM must turn braid itself on or the fixture is a no-op):

```nix
{ braid }:
{ pkgs, lib, ... }:
{
  name = "ups-credential-lifecycle";
  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/ups-fixture.nix { })
      ];
      braid = {
        enable = true;
        package = braid;
      };
      virtualisation.memorySize = 1024;
    };
  testScript = builtins.readFile ./ups-credential-lifecycle.py;
}
```

Omit `./lib/initrd-fixture.nix`, virtual disks, and the `braid unlock` override -- the test only needs boot-time secret rendering, not a pool.

**Assertions** (all read-only inside the VM, single boot):

1. `/var/lib/braid/upsmon.pass` exists, mode `0600 root:root` (redundant with `ups-lb-clean-shutdown` but cheap and self-documenting in this test).
2. The token's exact bytes do not appear under `/nix/store/`. Implementation: read the token, then run `grep -rlF -- "$TOKEN" /nix/store` and assert empty output. If a full-store scan is too slow on the linux-builder, restrict to the resolved unit paths via `systemctl show -p FragmentPath,DropInPaths,ExecStart upsd.service upsmon.service power.ups.target braid-ups-secrets.service` and grep just those store paths and their transitive `nix-store -qR` closures.
3. `/run/nut/upsd.users` and `/run/nut/upsmon.conf` exist, are not world- or group-readable by anyone outside `nut`, and do contain the token (positive control: confirms the file is the rendered consumer).
4. The token does not appear in `ps -eo args`, `ps -eo cmd`, or any process's `/proc/<pid>/environ`.
5. The token does not appear in `journalctl -b 0 -u upsd.service -u upsmon.service -u braid-ups-secrets.service --no-pager`.
6. The token does not appear in `systemctl show upsd.service upsmon.service braid-ups-secrets.service`.

**Test preamble** (per `docs/testing.md`):

- Intent: prove the upsmon token never reaches `/nix/store`, process argv, env, journal, or `systemctl show`, and that runtime-rendered NUT configs are restricted to the nut group.
- Why it exists: decision 020's "never enters the Nix store" claim is load-bearing; refactors to `power.ups` integration could silently regress it.
- Scenario: NAS boots, `braid-ups-secrets.service` mints `/var/lib/braid/upsmon.pass`, `power.ups` renders `/run/nut/*` from it; an operator running `nix-store --query` or `ps auxe` must not see the token.

**Register in `flake.nix` checks** next to `ups-lb-clean-shutdown` (`flake.nix:591`), following the same `pkgs.testers.nixosTest (import ./tests/module/<name>.nix { inherit braid; })` form used by the other `ups-*` checks.

### D. Document the zeroization contract

Add a short new section to `docs/luks-unlock.md` titled `## Credential memory hygiene`, placed after the existing `## Plaintext keyfile exposure (Unraid CVE)` section (around line 90) so it sits with the other credential-lifecycle content.

Content (one paragraph, no code, no new file):

- Passphrase buffers in the CLI are `Zeroizing<...>` from read to drop (`cli/src/luks.rs::read_line_into_zeroizing`, `read_file_into_zeroizing`); subprocess delivery is stdin-only (no argv, no temp file).
- Generated keyfile bytes are zeroized after write (`cli/src/enroll_key_file.rs::generate_key_file`).
- Passphrases and keyfile bytes never enter the Nix store; the upsmon token is generated at runtime per decision 020 and the USB keyfile lives only on the USB stick mounted into `/run/braid-key/mnt/`.

Cross-link `docs/decisions/020-ups-integration.md` for the UPS side and reference commit `df706c44875f` for the USB side.

### E. No `docs/decisions/` change

The relevant invariants live in decision 004, 018, 020, and `docs/luks-unlock.md`. None need a status change.

## Files touched

- **Add:** `tests/module/ups-credential-lifecycle.nix`
- **Add:** `tests/module/ups-credential-lifecycle.py`
- **Edit:** `flake.nix` (register the new check next to existing `ups-*` checks)
- **Edit:** `docs/luks-unlock.md` (one new subsection on the zeroization + no-store-leak contract)

No CLI code changes. No `modules/braid/` changes.

## Verification

- `just test-vm ups-credential-lifecycle` -- run the new test in isolation.
- `just test-vm ups-lb-clean-shutdown ups-credential-lifecycle braid-status-ups braid-doctor-ups` -- confirm no regression in adjacent UPS tests.
- `just test-rust` -- sanity check (no Rust changes, but cheap).
- Manual inspection: read `flake.nix` to confirm the new check is in `checks.aarch64-darwin` and runnable.

## Items to close after this lands

- "Decide whether to extract shared secret helpers" -- closed with rationale (no real duplication).
- "Audit module-side file credentials / USB auto-unlock keyfile" -- closed; covered by `df706c44875f` + existing docs.
- "Audit module-side file credentials / UPS-NUT password" -- closed; new test locks in decision 020.
