use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::ParseError;

/// UI-only projection of `systemctl list-units --output=json` for Browse.
/// Kept outside the parser-critical contract so failure can degrade to raw
/// text while still giving the TUI a stable picker shape when JSON parses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SystemdUnitRow {
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub load: String,
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub description: String,
}

/// Parses the tolerant Systemd Browse picker source.
pub fn parse_systemctl_list_units_json(
    raw: &RawCommandOutput,
) -> Result<Vec<SystemdUnitRow>, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    serde_json::from_str(&raw.stdout).map_err(|e| ParseError::InvalidJson {
        cmd: raw.cmd.clone(),
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    /*
     * Intent: parse the Systemd Browse picker's JSON row shape.
     *
     * Why it exists: the TUI uses these fields for selectable unit rows while
     * preserving raw-output fallback on parser failure.
     *
     * Scenario: user opens Browse > Systemd > Status on a braid host and sees
     * braid unit rows instead of raw JSON.
     */
    #[test]
    fn list_units_json_parses_fixture() {
        let raw = RawCommandOutput {
            cmd: "systemctl list-units --output=json --all braid-* hddfancontrol-braid.service"
                .into(),
            stdout: fixture("systemctl-list-units-braid.json"),
            stderr: String::new(),
            exit_status: 0,
        };

        let rows = parse_systemctl_list_units_json(&raw).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].unit, "braid-online.service");
        assert_eq!(rows[0].load, "loaded");
        assert_eq!(rows[0].active, "active");
        assert_eq!(rows[0].sub, "exited");
        assert_eq!(rows[0].description, "braid pool online sentinel");
        assert_eq!(rows[1].unit, "hddfancontrol-braid.service");
    }

    /*
     * Intent: non-zero systemctl exits report command failure rather than
     * deserializing stderr as JSON.
     *
     * Why it exists: Browse disables drill-in on picker source failure; callers
     * need a parser error that preserves the command status.
     *
     * Scenario: systemd is unavailable in a test wrapper or chroot.
     */
    #[test]
    fn list_units_json_rejects_failed_command() {
        let raw = RawCommandOutput {
            cmd: "systemctl list-units --output=json --all braid-*".into(),
            stdout: String::new(),
            stderr: "Failed to connect to bus\n".into(),
            exit_status: 1,
        };

        let err = parse_systemctl_list_units_json(&raw).unwrap_err();

        assert!(matches!(
            err,
            ParseError::CommandFailed { exit_code: 1, .. }
        ));
    }
}
