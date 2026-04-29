use std::io;

use crate::probe::Filesystem;

const MOUNTINFO_PATH: &str = "/proc/self/mountinfo";

#[derive(Debug, thiserror::Error)]
pub enum MountInfoError {
    #[error("io reading mountinfo: {0}")]
    Io(#[from] io::Error),
    #[error("malformed mountinfo line: {line}")]
    Malformed { line: String },
    #[error("mountinfo contains multiple entries for target {target}")]
    DuplicateTarget { target: String },
}

/// Returns the fstype mounted at `target`, or Ok(None) if the well-formed
/// mountinfo content has no entry for `target`. Returns Err for any malformed
/// non-empty line (related or not) and for any case where multiple entries
/// match `target` -- both are anomalies the safety-critical caller must treat
/// as suspend-blocking.
pub fn fstype_at_mount(content: &str, target: &str) -> Result<Option<String>, MountInfoError> {
    let mut hit: Option<String> = None;
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let parsed = parse_line(line).ok_or_else(|| MountInfoError::Malformed {
            line: line.to_string(),
        })?;
        if parsed.mount_point == target {
            if hit.is_some() {
                return Err(MountInfoError::DuplicateTarget {
                    target: target.to_string(),
                });
            }
            hit = Some(parsed.fstype);
        }
    }
    Ok(hit)
}

struct ParsedLine {
    mount_point: String,
    fstype: String,
}

fn parse_line(line: &str) -> Option<ParsedLine> {
    // split(' '), NOT split_whitespace(): empty source fields appear in real
    // mountinfo (e.g. "... - tmpfs  rw") and must parse as a present-but-empty
    // field, not be silently collapsed. coreutils df has had bugs from getting
    // this wrong; see reference/coreutils/NEWS.
    let mut fields = line.split(' ');
    for _ in 0..4 {
        fields.next()?;
    }
    let mount_point = decode_octal_escapes(fields.next()?);
    fields.next()?; // mount_options
    let mut saw_dash = false;
    for f in fields.by_ref() {
        if f == "-" {
            saw_dash = true;
            break;
        }
    }
    if !saw_dash {
        return None;
    }
    let fstype = fields.next()?.to_string();
    fields.next()?; // source (may be empty for some pseudo-fs entries)
    fields.next()?; // super_options
    if fields.next().is_some() {
        return None;
    }
    Some(ParsedLine {
        mount_point,
        fstype,
    })
}

