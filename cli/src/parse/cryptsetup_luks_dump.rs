use std::collections::BTreeMap;

use serde::Deserialize;

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{CryptsetupLuksDumpOutput, Luks2SegmentSize};

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
    offset: String,
    size: String,
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

    let segment = parsed
        .segments
        .get("0")
        .ok_or_else(|| ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: "segments.0 missing".to_owned(),
        })?;

    let segment_offset_bytes =
        segment
            .offset
            .parse::<u64>()
            .map_err(|e| ParseError::InvalidJson {
                cmd: raw.cmd.clone(),
                detail: format!("segments.0.offset: {e}"),
            })?;

    let segment_size = if segment.size == "dynamic" {
        Luks2SegmentSize::Dynamic
    } else {
        Luks2SegmentSize::Fixed(segment.size.parse::<u64>().map_err(|e| {
            ParseError::InvalidJson {
                cmd: raw.cmd.clone(),
                detail: format!("segments.0.size: {e}"),
            }
        })?)
    };

    let cipher = segment.encryption.clone();
    if cipher.is_empty() {
        return Err(ParseError::InvalidJson {
            cmd: raw.cmd.clone(),
            detail: "segments.0.encryption is empty".to_owned(),
        });
    }

    let key_size_bits = match parsed.keyslots.iter().next() {
        None => 0,
        Some((slot, k)) => {
            let bits = k
                .key_size
                .checked_mul(8)
                .ok_or_else(|| ParseError::InvalidJson {
                    cmd: raw.cmd.clone(),
                    detail: format!("keyslots.{slot}.key_size {} overflows u64 (*8)", k.key_size),
                })?;
            u32::try_from(bits).map_err(|_| ParseError::InvalidJson {
                cmd: raw.cmd.clone(),
                detail: format!("keyslots.{slot}.key_size {bits} bits exceeds u32"),
            })?
        }
    };

    let keyslot_count = parsed.keyslots.len() as u32;

    Ok(CryptsetupLuksDumpOutput {
        cipher,
        key_size_bits,
        keyslot_count,
        segment_offset_bytes,
        segment_size,
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

    fn raw_with_key_size(key_size: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: r#"{
  "keyslots": {
    "0": {
      "type": "luks2", "key_size": __KEY_SIZE__,
      "af": {}, "area": {}, "kdf": {}
    }
  },
  "tokens": {},
  "segments": {"0": {"type":"crypt","offset":"16777216","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096}},
  "digests": {},
  "config": {}
}"#
            .replace("__KEY_SIZE__", key_size),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    // --- Contract tests (nixos-25.11 fixtures) ---

    #[test]
    // Intent: the stable cryptsetup JSON fixture yields core header fields
    //   plus the default dynamic segment model.
    // Why it exists: parser drift here breaks TUI metadata and replace
    //   target-capacity preflight.
    // Scenario: nixos-25.11 cryptsetup emits one keyslot and segment 0.
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
        assert_eq!(out.segment_offset_bytes, 16_777_216);
        assert_eq!(out.segment_size, Luks2SegmentSize::Dynamic);
    }

    // --- Synthetic tests (inline) ---

    #[test]
    // Intent: multiple keyslots are counted while segment 0 still provides
    //   cipher, offset, and size metadata.
    // Why it exists: keyslot inventory and segment-capacity parsing share
    //   one JSON parser and must not regress each other.
    // Scenario: synthetic LUKS2 metadata has two keyslots and a dynamic segment.
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
        assert_eq!(out.segment_offset_bytes, 16_777_216);
        assert_eq!(out.segment_size, Luks2SegmentSize::Dynamic);
    }

    #[test]
    // Intent: a numeric LUKS2 segment size parses as `Fixed(bytes)`.
    // Why it exists: replace preflight must handle fixed-size segment
    //   metadata without using `raw - offset`.
    // Scenario: synthetic segment 0 reports `"size":"1073741824"`.
    fn parse_extracts_fixed_segment_size() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: r#"{
  "keyslots": {
    "0": {
      "type": "luks2", "key_size": 64,
      "af": {}, "area": {}, "kdf": {}
    }
  },
  "tokens": {},
  "segments": {"0": {"type":"crypt","offset":"16777216","size":"1073741824","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096}},
  "digests": {},
  "config": {}
}"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_cryptsetup_luks_dump(&raw).unwrap();
        assert_eq!(out.segment_offset_bytes, 16_777_216);
        assert_eq!(out.segment_size, Luks2SegmentSize::Fixed(1_073_741_824));
    }

    // Intent: key_size values that fit u64 but exceed the output u32 field are
    // rejected as malformed metadata.
    // Why it exists: silently truncating LUKS key sizes would make status and
    // TUI metadata lie about cryptsetup output.
    // Scenario: corrupt luksDump JSON reports an oversized keyslot key size.
    #[test]
    fn parse_rejects_key_size_bits_exceeding_u32() {
        let err = parse_cryptsetup_luks_dump(&raw_with_key_size("600000000")).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidJson { .. }),
            "unexpected error: {err}"
        );
    }

    // Intent: key_size multiplication itself is checked before converting to
    // the output field.
    // Why it exists: debug overflow checks must not panic on adversarial but
    // syntactically valid cryptsetup JSON.
    // Scenario: corrupt luksDump JSON reports u64::MAX as the keyslot key size.
    #[test]
    fn parse_rejects_key_size_bits_multiplication_overflow() {
        let err =
            parse_cryptsetup_luks_dump(&raw_with_key_size("18446744073709551615")).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidJson { .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: missing segment 0 is rejected even if another segment exists.
    // Why it exists: braid's capacity model is defined for LUKS2 crypt
    //   segment 0, not arbitrary map iteration order.
    // Scenario: synthetic metadata has only segment 1.
    fn parse_rejects_missing_segment_zero() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: r#"{
  "keyslots": {},
  "tokens": {},
  "segments": {"1": {"type":"crypt","offset":"16777216","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096}},
  "digests": {},
  "config": {}
}"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_dump(&raw).unwrap_err();
        assert!(
            err.to_string().contains("segments.0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: malformed segment offset is rejected with a field-specific
    //   parse error.
    // Why it exists: replace preflight must fail closed rather than
    //   guessing a data offset.
    // Scenario: segment 0 has `"offset":"not-bytes"`.
    fn parse_rejects_malformed_offset() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: r#"{
  "keyslots": {},
  "tokens": {},
  "segments": {"0": {"type":"crypt","offset":"not-bytes","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096}},
  "digests": {},
  "config": {}
}"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_dump(&raw).unwrap_err();
        assert!(
            err.to_string().contains("segments.0.offset"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: malformed fixed segment size is rejected with a field-specific
    //   parse error.
    // Why it exists: replace preflight must fail closed rather than
    //   guessing mapper capacity.
    // Scenario: segment 0 has `"size":"not-bytes"`.
    fn parse_rejects_malformed_size() {
        let raw = RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: r#"{
  "keyslots": {},
  "tokens": {},
  "segments": {"0": {"type":"crypt","offset":"16777216","size":"not-bytes","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096}},
  "digests": {},
  "config": {}
}"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_cryptsetup_luks_dump(&raw).unwrap_err();
        assert!(
            err.to_string().contains("segments.0.size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: non-JSON cryptsetup output is rejected.
    // Why it exists: JSON parsing failures must surface as parser errors.
    // Scenario: command succeeds but stdout is not valid LUKS JSON.
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
    // Intent: non-zero cryptsetup exit status is reported as command failure.
    // Why it exists: callers need to distinguish unreadable headers from
    //   malformed successful JSON.
    // Scenario: cryptsetup reports that a device does not exist.
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
