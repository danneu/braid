#!@shell@
# shellcheck shell=bash
export PATH="@toolPath@:$PATH"

# Parse subcommand before execution — needed to decide whether to acquire
# the pool lock and which post-processing path to take.
# Mirrors the global CLI shape in cli/src/main.rs (struct Cli).
# If global options change there, update this parser to match.
subcmd=""
skip_next=false
for arg in "$@"; do
  if $skip_next; then
    skip_next=false
    continue
  fi
  case "$arg" in
    --config) skip_next=true ;;
    --config=*) ;;
    -*) ;;
    *) subcmd="$arg"; break ;;
  esac
done

skip_fixup=false
for arg in "$@"; do
  case "$arg" in
    --help|-h|--version|-V|--dry-run) skip_fixup=true; break ;;
  esac
done

in_systemd_execstop=false
case "${BRAID_SYSTEMD_EXECSTOP:-}" in
  1|true|yes) in_systemd_execstop=true ;;
esac

# Pool mutators, alert-state mutators, and discover acquire
# /run/braid-pool.lock before the CLI runs. The lock intentionally spans
# subprocess I/O as well as JSON file writes, so monitor cannot compare
# pre-ack btrfs stats against a post-ack baseline and re-latch a stale
# alert.
#
# Contention behavior is per command:
# - unlock/add/recover/remove/remove-missing/replace/discover: non-blocking
#   fail-fast for interactive pool-state work; the user retries once the active
#   operation finishes.
# - ack: wait briefly for a monitor cycle, but bound the wait so a stuck
#   pool operation still returns a clear retry message.
# - monitor: non-blocking silent exit 0; a missed timer cycle is harmless,
#   and exit 1 would falsely start the alert service.
# This enforces mutual exclusion at the critical section itself, not via
# systemd unit topology. See Principle 12.
case "$subcmd" in
  unlock|add|recover|remove|remove-missing|replace|discover)
    if ! $skip_fixup; then
      exec 9>/run/braid-pool.lock
      if ! @flockBin@ -n 9; then
        echo "braid: another braid operation is already in progress (pool lock /run/braid-pool.lock is held); retry once it finishes" >&2
        exit 1
      fi
    fi
    ;;
  ack)
    if ! $skip_fixup; then
      exec 9>/run/braid-pool.lock
      if ! @flockBin@ -w 10 9; then
        echo "braid: another braid operation is in progress (pool lock /run/braid-pool.lock is held); retry once it finishes" >&2
        exit 1
      fi
    fi
    ;;
  monitor)
    if ! $skip_fixup; then
      exec 9>/run/braid-pool.lock
      if ! @flockBin@ -n 9; then
        exit 0
      fi
    fi
    ;;
esac

# Stop scrub timer, resume trigger, and service before CLI lock attempts unmount.
# Timer must stop first -- otherwise it can re-trigger the service between
# service stop and unmount. The trigger stops before the service so a freshly
# fired trigger cannot queue a new service start after we stop it.
# braid-scrub.service holds the mount busy while running (-B flag); without
# this, umount would fail with EBUSY. Harmless no-op when autoScrub is disabled
# (units don't exist) or scrub isn't running.
case "$subcmd" in
  lock)
    if ! $skip_fixup; then
      @systemctlBin@ stop braid-scrub.timer 2>/dev/null || true
      @systemctlBin@ stop braid-scrub-resume-trigger.service 2>/dev/null || true
      @systemctlBin@ stop braid-scrub.service 2>/dev/null || true
    fi
    ;;
esac

# Pre-stop pool consumers (samba, nfs, future) declared via
# BindsTo=braid-online.service. systemd exposes the inverse as the BoundBy=
# read-only property, making this single-source-of-truth: a new consumer
# only needs the BindsTo declaration, no wrapper edit. Without this, a
# direct user-initiated `braid lock` would hit EBUSY on umount because
# the BindsTo cascade only fires when systemd itself drives the stop.
#
# The scrub block above is left in place because it encodes
# timer->trigger->service ordering to prevent re-trigger races that don't
# apply to long-running consumers. We skip the three scrub units here to
# avoid cosmetic re-stop noise.
#
# Error reporting differs from the scrub block: the scrub block suppresses
# errors because units may not exist (autoScrub disabled). Here we trust
# BoundBy -- anything systemd reports as bound exists, so a non-zero exit
# is a real failure the user should see. We still attempt the lock; the
# consumer may have been mid-deactivation and umount may still succeed.
case "$subcmd" in
  lock)
    if ! $skip_fixup; then
      bound_by=$(@systemctlBin@ show -P BoundBy braid-online.service 2>/dev/null || true)
      for unit in $bound_by; do
        case "$unit" in
          braid-scrub.timer|braid-scrub.service|braid-scrub-resume-trigger.service)
            continue ;;
        esac
        ec=0
        @systemctlBin@ stop "$unit" || ec=$?
        if [ "$ec" -ne 0 ]; then
          echo "braid: WARNING: failed to stop $unit (exit $ec) -- continuing; umount may fail" >&2
        fi
      done
    fi
    ;;
esac

# 9>&-: drop the pool-lock fd in the forked child before exec, so braid
# (and any descendant it spawns -- notably the long-lived systemd-inhibit
# subprocess in cli/src/inhibit.rs, which is in its own pgroup and can
# survive a SIGKILL/OOM/default-signal-termination of braid) does not
# inherit fd 9. The wrapper bash itself keeps fd 9 open, so the flock is
# held for the entire operation and released by the kernel when this
# wrapper exits (whether braid succeeded, failed, or was killed).
@braidBin@ "$@" 9>&-
ret=$?

if [ "$ret" -eq 0 ] && ! $skip_fixup; then
  case "$subcmd" in
    unlock|add|recover)
      if @mountpointBin@ -q "@mountPointPath@" 2>/dev/null; then
        # shellcheck disable=SC2157  # @storageGroup@ is a Nix substitution, may be empty
        if [ -n "@storageGroup@" ]; then
          if ! @chownBin@ "root:@storageGroup@" "@mountPointPath@"; then
            echo "braid: WARNING: failed to set ownership on @mountPointPath@" >&2
          fi
          if ! @chmodBin@ 2770 "@mountPointPath@"; then
            echo "braid: WARNING: failed to set permissions on @mountPointPath@" >&2
          fi
        fi
        if ! @systemctlBin@ start braid-online.service 2>/dev/null; then
          echo "braid: WARNING: failed to activate braid-online.service -- pool is mounted but shutdown may not lock automatically" >&2
        fi
      fi
      ;;
    lock)
      if ! @mountpointBin@ -q "@mountPointPath@" 2>/dev/null; then
        if ! $in_systemd_execstop; then
          @systemctlBin@ stop braid-online.service 2>/dev/null || true
        fi
      fi
      ;;
  esac
fi
exit $ret
