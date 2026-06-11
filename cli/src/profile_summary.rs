use serde::{Deserialize, Serialize};

use crate::parse::types::{BtrfsBgType, BtrfsDfEntry};
use crate::status::AllocationEntry;

/// Per-block-group-type redundancy summary for `braid status`.
/// One classifier feeds the human and JSON status surfaces; rendering is per-caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    pub data: TypeProfile,
    pub metadata: TypeProfile,
    pub system: TypeProfile,
}

/// One block-group-type's redundancy classification plus the raw profile
/// names that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeProfile {
    pub profiles: Vec<String>,
    pub class: Redundancy,
}

/// Coarse redundancy category used to choose human status render suffixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redundancy {
    Mirrored,
    SameDisk,
    NoRedundancy,
    Mixed,
    Unknown,
}

/// Per-block-group-type profile payload for `braid status --json`.
/// Carries raw btrfs profile names, not braid's human-facing classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileJson {
    pub data: Vec<String>,
    pub metadata: Vec<String>,
    pub system: Vec<String>,
}

impl From<&ProfileSummary> for ProfileJson {
    fn from(summary: &ProfileSummary) -> Self {
        Self {
            data: summary.data.profiles.clone(),
            metadata: summary.metadata.profiles.clone(),
            system: summary.system.profiles.clone(),
        }
    }
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

fn summarize_profiles<I>(profiles: I) -> TypeProfile
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
    let profiles: Vec<String> = unique.into_iter().map(|(profile, _)| profile).collect();
    let class = classify_profiles(&profiles);

    TypeProfile { profiles, class }
}

