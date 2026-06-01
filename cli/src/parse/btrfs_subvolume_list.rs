use nom::{
    IResult, Parser,
    bytes::complete::tag,
    character::complete::{digit1, space1},
    combinator::{map_res, rest},
};

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{BtrfsSubvolume, BtrfsSubvolumeListOutput};

/// Parses one line of `btrfs subvolume list` output.
///
/// Format: `ID <id> gen <gen> top level <top_level> path <path>`
///
/// Example: `ID 256 gen 30 top level 5 path snapshots/daily`
fn parse_subvolume_line(input: &str) -> IResult<&str, BtrfsSubvolume> {
    let (input, _) = tag("ID")(input)?;
    let (input, _) = space1(input)?;
    let (input, id) = map_res(digit1, str::parse::<u64>).parse(input)?;
    let (input, _) = space1(input)?;
    let (input, _) = tag("gen")(input)?;
    let (input, _) = space1(input)?;
    let (input, generation) = map_res(digit1, str::parse::<u64>).parse(input)?;
    let (input, _) = space1(input)?;
    let (input, _) = tag("top")(input)?;
    let (input, _) = space1(input)?;
    let (input, _) = tag("level")(input)?;
    let (input, _) = space1(input)?;
    let (input, top_level) = map_res(digit1, str::parse::<u64>).parse(input)?;
    let (input, _) = space1(input)?;
    let (input, _) = tag("path")(input)?;
    let (input, _) = space1(input)?;
    let (input, path) = rest(input)?;

    Ok((
        input,
        BtrfsSubvolume {
            id,
            generation,
            top_level,
            path: path.to_owned(),
        },
    ))
}

pub fn parse_btrfs_subvolume_list(
    raw: &RawCommandOutput,
) -> Result<BtrfsSubvolumeListOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let mut subvolumes = Vec::new();

    for line in raw.stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (_, subvol) = parse_subvolume_line(trimmed).map_err(|_| ParseError::InvalidText {
            cmd: raw.cmd.clone(),
            detail: format!("unexpected subvolume list line: {trimmed:?}"),
        })?;
        subvolumes.push(subvol);
    }

    Ok(BtrfsSubvolumeListOutput { subvolumes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/nixos-26.05/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    // --- Contract tests (nixos-26.05 fixtures) ---

    /*
     * Intent: parse a realistic multi-subvolume listing from a real system.
     *
     * Why it exists: ensures the nom grammar matches the actual output format
     * produced by btrfs-progs on NixOS 26.05.
     *
     * Scenario: user opens the TUI Browse tab, switches to Subvolumes, and
     * the TUI parses the listing to populate the selectable subvolume list.
     */
    #[test]
    fn subvolume_list_parses_nixos_26_05_fixture() {
        let raw = RawCommandOutput {
            cmd: "btrfs subvolume list /mnt/storage".into(),
            stdout: fixture("btrfs-subvolume-list.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_subvolume_list(&raw).unwrap();
        assert!(out.subvolumes.len() >= 2, "expected at least 2 subvolumes");
        assert_eq!(out.subvolumes[0].id, 256);
        assert!(
            out.subvolumes[0].generation > 0,
            "generation should be positive"
        );
        assert_eq!(out.subvolumes[0].top_level, 5);
        assert_eq!(out.subvolumes[0].path, "data");
        assert_eq!(out.subvolumes[1].path, "snapshots");
    }

    // --- Synthetic tests (inline) ---

    /*
     * Intent: empty stdout (no subvolumes) returns an empty vec, not an error.
     *
     * Why it exists: a fresh pool with no subvolumes is a valid state; the
     * parser must not reject it.
     *
     * Scenario: user creates a new pool and the TUI Browse Subvolumes view
     * shows "(no subvolumes)" instead of crashing.
     */
    #[test]
    fn subvolume_list_empty_output() {
        let raw = RawCommandOutput {
            cmd: "btrfs subvolume list /mnt/storage".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_subvolume_list(&raw).unwrap();
        assert!(out.subvolumes.is_empty());
    }

    /*
     * Intent: paths with spaces are captured verbatim.
     *
     * Why it exists: btrfs allows arbitrary subvolume names including spaces;
     * the parser uses `rest` after "path " so it must not truncate at spaces.
     *
     * Scenario: user created `btrfs subvolume create /mnt/storage/my media`.
     */
    #[test]
    fn subvolume_list_path_with_spaces() {
        let raw = RawCommandOutput {
            cmd: "btrfs subvolume list /mnt/storage".into(),
            stdout: "ID 300 gen 10 top level 5 path my media files\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_subvolume_list(&raw).unwrap();
        assert_eq!(out.subvolumes.len(), 1);
        assert_eq!(out.subvolumes[0].path, "my media files");
    }

    /*
     * Intent: deeply nested paths are parsed correctly.
     *
     * Why it exists: subvolumes can be nested arbitrarily deep.
     *
     * Scenario: user has snapshot hierarchies like snapshots/daily/2026/03/01.
     */
    #[test]
    fn subvolume_list_deeply_nested_path() {
        let raw = RawCommandOutput {
            cmd: "btrfs subvolume list /mnt/storage".into(),
            stdout: "ID 500 gen 99 top level 256 path a/b/c/d/e/f\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_subvolume_list(&raw).unwrap();
        assert_eq!(out.subvolumes[0].path, "a/b/c/d/e/f");
    }

    /*
     * Intent: non-zero exit code returns CommandFailed.
     *
     * Why it exists: if the filesystem is not mounted, btrfs returns an error;
     * the parser must propagate it rather than attempting to parse garbage.
     *
     * Scenario: the Browse loader requests subvolumes while the pool is
     * unmounted.
     */
    #[test]
    fn subvolume_list_nonzero_exit() {
        let raw = RawCommandOutput {
            cmd: "btrfs subvolume list /mnt/storage".into(),
            stdout: String::new(),
            stderr: "ERROR: not a btrfs filesystem".into(),
            exit_status: 1,
        };
        let err = parse_btrfs_subvolume_list(&raw).unwrap_err();
        assert!(matches!(
            err,
            ParseError::CommandFailed { exit_code: 1, .. }
        ));
    }

    /*
     * Intent: multiple subvolumes with varying IDs and nesting are all parsed.
     *
     * Why it exists: exercises the line-by-line iteration and accumulation.
     *
     * Scenario: production pool with a mix of top-level and nested subvolumes.
     */
    #[test]
    fn subvolume_list_multi_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs subvolume list /mnt/storage".into(),
            stdout: "ID 256 gen 10 top level 5 path videos\n\
                     ID 257 gen 20 top level 5 path music\n\
                     ID 258 gen 30 top level 256 path videos/archive\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_subvolume_list(&raw).unwrap();
        assert_eq!(out.subvolumes.len(), 3);
        assert_eq!(out.subvolumes[0].path, "videos");
        assert_eq!(out.subvolumes[1].path, "music");
        assert_eq!(out.subvolumes[2].path, "videos/archive");
        assert_eq!(out.subvolumes[2].top_level, 256);
    }
}
