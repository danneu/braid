#!@shell@
export PATH="@toolPath@:$PATH"
@braidBin@ "$@"
ret=$?

if [ -n "@storageGroup@" ] && [ "$ret" -eq 0 ]; then
  # Subcommand detection mirrors the global CLI shape in cli/src/main.rs (struct Cli).
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

  if ! $skip_fixup; then
    case "$subcmd" in
      unlock|add)
        if @mountpointBin@ -q "@mountPointPath@" 2>/dev/null; then
          if ! @chownBin@ "root:@storageGroup@" "@mountPointPath@"; then
            echo "braid: WARNING: failed to set ownership on @mountPointPath@" >&2
          fi
          if ! @chmodBin@ 2770 "@mountPointPath@"; then
            echo "braid: WARNING: failed to set permissions on @mountPointPath@" >&2
          fi
        fi
        ;;
    esac
  fi
fi
exit $ret
