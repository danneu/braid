use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::AutoSuspend;

/// Tri-state WoL readiness derived from one `ethtool <iface>` invocation.
/// Shared by the doctor check and the autosuspend `wol-ready` gate so the
/// two cannot disagree about what counts as "magic-packet armed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WolReadiness {
    Armed { active: String },
    Disabled { active: String, supports: String },
    Unsupported { supports: String },
    Unparseable,
    QueryFailed { exit: i32, detail: String },
}

/// Extract one ethtool text field under the `LC_ALL=C` command-runner
/// contract so all WoL checks share one label parser.
pub(crate) fn ethtool_field<'a>(stdout: &'a str, label: &str) -> Option<&'a str> {
    stdout
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(label).map(str::trim))
        .filter(|value| !value.is_empty())
}

/// Reject unknown ethtool WoL mode characters so output drift cannot be
/// misread as either safe or unsupported.
pub(crate) fn wol_modes_parseable(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| "pumbagsfd".contains(c))
}

/// Classify one raw ethtool result without side effects so doctor, autosuspend,
/// and unit tests all pin the same Wake-on-LAN safety boundary.
pub(crate) fn classify_wol(stdout: &str, stderr: &str, exit: i32) -> WolReadiness {
    if exit != 0 {
        return WolReadiness::QueryFailed {
            exit,
            detail: stderr.trim().to_owned(),
        };
    }

    let Some(supports) =
        ethtool_field(stdout, "Supports Wake-on:").filter(|value| wol_modes_parseable(value))
    else {
        return WolReadiness::Unparseable;
    };

    let Some(active) = ethtool_field(stdout, "Wake-on:").filter(|value| wol_modes_parseable(value))
    else {
        return WolReadiness::Unparseable;
    };

    if !supports.contains('g') {
        return WolReadiness::Unsupported {
            supports: supports.to_owned(),
        };
    }

    if !active.contains('g') {
        return WolReadiness::Disabled {
            active: active.to_owned(),
            supports: supports.to_owned(),
        };
    }

    WolReadiness::Armed {
        active: active.to_owned(),
    }
}

/// Exit-code-facing result for the hidden autosuspend gate: `Armed` allows
/// suspend, `NotReady` blocks suspend, and `SetupError` marks bad wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WolReadyOutcome {
    Armed,
    NotReady(String),
    SetupError(String),
}

/// Autosuspend gate that proves the configured WoL interface is armed before
/// braid's automatic suspend path is allowed to proceed.
pub fn cmd_wol_ready<R: CommandRunner>(runner: &R, auto: Option<&AutoSuspend>) -> WolReadyOutcome {
    let Some(auto) = auto else {
        return WolReadyOutcome::SetupError("braid.autoSuspend is not configured".into());
    };
    let interface = auto.wol_interface.as_str();

    let out = match runner.run(&CmdRequest::EthtoolShow {
        interface: interface.to_owned(),
    }) {
        Ok(out) => out,
        Err(e) => {
            return WolReadyOutcome::NotReady(format!(
                "cannot verify Wake-on-LAN for {interface}: {e}"
            ));
        }
    };

    match classify_wol(&out.stdout, &out.stderr, out.exit_status) {
        WolReadiness::Armed { .. } => WolReadyOutcome::Armed,
        other => WolReadyOutcome::NotReady(wol_not_ready_reason(interface, other)),
    }
}

