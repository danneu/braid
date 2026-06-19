//! Shared RAID1 capacity math for preflight checks and read-only advisories.
//!
//! Keeping this separate from command modules makes status, doctor, and
//! mutating preflights use the same btrfs chunk-pair geometry.

use crate::confirm::format_bytes;
use crate::parse::types::BtrfsDeviceUsageEntry;

const GIB: u64 = 1 << 30;

/// Re-alert step for an acked-but-worsening ENOSPC risk: the monitor re-fires
/// only when the live `margin` has fallen this many bytes below the acked
/// baseline. Half a btrfs data chunk (512 MiB), deliberately *below* the ~1 GiB
/// threshold rather than equal to it: an at-risk `margin` is bounded in
/// `[-threshold, 0)` (unallocated and chunk-pair capacity are both >= 0), so a
/// full-chunk step would push `baseline_margin - step` past the floor and make
/// the re-fire branch unreachable. Half a chunk keeps "materially worse"
/// meaningful while still firing as an acked pool fills toward empty.
pub const ENOSPC_WORSEN_STEP: u64 = GIB / 2;

/// Re-arm threshold for ENOSPC risk: the monitor drops a stored baseline (so a
/// future recurrence alerts fresh) once the predicate's signed surplus climbs
/// back to at least this many bytes. One btrfs data chunk (~1 GiB) of hysteresis
/// above the `margin < 0` fire boundary keeps a pool hovering at the edge from
/// flapping between armed and re-armed.
pub const ENOSPC_REARM_MARGIN: u64 = GIB;

/// Kernel-aligned low-unallocated threshold for proactive ENOSPC advisories.
///
/// Mirrors btrfs's effective data chunk-size cap for non-zoned filesystems:
/// 10% of total writable device bytes, capped at 1 GiB.
pub fn enospc_risk_threshold(total_device_bytes: u64) -> u64 {
    GIB.min(total_device_bytes / 10)
}

/// Canonical data-only balance command for proactive ENOSPC guidance.
///
/// The helper makes metadata balance parameters unrepresentable for the status
/// advisory and doctor's metadata-pressure check, where preserving metadata
/// block-group headroom is the safety invariant.
pub(crate) fn compact_data_command(mount: &str, usage: u8) -> String {
    format!("btrfs balance start -dusage={usage} {mount}")
}

/// Compute usable RAID1 chunk-pair capacity from sorted per-device headroom.
///
/// The caller supplies unallocated bytes in descending order so preflight and
/// advisory paths can share the same RAID1 bottleneck calculation.
pub fn raid1_chunk_pair_capacity(unallocated_desc: &[u64]) -> u64 {
    if unallocated_desc.len() < 2 {
        return 0;
    }

    let largest = unallocated_desc[0];
    let rest: u64 = unallocated_desc[1..].iter().sum();
    let total = largest + rest;
    if largest > rest { rest } else { total / 2 }
}

/// Sentinel `margin` for pools where the chunk-pair predicate does not apply
/// (degraded, or fewer than two devices). Callers treat it as healthy: a degraded
/// pool alerts louder through `MissingDevice`, and a single-device pool has no
/// RAID1 geometry. Capping at `i64::MAX` keeps `at_risk()` false and lifts the
/// monitor's re-arm branch (`margin >= ENOSPC_REARM_MARGIN`) -- but the monitor
/// skips ENOSPC entirely on a degraded pool *before* reaching that branch, so the
/// sentinel never silently re-arms a still-at-risk pool across a device blip.
const ENOSPC_MARGIN_NOT_APPLICABLE: i64 = i64::MAX;

/// Typed ENOSPC-risk decision shared by `status`, `doctor`, and `monitor` so the
/// chunk-pair thresholds live in exactly one place and the three surfaces cannot
/// disagree about whether a pool is at risk.
///
/// `margin` is the binding signed surplus/deficit in bytes: negative is at-risk
/// depth (a disk loss may leave the pool unable to allocate RAID1 chunk pairs to
/// restore redundancy), positive is healthy headroom. It is both the `at_risk`
/// predicate (`margin < 0`) and the monotonic risk magnitude the monitor
/// baselines and re-arms on. `count_below` and `device_count` render the advisory
/// line without a re-probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnospcRiskAssessment {
    pub margin: i64,
    pub count_below: usize,
    pub device_count: usize,
    pub threshold: u64,
}

