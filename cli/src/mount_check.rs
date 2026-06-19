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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub fstype: String,
    pub vfs_options: String,
    pub fs_options: String,
}

/// Returns the fstype mounted at `target`, or Ok(None) if the well-formed
/// mountinfo content has no entry for `target`. Returns Err for any malformed
/// non-empty line (related or not) and for any case where multiple entries
/// match `target` -- both are anomalies the safety-critical caller must treat
/// as suspend-blocking.
pub fn fstype_at_mount(content: &str, target: &str) -> Result<Option<String>, MountInfoError> {
    Ok(find_unique_target_entry(content, target)?.map(|entry| entry.fstype))
}

/// Returns the mounted entry at `target`, including both mountinfo option
/// fields. Field 6 (`vfs_options`) carries per-mount VFS flags such as
/// `rw`/`ro`; field 11 (`fs_options`) carries filesystem/superblock options,
/// including the filesystem-level `rw`/`ro` state.
pub fn mount_entry_at(content: &str, target: &str) -> Result<Option<MountEntry>, MountInfoError> {
    Ok(
        find_unique_target_entry(content, target)?.map(|entry| MountEntry {
            fstype: entry.fstype,
            vfs_options: entry.vfs_options,
            fs_options: entry.fs_options,
        }),
    )
}

fn find_unique_target_entry(
    content: &str,
    target: &str,
) -> Result<Option<ParsedLine>, MountInfoError> {
    // mountinfo emits canonical paths with no trailing slash for non-root
    // mounts. Normalize only the caller-supplied target so configs like
    // "/mnt/storage/" still match the kernel's "/mnt/storage"; preserve root
    // because mountinfo emits "/" for it. Without this, the idle mount probe
    // can miss a mounted pool and let autosuspend proceed.
    let target = if target == "/" {
        target
    } else {
        target.trim_end_matches('/')
    };
    let mut hit: Option<ParsedLine> = None;
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
            hit = Some(parsed);
        }
    }
    Ok(hit)
}

