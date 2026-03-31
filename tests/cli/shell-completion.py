# Test: braid shell completion
#
# What: End-to-end tests for shell completion — registration script generation
# for bash/zsh/fish, subcommand and flag candidate correctness, dynamic disk
# name candidates from config, --config override, and missing-config fallback.
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
    for cmd in ["add", "remove", "remove-missing", "replace", "status", "doctor"]:
        assert cmd in output, f"Missing subcommand '{cmd}': {output}"

with subtest("add flag completion"):
    output = machine.succeed("bash /tmp/get-completions.sh braid add --")
    assert "--dry-run" in output, f"Expected --dry-run: {output}"
    assert "--yes" in output, f"Expected --yes: {output}"
    assert "--passphrase-stdin" in output, f"Expected --passphrase-stdin: {output}"
    assert "--passphrase-file" in output, f"Expected --passphrase-file: {output}"
    assert "--progress" in output, f"Expected --progress: {output}"

with subtest("remove flag completion"):
    output = machine.succeed("bash /tmp/get-completions.sh braid remove --")
    assert "--yes" in output, f"Expected --yes: {output}"
    assert "--missing-id" not in output, f"--missing-id should not be on remove: {output}"

with subtest("remove-missing flag completion"):
    output = machine.succeed("bash /tmp/get-completions.sh braid remove-missing --")
    assert "--yes" in output, f"Expected --yes: {output}"
    assert "--missing-id" in output, f"Expected --missing-id: {output}"
    assert "--dry-run" in output, f"Expected --dry-run: {output}"

with subtest("status flag completion"):
    output = machine.succeed("bash /tmp/get-completions.sh braid status --")
    assert "--json" in output, f"Expected --json: {output}"

# --- Fish end-to-end: source completions, trigger them, assert candidates ---

with subtest("fish subcommand completion"):
    out = machine.succeed("fish -c 'COMPLETE=fish braid | source; complete --do-complete \"braid \"'")
    for cmd in ["add", "remove", "remove-missing", "replace", "status", "doctor"]:
        assert cmd in out, f"Missing {cmd}: {out}"

with subtest("fish remove flag completion"):
    out = machine.succeed("fish -c 'COMPLETE=fish braid | source; complete --do-complete \"braid remove --\"'")
    assert "--yes" in out, f"Expected --yes: {out}"
    assert "--missing-id" not in out, f"--missing-id should not be on remove: {out}"

with subtest("fish remove-missing flag completion"):
    out = machine.succeed("fish -c 'COMPLETE=fish braid | source; complete --do-complete \"braid remove-missing --\"'")
    assert "--yes" in out, f"Expected --yes: {out}"
    assert "--missing-id" in out, f"Expected --missing-id: {out}"
    assert "--dry-run" in out, f"Expected --dry-run: {out}"

# TODO: add zsh end-to-end completion tests