impl EnospcRiskAssessment {
    /// The pool is at risk exactly when the binding margin is negative. This is
    /// the single predicate `status`, `doctor`, and `monitor` branch on.
    pub fn at_risk(&self) -> bool {
        self.margin < 0
    }
}

/// Evaluate RAID1 chunk-pair ENOSPC risk as a signed margin.
///
/// This is the decision lifted out of `enospc_risk_advisory`'s prose: the
/// degraded / single-disk guard, the 2-device branch, and the 3+-device per-loss
/// simulation, kept signed instead of collapsed to a bool so `monitor` gets the
/// surplus it needs to baseline and re-arm. `margin < 0` is exactly the legacy
/// `at_risk` predicate, so `status` and `doctor` see identical decisions.
pub fn evaluate_enospc_risk(
    devices: &[BtrfsDeviceUsageEntry],
    missing_count: u64,
) -> EnospcRiskAssessment {
    if missing_count > 0 || devices.len() < 2 {
        return EnospcRiskAssessment {
            margin: ENOSPC_MARGIN_NOT_APPLICABLE,
            count_below: 0,
            device_count: devices.len(),
            threshold: 0,
        };
    }

    let current_total: u64 = devices.iter().map(|device| device.device_size).sum();
    let current_threshold = enospc_risk_threshold(current_total);
    // count_below intentionally uses the pre-loss threshold rendered in the
    // advisory, while the 3+ disk margin below compares each survivor set
    // with its no-larger post-loss threshold. That cannot produce a firing
    // advisory with a "0 of N" count: survivor_threshold <= current_threshold,
    // and any survivor set whose members are all >= current_threshold has
    // chunk-pair capacity >= current_threshold, therefore >= survivor_threshold.
    // So margin < 0 implies at least one device is below current_threshold.
    let count_below = devices
        .iter()
        .filter(|device| device.unallocated < current_threshold)
        .count();

    // `margin < 0` iff some term is negative, so taking the minimum over the
    // per-device (2-disk) or per-loss (3+-disk) surplus reproduces the legacy
    // `any(... < threshold)` predicate while keeping the deepest deficit signed.
    let margin = if devices.len() == 2 {
        devices
            .iter()
            .map(|device| device.unallocated as i64 - current_threshold as i64)
            .min()
            .expect("two-device pool always has a minimum")
    } else {
        (0..devices.len())
            .map(|lost_index| {
                let survivor_total: u64 = devices
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != lost_index)
                    .map(|(_, device)| device.device_size)
                    .sum();
                let survivor_threshold = enospc_risk_threshold(survivor_total);
                let mut survivor_unallocated: Vec<u64> = devices
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != lost_index)
                    .map(|(_, device)| device.unallocated)
                    .collect();
                survivor_unallocated.sort_unstable_by(|a, b| b.cmp(a));
                raid1_chunk_pair_capacity(&survivor_unallocated) as i64 - survivor_threshold as i64
            })
            .min()
            .expect("three-or-more-device pool always has a minimum")
    };

    EnospcRiskAssessment {
        margin,
        count_below,
        device_count: devices.len(),
        threshold: current_threshold,
    }
}