struct ParsedLine {
    mount_point: String,
    fstype: String,
    vfs_options: String,
    fs_options: String,
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
    let vfs_options = fields.next()?.to_string();
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
    let fs_options = fields.next()?.to_string();
    if fields.next().is_some() {
        return None;
    }
    Some(ParsedLine {
        mount_point,
        fstype,
        vfs_options,
        fs_options,
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

/// Checks whether `target` is a mounted btrfs filesystem.
///
/// Reads `/proc/self/mountinfo` through the existing `Filesystem` trait. An
/// `Ok(false)` result only means a well-formed mountinfo body has no btrfs
/// mount at `target`. IO errors, malformed lines, and duplicate target entries
/// return `MountInfoError`; safety-critical callers should treat those errors
/// as indeterminate and fail closed.
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

/// IO-shimmed variant of `mount_entry_at` that reads
/// `/proc/self/mountinfo` through the existing `Filesystem` trait.
pub fn mount_entry_at_via_fs<F: Filesystem + ?Sized>(
    fs: &F,
    target: &str,
) -> Result<Option<MountEntry>, MountInfoError> {
    let content = fs.read_to_string(MOUNTINFO_PATH)?;
    mount_entry_at(&content, target)
}

/// True if either mountinfo option field marks the mount read-only.
/// Field 6 (vfs_options) carries VFS-level per-mount flags; field 11
/// (fs_options) carries superblock/filesystem options. Both can independently
/// carry `ro`, so the field that carries it is state evidence, not source
/// attribution.
pub(crate) fn entry_is_read_only(entry: &MountEntry) -> bool {
    has_ro(&entry.vfs_options) || has_ro(&entry.fs_options)
}

fn has_ro(opts: &str) -> bool {
    opts.split(',').any(|opt| opt.trim() == "ro")
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

    /* Intent: a configured target with a trailing slash matches the
     *   canonical non-root mount point emitted by mountinfo.
     * Why: all safety-critical callers share this helper, so normalization
     *   must happen before the exact mount-point comparison.
     * Scenario: autosuspend fail-open seam: `! braid idle` runs against a
     *   mounted pool whose config says "/mnt/storage/"; without this match,
     *   idle reports PoolOffline and allows suspend on a mounted pool.
     */
    #[test]
    fn fstype_at_mount_matches_trailing_slash_target() {
        let body = format!("{ROOT_LINE}{}", target_btrfs_line());
        assert_eq!(
            fstype_at_mount(&body, "/mnt/storage/").unwrap(),
            Some("btrfs".to_string())
        );
    }

    /* Intent: the root target remains "/" instead of being normalized to an
     *   empty string.
     * Why: a blanket trim_end_matches('/') would break root mount lookups.
     * Scenario: a caller probes the root filesystem entry in mountinfo.
     */
    #[test]
    fn fstype_at_mount_root_target_still_matches_root_entry() {
        assert_eq!(
            fstype_at_mount(ROOT_LINE, "/").unwrap(),
            Some("ext4".to_string())
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

    /* Intent: a probed target containing a space matches a mountinfo entry
     *   whose mount-point field contains \040.
     * Why: kernel escapes whitespace as octal; without decoding, the
     *   comparison silently misses mounted paths that contain whitespace.
     * Scenario: an unrelated mount elsewhere in the table is rendered as an
     *   escaped path, and the parser still decodes it before comparison.
     */
    #[test]
    fn fstype_at_mount_decodes_octal_escaped_path() {
        let body = "36 35 0:32 / /mnt/other\\040backup rw shared:1 - btrfs /dev/mapper/other rw\n";
        assert_eq!(
            fstype_at_mount(body, "/mnt/other backup").unwrap(),
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
     *   mojibake and silently missing a matching target path.
     * Scenario: an unrelated mountinfo entry contains U+00E9 (two bytes
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

    /* Intent: mount_entry_at returns the fstype plus both mountinfo option
     *   fields for the configured target.
     * Why: the read-only preflight must inspect both the VFS mount flags and
     *   the filesystem/superblock flags.
     * Scenario: btrfs is mounted rw at /mnt/storage with space_cache enabled.
     */
    #[test]
    fn mount_entry_at_returns_fstype_and_both_option_fields_for_target() {
        let body = format!(
            "36 35 0:32 / {TARGET} rw,relatime shared:1 - btrfs /dev/mapper/braid-vdb rw,space_cache=v2\n"
        );
        assert_eq!(
            mount_entry_at(&body, TARGET).unwrap(),
            Some(MountEntry {
                fstype: "btrfs".to_string(),
                vfs_options: "rw,relatime".to_string(),
                fs_options: "rw,space_cache=v2".to_string(),
            })
        );
    }

    /* Intent: mount_entry_at also matches a trailing-slash target against the
     *   canonical non-root mount point emitted by mountinfo.
     * Why: read-only preflight depends on this helper to inspect both
     *   mountinfo option fields, so the normalization must apply here too.
     * Scenario: a mutating command runs with braid.mountPoint =
     *   "/mnt/storage/" while /mnt/storage is mounted read-only.
     */
    #[test]
    fn mount_entry_at_matches_trailing_slash_target() {
        let body = format!(
            "36 35 0:32 / {TARGET} ro,relatime shared:1 - btrfs /dev/mapper/braid-vdb ro,space_cache=v2\n"
        );
        assert_eq!(
            mount_entry_at(&body, "/mnt/storage/").unwrap(),
            Some(MountEntry {
                fstype: "btrfs".to_string(),
                vfs_options: "ro,relatime".to_string(),
                fs_options: "ro,space_cache=v2".to_string(),
            })
        );
    }

    /* Intent: mount_entry_at returns Ok(None) when mountinfo lacks the target.
     * Why: callers need to distinguish a readable table with no entry from a
     *   malformed or unreadable table.
     * Scenario: NAS booted with the pool still locked.
     */
    #[test]
    fn mount_entry_at_returns_none_when_target_absent() {
        assert_eq!(mount_entry_at(ROOT_LINE, TARGET).unwrap(), None);
    }

    /* Intent: mount_entry_at errors when the target line is malformed.
     * Why: a truncated target line must not be interpreted as "target absent".
     * Scenario: mountinfo line for the pool is missing the dash separator.
     */
    #[test]
    fn mount_entry_at_errors_on_malformed_target_line() {
        let body = format!("36 35 0:32 / {TARGET} rw,noatime shared:1 garbage_no_dash_separator\n");
        assert!(matches!(
            mount_entry_at(&body, TARGET),
            Err(MountInfoError::Malformed { .. })
        ));
    }

    /* Intent: mount_entry_at errors on malformed unrelated lines too.
     * Why: the strict-on-every-line policy prevents safety checks from using
     *   partial mountinfo.
     * Scenario: target is present, but another line in mountinfo is corrupt.
     */
    #[test]
    fn mount_entry_at_errors_on_malformed_unrelated_line() {
        let body = format!(
            "{}\
             this is not a mountinfo line\n",
            target_btrfs_line()
        );
        assert!(matches!(
            mount_entry_at(&body, TARGET),
            Err(MountInfoError::Malformed { .. })
        ));
    }

    /* Intent: duplicate target entries are rejected.
     * Why: overmounts make "pick one" unsafe for preflight decisions.
     * Scenario: two mountinfo entries claim /mnt/storage.
     */
    #[test]
    fn mount_entry_at_errors_on_duplicate_target_entries() {
        let body = format!("{}{}", target_btrfs_line(), target_btrfs_line());
        assert!(matches!(
            mount_entry_at(&body, TARGET),
            Err(MountInfoError::DuplicateTarget { .. })
        ));
    }

    /* Intent: optional fields before the dash do not shift option parsing.
     * Why: mountinfo optional_fields is variable length.
     * Scenario: a shared mount includes a master:N optional field.
     */
    #[test]
    fn mount_entry_at_handles_optional_fields() {
        let body = format!(
            "36 35 0:32 / {TARGET} rw,noatime shared:1 master:7 - btrfs /dev/mapper/braid-disk1 rw,space_cache=v2\n"
        );
        let entry = mount_entry_at(&body, TARGET).unwrap().unwrap();
        assert_eq!(entry.fstype, "btrfs");
        assert_eq!(entry.vfs_options, "rw,noatime");
        assert_eq!(entry.fs_options, "rw,space_cache=v2");
    }

    /* Intent: empty source fields still parse.
     * Why: split(' ') must be preserved; split_whitespace would collapse the
     *   empty source field and shift super_options.
     * Scenario: pseudo-fs entry with an empty source field.
     */
    #[test]
    fn mount_entry_at_accepts_empty_source_field() {
        let body = format!("36 35 0:32 / {TARGET} rw shared:1 - tmpfs  rw,size=1G\n");
        let entry = mount_entry_at(&body, TARGET).unwrap().unwrap();
        assert_eq!(entry.fstype, "tmpfs");
        assert_eq!(entry.vfs_options, "rw");
        assert_eq!(entry.fs_options, "rw,size=1G");
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
                .clone()
                .map_err(|kind| std::io::Error::new(kind, "mock mountinfo read failed"))
        }
        fn create_dir_all(&self, _: &str) -> Result<(), std::io::Error> {
            unreachable!("MockMountInfoFs: read-only fixture; create_dir_all must never be called")
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

    /* Intent: mount_entry_at_via_fs propagates mountinfo read failures.
     * Why: callers must surface indeterminate mount state instead of assuming
     *   writable or absent.
     * Scenario: `/proc/self/mountinfo` cannot be read.
     */
    #[test]
    fn mount_entry_at_via_fs_propagates_io_failure() {
        let fs = MockMountInfoFs {
            mountinfo: Err(std::io::ErrorKind::PermissionDenied),
        };
        assert!(matches!(
            mount_entry_at_via_fs(&fs, TARGET),
            Err(MountInfoError::Io(_))
        ));
    }

    /* Intent: mount_entry_at_via_fs decodes kernel octal escapes in targets.
     * Why: paths containing spaces must still compare against decoded
     *   mountinfo fields.
     * Scenario: an unrelated mount elsewhere in the table is rendered as an
     *   escaped path, and the parser still decodes it before comparison.
     */
    #[test]
    fn mount_entry_at_via_fs_decodes_octal_escaped_path() {
        let fs = MockMountInfoFs {
            mountinfo: Ok(
                "36 35 0:32 / /mnt/other\\040backup rw shared:1 - btrfs /dev/mapper/other rw\n"
                    .to_string(),
            ),
        };
        let entry = mount_entry_at_via_fs(&fs, "/mnt/other backup")
            .unwrap()
            .unwrap();
        assert_eq!(entry.fstype, "btrfs");
        assert_eq!(entry.vfs_options, "rw");
        assert_eq!(entry.fs_options, "rw");
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
            fn create_dir_all(&self, _: &str) -> Result<(), std::io::Error> {
                unreachable!("FailingFs: read-only fixture; create_dir_all must never be called")
            }
        }
        let result = is_btrfs_mounted(&FailingFs, TARGET);
        assert!(matches!(result, Err(MountInfoError::Io(_))));
    }
}
