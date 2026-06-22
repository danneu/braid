# Plan: validate positional argv inputs at the type boundary

## Context

A security audit (`findings/5-command-injection-input-validation.md`, Finding 1)
flagged that braid never emits a `--` end-of-options separator before positional
device/path arguments to cryptsetup, btrfs, mount, wipefs, mkfs, etc. Flag-injection
safety therefore rests entirely on upstream newtype validators rejecting leading
dashes. The finding's proposed fix was to sprinkle `--` into every `to_argv` arm.

**We are NOT doing that.** A `verify-issue` pass concluded the `--` approach is the
inferior shape:

- It is pure redundancy on the majority of slots, which already receive a validated
  newtype (`ByIdPath`, `DiskName`-derived mapper `dev_path()`, `Devid`) that provably
  cannot start with `-`.
- It is unverifiable/risky on `ethtool`, whose userspace CLI uses a hand-rolled
  (non-getopt) parser, is the one tool *not* vendored in `reference/`, and would
  likely reject a bare `--` -- turning a hardening change into a regression of the
  Wake-on-LAN diagnostic.
- It churns dozens of `*_generates_correct_argv` pins and changes user-facing dry-run
  / TUI command previews (ADR-022) for zero reachable benefit.

The genuinely-unvalidated values are a small, **enumerable** set of strings that reach
positional argv slots without passing through a validating newtype. Every one is root-
or kernel-sourced today (so this is Low severity, defense-in-depth), but each is a
documented exception to braid's "every argv-bound identifier is a validating newtype"
architecture. The complete set, each closed by a workstream below: the `mountPoint`
config/CLI value (A), `ups.name` + `wolInterface` config (B), the `cryptsetup status`
backing-device path (D), and the generated-keyfile mount-probe directory (F, the
CLI-derived `enroll --generate` path). `MapperName` stays a deliberate observation door
(C, debug-assert only); the same three module-config inputs also get Nix eval-time
assertions at their outermost boundary (E). **The ideal fix closes the gaps at the type
boundary** -- consistent with `DiskName`/`ByIdPath`/`LuksUuid`, stronger than `--` (it
stops whitespace, control chars, and non-absolute paths, not just leading dashes), and
independent of each downstream tool's `--` handling.

### Outcome

Validate **every source** that feeds an argv positional, at the outermost boundary it
enters and kept validated all the way down -- from the Nix module option to the Rust argv
builder. This closes the remaining unvalidated `String`-into-argv *sources*: the config
`mountPoint`, `ups.name`/`wolInterface`, the `cryptsetup status` backing path, the
generated-keyfile probe dir, and the residual `MountPoint::new` doors. The invariant is
about *sources*, not field types: several `CmdRequest` argv fields stay `String`
(`CryptsetupLuksUuid.device`, `BtrfsFilesystemShowTarget.target`) and `PoolDevice.underlying`
(`types.rs:658`) stays `String` -- retained by design (see the `BtrfsFilesystemShowTarget`
non-goal) and now fed **only** validated values. No `--` separators; no change to
dry-run/TUI command rendering.

## Design principle

Validate at the **outermost boundary** the value enters, and keep it validated all the
way down:

- For module-config values (`mountPoint`, `ups.name`, `wolInterface`) the outermost
  boundary is the **Nix module option** -- they are accepted by `lib.types.path`/`str`
  (which do **no** content validation) and interpolated into systemd unit text by
  `modules/braid/*.nix` *before* Rust ever runs (see Workstream E). So the same rule is
  enforced twice: a Nix eval-time assertion (clear error at `nixos-rebuild`) **and** the
  Rust newtype (config deserialize + CLI arg). For `mountPoint` the Nix assertion
  already ships (`options.nix#mountPointOk`, commit `0b9333ed`) and is the **source of
  truth**; the Rust grammar is brought into line with it, not the reverse. Because the
  two grammars live in two languages, they are not "defined once" -- they are kept
  identical by an **executed parity sample matrix** asserted on both sides (Verification).
- For CLI args, validate at clap parse time via `value_parser` (precedent:
  `cli/src/main.rs:271`), so a bad value fails before any command executes.
- For tool-output strings, validate at the single parse function that produces them
  (`parse_cryptsetup_status`), returning a validated newtype so missed re-spend sites
  become compile errors.
- Keep **infallible constructors** only for values already validated or derived from
  trusted/observed sources (mirroring the existing `MapperName::from_basename`
  "observation door").

Reuse the established `parse(&str) -> Result<Self, XxxParseError>` + custom `Deserialize`
pattern from `cli/src/types.rs#ByIdPath`. A single shared private helper enforces the
common "argv-safe token" invariant so the new newtypes do not duplicate logic.

**Why the rules are conservative allowlists, not just "no leading dash":** these values
are interpolated **unescaped** into shell/systemd snippets by the Nix module
(`modules/braid/storage.nix:16,27,93` interpolate `cfg.mountPoint` into a `script` body
and into `ExecStart`/`mountpoint -q`). A space or shell metacharacter is therefore a
real defect (broken word-splitting) or injection at the module layer -- not merely an
argv-leading-dash concern. The validators below restrict to characters that are safe in
both an unescaped shell context and a single argv element.

---

## Workstream A: `MountPoint` validation (highest value -- most argv slots)

`MountPoint` feeds ~30 positional btrfs/mount/umount slots and is the weakest type
today: `#[serde(transparent)]` derive + an unvalidated `new()`
(`cli/src/types.rs#MountPoint`). Config deserialization currently bypasses all
validation.

**Changes (`cli/src/types.rs`):**