fn wol_not_ready_reason(interface: &str, readiness: WolReadiness) -> String {
    match readiness {
        WolReadiness::Armed { .. } => format!("{interface} reports Wake-on-LAN armed"),
        WolReadiness::Disabled { active, .. } => {
            format!("{interface} reports Wake-on: {active} -- magic-packet wake is not armed")
        }
        WolReadiness::Unsupported { supports } => {
            format!(
                "{interface} reports Supports Wake-on: {supports} -- magic-packet wake is unsupported"
            )
        }
        WolReadiness::Unparseable => {
            format!("{interface} ethtool output is unparseable -- cannot verify Wake-on-LAN")
        }
        WolReadiness::QueryFailed { exit, detail } => {
            if detail.is_empty() {
                format!("ethtool {interface} failed with exit {exit} -- cannot verify Wake-on-LAN")
            } else {
                format!(
                    "ethtool {interface} failed with exit {exit}: {detail} -- cannot verify Wake-on-LAN"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, MockRunner, RawCommandOutput};

    fn auto_suspend() -> AutoSuspend {
        AutoSuspend {
            wol_interface: "eno1".into(),
        }
    }

    fn ethtool_out(stdout: &str, stderr: &str, exit_status: i32) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "ethtool eno1".into(),
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_status,
        }
    }

    fn wol_runner(output: RawCommandOutput) -> MockRunner {
        MockRunner::default().with_output(
            CmdRequest::EthtoolShow {
                interface: "eno1".into(),
            },
            output,
        )
    }

    // Intent: classify_wol reports Armed when ethtool shows magic-packet wake
    // at runtime.
    // Why it exists: this is the only autosuspend-allowing WoL state, so a
    // false negative would keep a healthy NAS awake.
    // Scenario: the NixOS interface setup succeeded and ethtool reports
    // `Wake-on: g`.
    #[test]
    fn classify_wol_armed_when_magic_packet_enabled() {
        let readiness = classify_wol(
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: g\n",
            "",
            0,
        );

        assert_eq!(readiness, WolReadiness::Armed { active: "g".into() });
    }

    // Intent: classify_wol treats compact multi-flag active modes containing
    // `g` as armed.
    // Why it exists: guards against a naive `Wake-on: g` substring check that
    // would reject valid `Wake-on: ug` output.
    // Scenario: a NIC arms unicast plus magic-packet wake and ethtool renders
    // both flags on one line.
    #[test]
    fn classify_wol_armed_when_magic_packet_is_one_of_multiple_modes() {
        let readiness = classify_wol(
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: ug\n",
            "",
            0,
        );

        assert_eq!(
            readiness,
            WolReadiness::Armed {
                active: "ug".into()
            }
        );
    }

    // Intent: classify_wol reports Disabled when magic-packet wake is
    // supported but not active.
    // Why it exists: `Wake-on: d` is the unsafe runtime drift that would let
    // autosuspend strand the machine.
    // Scenario: BIOS, driver, or rebuild state leaves WoL disabled on the
    // configured interface.
    #[test]
    fn classify_wol_disabled_when_supported_but_inactive() {
        let readiness = classify_wol(
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: d\n",
            "",
            0,
        );

        assert_eq!(
            readiness,
            WolReadiness::Disabled {
                active: "d".into(),
                supports: "pumbg".into()
            }
        );
    }

    // Intent: classify_wol reports Unsupported when the support modes lack
    // magic-packet wake.
    // Why it exists: braid cannot make autosuspend wakeable on a driver or NIC
    // that does not expose the required `g` capability.
    // Scenario: the operator configured the wrong NIC or an unsupported NIC.
    #[test]
    fn classify_wol_unsupported_when_support_modes_lack_magic_packet() {
        let readiness = classify_wol(
            "Settings for eno1:\n\tSupports Wake-on: d\n\tWake-on: d\n",
            "",
            0,
        );

        assert_eq!(
            readiness,
            WolReadiness::Unsupported {
                supports: "d".into()
            }
        );
    }

    // Intent: classify_wol reports Unparseable for successful ethtool output
    // that lacks expected WoL fields.
    // Why it exists: parser drift must fail closed instead of being confused
    // with disabled, unsupported, or armed states.
    // Scenario: a future ethtool changes field labels or suppresses WoL lines.
    #[test]
    fn classify_wol_unparseable_when_fields_are_missing() {
        let readiness = classify_wol("Settings for eno1:\n\tLink detected: yes\n", "", 0);

        assert_eq!(readiness, WolReadiness::Unparseable);
    }

    // Intent: classify_wol reports Unparseable when WoL mode strings contain
    // unknown characters.
    // Why it exists: unknown mode tokens indicate output drift and must not be
    // treated as a safe active state.
    // Scenario: ethtool gains a new WoL token before braid's parser is updated.
    #[test]
    fn classify_wol_unparseable_when_modes_drift() {
        let readiness = classify_wol(
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: garbage\n",
            "",
            0,
        );

        assert_eq!(readiness, WolReadiness::Unparseable);
    }

    // Intent: classify_wol reports QueryFailed when ethtool exits non-zero.
    // Why it exists: a failed query means braid cannot prove the wake path, so
    // autosuspend must block.
    // Scenario: the configured interface was renamed or removed.
    #[test]
    fn classify_wol_query_failed_on_nonzero_exit() {
        let readiness = classify_wol(
            "",
            "Cannot get device wake-on-lan settings: No such device\n",
            1,
        );

        assert_eq!(
            readiness,
            WolReadiness::QueryFailed {
                exit: 1,
                detail: "Cannot get device wake-on-lan settings: No such device".into()
            }
        );
    }

    // Intent: cmd_wol_ready returns Armed when the configured interface has
    // magic-packet wake active.
    // Why it exists: this is the exit-0 path autosuspend inverts to "no
    // activity, suspend may proceed."
    // Scenario: autosuspend checks a host with `Wake-on: g` immediately before
    // sleeping.
    #[test]
    fn wol_ready_armed_when_magic_packet_enabled() {
        let runner = wol_runner(ethtool_out(
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: g\n",
            "",
            0,
        ));

        let outcome = cmd_wol_ready(&runner, Some(&auto_suspend()));

        assert_eq!(outcome, WolReadyOutcome::Armed);
    }

    // Intent: cmd_wol_ready returns NotReady when magic-packet wake is
    // supported but disabled.
    // Why it exists: the autosuspend gate must block the concrete unsafe
    // `Wake-on: d` state instead of letting the machine sleep.
    // Scenario: a driver reset disables WoL after resume.
    #[test]
    fn wol_ready_not_ready_when_disabled() {
        let runner = wol_runner(ethtool_out(
            "Settings for eno1:\n\tSupports Wake-on: pumbg\n\tWake-on: d\n",
            "",
            0,
        ));

        let outcome = cmd_wol_ready(&runner, Some(&auto_suspend()));

        assert!(
            matches!(&outcome, WolReadyOutcome::NotReady(reason) if reason.contains("Wake-on: d")),
            "got: {outcome:?}"
        );
    }

    // Intent: cmd_wol_ready returns NotReady when the configured interface
    // lacks magic-packet support.
    // Why it exists: autosuspend cannot be safe when the selected NIC cannot
    // wake the machine.
    // Scenario: the operator points wolInterface at an unsupported interface.
    #[test]
    fn wol_ready_not_ready_when_unsupported() {
        let runner = wol_runner(ethtool_out(
            "Settings for eno1:\n\tSupports Wake-on: d\n\tWake-on: d\n",
            "",
            0,
        ));

        let outcome = cmd_wol_ready(&runner, Some(&auto_suspend()));

        assert!(
            matches!(&outcome, WolReadyOutcome::NotReady(reason) if reason.contains("unsupported")),
            "got: {outcome:?}"
        );
    }

    // Intent: cmd_wol_ready returns NotReady when ethtool output cannot be
    // parsed.
    // Why it exists: output drift must block autosuspend until braid's parser
    // is updated.
    // Scenario: a future ethtool changes the WoL labels.
    #[test]
    fn wol_ready_not_ready_when_unparseable() {
        let runner = wol_runner(ethtool_out("Settings for eno1:\n", "", 0));

        let outcome = cmd_wol_ready(&runner, Some(&auto_suspend()));

        assert!(
            matches!(&outcome, WolReadyOutcome::NotReady(reason) if reason.contains("unparseable")),
            "got: {outcome:?}"
        );
    }

    // Intent: cmd_wol_ready returns NotReady when ethtool returns non-zero.
    // Why it exists: a failed query means braid cannot prove the host is
    // wakeable, so the gate must fail closed.
    // Scenario: the configured interface no longer exists.
    #[test]
    fn wol_ready_not_ready_when_query_fails() {
        let runner = wol_runner(ethtool_out("", "No such device\n", 1));

        let outcome = cmd_wol_ready(&runner, Some(&auto_suspend()));

        assert!(
            matches!(&outcome, WolReadyOutcome::NotReady(reason) if reason.contains("exit 1")),
            "got: {outcome:?}"
        );
    }

    // Intent: cmd_wol_ready returns NotReady when ethtool cannot be spawned.
    // Why it exists: missing wrapper PATH plumbing must block autosuspend
    // rather than pretending the host is wakeable.
    // Scenario: the deployed wrapper omits braid.packages.ethtool.
    #[test]
    fn wol_ready_not_ready_when_ethtool_spawn_fails() {
        let runner = MockRunner::default().with_handler(|request| match request {
            CmdRequest::EthtoolShow { interface } if interface == "eno1" => Some(Err(
                CmdError::Failed("ethtool eno1: No such file or directory".into()),
            )),
            _ => None,
        });

        let outcome = cmd_wol_ready(&runner, Some(&auto_suspend()));

        assert!(
            matches!(&outcome, WolReadyOutcome::NotReady(reason) if reason.contains("cannot verify")),
            "got: {outcome:?}"
        );
    }

    // Intent: cmd_wol_ready returns SetupError when no autoSuspend config was
    // loaded.
    // Why it exists: this hidden command should only be wired when the NixOS
    // module enabled autosuspend, so missing config is setup drift.
    // Scenario: an operator invokes `braid wol-ready` on a non-autosuspend
    // host or a stale generated config.
    #[test]
    fn wol_ready_setup_error_without_auto_suspend_config() {
        let runner = MockRunner::default();

        let outcome = cmd_wol_ready(&runner, None);

        assert_eq!(
            outcome,
            WolReadyOutcome::SetupError("braid.autoSuspend is not configured".into())
        );
        assert!(runner.requests().is_empty(), "ethtool should not run");
    }
}
