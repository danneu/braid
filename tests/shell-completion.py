# Test: braid shell completion
#
# What: End-to-end tests for shell completion — registration script generation
# for bash/zsh/fish, subcommand and flag candidate correctness, dynamic disk
# path candidates from config, --config override, and missing-config fallback.
#
# Why: Completions are user-facing and must stay in sync with the CLI structure
# and the NixOS-generated config. Regressions here silently break the UX.
#
# Dependencies: Rust braid binary with clap_complete CompleteEnv support.

# --- Registration scripts are generated for each shell ---

with subtest("bash registration"):
    reg = machine.succeed("COMPLETE=bash braid")
    assert "braid" in reg
    assert "complete" in reg.lower()

with subtest("zsh registration"):
    reg = machine.succeed("COMPLETE=zsh braid")
    assert "braid" in reg

with subtest("fish registration"):
    reg = machine.succeed("COMPLETE=fish braid")
    assert "braid" in reg

# --- Bash end-to-end: source completions, trigger them, assert candidates ---

# Write a helper script that sources the registration and invokes the
# completion function for a given partial command line.
machine.succeed("""
cat > /tmp/get-completions.sh << 'SCRIPT'
#!/usr/bin/env bash
eval "$(COMPLETE=bash braid)"
COMP_WORDS=("$@")
COMP_CWORD=$((${#COMP_WORDS[@]}-1))
COMP_LINE="${COMP_WORDS[*]}"
COMP_POINT=${#COMP_LINE}
COMPREPLY=()
func=$(complete -p braid 2>/dev/null | grep -oP '(?<=-F )\\S+')
"$func"
printf '%s\\n' "${COMPREPLY[@]}"
SCRIPT
chmod +x /tmp/get-completions.sh
""")

with subtest("subcommand completion"):
    output = machine.succeed("bash /tmp/get-completions.sh braid ''")
    for cmd in ["init-disk", "plan", "apply", "status", "doctor"]:
        assert cmd in output, f"Missing subcommand '{cmd}': {output}"

with subtest("init-disk disk path completion"):
    output = machine.succeed("bash /tmp/get-completions.sh braid init-disk ''")
    assert "/dev/disk/by-id/virtio-disk1" in output
    assert "/dev/disk/by-id/virtio-disk2" in output

with subtest("init-disk flag completion"):
    output = machine.succeed("bash /tmp/get-completions.sh braid init-disk --")
    assert "--force" in output

with subtest("plan flag completion"):
    output = machine.succeed("bash /tmp/get-completions.sh braid plan --")
    assert "--json" in output
    assert "--allow-remove-missing" in output

with subtest("status flag completion"):
    output = machine.succeed("bash /tmp/get-completions.sh braid status --")
    assert "--verbose" in output
    assert "--json" in output

# --- --config override during completion (bash) ---

machine.succeed("""echo '{"disks":["/dev/disk/by-id/alt-disk"],"mountPoint":"/mnt/alt"}' > /tmp/alt-config.json""")

with subtest("disk completion respects --config"):
    output = machine.succeed("bash /tmp/get-completions.sh braid --config /tmp/alt-config.json init-disk ''")
    assert "/dev/disk/by-id/alt-disk" in output, f"Expected alt-disk: {output}"
    # Should NOT contain the default config's disks
    assert "/dev/disk/by-id/virtio-disk1" not in output, f"Should not contain default disk: {output}"

with subtest("disk completion with missing config returns empty"):
    output = machine.succeed("bash /tmp/get-completions.sh braid --config /tmp/nonexistent.json init-disk ''")
    assert "/dev/disk/by-id/" not in output, f"Expected no disk candidates: {output}"

# --- Fish end-to-end: source completions, trigger them, assert candidates ---

with subtest("fish subcommand completion"):
    out = machine.succeed("fish -c 'COMPLETE=fish braid | source; complete --do-complete \"braid \"'")
    for cmd in ["init-disk", "plan", "apply", "status", "doctor"]:
        assert cmd in out, f"Missing {cmd}: {out}"

with subtest("fish init-disk disk path completion"):
    out = machine.succeed("fish -c 'COMPLETE=fish braid | source; complete --do-complete \"braid init-disk \"'")
    assert "/dev/disk/by-id/virtio-disk1" in out
    assert "/dev/disk/by-id/virtio-disk2" in out

with subtest("fish plan flag completion"):
    out = machine.succeed("fish -c 'COMPLETE=fish braid | source; complete --do-complete \"braid plan --\"'")
    assert "--json" in out
    assert "--allow-remove-missing" in out

# --- --config override during completion (fish) ---

with subtest("fish completion respects --config"):
    out = machine.succeed("fish -c 'COMPLETE=fish braid | source; complete --do-complete \"braid --config /tmp/alt-config.json init-disk \"'")
    assert "/dev/disk/by-id/alt-disk" in out, f"Expected alt-disk: {out}"

# TODO: add zsh end-to-end completion tests