- Add `MountPoint::parse(&str) -> Result<Self, MountPointParseError>`. The grammar is
  the **canonical-segment** rule, and it must match the already-shipped Nix assertion
  `modules/braid/options.nix#mountPointOk` (commit `0b9333ed`) byte-for-byte -- that
  assertion is the source of truth (see Workstream E). A flat character-class allowlist
  (the earlier draft's `[A-Za-z0-9._/-]` over the whole string) is **wrong**: it admits
  the bare root `/`, doubled separators `/mnt//storage`, and `.`/`..` segments
  `/mnt/./storage` / `/mnt/../storage`, all of which `mountpoint-assertion-fails.nix`
  already rejects -- so it would break the "identical grammar" claim. Rules:
  - must start with `/` (absolute path -- this also guarantees no leading `-`),
  - strip exactly one optional trailing `/` (unless the whole value is the bare `/`),
    then split on `/`; the leading segment is empty (from the leading `/`) and there
    must be at least one further segment (so the bare `/` is rejected),
  - every remaining segment must be non-empty, not `.`, not `..`, and match
    `[A-Za-z0-9_.-]+` (the per-segment charset -- note no `/` *within* a segment). This
    rejects whitespace (`/mnt/my drive`), ASCII control / NUL, every shell metacharacter
    (`;`, `$`, backtick, quotes, `&`, `|`, ...), the bare `/`, doubled separators, and
    `.`/`..` segments. Real NAS mount points (`/mnt/storage`, `/srv/data`,
    `/mnt/tank-1`), one trailing slash (`/mnt/storage/`), and hidden segments
    (`/mnt/.snapshots`) all satisfy it -- matching `mountpoint-assertion-ok.nix`.
    (Conservative ASCII, fail-closed; identical to the Nix grammar.)
- Add `thiserror`-derived `MountPointParseError { raw: String }`, message:
  `invalid mount point '{raw}': must be a canonical absolute path -- segments of [A-Za-z0-9_.-] separated by single '/', no empty/'.'/'..' segments`.
- Replace `#[derive(Serialize, Deserialize)] #[serde(transparent)]` with a manual
  `Serialize` (transparent string) + custom `Deserialize` routing through `parse`
  (mirror `ByIdPath`). This is the load-bearing change: config.json can no longer
  carry a non-absolute / control-laden `mount_point`.
- Keep `new(String)` infallible, but rewrite its doc comment to state the contract:
  *the infallible internal door for a `MountPoint` already validated -- re-wrapping a
  value that has already passed `parse`/the Nix assertion (config round-trip); external
  string boundaries must use `parse`.* (Parallels the `from_basename` observation door.)
- **Close the two `tui/model.rs` `MountPoint::new` doors** (`cli/src/tui/model.rs`,
  `cli/src/tui/mod.rs`). `Model::new` takes `mount_point: String` and re-wraps via
  `MountPoint::new` (`model.rs:414`) while `tui/mod.rs#run` feeds it
  `config.mount_point().as_str().to_owned()` -- a validated `MountPoint` downgraded to a raw
  `String`; retype the parameter to `mount_point: MountPoint` and pass
  `config.mount_point().clone()` from `run`. `Model::new_demo` builds
  `MountPoint::new(String::new())` -- an **empty, grammar-invalid** value reachable in
  production via `braid tui --demo` (allowed without root, `main.rs:515`); replace it with
  `MountPoint::parse("/mnt/storage").expect("valid demo mount point")`. Because `new` stays
  infallible **the compiler flags neither site**, so they must be closed here or survive
  silently.

**Why `new` stays infallible:** once this plan closes every *external* string boundary --
CLI args via `value_parser` (above), config deserialize via `parse` (above), the two
`tui/model.rs` doors (above), and the generated-keyfile probe plus the
`online_state`/`doctor` mountpoint probe via Workstream F -- **no production path feeds an
external or unvalidated string into `MountPoint::new`**. Its remaining callers all hold
already-validated values: the deserialize constructor `Config::new(raw.mount_point)`
(`cli/src/config.rs:164`) receives exactly what `parse` just validated, and the bulk of
`MountPoint::new` sites are test fixtures building known-good literals (`/mnt/storage`,
...). `new` survives as the infallible internal constructor for already-proven values,
mirroring the `from_basename` observation door -- not as a live boundary.
(Earlier drafts cited `cli/src/online_state.rs#is_mountpoint` as the sole round-trip caller;
that was inaccurate on two counts -- its signature took an arbitrary `&Path` and re-wrapped
through `new`, **and** the two `tui/model.rs` sites were missed entirely (`Model::new`'s
`String` round-trip and `new_demo`'s empty value, neither compiler-flagged). Workstream F
retypes the probe to `&MountPoint`; the bullet above closes the TUI doors; the
`cli/src/enroll_key_file.rs` keyfile-dir caller is moved to its own validated type by
Workstream F.)

**CLI boundary (`cli/src/main.rs`):** the `--mount` flag for the systemd ExecStop
hooks (`ScrubCancel`/`ScrubCancelArgs`, `ScrubNeedsResume`/`ScrubResumeOrStart` via
`ScrubMountArgs`) currently builds `MountPoint::new(args.mount.clone())` at dispatch
(~lines 866/877/893). **Pivot to validating at clap parse time:** change the `mount`
field on both arg structs to `mount: MountPoint` with
`#[arg(long, value_parser = MountPoint::parse)]` (same mechanism already used at
`cli/src/main.rs:271`, `value_parser = clap::value_parser!(u64).range(1..)`).
`MountPointParseError` is a `thiserror` error, so it satisfies clap's value-parser
error bound directly. This:
- removes every `MountPoint::new(args.mount...)` call from `main.rs` (the dispatch arms
  use `&args.mount`), so the "impl forgets to wire the boundary" failure mode is
  structurally impossible -- the arg *is* a `MountPoint`;
- fails a bad `--mount` before any command executes, with clap's standard
  `ValueValidation` error and exit code 2 (a usage error -- correct for a malformed
  flag, and it does not collide with the commands' runtime exit semantics, which are
  never reached);
- is pure string validation (no filesystem access), respecting ADR-018's
  thin-systemd-layer / zero-FS-dependency constraint for ExecStop.

In production the Workstream E Nix assertion guarantees `cfg.mountPoint` already
satisfies the grammar, so the units' `--mount ${cfg.mountPoint}` always parses; the
value_parser is the defense-in-depth backstop and the test seam.

**Note:** `Config::new`'s existing empty-check (`cli/src/config.rs:64`) stays -- it
still guards programmatic misuse via the infallible `new`. It becomes redundant only
for the deserialize path (parse already rejects empty).

---

## Workstream B: `UpsName` + `Interface` config newtypes (consistency + domain modeling)

`Ups.name` and `AutoSuspend.wol_interface` (`cli/src/config.rs:40,48`) are the only
two raw `String`s that reach argv (`upsc/upscmd/upsrw <name>`, `ethtool <iface>`).
Wrap both in validating newtypes so the invariant lives in the type.

**New types (`cli/src/types.rs`):**

Both new types use a **conservative ASCII allowlist** rather than a denylist, for one
hard reason: the Workstream E Nix assertion must enforce the *identical* grammar via
`builtins.match`, and an allowlist regex is the only form expressible identically in
both Nix and Rust. The allowlist is a strict subset of what each tool would accept, so
it is never more permissive than the underlying contract.

- Private shared helper enforcing the floor: non-empty; first char not `-` (argv
  safety). Each newtype layers its allowlist + length on top (the allowlist already
  excludes whitespace and control).
- `UpsName`: a **local NUT ups identifier**, not a remote query target. braid's
  `ups.name` is used by `modules/braid/ups.nix` as a NixOS attribute key
  (`power.ups.ups.${name}`, `users.${name}`, `upsmon.monitor.${name}`) and braid itself
  appends the host: `system = "${ups.name}@localhost"` (`ups.nix:119`). So accepting a
  `upsname@host:port` form would corrupt the generated NUT config. Rule: allowlist
  `[A-Za-z0-9._-]` (single word), non-empty, no leading `-`, max length 32. This
  explicitly **rejects** `@`, `:`, and whitespace. (Replaces the earlier
  `ups@host:3493`-permissive design, which modeled the wrong thing.) A separate type can
  be introduced later if braid ever supports remote NUT query targets.
- `Interface`: a conservative allowlist that is a strict subset of the Linux kernel's
  `dev_valid_name` acceptance (`reference/linux/net/core/dev.c#dev_valid_name`), chosen
  so the Rust and Nix grammars are identical and so all real interface names
  (`eno1`, `eth0`, `br0`, `enp3s0`, VLAN `eth0.100`) pass. Rule: allowlist
  `[A-Za-z0-9._-]`, non-empty, length <= 15 (`IFNAMSIZ`-1), not the literal `.` or `..`,
  no leading `-`. This satisfies F4 -- it rejects `:`, `/`, whitespace, `.`, `..` that
  the plan's earlier `/`+length-only rule wrongly admitted -- while staying at least as
  strict as `dev_valid_name`. Turns a misconfig into an early clear error instead of a
  downstream `ethtool: bad command line argument(s)`.
- Each gets a `thiserror` error type, `parse`, `as_str`, `Display`, manual
  `Serialize`, custom `Deserialize` (mirror `ByIdPath`).

**Field type changes (`cli/src/config.rs`):** `Ups.name: String -> UpsName`,
`AutoSuspend.wol_interface: String -> Interface`. Validation now happens during
`serde_json::from_str` in `config_read`, surfaced via the existing
`ConfigError::Parse` path.

**Consumer updates (mechanical, ~15 sites):**
- Already call `.as_str()` (keep working): `add.rs`, `remove.rs`, `remove_missing.rs`,
  `replace.rs` (`.map(|u| u.name.as_str())`), `doctor.rs:1358`, `wol.rs:86`,
  `ups.rs:161`.
- `Display`-interpolated format strings (work unchanged via the new `Display` impls):
  `ups.rs:181`, `doctor.rs:1398/1406/1409`.
- `.clone()` into `CmdRequest` `String` fields (`cli/src/tui/browse/state.rs:984-993`,
  four sites): change to `.name.as_str().to_owned()`.
- Tests asserting raw equality (`config.rs:355,371`): change to `.as_str()` compare.

---

## Workstream C: `MapperName::from_basename` debug-assert (keep the observation door)

`from_basename` is unvalidated by design -- it ingests kernel/btrfs-reported mapper
basenames and must accept whatever the kernel produces in release builds. Do **not**
make it fallible.

**Change (`cli/src/types.rs#MapperName`):** add inside `from_basename`:
```
debug_assert!(is_plausible_mapper_basename(&name), "...");
```
where the predicate requires: non-empty, no leading `-`, no ASCII whitespace/control,
no `/`. This catches a *parser regression* (a `from_basename` fed a malformed value)
during debug/test runs without changing release behavior. Tighten the doc comment to
record the asserted contract.

**Pin that the assert fires:** add a `#[should_panic]` unit test on
`MapperName::from_basename("-x")` (and `"a b"`), gated `#[cfg(debug_assertions)]` so
`cargo test --release` -- where `debug_assert!` compiles out and nothing would panic --
does not run a guaranteed-to-fail test. The debug-assert is the entire behavioral content
of this workstream; without this test nothing locks that `is_plausible_mapper_basename`
actually rejects an implausible basename, so a future weakening of the predicate would
pass silently.

**Caller provenance (all already valid):** 5 sites in `cli/src/probe.rs` (basenames
stripped after a `/dev/mapper/` gate) + `cli/src/config.rs:112` (`braid-<DiskName>`).
Spot-check test fixtures that call `from_basename` use plausible names so debug builds
do not panic.

---

## Workstream D: `BackingPath` newtype at the `cryptsetup status` parse boundary

The `device:` value from `cryptsetup status` output is lifted by
`parse_cryptsetup_status` (`cli/src/parse/cryptsetup_status.rs:75`) as
`BackingDevice::Path(String)` and then re-spent as the positional device in
`CmdRequest::CryptsetupLuksUuid`. An earlier draft gated only `probe_pool`, but the
same backing-path value reaches `CryptsetupLuksUuid` through **at least five** flows --
`cli/src/probe.rs` (`probe_pool` / `probe_pool_alerts`), `cli/src/luks.rs#classify_mapper_ownership`,
`cli/src/probe_mapper_uuid.rs#probe_observed_mapper_uuid`, `cli/src/lock.rs#classify_candidate_mapper`,
and `cli/src/tui/probe.rs#fallback_disk_luks_lock`. A per-site gate is incomplete and
fragile.

**Pivot to a validated newtype at the single parse boundary:**

- Add a `BackingPath` newtype (`cli/src/types.rs`, imported by
  `cli/src/parse/types.rs`). `parse`: must start with `/` (absolute backing device --
  `/dev/...`); reject ASCII control/whitespace. (No `--` and no shell concern here -- the
  backing path is never module-interpolated; leading-`/` + clean is enough for argv
  safety.) `thiserror` error type `BackingPathParseError { raw: String, detail: String }`,
  `as_str`, `Display`, mirror `ByIdPath`. The error must expose `raw` and `detail`
  fields (like `LuksUuid`/`Fsid`'s parse errors) so the parse boundary can forward them
  into `ParseError::InvalidValue` (next bullet).
- Change `BackingDevice::Path(String)` -> `BackingDevice::Path(BackingPath)`
  (`cli/src/parse/types.rs`).
- `parse_cryptsetup_status` validates at construction (line ~75). The `?`/`From` shortcut
  does **not** apply -- there is no `From<BackingPathParseError> for ParseError`, and
  adding a blanket one would erase the field/raw/detail structure. Instead map explicitly
  onto the existing structured variant, mirroring the sibling boundary
  `cli/src/parse/cryptsetup_luks_uuid.rs#parse_cryptsetup_luks_uuid` (which does the same
  for `LuksUuid`):
  ```
  BackingDevice::Path(
      BackingPath::parse(&device).map_err(|e| ParseError::InvalidValue {
          cmd: raw.cmd.clone(),
          field: "device".into(),
          raw: e.raw,
          detail: e.detail,
      })?,
  )
  ```
  So an invalid `device:` line surfaces as `ParseError::InvalidValue { field: "device", .. }`
  (`parse/mod.rs#ParseError`), preserving the `cmd`/`field`/`raw`/`detail` structure
  consumers depend on -- not a generic stringly error. This is the **single** tool-output
  boundary. (The empty/`(null)` device already routes to `BackingDevice::Null` upstream,
  so `parse` only ever sees a non-empty candidate.)
- Every consumer now carries `BackingPath` into
  `CmdRequest::CryptsetupLuksUuid { device: backing.as_str().to_owned() }`. Because the
  variant no longer holds a `String`, any current or future site that pulled the raw
  string into argv is a **compile error** until it goes through the newtype -- which is
  the structural guarantee the per-site gate could not give.

This subsumes the old `probe.rs` `/dev/mapper/` `underlying` gate idea entirely.

---

## Workstream E: Nix module-option assertions (the outermost boundary)

`braid.mountPoint` (`modules/braid/options.nix:30`, `lib.types.path`),
`braid.ups.name` (`modules/braid/ups.nix:28`), and `braid.autoSuspend.wolInterface`
(`modules/braid/auto-suspend.nix:35`, `lib.types.nullOr lib.types.str`) are accepted by
Nix option types that perform **no** content validation (`nix eval` confirms
`lib.types.path` accepts `"/mnt/my drive;touch /tmp/x"`), and are then interpolated
**unescaped** into systemd unit text -- e.g. `modules/braid/storage.nix:16,27,93`
(`mountpoint -q ${cfg.mountPoint}`, `--mount ${cfg.mountPoint}` in a `script` body and
in `ExecStart`) and `modules/braid/ups.nix` (`power.ups.ups.${ups.name}`,
`system = "${ups.name}@localhost"`). A whitespace or shell-metacharacter value breaks
word-splitting or injects at this layer, before the Rust validators ever run.

**`mountPoint` already ships its assertion (the source-of-truth grammar).** Commit
`0b9333ed` added `modules/braid/options.nix#mountPointOk` plus eval tests
(`tests/eval/mountpoint-assertion-{ok,fails}.nix`). Its grammar is **canonical-segment**,
not a flat regex: strip one optional trailing `/`, split on `/`, and require the body to
be a non-empty list of segments each non-empty / not `.` / not `..` / matching
`[A-Za-z0-9_.-]+`. (A flat `builtins.match "/[A-Za-z0-9._/-]*"` is **insufficient** -- it
cannot reject the bare `/`, `//`, or `.`/`..` segments.) Workstream A's `MountPoint::parse`
is brought into line with this grammar.

**Factor the three Nix grammars into one shared `modules/braid/grammar.nix` predicate
module**, so each grammar has a single Nix definition that the module assertion *and* the
parity eval check both consume (no duplicated regex/length logic across `.nix` files):

- `mountPointOk = path: ...` -- the canonical-segment rule above, lifted out of the
  `options.nix` `let`-binding into a function of the path; `options.nix` becomes
  `assertion = grammar.mountPointOk cfg.mountPoint` (behaviour unchanged).
- `isValidUpsName = name: ...` -- allowlist `[A-Za-z0-9._-]+` **plus** no-leading-`-`,
  max-length-32.
- `isValidInterface = iface: ...` -- allowlist `[A-Za-z0-9._-]+` **plus** length <= 15,
  not the literal `.`/`..`, no-leading-`-`.

Each predicate is the **full** grammar (charset + length + `.`/`..` + leading-dash),
matching its Rust newtype's `parse` exactly. The module assertions call them inside their
existing feature-gated blocks:

- `ups.nix` (in `lib.mkIf (cfg.enable && ups.enable)`, `ups.nix:60`):
  `assertion = grammar.isValidUpsName ups.name;` -- supersedes the bare non-empty check at
  `ups.nix:63`.
- `auto-suspend.nix` (in `lib.mkIf (cfg.enable && cfg.autoSuspend.enable)`,
  `auto-suspend.nix:42`):
  `assertion = cfg.autoSuspend.wolInterface == null || grammar.isValidInterface cfg.autoSuspend.wolInterface;`
  -- alongside the existing non-null requirement and `wl`-prefix WiFi warning.

A comment in `grammar.nix` cross-references `cli/src/types.rs`. The Rust/Nix parity is then
an **executed comparison of named predicates** (Verification), not of full system evals --
which also sidesteps the trap that the `ups.name`/`wolInterface` assertions are feature-gated:
exercising them through a full `nixosSystem` would require `braid.ups.enable` /
`autoSuspend.enable`, dragging NUT (upsd/upsmon, `braid-ups-secrets.service`) and
autosuspend (`services.autosuspend`, `networking.interfaces.<iface>.wakeOnLan`) into the
eval for no parity benefit. Testing the predicate directly needs none of that. With the
assertions in place the unescaped interpolation in `storage.nix`/`ups.nix` is provably
safe; escaping those sites is optional follow-up, not required here.

---

## Workstream F: `MountpointCheckPath` -- the generated-keyfile probe boundary

The last raw `String -> argv` boundary the earlier draft missed (and wrongly listed as a
non-goal). `braid enroll --generate <DIR>` takes a **directory** positional
(`cli/src/main.rs#EnrollKeyFileArgs`: `dir: PathBuf`, help "DIR must already be a mount
point"); braid forms the keyfile path `<DIR>/braid.key` and
`cli/src/enroll_key_file.rs#validate_generated_keyfile_target` recovers its parent back to
`<DIR>` via `key_file_directory`
(`dir = key_file_directory(key_file_path); dir_display = dir.display().to_string()`), then
feeds `dir_display` through the **infallible, unvalidated** `MountPoint::new` into
`CmdRequest::MountpointCheck`, which `cmd.rs` renders as the bare positional
`mountpoint -q <dir_display>` (`cli/src/cmd.rs#CmdRequest::to_argv`, the `MountpointCheck`
arm -- no `--`). So a DIR that is itself flag-shaped
(`braid enroll --generate -- -o` -> keyfile `-o/braid.key` -> `dir_display = "-o"`) yields
`mountpoint -q -o`, a flag-shaped positional. (Reachable only if such a directory exists, because a
`std::fs::metadata` check precedes the probe -- so this is Low-severity defense-in-depth,
matching the rest of the plan, but it is a genuine exception to the newtype discipline
the Outcome promises to eliminate.)

This value is **not** a pool `MountPoint`: it may be **relative** (`key_file_directory`
falls back to `.` for a bare filename) and is **not** module-interpolated into any shell
/ systemd context, so it neither can nor should be forced through `MountPoint`'s
absolute-canonical grammar. It needs its own narrower invariant: *argv-safe path token*.

**Changes:**

- Add a `MountpointCheckPath` newtype (`cli/src/types.rs`) built on the **same shared
  floor helper** as `UpsName`/`Interface` (non-empty, first char not `-`) plus: reject
  ASCII control / NUL / newline. Deliberately **allow** relative paths (`.`, `../x`,
  `media/usb`) and **interior spaces** (`/media/My USB`) -- a real USB mount directory
  can contain a space, and because this token reaches a single argv element (never an
  unescaped shell/systemd interpolation, unlike `MountPoint`) an interior space is
  argv-safe. `parse(&str) -> Result<Self, MountpointCheckPathParseError>`, `as_str`,
  `Display`. No serde (not a config field).
- Add an infallible `From<MountPoint> for MountpointCheckPath` (a validated
  absolute-canonical `MountPoint` is trivially an argv-safe path token), so the existing
  pool-mount callers of `MountpointCheck` convert mechanically.
- Change `CmdRequest::MountpointCheck { path: MountPoint }` ->
  `{ path: MountpointCheckPath }` (`cli/src/cmd.rs`). **Blast radius (larger than the
  reviewer's "localized" framing):** every `MountpointCheck` construction must convert.
  The production pool-mount callers in `cli/src/mount.rs` and `cli/src/lock.rs` hold a real
  `MountPoint` -> `.into()` (infallible); the many `lock.rs` tests that build
  `MountpointCheck` update mechanically (wrap their `MountPoint`, or construct the new
  type directly). The `to_argv` arm is unchanged textually (still `mountpoint -q
  <path.as_str()>`), so no dry-run/preview pin moves.
- **Retype the `online_state` mountpoint probe through `MountPoint`, not `&Path`**
  (`cli/src/online_state.rs`, `cli/src/doctor.rs`). `OnlineStateOps::is_mountpoint`
  currently takes an arbitrary `&Path` and re-wraps it via the infallible
  `MountPoint::new(path.display().to_string())` (`online_state.rs:148`) before building
  `MountpointCheck` -- an unvalidated `MountPoint::new` door (the two `tui/model.rs` sites
  closed in Workstream A are the others), and a site that would also fail to compile once
  the field type changes. Change the trait
  method to `is_mountpoint(&self, mount_point: &MountPoint)` and render
  `CmdRequest::MountpointCheck { path: mount_point.clone().into() }` (the new
  `From<MountPoint>`). Its callers already hold a config `MountPoint` and pass
  `Path::new(mp.as_str())` today -- `mark_online`/`mark_offline` (`online_state.rs:266,342`)
  and the three `doctor.rs` sites (`624,1448,1606`) -- so they pass the `&MountPoint`
  directly instead (where a caller also needs the `&Path` for a separate probe, e.g.
  `doctor.rs:1607` `is_immutable`, that local stays). The `RecordingOnlineStateOps` test
  double's signature and the four `RealOnlineStateOps` MockRunner expectations
  (`online_state.rs:567-646`, which build `MountpointCheck { path: MountPoint::new(..) }`
  and call `is_mountpoint(Path::new(..))`) update mechanically. This removes the probe's
  dependence on `MountPoint::new` (see Workstream A, "Why new stays infallible").
- In `validate_generated_keyfile_target`, parse `dir_display` into `MountpointCheckPath`
  **immediately after deriving it** (before the `std::fs::metadata` existence check), so a
  flag-shaped / empty / control directory fails fast with a clear braid Validation error
  regardless of whether such a directory happens to exist on disk -- and the regression
  does not depend on creating a `-o` directory. The valid relative `.` case still parses.
- This **removes** the keyfile-dir exception from `MountPoint::new` (Workstream A): after
  this change `new` only re-wraps already-validated `MountPoint`s.

---

## Non-goals (explicit)

- **No `--` separators** in `to_argv` (the rejected approach: redundant on validated
  slots, unverifiable/risky on ethtool's custom parser, large pin/preview churn for no
  reachable benefit).
- **No re-typing of `BtrfsFilesystemShowTarget { target: String }`** -- it is
  internally derived from a validated `ByIdPath`/mapper `dev_path()` (absolute, already
  argv-safe). A stronger type here is a separate cleanup, not this fix.
- (Removed -- formerly "no change to `MountpointCheck`.") The generated-keyfile probe
  **is** now in scope: it was a genuine raw-`String` -> argv boundary, so deferring it
  contradicted the Outcome. It is closed by Workstream F (`MountpointCheckPath`).

---

## Files touched

- `cli/src/types.rs` -- `MountPoint` (parse/error/serde/`new` doc); new `UpsName`,
  `Interface`, `BackingPath`, `MountpointCheckPath` (+ `From<MountPoint>`), shared token
  helper; `MapperName::from_basename` debug-assert; new unit tests.
- `cli/src/cmd.rs` -- `CmdRequest::MountpointCheck { path: MountPoint }` ->
  `{ path: MountpointCheckPath }` (the `to_argv` arm text is unchanged).
- `cli/src/enroll_key_file.rs` -- `validate_generated_keyfile_target` parses `dir_display`
  into `MountpointCheckPath` (before the metadata check) instead of `MountPoint::new`.
- `cli/src/mount.rs`, `cli/src/lock.rs` -- pool-mount `MountpointCheck` callers convert
  their validated `MountPoint` via `.into()`; `lock.rs` tests that build `MountpointCheck`
  update mechanically.
- `cli/src/online_state.rs`, `cli/src/doctor.rs` -- retype `OnlineStateOps::is_mountpoint`
  to take `&MountPoint`; `mark_online`/`mark_offline` and the three `doctor.rs` callers
  pass the config `MountPoint` directly; the `RecordingOnlineStateOps` double and the four
  `RealOnlineStateOps` MockRunner expectations update mechanically (closes that production
  `MountPoint::new` door).
- `cli/src/tui/model.rs`, `cli/src/tui/mod.rs` -- close the two TUI `MountPoint::new` doors
  (Workstream A): `Model::new` takes `mount_point: MountPoint` (caller `tui/mod.rs#run`
  passes `config.mount_point().clone()`); `Model::new_demo` builds
  `MountPoint::parse("/mnt/storage").expect("valid demo mount point")` instead of
  `MountPoint::new(String::new())`. Neither is compiler-flagged (`new` stays infallible), so
  both are explicit must-fix sites.
- `cli/src/recover.rs`, `cli/src/unlock.rs`, and the shared fixtures
  `cli/src/test_fixtures/{lock,mount,enroll_key_file,doctor}.rs` -- the remaining
  `MountpointCheck` construction sites the earlier Files-touched list omitted.
  `recover.rs#discover_add_targets_before_mount` (~line 2029) is production, holding a
  `MountPoint` -> `.into()`; the rest are tests/fixtures and update mechanically (`.into()`
  a held `MountPoint`, or build `MountpointCheckPath` directly). `test_fixtures/doctor.rs`
  also `match`es the variant with `path.as_str() == "/mnt/storage"`, which keeps working
  via `MountpointCheckPath::as_str`.
- `cli/src/config.rs` -- `Ups.name`/`AutoSuspend.wol_interface` field types; test
  assertions.
- `cli/src/main.rs` -- `ScrubCancelArgs` + `ScrubMountArgs` `mount` field becomes
  `MountPoint` via `value_parser = MountPoint::parse`; remove the `MountPoint::new`
  dispatch calls; add `#[cfg(test)]` clap parse tests.
- `cli/src/parse/types.rs` -- `BackingDevice::Path(String)` -> `Path(BackingPath)`.
- `cli/src/parse/cryptsetup_status.rs` -- validate at construction; fixture update.
- Backing-path consumers carry the newtype into `CryptsetupLuksUuid`:
  `cli/src/probe.rs`, `cli/src/luks.rs#classify_mapper_ownership`,
  `cli/src/probe_mapper_uuid.rs`, `cli/src/lock.rs`, `cli/src/tui/probe.rs`.
- `cli/src/tui/browse/state.rs` -- four `.name.as_str().to_owned()` updates.
- `modules/braid/grammar.nix` (**new**) -- single Nix home for the three grammar predicates
  `mountPointOk` / `isValidUpsName` / `isValidInterface`, each matching its Rust newtype;
  comment cross-references `cli/src/types.rs`.
- `modules/braid/options.nix` -- lift the inline `mountPointOk` `let`-binding into
  `grammar.nix`; the assertion becomes `grammar.mountPointOk cfg.mountPoint` (behaviour
  unchanged, so the shipped `mountpoint-assertion-{ok,fails}.nix` still pass).
- `modules/braid/ups.nix`, `modules/braid/auto-suspend.nix` -- add
  `grammar.isValidUpsName ups.name` (supersede the bare non-empty check at `ups.nix:63`)
  and `grammar.isValidInterface` (when non-null) inside their existing feature-gated `mkIf`
  blocks.
- `tests/eval/` -- add a **predicate-parity** check (e.g. `grammar-parity.nix`) that imports
  `modules/braid/grammar.nix` and runs the shared accept/reject matrix through `mountPointOk`
  / `isValidUpsName` / `isValidInterface` directly (accept -> `true`, reject -> `false`),
  asserting the exact lists the Rust `parse` tests use. No `nixosSystem`, no feature-enable.
  The shipped `mountpoint-assertion-{ok,fails}.nix` **stay as-is** (full-system wiring +
  message coverage for the non-gated mountPoint assertion); they are not extended to the
  full matrix (the predicate check owns matrix parity), and `_braid-eval-harness.nix` needs
  no `ups`/`autoSuspend` parametrization.
- `flake.nix` -- register the predicate-parity check as a `checksFor` attr (e.g.
  `eval-grammar-parity = import ./tests/eval/grammar-parity.nix { ... };`) so `nix flake
  check` runs it. Eval checks are **not** auto-discovered: the existing
  `eval-mountpoint-accepts-valid` / `eval-mountpoint-rejects-bad-chars`
  (`flake.nix:919,911`) are `import`ed explicitly and keep their attrs. Per the `flake.nix`
  checks-registration rule in `docs/dev/testing.md`.
- Test-only mechanical touch-ups where `.as_str()` is now required.

## Verification

- `just test-rust` -- full unit suite, including (Intent/Why/Scenario comment style per
  existing `by_id_path_parse_requires_prefix`):
  - `MountPoint::parse` accepts `/mnt/storage`, `/mnt/tank-1`, `/mnt/storage/` (one
    trailing slash), `/mnt/.snapshots` (hidden segment); rejects `mnt/storage`, `-o`,
    ``, `/` (bare root), `/mnt//storage` (doubled separator), `/mnt/./storage`,
    `/mnt/../storage` (`.`/`..` segments), `/mnt/my drive` (space), and `/mnt/x;touch`
    (metacharacter). This is the **same** accept/reject set the `grammar.nix` `mountPointOk`
    predicate is run through in the `tests/eval` parity check (see the parity bullet below),
    so the Rust and Nix grammars share one executed matrix.
  - Config deserialize: a config.json with a relative / `-o` / space-bearing
    `mount_point` fails `serde_json::from_str` with a clear message; a UPS name with
    `@`/`:`/whitespace and an interface that is `.`/`..`/`eth:0`/`eth/0`/16-char/`-i`
    fail too.
  - `UpsName` accepts `ups`, `my-ups`, `ups_1`; **rejects** `ups@host:3493`, `ups:1`,
    `-x`, `with space`. `Interface` accepts `eno1`, `br0`, `eth0.100`; rejects `eth/0`,
    `eth:0`, `.`, `..`, a 16-char name, `-i`, `with space`.
  - `BackingPath::parse` accepts `/dev/vdb`, `/dev/mapper/braid-x`; rejects a
    non-absolute / flag-like / whitespace value. A `parse_cryptsetup_status` test feeds
    a non-absolute `device:` line and asserts the **exact** variant
    `ParseError::InvalidValue { field: "device", .. }` (panicking on any other variant,
    mirroring the `cli/src/parse/cryptsetup_luks_uuid.rs` UUID tests that assert
    `InvalidValue` with its field name) -- so the field/raw/detail structure is
    preserved and no `CryptsetupLuksUuid` is ever issued on a flag-like device, across
    **all** consumer flows, not just `probe_pool`.
  - **CLI boundary** (`#[cfg(test)]` in `main.rs`): `Cli::try_parse_from` for
    `scrub-cancel`, `scrub-needs-resume`, and `scrub-resume-or-start` each with
    `--mount=-o` returns `Err` with clap `ErrorKind::ValueValidation`; the valid
    `--mount=/mnt/storage` form parses to the expected `MountPoint`. (Guards the
    "boundary left unwired" regression F5 calls out -- there is no `MountPoint::new`
    left to forget.)
  - **Generated-keyfile probe** (Workstream F): `MountpointCheckPath::parse` accepts
    `.`, `media/usb` (relative), `/media/usb`, `/media/My USB` (interior space); rejects
    ``, `-o` (leading dash), and a control/newline-bearing value. A
    `validate_generated_keyfile_target` test with a fake `CommandRunner` and the keyfile
    path `-o/braid.key` (the real `<DIR>/braid.key` shape with DIR `-o`, so
    `key_file_directory` yields `dir_display = "-o"` -- *not* a bare `-o` keyfile path,
    whose parent normalizes to `.`) returns the **specific `MountpointCheckPath`
    leading-dash parse rejection** and issues **no** `mountpoint -q` command (assert the
    fake runner recorded zero calls). Asserting the *specific* parse error -- not merely
    "some `Validation` error" -- is what proves parse runs **before** the
    `std::fs::metadata` check: `-o` does not exist in the test, so a metadata-first
    ordering would instead surface the distinct `keyfile directory does not exist: -o`
    Validation error (`enroll_key_file.rs:678`), also with zero runner calls -- a weaker
    assertion would pass either way. The valid `.`/absolute cases still reach the probe.
    (Equivalent CLI-level repro: `braid enroll --generate -- -o`.)
  - **`from_basename` debug-assert** (Workstream C): a `#[cfg(debug_assertions)]`
    `#[should_panic]` test on `MapperName::from_basename("-x")` (and `"a b"`) pins that
    `is_plausible_mapper_basename` rejects a leading-dash / whitespace basename, closing the
    plan's one remaining test-coverage gap.
- **Module boundary** (eval checks under `tests/eval/`, registered in `flake.nix#checksFor`
  -- Files touched -- or they never run): the shipped full-system
  `mountpoint-assertion-{ok,fails}.nix` (commit `0b9333ed`) stay as wiring + braid-message
  coverage for the non-gated mountPoint assertion. The `ups.name`/`wolInterface` grammars
  are covered by the predicate-parity check (next bullet), **not** full-system eval --
  deliberately, since their assertions are feature-gated behind
  `braid.ups.enable`/`autoSuspend.enable` and a full `nixosSystem` would pull
  NUT/autosuspend into the eval for no parity benefit. Residual (accepted): the one-line
  `grammar.isValidUpsName`/`isValidInterface` assertion *wiring* in
  `ups.nix`/`auto-suspend.nix` is not itself eval-tested; the predicate it calls is.
- **Grammar parity matrix (executed, not documented):** one shared accept/reject sample
  list per type (`mountPoint`/`UpsName`/`Interface`) is run through **both** sides and must
  agree -- each sample asserted in the Rust `parse` unit tests (accept -> `Ok`, reject ->
  `Err`) **and** in the `tests/eval` predicate-parity check that calls the matching
  `grammar.nix` predicate on the **raw string** (accept -> `true`, reject -> `false`).
  Because the predicate check feeds raw strings straight to `mountPointOk` /
  `isValidUpsName` / `isValidInterface`, it sidesteps `lib.types.path` entirely, so the
  relative/empty `mountPoint` samples (`mnt/storage`, ``) are exercised directly against the
  grammar with no dependence on whether the option type pre-filters them. (For the record,
  `lib.types.path` *does* pre-filter relative/empty via its leading-`/` `check` -- now
  irrelevant to parity, since the matrix no longer routes through the option type; it
  matters only to the shipped full-system `mountpoint-assertion-fails.nix`, which keeps its
  existing absolute-but-bad samples that the assertion itself rejects.) This **replaces** the
  earlier "test or shared documented constant" divergence guard with an executed
  cross-boundary check, so the grammars cannot drift silently (the exact failure the audit's
  Finding 1 caught: a looser Rust rule the Nix side already rejected).
- `cargo build -p braid_cli` -- the `String -> newtype` changes (config fields,
  `BackingDevice::Path`, the `mount` args) and the `MountpointCheck` field-type change
  (which forces the `online_state`/`doctor` `is_mountpoint` conversions) make every missed
  consumer site a compile error.
- `rg -F 'CmdRequest::MountpointCheck' cli/src` -- enumerate every construction site (12
  files: `cmd.rs` definition + `to_argv` arm, `mount.rs`, `lock.rs`, `recover.rs`,
  `unlock.rs`, `online_state.rs`, `doctor.rs`, `enroll_key_file.rs`, and the four
  `test_fixtures/{lock,mount,enroll_key_file,doctor}.rs`; `probe.rs`'s lone match is a
  comment, not a construction) and confirm none still feeds a bare `MountPoint` into
  `path:` -- each is either an `.into()` of a held `MountPoint` or a directly-built
  `MountpointCheckPath`. The field-type change already makes any miss a compile error
  (above); this `rg` is the human cross-check that the conversion is total, since the
  multi-line `path:` constructions are not all visible to a single-line grep of the old
  `{ path: MountPoint` pattern.
- `cargo clippy` -- clean.
- `scripts/docs/check-output-ascii.py` -- new Rust error messages are ASCII; new Nix
  assertion messages are ASCII (`--`, `'`).
- `rg` the VM-test configs and `modules/` examples under `tests/` to confirm fixtures
  use absolute mount points and ordinary UPS/interface names (`/mnt/...`, `ups`, `eno1`)
  so no NixOS VM test regresses on the new assertions.
- Confirm dry-run output is byte-identical (no `to_argv` arm changed) -- the
  `*_generates_correct_argv` pins should pass untouched, proving the fix is invisible
  to command rendering.