fn decode_octal_escapes(s: &str) -> String {
    // Kernel only emits \040 \011 \012 \134; decode those, leave every other
    // byte (including multi-byte UTF-8 continuation bytes) untouched.
    //
    // Operate on bytes -- not chars -- because the input may contain non-ASCII
    // UTF-8 paths (e.g. a path ending in U+00E9, two bytes 0xC3 0xA9). A naive
    // `bytes[i] as char` loop would interpret each UTF-8 continuation byte as
    // a separate Latin-1 code point and produce mojibake, causing the target
    // comparison to silently miss a mounted pool and fall through to
    // PoolOffline -- a fail-open result in the safety-critical check.
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            match &bytes[i + 1..i + 4] {
                b"040" => {
                    out.push(b' ');
                    i += 4;
                    continue;
                }
                b"011" => {
                    out.push(b'\t');
                    i += 4;
                    continue;
                }
                b"012" => {
                    out.push(b'\n');
                    i += 4;
                    continue;
                }
                b"134" => {
                    out.push(b'\\');
                    i += 4;
                    continue;
                }
                _ => {}
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("UTF-8 preserved by construction")
}

/// IO wrapper that goes through the existing `Filesystem` trait so tests
/// can mock `/proc/self/mountinfo` content via the same MockFs they use for
/// sysfs reads. Production paths get `RealFilesystem`, which delegates to
/// `std::fs::read_to_string`.
pub fn is_btrfs_mounted<F: Filesystem + ?Sized>(
    fs: &F,
    target: &str,
) -> Result<bool, MountInfoError> {
    let content = fs.read_to_string(MOUNTINFO_PATH)?;
    Ok(fstype_at_mount(&content, target)?.as_deref() == Some("btrfs"))
}

/// IO-shimmed variant of `fstype_at_mount` that reads
/// `/proc/self/mountinfo` through the existing `Filesystem` trait.
pub fn fstype_at_mount_via_fs<F: Filesystem + ?Sized>(
    fs: &F,
    target: &str,
) -> Result<Option<String>, MountInfoError> {
    let content = fs.read_to_string(MOUNTINFO_PATH)?;
    fstype_at_mount(&content, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "/mnt/storage";
    const ROOT_LINE: &str = "26 25 0:23 / / rw,noatime shared:1 - ext4 /dev/sda1 rw\n";

    fn target_btrfs_line() -> String {
        format!("36 35 0:32 / {TARGET} rw,noatime shared:1 - btrfs /dev/mapper/braid-disk1 rw\n")
    }

    /* Intent: well-formed mountinfo body with the configured target mounted as
     *   btrfs returns Ok(Some("btrfs")).
     * Why: baseline happy path -- without this passing, every other test is
     *   meaningless.
     * Scenario: standard nixos boot with /mnt/storage mounted from the LUKS
     *   mapper as btrfs.
     */
    #[test]
    fn fstype_at_mount_finds_btrfs_target() {
        let body = format!("{ROOT_LINE}{}", target_btrfs_line());
        assert_eq!(
            fstype_at_mount(&body, TARGET).unwrap(),
            Some("btrfs".to_string())
        );
    }

    /* Intent: well-formed mountinfo without the target returns Ok(None), not
     *   Err. Ok(None) is the legitimate "pool offline" signal.
     * Why: distinguishes "we read the file fine and the target is genuinely
     *   absent" from "we couldn't tell". Only the former should map to
     *   IdleResult::PoolOffline.
     * Scenario: NAS booted but pool not yet unlocked.
     */
    #[test]
    fn fstype_at_mount_returns_none_when_target_absent() {
        assert_eq!(fstype_at_mount(ROOT_LINE, TARGET).unwrap(), None);
    }

    /* Intent: target line present but fstype is not btrfs returns
     *   Ok(Some("ext4")), not None.
     * Why: proves we distinguish "wrong fs" from "not present". The caller
     *   (is_btrfs_mounted) compares to "btrfs" so this still reports
     *   not-mounted, but the parser surface should be exact.
     * Scenario: a misconfiguration mounts ext4 at /mnt/storage.
     */
    #[test]
    fn fstype_at_mount_returns_other_fstype() {
        let body = format!("36 35 0:32 / {TARGET} rw shared:1 - ext4 /dev/sdb1 rw\n");
        assert_eq!(
            fstype_at_mount(&body, TARGET).unwrap(),
            Some("ext4".to_string())
        );
    }

    /* Intent: mountinfo line with a master:N optional field before the dash
     *   separator parses correctly.
     * Why: optional_fields is variable-length and ends at the literal "-".
     *   Skipping past it incorrectly is a common parser bug.
     * Scenario: the target mount is a shared subtree (mount --make-shared).
     */
    #[test]
    fn fstype_at_mount_handles_optional_fields() {
        let body = format!(
            "36 35 0:32 / {TARGET} rw,noatime shared:1 master:7 - btrfs /dev/mapper/braid-disk1 rw\n"
        );
        assert_eq!(
            fstype_at_mount(&body, TARGET).unwrap(),
            Some("btrfs".to_string())
        );
    }

    /* Intent: multi-line mountinfo body with the target as a non-first entry
     *   parses correctly.
     * Why: confirms the loop visits all lines, not just the first.
     * Scenario: typical mountinfo has dozens of pseudo-fs entries before any
     *   user-mounted volume.
     */
    #[test]
    fn fstype_at_mount_handles_multiple_mounts() {
        let body = format!(
            "{ROOT_LINE}\
             27 26 0:24 / /proc rw,noatime shared:2 - proc proc rw\n\
             {}",
            target_btrfs_line()
        );
        assert_eq!(
            fstype_at_mount(&body, TARGET).unwrap(),
            Some("btrfs".to_string())
        );
    }

    /* Intent: configured target containing a space matches a mountinfo entry
     *   whose mount-point field contains \040.
     * Why: kernel escapes whitespace as octal; without decoding, the
     *   comparison silently misses the mounted pool and we fall through to
     *   PoolOffline -- a fail-open result.
     * Scenario: user configures braid.mountPoint = "/mnt/storage pool".
     */
    #[test]
    fn fstype_at_mount_decodes_octal_escaped_path() {
        let body =
            "36 35 0:32 / /mnt/storage\\040pool rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n";
        assert_eq!(
            fstype_at_mount(body, "/mnt/storage pool").unwrap(),
            Some("btrfs".to_string())
        );
    }

    /* Intent: a mountinfo line for the target that is missing the "-"
     *   separator must error, not silently report the target as absent.
     * Why: regression guard for the original bug shape -- a parser branch
     *   that silently maps "we couldn't parse" to "no entry" lets the caller
     *   conclude PoolOffline and allow suspend.
     * Scenario: a hypothetical kernel output where the target line is
     *   truncated / corrupted.
     */
    #[test]
    fn fstype_at_mount_errors_on_malformed_target_line() {
        let body = format!("36 35 0:32 / {TARGET} rw,noatime shared:1 garbage_no_dash_separator\n");
        assert!(matches!(
            fstype_at_mount(&body, TARGET),
            Err(MountInfoError::Malformed { .. })
        ));
    }

    /* Intent: a malformed line on an entry unrelated to the target also
     *   errors. The parser is strict on every non-empty line.
     * Why: pins the "strict on every line" policy. A future relaxation that
     *   skips unparseable unrelated lines would re-introduce ambiguity in a
     *   safety-critical check.
     * Scenario: a corrupt or short line elsewhere in mountinfo.
     */
    #[test]
    fn fstype_at_mount_errors_on_malformed_unrelated_line() {
        let body = format!(
            "{}\
             this is not a mountinfo line\n",
            target_btrfs_line()
        );
        assert!(matches!(
            fstype_at_mount(&body, TARGET),
            Err(MountInfoError::Malformed { .. })
        ));
    }

    /* Intent: target line ending after "... - btrfs" (no source / super_options)
     *   must error.
     * Why: pins the post-fstype validation. A future shortcut returning Some
     *   after just the fstype field would re-introduce a fail-open path on
     *   truncated lines.
     * Scenario: a truncated mountinfo line missing trailing fields.
     */
    #[test]
    fn fstype_at_mount_errors_on_target_line_truncated_after_fstype() {
        let body = format!("36 35 0:32 / {TARGET} rw shared:1 - btrfs\n");
        assert!(matches!(
            fstype_at_mount(&body, TARGET),
            Err(MountInfoError::Malformed { .. })
        ));
    }

    /* Intent: two well-formed entries with the same target return Err, not
     *   Ok(Some(...)). Don't guess which entry is "current".
     * Why: duplicates are real (overmounts, bind mounts) and a fail-closed
     *   safety gate must refuse to silently pick one. df has substantial
     *   duplicate-handling code in coreutils for the same reason.
     * Scenario: an overmount or rebind landed at the same target as the pool.
     */
    #[test]
    fn fstype_at_mount_errors_on_duplicate_target_entries() {
        let body = format!("{}{}", target_btrfs_line(), target_btrfs_line());
        assert!(matches!(
            fstype_at_mount(&body, TARGET),
            Err(MountInfoError::DuplicateTarget { .. })
        ));
    }

    /* Intent: the four kernel-emitted octal escapes decode to the expected
     *   characters and unrelated backslash sequences pass through literally.
     * Why: the decoder is the load-bearing piece of escape handling; pin its
     *   behavior so a future refactor can't drop a case.
     * Scenario: paths or fstype-in-subtype strings containing whitespace,
     *   tabs, newlines, or backslashes.
     */
    #[test]
    fn decode_octal_escapes_handles_all_four_kernel_escapes() {
        assert_eq!(decode_octal_escapes("a\\040b"), "a b");
        assert_eq!(decode_octal_escapes("a\\011b"), "a\tb");
        assert_eq!(decode_octal_escapes("a\\012b"), "a\nb");
        assert_eq!(decode_octal_escapes("a\\134b"), "a\\b");
        // Unrelated backslash sequence passes through.
        assert_eq!(decode_octal_escapes("a\\999b"), "a\\999b");
        // No escapes at all.
        assert_eq!(decode_octal_escapes("/mnt/storage"), "/mnt/storage");
    }

    /* Intent: a non-ASCII UTF-8 mount path round-trips through the decoder
     *   unchanged.
     * Why: a `bytes[i] as char` decoder would interpret each UTF-8
     *   continuation byte as a separate Latin-1 code point, producing
     *   mojibake and silently missing the target. Regression guard for
     *   the UTF-8 finding.
     * Scenario: a configured mount path containing U+00E9 (two bytes
     *   0xC3 0xA9), which the kernel passes through verbatim.
     */
    #[test]
    fn fstype_at_mount_preserves_non_ascii_utf8_path() {
        let target = "/mnt/caf\u{e9}";
        let body =
            format!("36 35 0:32 / {target} rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n");
        assert_eq!(
            fstype_at_mount(&body, target).unwrap(),
            Some("btrfs".to_string())
        );
    }

    /* Intent: a target line with an extra unexpected field after super_options
     *   errors.
     * Why: the no-trailing-junk policy. Any deviation from the documented
     *   format must surface as Err in this safety-critical context.
     * Scenario: a hypothetical kernel output (or future format extension)
     *   that adds fields beyond super_options.
     */
    #[test]
    fn fstype_at_mount_errors_on_trailing_junk_after_super_options() {
        let body = format!(
            "36 35 0:32 / {TARGET} rw shared:1 - btrfs /dev/mapper/braid-disk1 rw extra_unexpected_field\n"
        );
        assert!(matches!(
            fstype_at_mount(&body, TARGET),
            Err(MountInfoError::Malformed { .. })
        ));
    }

    /* Intent: a target line with two consecutive spaces between fstype and
     *   super_options (representing an empty source field) is valid mountinfo
     *   and parses successfully.
     * Why: pins the requirement that the parser uses split(' ') rather than
     *   split_whitespace(). Empty source fields are real -- coreutils df has
     *   recorded bugs around exactly this case (reference/coreutils/NEWS).
     * Scenario: a tmpfs / pseudo-fs entry whose source field is empty.
     */
    #[test]
    fn fstype_at_mount_accepts_empty_source_field() {
        let body = format!("36 35 0:32 / {TARGET} rw shared:1 - tmpfs  rw,size=1G\n");
        assert_eq!(
            fstype_at_mount(&body, TARGET).unwrap(),
            Some("tmpfs".to_string())
        );
    }

    struct MockMountInfoFs {
        mountinfo: Result<String, std::io::ErrorKind>,
    }

    impl Filesystem for MockMountInfoFs {
        fn exists(&self, _: &str) -> bool {
            false
        }
        fn is_block_device(&self, _: &str) -> bool {
            false
        }
        fn list_dir(&self, _: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            assert_eq!(path, "/proc/self/mountinfo");
            self.mountinfo
                .as_ref()
                .map(|body| body.clone())
                .map_err(|kind| std::io::Error::new(*kind, "mock mountinfo read failed"))
        }
    }

    /* Intent: the IO-shimmed helper returns the target fstype when
     *   mountinfo contains the configured mount point.
     * Why: pins the behavior added by the Filesystem wrapper without
     *   duplicating the lower-level parser matrix.
     * Scenario: probe_pool reads a mounted btrfs pool through MockFs.
     */
    #[test]
    fn fstype_at_mount_via_fs_returns_btrfs_when_mounted() {
        let fs = MockMountInfoFs {
            mountinfo: Ok(format!("{ROOT_LINE}{}", target_btrfs_line())),
        };
        assert_eq!(
            fstype_at_mount_via_fs(&fs, TARGET).unwrap(),
            Some("btrfs".to_string())
        );
    }

    /* Intent: the IO-shimmed helper returns Ok(None) when mountinfo is
     *   well-formed but lacks the configured target.
     * Why: preserves the "absent target is legitimate offline" distinction.
     * Scenario: NAS booted, pool still locked, mountinfo readable.
     */
    #[test]
    fn fstype_at_mount_via_fs_returns_none_when_target_absent() {
        let fs = MockMountInfoFs {
            mountinfo: Ok(ROOT_LINE.to_string()),
        };
        assert_eq!(fstype_at_mount_via_fs(&fs, TARGET).unwrap(), None);
    }

    /* Intent: a Filesystem read failure surfaces as MountInfoError::Io.
     * Why: the safety-critical caller must treat "cannot read mountinfo" as
     *   indeterminate, not as an offline pool.
     * Scenario: `/proc/self/mountinfo` is unreadable.
     */
    #[test]
    fn fstype_at_mount_via_fs_propagates_io_failure() {
        let fs = MockMountInfoFs {
            mountinfo: Err(std::io::ErrorKind::PermissionDenied),
        };
        assert!(matches!(
            fstype_at_mount_via_fs(&fs, TARGET),
            Err(MountInfoError::Io(_))
        ));
    }

    /* Intent: when the underlying Filesystem read fails, is_btrfs_mounted
     *   surfaces it as MountInfoError::Io, not Ok(false).
     * Why: regression guard for the IO-failure path. The original bug shape
     *   was "we don't know -> assume offline -> allow suspend"; the
     *   replacement must surface "we don't know" as an error.
     * Scenario: /proc/self/mountinfo is unreadable.
     */
    #[test]
    fn is_btrfs_mounted_io_error_when_read_fails() {
        struct FailingFs;
        impl Filesystem for FailingFs {
            fn exists(&self, _: &str) -> bool {
                false
            }
            fn is_block_device(&self, _: &str) -> bool {
                false
            }
            fn list_dir(&self, _: &str) -> Result<Vec<String>, std::io::Error> {
                Ok(vec![])
            }
            fn read_to_string(&self, _: &str) -> Result<String, std::io::Error> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "test: read denied",
                ))
            }
        }
        let result = is_btrfs_mounted(&FailingFs, TARGET);
        assert!(matches!(result, Err(MountInfoError::Io(_))));
    }
}
