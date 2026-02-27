use std::collections::BTreeMap;

use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::types::CryptsetupLuksDumpOutput;
use super::ParseError;

// --- Serde helper structs (not exposed to domain code) ---

#[derive(Deserialize)]
struct RawLuksDump {
    keyslots: BTreeMap<String, RawKeyslot>,
    segments: BTreeMap<String, RawSegment>,
    // Fields present in the JSON but unused by domain code.
    #[allow(dead_code)]
    tokens: serde_json::Value,
    #[allow(dead_code)]
    digests: serde_json::Value,
    #[allow(dead_code)]
    config: serde_json::Value,
}

#[derive(Deserialize)]
struct RawKeyslot {
    key_size: u64,
    // Fields present in the JSON but unused by domain code.
    #[allow(dead_code)]
    r#type: String,
    #[allow(dead_code)]
    kdf: serde_json::Value,
    #[allow(dead_code)]
    af: serde_json::Value,
    #[allow(dead_code)]
    area: serde_json::Value,
}

#[derive(Deserialize)]
struct RawSegment {
    encryption: String,
}

// --- Public parse function ---

pub fn parse_cryptsetup_luks_dump(
    raw: &RawCommandOutput,
) -> Result<CryptsetupLuksDumpOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let parsed: RawLuksDump =
        serde_json::from_str(&raw.stdout).map_err(|e| ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: e.to_string(),
        })?;

    let cipher = parsed
        .segments
        .values()
        .next()
        .map(|s| s.encryption.clone())
        .ok_or_else(|| ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: "no segments found".to_owned(),
        })?;

    let key_size_bits = parsed
        .keyslots
        .values()
        .next()
        .map(|k| k.key_size * 8)
        .unwrap_or(0) as u32;

    let keyslot_count = parsed.keyslots.len() as u32;

    Ok(CryptsetupLuksDumpOutput {
        cipher,
        key_size_bits,
        keyslot_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/nixos-25.11/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    // --- Contract tests (nixos-25.11 fixtures) ---

    #[test]
    fn luks_dump_parses_single_keyslot_fixture() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: fixture("cryptsetup-luks-dump.json"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_cryptsetup_luks_dump(&raw).unwrap();
        assert_eq!(out.cipher, "aes-xts-plain64");
        assert_eq!(out.key_size_bits, 512);
        assert_eq!(out.keyslot_count, 1);
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn luks_dump_parses_multiple_keyslots() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: r#"{
  "keyslots": {
    "0": {
      "type": "luks2", "key_size": 64,
      "af": {"type":"luks1","stripes":4000,"hash":"sha256"},
      "area": {"type":"raw","offset":"32768","size":"258048","encryption":"aes-xts-plain64","key_size":64},
      "kdf": {"type": "argon2id", "time": 6, "memory": 1048576, "cpus": 4, "salt": "abc="}
    },
    "1": {
      "type": "luks2", "key_size": 64,
      "af": {"type":"luks1","stripes":4000,"hash":"sha256"},
      "area": {"type":"raw","offset":"290816","size":"258048","encryption":"aes-xts-plain64","key_size":64},
      "kdf": {"type": "pbkdf2", "hash": "sha256", "iterations": 1000, "salt": "def="}
    }
  },
  "tokens": {},
  "segments": {"0": {"type":"crypt","offset":"16777216","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096}},
  "digests": {"0": {"type":"pbkdf2","keyslots":["0","1"],"segments":["0"],"hash":"sha256","iterations":1000,"salt":"xyz=","digest":"abc="}},
  "config": {"json_size":"12288","keyslots_size":"16744448"}
}"#.into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_cryptsetup_luks_dump(&raw).unwrap();
        assert_eq!(out.cipher, "aes-xts-plain64");
        assert_eq!(out.key_size_bits, 512);
        assert_eq!(out.keyslot_count, 2);
    }

    #[test]
    fn luks_dump_rejects_malformed_json() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: r#"{"keyslots": "not a map"}"#.into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_dump(&raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson { .. }));
    }

    #[test]
    fn luks_dump_rejects_nonzero_exit() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: String::new(),
            stderr: "Device /dev/vdz does not exist.".into(),
            exit_status: 5,
        };
        let err = parse_cryptsetup_luks_dump(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }
}
