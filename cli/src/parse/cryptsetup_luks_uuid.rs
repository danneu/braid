use crate::cmd::RawCommandOutput;
use crate::types::LuksUuid;

use super::types::CryptsetupLuksUuidOutput;
use super::ParseError;

pub fn parse_cryptsetup_luks_uuid(
    raw: &RawCommandOutput,
) -> Result<CryptsetupLuksUuidOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let trimmed = raw.stdout.trim();

    // Validate UUID format using uuid crate
    uuid::Uuid::parse_str(trimmed).map_err(|_| ParseError::InvalidText {
        cmd: raw.cmd.clone(),
        detail: format!("not a valid UUID: {trimmed:?}"),
    })?;

    Ok(CryptsetupLuksUuidOutput {
        uuid: LuksUuid(trimmed.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/phase2/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    #[test]
    fn luks_uuid_parses_valid_fixture() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: fixture("cryptsetup-luks-uuid.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_cryptsetup_luks_uuid(&raw).unwrap();
        assert_eq!(out.uuid.0, "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
    }

    #[test]
    fn luks_uuid_rejects_invalid_uuid() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: "not-a-uuid\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_uuid(&raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidText { .. }));
    }

    #[test]
    fn luks_uuid_errors_on_nonzero_exit() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: String::new(),
            stderr: "Device /dev/vdz does not exist.".into(),
            exit_status: 5,
        };
        let err = parse_cryptsetup_luks_uuid(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }
}