fn classify_profiles(profiles: &[String]) -> Redundancy {
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

/// Build the shared profile summary from parsed btrfs df entries.
pub fn from_df_entries(entries: &[BtrfsDfEntry]) -> ProfileSummary {
    ProfileSummary {
        data: summarize_profiles(
            entries
                .iter()
                .filter(|entry| entry.bg_type == BtrfsBgType::Data)
                .map(|entry| entry.bg_profile.to_string()),
        ),
        metadata: summarize_profiles(
            entries
                .iter()
                .filter(|entry| entry.bg_type == BtrfsBgType::Metadata)
                .map(|entry| entry.bg_profile.to_string()),
        ),
        system: summarize_profiles(
            entries
                .iter()
                .filter(|entry| entry.bg_type == BtrfsBgType::System)
                .map(|entry| entry.bg_profile.to_string()),
        ),
    }
}

/// Build the shared profile summary from serialized status allocation rows.
pub fn from_allocation(entries: &[AllocationEntry]) -> ProfileSummary {
    ProfileSummary {
        data: summarize_profiles(
            entries
                .iter()
                .filter(|entry| entry.bg_type == "Data")
                .map(|entry| entry.profile.clone()),
        ),
        metadata: summarize_profiles(
            entries
                .iter()
                .filter(|entry| entry.bg_type == "Metadata")
                .map(|entry| entry.profile.clone()),
        ),
        system: summarize_profiles(
            entries
                .iter()
                .filter(|entry| entry.bg_type == "System")
                .map(|entry| entry.profile.clone()),
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

    // Intent: a clean three-disk RAID1 pool classifies every block-group type
    // as mirrored.
    // Why it exists: the healthy RAID1 baseline is the reference state every
    // status surface reports; misclassifying it would mislabel a normal pool.
    // Scenario: a normal fully-balanced pool reports RAID1 Data, Metadata, and System rows.
    #[test]
    fn summary_for_3disk_raid1_pool() {
        let summary = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
            entry(BtrfsBgType::Metadata, BtrfsProfile::Raid1),
            entry(BtrfsBgType::System, BtrfsProfile::Raid1),
        ]);

        assert_eq!(summary.data.class, Redundancy::Mirrored);
        assert_eq!(summary.metadata.class, Redundancy::Mirrored);
        assert_eq!(summary.system.class, Redundancy::Mirrored);
        assert_eq!(summary.data.profiles, ["RAID1"]);
        assert_eq!(summary.metadata.profiles, ["RAID1"]);
        assert_eq!(summary.system.profiles, ["RAID1"]);
    }

    // Intent: a single-disk bootstrap reports data as unprotected while
    // metadata and system are same-disk DUP.
    // Why it exists: a scalar "single" profile hides the DUP rows' actual but
    // non-disk-redundant protection story.
    // Scenario: the first `braid add` creates data=single plus metadata/system=DUP.
    #[test]
    fn summary_for_single_disk_pool() {
        let summary = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Single),
            entry(BtrfsBgType::Metadata, BtrfsProfile::Dup),
            entry(BtrfsBgType::System, BtrfsProfile::Dup),
        ]);

        assert_eq!(summary.data.class, Redundancy::NoRedundancy);
        assert_eq!(summary.metadata.class, Redundancy::SameDisk);
        assert_eq!(summary.system.class, Redundancy::SameDisk);
        assert_eq!(summary.data.profiles, ["single"]);
        assert_eq!(summary.metadata.profiles, ["DUP"]);
        assert_eq!(summary.system.profiles, ["DUP"]);
    }

    // Intent: mixed data profiles retain canonical domain order.
    // Why it exists: alphabetical sorting would put RAID1 before single and
    // make human and JSON surfaces disagree with the intended examples.
    // Scenario: degraded writes created single data chunks before RAID1 was restored.
    #[test]
    fn summary_for_mixed_data_profile() {
        let summary = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
            entry(BtrfsBgType::Data, BtrfsProfile::Single),
        ]);

        assert_eq!(summary.data.class, Redundancy::Mixed);
        assert_eq!(summary.data.profiles, ["single", "RAID1"]);
    }

    // Intent: mixed metadata profiles are detected independently of data.
    // Why it exists: metadata and data have separate btrfs profile state and
    // status must not infer one from the other.
    // Scenario: an interrupted metadata balance leaves both DUP and RAID1 chunks.
    #[test]
    fn summary_for_mixed_metadata_profile() {
        let summary = from_df_entries(&[
            entry(BtrfsBgType::Metadata, BtrfsProfile::Dup),
            entry(BtrfsBgType::Metadata, BtrfsProfile::Raid1),
        ]);

        assert_eq!(summary.metadata.class, Redundancy::Mixed);
        assert_eq!(summary.metadata.profiles, ["DUP", "RAID1"]);
    }

    // Intent: GlobalReserve rows never appear in the per-type profile summary.
    // Why it exists: GlobalReserve is an internal metadata reservation and not
    // a Data, Metadata, or System block-group type.
    // Scenario: btrfs df includes its normal GlobalReserve single row.
    #[test]
    fn summary_omits_global_reserve() {
        let summary = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
            entry(BtrfsBgType::GlobalReserve, BtrfsProfile::Single),
        ]);

        assert_eq!(summary.data.profiles, ["RAID1"]);
        assert!(summary.metadata.profiles.is_empty());
        assert!(summary.system.profiles.is_empty());
    }

    // Intent: empty df data reports Unknown with empty profile vectors.
    // Why it exists: renderers need to distinguish "no probe data" from a
    // non-empty unclassified profile name such as RAID5.
    // Scenario: status asks for a summary before df rows are available.
    #[test]
    fn summary_for_empty_df() {
        let summary = from_df_entries(&[]);

        assert_eq!(summary.data.class, Redundancy::Unknown);
        assert_eq!(summary.metadata.class, Redundancy::Unknown);
        assert_eq!(summary.system.class, Redundancy::Unknown);
        assert!(summary.data.profiles.is_empty());
        assert!(summary.metadata.profiles.is_empty());
        assert!(summary.system.profiles.is_empty());
    }

    // Intent: RAID0 is classified as no redundancy.
    // Why it exists: RAID0 is not produced by braid, but callers must not
    // render it as mirrored or unknown if btrfs reports it.
    // Scenario: an operator inspects a pool whose data chunks are RAID0.
    #[test]
    fn summary_for_raid0_data() {
        let summary = from_df_entries(&[entry(BtrfsBgType::Data, BtrfsProfile::Raid0)]);

        assert_eq!(summary.data.class, Redundancy::NoRedundancy);
        assert_eq!(summary.data.profiles, ["RAID0"]);
    }

    // Intent: RAID5 is surfaced verbatim but left unclassified.
    // Why it exists: parity profiles have a different redundancy story than
    // braid's RAID1-only policy, so status must not over- or under-promise.
    // Scenario: a non-braid-created pool reports Data=RAID5.
    #[test]
    fn summary_for_raid5_data_is_unknown() {
        let summary = from_df_entries(&[entry(BtrfsBgType::Data, BtrfsProfile::Raid5)]);

        assert_eq!(summary.data.class, Redundancy::Unknown);
        assert_eq!(summary.data.profiles, ["RAID5"]);
    }

    // Intent: unparsed future profile names are preserved exactly.
    // Why it exists: operators need to see the raw profile name btrfs reported
    // instead of a braid-invented replacement token.
    // Scenario: btrfs introduces a profile string this braid build does not know.
    #[test]
    fn summary_for_unparsed_profile_is_unknown() {
        let summary = from_df_entries(&[entry(
            BtrfsBgType::Data,
            BtrfsProfile::Unknown("foo".to_owned()),
        )]);

        assert_eq!(summary.data.class, Redundancy::Unknown);
        assert_eq!(summary.data.profiles, ["foo"]);
    }

    // Intent: unknown profile names keep btrfs report order after known names.
    // Why it exists: a set-based dedupe would alphabetize unknown tails and
    // make JSON arrays unstable relative to the source report.
    // Scenario: btrfs reports RAID1, then two unknown profile names, then RAID1 again.
    #[test]
    fn summary_preserves_unknown_tail_order() {
        let summary = from_df_entries(&[
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
            entry(BtrfsBgType::Data, BtrfsProfile::Unknown("XENO".to_owned())),
            entry(
                BtrfsBgType::Data,
                BtrfsProfile::Unknown("FOOBAR".to_owned()),
            ),
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
        ]);

        assert_eq!(summary.data.class, Redundancy::Mixed);
        assert_eq!(summary.data.profiles, ["RAID1", "XENO", "FOOBAR"]);
    }

    // Intent: allocation rows and parsed df entries feed the same classifier.
    // Why it exists: status classifies parsed df entries when building its
    // report and serialized allocation when rendering the human form; both
    // must tell the same profile story.
    // Scenario: status builds its report from df, then renders from the allocation field.
    #[test]
    fn from_allocation_matches_from_df_entries() {
        let entries = vec![
            entry(BtrfsBgType::Data, BtrfsProfile::Single),
            entry(BtrfsBgType::Data, BtrfsProfile::Raid1),
            entry(BtrfsBgType::Metadata, BtrfsProfile::Raid1),
            entry(BtrfsBgType::System, BtrfsProfile::Raid1),
            entry(BtrfsBgType::GlobalReserve, BtrfsProfile::Single),
        ];
        let allocation = vec![
            AllocationEntry {
                bg_type: "Data".to_owned(),
                profile: "single".to_owned(),
                used_bytes: 1,
                allocated_bytes: 2,
            },
            AllocationEntry {
                bg_type: "Data".to_owned(),
                profile: "RAID1".to_owned(),
                used_bytes: 1,
                allocated_bytes: 2,
            },
            AllocationEntry {
                bg_type: "Metadata".to_owned(),
                profile: "RAID1".to_owned(),
                used_bytes: 1,
                allocated_bytes: 2,
            },
            AllocationEntry {
                bg_type: "System".to_owned(),
                profile: "RAID1".to_owned(),
                used_bytes: 1,
                allocated_bytes: 2,
            },
            AllocationEntry {
                bg_type: "GlobalReserve".to_owned(),
                profile: "single".to_owned(),
                used_bytes: 1,
                allocated_bytes: 2,
            },
        ];

        assert_eq!(from_allocation(&allocation), from_df_entries(&entries));
    }
}
