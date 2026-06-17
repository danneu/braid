use serde::{Deserialize, Serialize};

use crate::parse::types::{BtrfsBgType, BtrfsDfEntry};

/// Coarse redundancy category used to choose human status render suffixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redundancy {
    Mirrored,
    SameDisk,
    NoRedundancy,
    Mixed,
    Unknown,
}

/// Per-block-group-type profile payload for `braid status`.
/// Carries canonical raw btrfs profile names shared by JSON and human surfaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileJson {
    pub data: Vec<String>,
    pub metadata: Vec<String>,
    pub system: Vec<String>,
}

#[cfg(test)]
impl ProfileJson {
    /// Test convenience for legacy status fixtures where every block-group
    /// type carried the same single profile string.
    pub fn uniform(name: &str) -> Self {
        Self {
            data: vec![name.to_owned()],
            metadata: vec![name.to_owned()],
            system: vec![name.to_owned()],
        }
    }
}

fn profile_display_order(profile: &str) -> u8 {
    match profile {
        "single" => 0,
        "DUP" => 1,
        "RAID0" => 2,
        "RAID1" => 3,
        "RAID1C3" => 4,
        "RAID1C4" => 5,
        "RAID5" => 6,
        "RAID6" => 7,
        "RAID10" => 8,
        _ => 255,
    }
}

fn normalize_profiles<I>(profiles: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut unique: Vec<(String, usize)> = Vec::new();
    for (index, profile) in profiles.into_iter().enumerate() {
        if unique.iter().any(|(seen, _)| seen == &profile) {
            continue;
        }
        unique.push((profile, index));
    }

    unique.sort_by_key(|(profile, first_seen)| (profile_display_order(profile), *first_seen));
    unique.into_iter().map(|(profile, _)| profile).collect()
}

/// Classify canonical profile names for the human `braid status` renderer.
pub(crate) fn classify_profiles(profiles: &[String]) -> Redundancy {
    if profiles.is_empty() {
        return Redundancy::Unknown;
    }
    if profiles.len() > 1 {
        return Redundancy::Mixed;
    }

    match profiles[0].as_str() {
        "RAID1" | "RAID1C3" | "RAID1C4" | "RAID10" => Redundancy::Mirrored,
        "DUP" => Redundancy::SameDisk,
        "single" | "RAID0" => Redundancy::NoRedundancy,
        _ => Redundancy::Unknown,
    }
}