/// Advisory for pools that are one disk-loss away from RAID1 chunk ENOSPC.
///
/// A thin formatter over `evaluate_enospc_risk` so status and doctor render the
/// exact same string while sharing one predicate. Returns a vector to match the
/// other status advisory helpers, while emitting at most one message.
pub fn enospc_risk_advisory(devices: &[BtrfsDeviceUsageEntry], missing_count: u64) -> Vec<String> {
    let assessment = evaluate_enospc_risk(devices, missing_count);
    if !assessment.at_risk() {
        return Vec::new();
    }

    let cmd = compact_data_command("<mount>", 50);
    vec![format!(
        "ENOSPC risk: {count_below} of {device_count} devices have less than {threshold} unallocated -- if a disk fails, the pool may be unable to allocate RAID1 chunks to restore redundancy. Add capacity with 'braid add', delete unneeded files or snapshots, or compact data chunks with '{cmd}' (data only; do not balance metadata).",
        count_below = assessment.count_below,
        device_count = assessment.device_count,
        threshold = format_bytes(assessment.threshold),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Devid;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const TIB: u64 = 1 << 40;

    fn device(devid: Devid, device_size: u64, unallocated: u64) -> BtrfsDeviceUsageEntry {
        BtrfsDeviceUsageEntry {
            path: format!("/dev/mapper/braid-disk{}", devid.get()),
            devid,
            device_size,
            device_slack: 0,
            allocations: Vec::new(),
            unallocated,
        }
    }

    fn assert_data_only_recovery_advice(advisory: &str) {
        assert!(
            advisory.contains("braid add"),
            "should recommend braid add before raw btrfs recovery: {advisory}"
        );
        assert!(
            advisory.contains("-dusage=50"),
            "should recommend data compaction at a useful threshold: {advisory}"
        );
        assert!(
            !advisory.contains("mconvert") && !advisory.contains("musage"),
            "must not recommend metadata balancing: {advisory}"
        );
    }

    // Intent: enospc_risk_threshold caps large pools at btrfs's 1 GiB data
    //   chunk-size ceiling.
    // Why it exists: a proportional-only threshold would over-warn large NAS
    //   pools compared with the kernel allocator's urgency band.
    // Scenario: a multi-TiB RAID1 pool still uses the 1 GiB advisory threshold.
    #[test]
    fn enospc_risk_threshold_caps_at_1_gib() {
        assert_eq!(enospc_risk_threshold(100 * TIB), GIB);
    }

    // Intent: enospc_risk_threshold scales below the 10 GiB boundary.
    // Why it exists: small VM fixtures must not be permanently noisy because
    //   their whole pool is smaller than the large-pool cap.
    // Scenario: a 5 GiB test pool gets a 512 MiB threshold.
    #[test]
    fn enospc_risk_threshold_scales_below_10_gib() {
        assert_eq!(enospc_risk_threshold(5 * GIB), 512 * MIB);
    }

    // Intent: enospc_risk_threshold returns zero for an empty input.
    // Why it exists: callers should get a deterministic lower boundary
    //   instead of underflowing or warning on zero-sized fixture data.
    // Scenario: parser output reports no usable bytes.
    #[test]
    fn enospc_risk_threshold_zero() {
        assert_eq!(enospc_risk_threshold(0), 0);
    }

    // Intent: raid1_chunk_pair_capacity returns zero for no devices.
    // Why it exists: RAID1 allocation needs two devices, so empty input has no
    //   usable chunk-pair capacity.
    // Scenario: a caller filters away every survivor.
    #[test]
    fn raid1_chunk_pair_capacity_empty() {
        assert_eq!(raid1_chunk_pair_capacity(&[]), 0);
    }

    // Intent: raid1_chunk_pair_capacity returns zero for one device.
    // Why it exists: single-device headroom cannot form a RAID1 chunk pair.
    // Scenario: a post-loss survivor set has only one member.
    #[test]
    fn raid1_chunk_pair_capacity_single_device() {
        assert_eq!(raid1_chunk_pair_capacity(&[5]), 0);
    }

    // Intent: raid1_chunk_pair_capacity uses half the total for balanced pairs.
    // Why it exists: equal two-device headroom should be fully pairable.
    // Scenario: two devices each have 5 bytes of unallocated space.
    #[test]
    fn raid1_chunk_pair_capacity_two_equal() {
        assert_eq!(raid1_chunk_pair_capacity(&[5, 5]), 5);
    }

    // Intent: raid1_chunk_pair_capacity bottlenecks oversized devices by the
    //   sum of all other devices.
    // Why it exists: the largest device cannot pair space with itself.
    // Scenario: one device has 10 bytes and two smaller devices have 1 byte.
    #[test]
    fn raid1_chunk_pair_capacity_bottlenecked_by_largest() {
        assert_eq!(raid1_chunk_pair_capacity(&[10, 1, 1]), 2);
    }

    // Intent: raid1_chunk_pair_capacity uses half the total when no single
    //   device dominates the survivor set.
    // Why it exists: balanced 3+ device pools can pair across the set.
    // Scenario: unallocated headroom [6, 4, 4] yields 7 bytes of RAID1 space.
    #[test]
    fn raid1_chunk_pair_capacity_balanced_three_disk() {
        assert_eq!(raid1_chunk_pair_capacity(&[6, 4, 4]), 7);
    }

    // Intent: enospc_risk_advisory stays silent for a single-device pool.
    // Why it exists: there is no RAID1 chunk-pair geometry to evaluate.
    // Scenario: an imported single-profile pool is mounted under braid.
    #[test]
    fn enospc_risk_advisory_silent_on_single_disk() {
        let devices = vec![device(Devid::new(1), 100 * GIB, 0)];
        assert!(enospc_risk_advisory(&devices, 0).is_empty());
    }

    // Intent: enospc_risk_advisory stays silent when the pool is degraded.
    // Why it exists: missing-device status is the louder operator signal.
    // Scenario: a two-device pool has one missing member.
    #[test]
    fn enospc_risk_advisory_silent_on_degraded() {
        let devices = vec![
            device(Devid::new(1), 100 * GIB, 0),
            device(Devid::new(2), 100 * GIB, 0),
        ];
        assert!(enospc_risk_advisory(&devices, 1).is_empty());
    }

    // Intent: enospc_risk_advisory uses the scaled threshold on tiny pools.
    // Why it exists: a hard-coded 1 GiB threshold would warn forever on small
    //   VM fixtures that are otherwise healthy.
    // Scenario: two 256 MiB disks each have about 200 MiB unallocated.
    #[test]
    fn enospc_risk_advisory_silent_on_healthy_tiny_raid1() {
        let devices = vec![
            device(Devid::new(1), 256 * MIB, 200 * MIB),
            device(Devid::new(2), 256 * MIB, 200 * MIB),
        ];
        assert!(enospc_risk_advisory(&devices, 0).is_empty());
    }

    // Intent: enospc_risk_advisory stays silent on a roomy large RAID1 pool.
    // Why it exists: the advisory should not add noise to ordinary healthy
    //   multi-disk NAS pools.
    // Scenario: three 12 TiB disks each have 5 TiB unallocated.
    #[test]
    fn enospc_risk_advisory_silent_on_healthy_large_raid1() {
        let devices = vec![
            device(Devid::new(1), 12 * TIB, 5 * TIB),
            device(Devid::new(2), 12 * TIB, 5 * TIB),
            device(Devid::new(3), 12 * TIB, 5 * TIB),
        ];
        assert!(enospc_risk_advisory(&devices, 0).is_empty());
    }

    // Intent: enospc_risk_advisory warns on a two-device pool when either
    //   member drops below the current threshold.
    // Why it exists: current RAID1 allocation needs chunk-pair space on both
    //   devices before any additional disk loss is considered.
    // Scenario: one 100 GiB disk has only 10 MiB unallocated.
    #[test]
    fn enospc_risk_advisory_fires_on_2_disk_pool_with_one_low() {
        let devices = vec![
            device(Devid::new(1), 100 * GIB, 10 * MIB),
            device(Devid::new(2), 100 * GIB, 10 * GIB),
        ];
        let advisories = enospc_risk_advisory(&devices, 0);

        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].starts_with("ENOSPC risk:"));
        assert!(advisories[0].contains("1 of 2 devices"));
        assert!(advisories[0].contains("1.00 GiB"));
        assert_data_only_recovery_advice(&advisories[0]);
    }

    // Intent: enospc_risk_advisory simulates every single-disk loss on 3+
    //   device pools and renders a non-zero count when the advisory fires.
    // Why it exists: this pins the count_below >= 1 invariant across a
    //   pre-loss 1 GiB threshold vs post-loss ~819.20 MiB survivor threshold
    //   gap, so a firing advisory cannot regress to "0 of N".
    // Scenario: three 4 GiB disks have unallocated [3 GiB, 3 GiB, 700 MiB].
    #[test]
    fn enospc_risk_advisory_fires_on_3_disk_loss_simulation() {
        let devices = vec![
            device(Devid::new(1), 4 * GIB, 3 * GIB),
            device(Devid::new(2), 4 * GIB, 3 * GIB),
            device(Devid::new(3), 4 * GIB, 700 * MIB),
        ];
        let advisories = enospc_risk_advisory(&devices, 0);

        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].starts_with("ENOSPC risk:"));
        assert!(advisories[0].contains("1 of 3 devices"));
        assert_data_only_recovery_advice(&advisories[0]);
    }

    // Intent: enospc_risk_advisory tolerates one low device when every
    //   single-disk-loss survivor set still has enough RAID1 capacity.
    // Why it exists: the predicate should be fault-tolerance aware, not a
    //   simple count of devices below threshold.
    // Scenario: four 100 GiB disks have unallocated [10 GiB, 10 GiB, 10 GiB, 50 MiB].
    #[test]
    fn enospc_risk_advisory_silent_on_4_disk_with_one_low() {
        let devices = vec![
            device(Devid::new(1), 100 * GIB, 10 * GIB),
            device(Devid::new(2), 100 * GIB, 10 * GIB),
            device(Devid::new(3), 100 * GIB, 10 * GIB),
            device(Devid::new(4), 100 * GIB, 50 * MIB),
        ];
        assert!(enospc_risk_advisory(&devices, 0).is_empty());
    }

    // Intent: enospc_risk_advisory compares survivor sets with their own
    //   post-loss threshold.
    // Why it exists: using the larger pre-loss threshold would false-positive
    //   on small pools whose threshold shrinks after one disk is gone.
    // Scenario: three 4 GiB disks have unallocated [3 GiB, 3 GiB, 900 MiB].
    #[test]
    fn enospc_risk_advisory_uses_survivor_threshold_not_pre_loss() {
        let devices = vec![
            device(Devid::new(1), 4 * GIB, 3 * GIB),
            device(Devid::new(2), 4 * GIB, 3 * GIB),
            device(Devid::new(3), 4 * GIB, 900 * MIB),
        ];
        assert!(enospc_risk_advisory(&devices, 0).is_empty());
    }

    // Intent: evaluate_enospc_risk returns a healthy sentinel margin for a
    //   single-device pool, so monitor/status/doctor all treat it as not-at-risk.
    // Why it exists: the typed predicate must reproduce the legacy guard that
    //   short-circuits before any chunk-pair math on a degenerate pool.
    // Scenario: an imported single-profile pool with one device.
    #[test]
    fn evaluate_enospc_risk_single_disk_not_applicable() {
        let devices = vec![device(Devid::new(1), 100 * GIB, 0)];
        let assessment = evaluate_enospc_risk(&devices, 0);
        assert!(!assessment.at_risk(), "single-disk pool is never at risk");
        assert_eq!(assessment.margin, i64::MAX, "healthy sentinel margin");
        assert_eq!(assessment.device_count, 1);
        assert_eq!(assessment.count_below, 0);
    }

    // Intent: evaluate_enospc_risk returns the healthy sentinel margin for a
    //   degraded pool regardless of how tight unallocated space is.
    // Why it exists: missing-device status is the louder operator signal, and
    //   the monitor relies on this sentinel staying not-at-risk so it can skip
    //   ENOSPC on degraded pools without inventing a separate guard.
    // Scenario: a two-device pool with one missing member and zero unallocated.
    #[test]
    fn evaluate_enospc_risk_degraded_not_applicable() {
        let devices = vec![
            device(Devid::new(1), 100 * GIB, 0),
            device(Devid::new(2), 100 * GIB, 0),
        ];
        let assessment = evaluate_enospc_risk(&devices, 1);
        assert!(
            !assessment.at_risk(),
            "degraded pool defers to MissingDevice"
        );
        assert_eq!(assessment.margin, i64::MAX);
    }

    // Intent: evaluate_enospc_risk reports a large positive margin for a roomy
    //   multi-TiB RAID1 pool.
    // Why it exists: the signed margin must stay well clear of zero on healthy
    //   pools so the monitor never spuriously fires or churns its baseline.
    // Scenario: three 12 TiB disks each with 5 TiB unallocated.
    #[test]
    fn evaluate_enospc_risk_healthy_large_positive_margin() {
        let devices = vec![
            device(Devid::new(1), 12 * TIB, 5 * TIB),
            device(Devid::new(2), 12 * TIB, 5 * TIB),
            device(Devid::new(3), 12 * TIB, 5 * TIB),
        ];
        let assessment = evaluate_enospc_risk(&devices, 0);
        assert!(!assessment.at_risk());
        assert!(
            assessment.margin > GIB as i64,
            "healthy large pool margin must be a comfortable surplus, got {}",
            assessment.margin
        );
    }

    // Intent: evaluate_enospc_risk reports a negative margin whose magnitude is
    //   the deepest device deficit on an at-risk 2-disk pool.
    // Why it exists: margin < 0 must be exactly the legacy at_risk predicate, and
    //   the magnitude is what monitor baselines on -- both pinned here.
    // Scenario: one 100 GiB disk has only 10 MiB unallocated, its peer 10 GiB.
    #[test]
    fn evaluate_enospc_risk_2disk_one_low_negative_margin() {
        let devices = vec![
            device(Devid::new(1), 100 * GIB, 10 * MIB),
            device(Devid::new(2), 100 * GIB, 10 * GIB),
        ];
        let assessment = evaluate_enospc_risk(&devices, 0);
        assert!(assessment.at_risk(), "a device below threshold is at risk");
        // threshold = min(1 GiB, 200 GiB / 10) = 1 GiB; deepest deficit is the
        // 10 MiB device: 10 MiB - 1 GiB.
        assert_eq!(assessment.margin, 10 * MIB as i64 - GIB as i64);
        assert_eq!(assessment.count_below, 1);
        assert_eq!(assessment.device_count, 2);
    }

    // Intent: evaluate_enospc_risk drives a negative margin from the single-disk
    //   loss simulation on a 3-disk pool.
    // Why it exists: pins the 3+-device branch's per-loss minimum, so a firing
    //   monitor alert carries the right magnitude and count.
    // Scenario: three 4 GiB disks have unallocated [3 GiB, 3 GiB, 700 MiB].
    #[test]
    fn evaluate_enospc_risk_3disk_loss_sim_negative_margin() {
        let devices = vec![
            device(Devid::new(1), 4 * GIB, 3 * GIB),
            device(Devid::new(2), 4 * GIB, 3 * GIB),
            device(Devid::new(3), 4 * GIB, 700 * MIB),
        ];
        let assessment = evaluate_enospc_risk(&devices, 0);
        assert!(assessment.at_risk());
        assert!(
            assessment.margin < 0,
            "loss simulation must deficit, got {}",
            assessment.margin
        );
        assert_eq!(assessment.count_below, 1);
        assert_eq!(assessment.device_count, 3);
    }

    // Intent: evaluate_enospc_risk uses each survivor set's own post-loss
    //   threshold, so a pool that is safe after any single loss reports a
    //   positive margin.
    // Why it exists: using the larger pre-loss threshold would flip this to a
    //   negative margin and false-fire the monitor on small pools.
    // Scenario: three 4 GiB disks have unallocated [3 GiB, 3 GiB, 900 MiB].
    #[test]
    fn evaluate_enospc_risk_survivor_threshold_positive_margin() {
        let devices = vec![
            device(Devid::new(1), 4 * GIB, 3 * GIB),
            device(Devid::new(2), 4 * GIB, 3 * GIB),
            device(Devid::new(3), 4 * GIB, 900 * MIB),
        ];
        let assessment = evaluate_enospc_risk(&devices, 0);
        assert!(!assessment.at_risk(), "safe after any single loss");
        assert!(assessment.margin > 0);
    }

    // Intent: a worse pool produces a strictly more-negative margin than a
    //   less-bad one (monotonicity of the risk magnitude).
    // Why it exists: the monitor's "materially worse" re-alert and re-arm logic
    //   depend on margin decreasing as the pool fills, not just on the sign.
    // Scenario: two 2-disk pools differing only in the low device's headroom
    //   (100 MiB vs 10 MiB).
    #[test]
    fn evaluate_enospc_risk_margin_is_monotonic_in_severity() {
        let less_bad = vec![
            device(Devid::new(1), 100 * GIB, 100 * MIB),
            device(Devid::new(2), 100 * GIB, 10 * GIB),
        ];
        let worse = vec![
            device(Devid::new(1), 100 * GIB, 10 * MIB),
            device(Devid::new(2), 100 * GIB, 10 * GIB),
        ];
        let less_bad_margin = evaluate_enospc_risk(&less_bad, 0).margin;
        let worse_margin = evaluate_enospc_risk(&worse, 0).margin;
        assert!(less_bad_margin < 0 && worse_margin < 0, "both at risk");
        assert!(
            worse_margin < less_bad_margin,
            "worse pool ({worse_margin}) must be more negative than less-bad ({less_bad_margin})"
        );
    }

    // Intent: a predicate-healthy 4-disk pool with one near-empty device returns
    //   a margin large enough to re-arm a stored monitor baseline.
    // Why it exists (F2): keying re-arm off raw min-headroom rather than the
    //   predicate margin would leave a healthy pool with one low device stuck
    //   below the re-arm gate, never clearing its baseline. The matching silent
    //   advisory (enospc_risk_advisory_silent_on_4_disk_with_one_low) proves it
    //   is not at risk; this proves the margin clears ENOSPC_REARM_MARGIN.
    // Scenario: four 100 GiB disks have unallocated [10 GiB, 10 GiB, 10 GiB, 50 MiB].
    #[test]
    fn evaluate_enospc_risk_4disk_one_low_margin_clears_rearm() {
        let devices = vec![
            device(Devid::new(1), 100 * GIB, 10 * GIB),
            device(Devid::new(2), 100 * GIB, 10 * GIB),
            device(Devid::new(3), 100 * GIB, 10 * GIB),
            device(Devid::new(4), 100 * GIB, 50 * MIB),
        ];
        let assessment = evaluate_enospc_risk(&devices, 0);
        assert!(!assessment.at_risk(), "fault-tolerant pool is healthy");
        assert!(
            assessment.margin >= ENOSPC_REARM_MARGIN as i64,
            "predicate-healthy pool must re-arm; margin {} < REARM {}",
            assessment.margin,
            ENOSPC_REARM_MARGIN
        );
    }

    // Intent: the ENOSPC hysteresis constants stay pinned -- re-arm at one btrfs
    //   data chunk of surplus, re-fire at half a chunk of additional deficit.
    // Why it exists: the monitor's re-alert step and re-arm gate are tuned to
    //   these exact values. The worsen step MUST stay below the ~1 GiB threshold:
    //   an at-risk margin is bounded in [-threshold, 0), so a full-chunk step
    //   would make the re-fire branch unreachable. A silent bump back to 1 GiB
    //   would quietly kill re-fire; this test fails loudly on that.
    // Scenario: a refactor edits the chunk-size assumption.
    #[test]
    fn enospc_hysteresis_constants_pinned() {
        assert_eq!(
            ENOSPC_REARM_MARGIN,
            1 << 30,
            "re-arm at one data chunk of surplus"
        );
        assert_eq!(
            ENOSPC_WORSEN_STEP,
            (1 << 30) / 2,
            "half a chunk -- below threshold so re-fire stays reachable"
        );
        assert!(
            (ENOSPC_WORSEN_STEP as i64) < ENOSPC_REARM_MARGIN as i64,
            "worsen step must stay below the threshold for re-fire to be reachable"
        );
    }
}
