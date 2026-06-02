# Shared UPS test harness for the forced-shutdown / recovery-proof matrix.
#
# NixOS module fragment -- import with parameters, e.g.:
#
#   imports = [
#     ../../modules/braid
#     (import ./lib/ups-fixture.nix {
#       upsName = "ups";
#       testopsPassword = "testpass";
#     })
#   ];
#
# What it sets up
# ----------------
#
# 1. `braid.ups.enable = true` with `driver = "dummy-ups"` and
#    `port = "ups.dev"`. The braid module wires nixpkgs `power.ups` plus the
#    SHUTDOWNCMD = systemctl poweroff override, and provisions the production
#    upsmon credential at /var/lib/braid/upsmon.pass.
#
# 2. `/etc/nut/ups.dev` populated with a default "OL" snapshot. The tests
#    flip ups.status to "OB LB" via upsrw at runtime to drive shutdown.
#
# 3. A test-only second upsd user `testops` carrying `actions = [ "SET" ]`
#    so upsrw can write to ups.status. The production upsmon user
#    intentionally does not carry SET.
#
# Load-bearing details
# --------------------
#
# - **dummy-once mode** (the `.dev` extension on `port`): per
#   `reference/nut/docs/man/dummy-ups.txt:90,100`, the file is parsed ONCE at
#   driver start and held in memory; subsequent `upsrw` writes mutate the
#   in-memory state and persist for the lifetime of the driver. The
#   alternative (`.seq` / `.dev` repeated, dummy-loop) re-reads the file on
#   every poll cycle, which CLOBBERS upsrw writes before upsmon has a chance
#   to react to a critical state. The matrix tests set "OB LB" via upsrw and
#   need the value to stick; dummy-once is the only correct choice. If a
#   future edit changes `port` to anything other than `ups.dev`, the matrix
#   tests will silently never trigger SHUTDOWNCMD and look like they pass for
#   the wrong reason.
#
# - **Test credential is separate from production**. Per
#   `reference/nut/docs/man/upsd.users.txt:78`, the `actions = [ "SET" ]`
#   privilege is only required by `upsrw` clients. The production upsmon
#   credential (the `users.${upsName}` block in `modules/braid/ups.nix`)
#   stays minimal. This harness adds a SECOND user named `testops` with SET
#   so the test script can drive `upsrw`. Refactors that consolidate the
#   two users would silently grant SET to production upsmon -- do not do
#   that.
#
# How tests drive UPS state
# -------------------------
#
# Inside the .py testScript, after `upsd.service` is up:
#
#   machine.succeed(
#       "upsrw -s 'ups.status=OB LB' "
#       "-u testops -p ${testopsPassword} ${upsName}@localhost"
#   )
#
# The `-s 'OB LB'` quoting is required so the multi-token status string
# arrives as a single value, not two argv tokens
# (reference/nut/clients/upsrw.c).
{
  upsName ? "ups",
  testopsPassword ? "testpass",
  # Battery snapshot the dummy-ups driver loads at boot. Default reports
  # OL with full charge so the test script can mount + stage data before
  # flipping to OB+LB. This OL default is load-bearing for the
  # mutating-command preflight pass path: the `ups-lb-during-*` matrix
  # starts its mutation while OL holds, and `check_ups_not_on_battery`
  # requires OL (not merely the absence of OB) to pass. Override (e.g.
  # raise `battery.runtime.low`) if a particular matrix test needs more of
  # a runtime budget for the interrupted mutation to reach a useful
  # in-flight state. See ADR 020 Open Question 3.
  devContent ? ''
    device.mfr: Dummy
    device.model: braid-ups-fixture
    ups.status: OL
    battery.charge: 100
    battery.charge.low: 10
    battery.runtime: 1800
    battery.runtime.low: 120
  '',
  # Upsmon poll/notify cadence overrides. The default squeezes POLLFREQ /
  # POLLFREQALERT / FINALDELAY (upstream 5/5/5) down to 1/1/0 so the
  # forced-shutdown matrix tests can land OB+LB while a slow mutation
  # (replace / balance / remove) is still in flight; if POLLFREQ +
  # FINALDELAY adds up to ~10s the mutation finishes first and the test
  # silently degrades to "no journal to recover from."
  #
  # Pass `upsmonTimings = null` to leave upsmon at upstream defaults --
  # required for `ups-lb-clean-shutdown`, which is the proof referenced
  # by ADR 020 Open Question 3 ("default runtime budget is sufficient").
  # That test must exercise production timings to be representative; it
  # is not racing an in-flight mutation, so the wider LB-detection window
  # is fine.
  upsmonTimings ? {
    POLLFREQ = 1;
    POLLFREQALERT = 1;
    # FINALDELAY=0 = SHUTDOWNCMD fires the moment upsmon detects critical.
    # Matrix tests need every millisecond of margin between the LB trigger
    # and the actual umount; the shutdown sequence (systemd unwind ->
    # braid-online ExecStop -> braid lock -> umount) already takes ~1s on
    # the test VMs, so even a 1-second FINALDELAY can let an in-memory
    # tmpfs-backed `btrfs replace` finish before the umount cancels it.
    FINALDELAY = 0;
  },
}:
{ pkgs, lib, ... }:
{
  braid.ups = {
    enable = true;
    name = upsName;
    driver = "dummy-ups";
    # `.dev` extension selects dummy-ONCE mode (load-bearing -- see top
    # comment). Do not change to `.seq` or any other extension without
    # also revisiting why the matrix tests rely on persistent upsrw writes.
    port = "ups.dev";
  };

  # Dummy-ups input file. dummy-once parses this ONCE at driver start;
  # later upsrw writes are kept in memory until the driver restarts.
  environment.etc."nut/ups.dev".text = devContent;

  # Apply the caller-controlled upsmon timing overrides (see the
  # `upsmonTimings` parameter docstring above). Production
  # `braid.ups.enable = true` ships upstream defaults via
  # `modules/braid/ups.nix`; these overrides apply only to test VMs that
  # import this fixture.
  #
  # Defaults from `reference/nut/conf/upsmon.conf.sample.in:236,249,542`
  # are 5/5/5 (POLLFREQ / POLLFREQALERT / FINALDELAY).
  power.ups.upsmon.settings = lib.mkIf (upsmonTimings != null) upsmonTimings;

  # Test-only user. Distinct from the production `users.${upsName}` user
  # provisioned by modules/braid/ups.nix. Keeping them separate is a
  # security invariant -- production upsmon does not need SET.
  power.ups.users.testops = {
    passwordFile = toString (pkgs.writeText "testops.pass" testopsPassword);
    actions = [ "SET" ];
  };

  # Tests need `upsrw`, plus `cryptsetup` / `btrfs` for assertions about
  # mappers and pool state, plus `lsblk` for the orphaned-mapper check.
  environment.systemPackages = [
    pkgs.btrfs-progs
    pkgs.cryptsetup
    pkgs.nut
    pkgs.util-linux
  ];
}