/// Build the canonical profile-name vectors from parsed btrfs df entries.
pub fn from_df_entries(entries: &[BtrfsDfEntry]) -> ProfileJson {
    ProfileJson {
        data: normalize_profiles(
            entries
                .iter()
                .filter(|entry| entry.bg_type == BtrfsBgType::Data)
                .map(|entry| entry.bg_profile.to_string()),
        ),
        metadata: normalize_profiles(
            entries
                .iter()
                .filter(|entry| entry.bg_type == BtrfsBgType::Metadata)
                .map(|entry| entry.bg_profile.to_string()),
        ),
        system: normalize_profiles(
            entries
                .iter()
                .filter(|entry| entry.bg_type == BtrfsBgType::System)
                .map(|entry| entry.bg_profile.to_string()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::types::BtrfsProfile;

    fn entry(bg_type: BtrfsBgType, bg_profile: BtrfsProfile) -> BtrfsDfEntry {
        BtrfsDfEntry {
            bg_type,
            bg_profile,
            bg_used: 1,
            bg_total: 2,
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    // Intent: btrfs mirror profiles classify as mirrored for human status.
    // Why it exists: the human renderer adds no warning suffix for profiles
    // that provide cross-disk redundancy.
    // Scenario: a pool reports any btrfs mirror-family profile.
    #[test]
    fn classify_mirrored_profiles() {
        for profile in ["RAID1", "RAID1C3", "RAID1C4", "RAID10"] {
            assert_eq!(classify_profiles(&names(&[profile])), Redundancy::Mirrored);
        }
    }

    // Intent: DUP classifies as same-disk copies.
    // Why it exists: DUP is protected against block corruption but not disk
    // loss, so the human renderer must choose the specific no-disk-redundancy suffix.
    // Scenario: a single-disk pool reports DUP metadata or system chunks.
    #[test]
    fn classify_same_disk_profile() {
        assert_eq!(classify_profiles(&names(&["DUP"])), Redundancy::SameDisk);
    }

    // Intent: unmirrored profiles classify as no redundancy.
    // Why it exists: single and RAID0 must surface as unprotected in human status.
    // Scenario: btrfs reports data chunks that cannot survive a disk loss.
    #[test]
    fn classify_no_redundancy_profiles() {
        for profile in ["single", "RAID0"] {
            assert_eq!(
                classify_profiles(&names(&[profile])),
                Redundancy::NoRedundancy
            );
        }
    }

    // Intent: multiple profile names classify as mixed.
    // Why it exists: human status needs the warning suffix whenever a block
    // group type spans more than one redundancy story.
    // Scenario: degraded writes created single data chunks before RAID1 was restored.
    #[test]
    fn classify_mixed_profiles() {
        assert_eq!(
            classify_profiles(&names(&["single", "RAID1"])),
            Redundancy::Mixed
        );
    }

    // Intent: missing profile data classifies as unknown.
    // Why it exists: no df row is distinct from a known no-redundancy profile.
    // Scenario: status asks for a per-type label before btrfs df data exists.
    #[test]
    fn classify_empty_profiles_as_unknown() {
        assert_eq!(classify_profiles(&[]), Redundancy::Unknown);
    }

    // Intent: unrecognized profile names classify as unknown.
    // Why it exists: future btrfs profile strings should render verbatim
    // without braid inventing a redundancy promise.
    // Scenario: btrfs introduces a profile string this braid build does not know.
    #[test]
    fn classify_unrecognized_profile_as_unknown() {
        assert_eq!(classify_profiles(&names(&["RAID5"])), Redundancy::Unknown);
        assert_eq!(classify_profiles(&names(&["foo"])), Redundancy::Unknown);
    }

    // Intent: a clean three-disk RAID1 pool reports RAID1 for every block-group type.
    // Why it exists: the healthy RAID1 baseline is the reference state every
    // status surface reports; dropping or renaming it would mislabel a normal pool.
    // Scenario: a normal fully-balanced pool reports RAID1 Data, Metadata, and System rows.
    #[test]
    fn profile_json_for_3disk_raid1_pool() {
        let profile = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
            entry(BtrfsBgType::Metadata, BtrfsProfile::Raid1),
            entry(BtrfsBgType::System, BtrfsProfile::Raid1),
        ]);

        assert_eq!(profile.data, ["RAID1"]);
        assert_eq!(profile.metadata, ["RAID1"]);
        assert_eq!(profile.system, ["RAID1"]);
    }

    // Intent: a single-disk bootstrap reports data as single while metadata
    // and system report DUP.
    // Why it exists: a scalar "single" profile hides the DUP rows' actual but
    // non-disk-redundant protection story.
    // Scenario: the first `braid add` creates data=single plus metadata/system=DUP.
    #[test]
    fn profile_json_for_single_disk_pool() {
        let profile = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Single),
            entry(BtrfsBgType::Metadata, BtrfsProfile::Dup),
            entry(BtrfsBgType::System, BtrfsProfile::Dup),
        ]);

        assert_eq!(profile.data, ["single"]);
        assert_eq!(profile.metadata, ["DUP"]);
        assert_eq!(profile.system, ["DUP"]);
    }

    // Intent: mixed data profiles retain canonical domain order.
    // Why it exists: alphabetical sorting would put RAID1 before single and
    // make human and JSON surfaces disagree with the intended examples.
    // Scenario: degraded writes created single data chunks before RAID1 was restored.
    #[test]
    fn profile_json_for_mixed_data_profile() {
        let profile = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
            entry(BtrfsBgType::Data, BtrfsProfile::Single),
        ]);

        assert_eq!(profile.data, ["single", "RAID1"]);
    }

    // Intent: mixed metadata profile names are normalized independently of data.
    // Why it exists: metadata and data have separate btrfs profile state and
    // status must not infer one from the other.
    // Scenario: an interrupted metadata balance leaves both DUP and RAID1 chunks.
    #[test]
    fn profile_json_for_mixed_metadata_profile() {
        let profile = from_df_entries(&[
            entry(BtrfsBgType::Metadata, BtrfsProfile::Dup),
            entry(BtrfsBgType::Metadata, BtrfsProfile::Raid1),
        ]);

        assert_eq!(profile.metadata, ["DUP", "RAID1"]);
    }

    // Intent: GlobalReserve rows never appear in the per-type profile summary.
    // Why it exists: GlobalReserve is an internal metadata reservation and not
    // a Data, Metadata, or System block-group type.
    // Scenario: btrfs df includes its normal GlobalReserve single row.
    #[test]
    fn profile_json_omits_global_reserve() {
        let profile = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
            entry(BtrfsBgType::GlobalReserve, BtrfsProfile::Single),
        ]);

        assert_eq!(profile.data, ["RAID1"]);
        assert!(profile.metadata.is_empty());
        assert!(profile.system.is_empty());
    }

    // Intent: empty df data reports empty profile vectors.
    // Why it exists: renderers need to distinguish "no probe data" from a
    // non-empty unclassified profile name such as RAID5.
    // Scenario: status asks for a summary before df rows are available.
    #[test]
    fn profile_json_for_empty_df() {
        let profile = from_df_entries(&[]);

        assert!(profile.data.is_empty());
        assert!(profile.metadata.is_empty());
        assert!(profile.system.is_empty());
    }

    // Intent: RAID0 is preserved as a profile name.
    // Why it exists: RAID0 is not produced by braid, but callers must not
    // lose the raw profile if btrfs reports it.
    // Scenario: an operator inspects a pool whose data chunks are RAID0.
    #[test]
    fn profile_json_for_raid0_data() {
        let profile = from_df_entries(&[entry(BtrfsBgType::Data, BtrfsProfile::Raid0)]);

        assert_eq!(profile.data, ["RAID0"]);
    }

    // Intent: RAID5 is surfaced verbatim.
    // Why it exists: parity profiles have a different redundancy story than
    // braid's RAID1-only policy, so status must not over- or under-promise.
    // Scenario: a non-braid-created pool reports Data=RAID5.
    #[test]
    fn profile_json_for_raid5_data() {
        let profile = from_df_entries(&[entry(BtrfsBgType::Data, BtrfsProfile::Raid5)]);

        assert_eq!(profile.data, ["RAID5"]);
    }

    // Intent: unparsed future profile names are preserved exactly.
    // Why it exists: operators need to see the raw profile name btrfs reported
    // instead of a braid-invented replacement token.
    // Scenario: btrfs introduces a profile string this braid build does not know.
    #[test]
    fn profile_json_for_unparsed_profile() {
        let profile = from_df_entries(&[entry(
            BtrfsBgType::Data,
            BtrfsProfile::Unknown("foo".to_owned()),
        )]);

        assert_eq!(profile.data, ["foo"]);
    }

    // Intent: unknown profile names keep btrfs report order after known names.
    // Why it exists: a set-based dedupe would alphabetize unknown tails and
    // make JSON arrays unstable relative to the source report.
    // Scenario: btrfs reports RAID1, then two unknown profile names, then RAID1 again.
    #[test]
    fn profile_json_preserves_unknown_tail_order() {
        let profile = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
            entry(BtrfsBgType::Data, BtrfsProfile::Unknown("XENO".to_owned())),
            entry(
                BtrfsBgType::Data,
                BtrfsProfile::Unknown("FOOBAR".to_owned()),
            ),
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
        ]);

        assert_eq!(profile.data, ["RAID1", "XENO", "FOOBAR"]);
    }
}
