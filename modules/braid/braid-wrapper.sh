#!@shell@
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

# Pool-mutating commands (unlock, add, recover) acquire an exclusive
# non-blocking flock on /run/braid-pool.lock. braid does not queue pool
# operations: a concurrent attempt fails fast with a clear message and
# the user must retry once the active operation finishes. This enforces
# mutual exclusion at the critical section itself, not via systemd unit
# topology. See Principle 12.
case "$subcmd" in
  unlock|add|recover)
    if ! $skip_fixup; then
      exec 9>/run/braid-pool.lock
      if ! @flockBin@ -n 9; then
        echo "braid: another braid operation is already in progress (pool lock /run/braid-pool.lock is held); retry once it finishes" >&2
        exit 1
      fi
    fi
    ;;
esac

# For unlock specifically: re-check after acquiring the lock — a prior
# unlock that ran sequentially (and released the lock) may have already
# mounted the pool. Does NOT apply to add/recover, which operate on a
# mounted pool.
case "$subcmd" in
  unlock)
    if ! $skip_fixup; then
      if @mountpointBin@ -q "@mountPointPath@" 2>/dev/null; then
        echo "pool already mounted at @mountPointPath@"
        exit 0
      fi
    fi
    ;;
esac

# Stop scrub timer and service before CLI lock attempts unmount.
# Timer must stop first — otherwise it can re-trigger the service between
# service stop and unmount. braid-scrub.service holds the mount busy while
# running (-B flag); without this, umount would fail with EBUSY.
# Harmless no-op when autoScrub is disabled (units don't exist) or scrub
# isn't running.
case "$subcmd" in
  lock)
    if ! $skip_fixup; then
      @systemctlBin@ stop braid-scrub.timer 2>/dev/null || true
      @systemctlBin@ stop braid-scrub.service 2>/dev/null || true
    fi
    ;;
esac

@braidBin@ "$@"
ret=$?

if [ "$ret" -eq 0 ] && ! $skip_fixup; then
  case "$subcmd" in
    unlock|add|recover)
      if @mountpointBin@ -q "@mountPointPath@" 2>/dev/null; then
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
        # --no-block: when braid-online.service's ExecStop runs `braid lock`,
        # the wrapper would call `systemctl stop braid-online.service` again.
        # A synchronous stop here deadlocks — systemd waits for ExecStop to
        # exit, but the wrapper is waiting for the stop to complete.  --no-block
        # queues the stop and returns immediately, breaking the cycle.
        @systemctlBin@ stop --no-block braid-online.service 2>/dev/null || true
      fi
      ;;
  esac
fi
exit $ret
