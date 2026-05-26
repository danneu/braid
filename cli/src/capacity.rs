//! Shared RAID1 capacity math for preflight checks and read-only advisories.
//!
//! Keeping this separate from command modules makes status, doctor, and
//! mutating preflights use the same btrfs chunk-pair geometry.

use crate::confirm::format_bytes;
use crate::parse::types::BtrfsDeviceUsageEntry;

const GIB: u64 = 1 << 30;

/// Kernel-aligned low-unallocated threshold for proactive ENOSPC advisories.
///
/// Mirrors btrfs's effective data chunk-size cap for non-zoned filesystems:
/// 10% of total writable device bytes, capped at 1 GiB.
pub fn enospc_risk_threshold(total_device_bytes: u64) -> u64 {
    GIB.min(total_device_bytes / 10)
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

/// Advisory for pools that are one disk-loss away from RAID1 chunk ENOSPC.
///
/// This intentionally returns a vector to match the other status advisory
/// helpers, while currently emitting at most one message.
pub fn enospc_risk_advisory(
    devices: &[BtrfsDeviceUsageEntry],
    missing_count: u64,
) -> Vec<String> {
    if missing_count > 0 || devices.len() < 2 {
        return Vec::new();
    }

    let current_total: u64 = devices.iter().map(|device| device.device_size).sum();
    let current_threshold = enospc_risk_threshold(current_total);
    // count_below intentionally uses the pre-loss threshold rendered in the
    // advisory, while the 3+ disk predicate below compares each survivor set
    // with its no-larger post-loss threshold. That cannot produce a firing
    // advisory with a "0 of N" count: survivor_threshold <= current_threshold,
    // and any survivor set whose members are all >= current_threshold has
    // chunk-pair capacity >= current_threshold, therefore >= survivor_threshold.
    // So at_risk implies at least one device is below current_threshold.
    let count_below = devices
        .iter()
        .filter(|device| device.unallocated < current_threshold)
        .count();

    let at_risk = if devices.len() == 2 {
        devices
            .iter()
            .any(|device| device.unallocated < current_threshold)
    } else {
        (0..devices.len()).any(|lost_index| {
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
            raid1_chunk_pair_capacity(&survivor_unallocated) < survivor_threshold
        })
    };

    if !at_risk {
        return Vec::new();
    }

    vec![format!(
        "ENOSPC risk: {count_below} of {} devices have less than {} unallocated -- pool may be unable to allocate new RAID1 chunks. Free up files or run 'btrfs balance start -dusage=0 -musage=0 <mount>' to reclaim empty chunks.",
        devices.len(),
        format_bytes(current_threshold),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const TIB: u64 = 1 << 40;

    fn device(devid: u64, device_size: u64, unallocated: u64) -> BtrfsDeviceUsageEntry {
        BtrfsDeviceUsageEntry {
            path: format!("/dev/mapper/braid-disk{devid}"),
            devid,
            device_size,
            device_slack: 0,
            allocations: Vec::new(),
            unallocated,
        }
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
        let devices = vec![device(1, 100 * GIB, 0)];
        assert!(enospc_risk_advisory(&devices, 0).is_empty());
    }

    // Intent: enospc_risk_advisory stays silent when the pool is degraded.
    // Why it exists: missing-device status is the louder operator signal.
    // Scenario: a two-device pool has one missing member.
    #[test]
    fn enospc_risk_advisory_silent_on_degraded() {
        let devices = vec![device(1, 100 * GIB, 0), device(2, 100 * GIB, 0)];
        assert!(enospc_risk_advisory(&devices, 1).is_empty());
    }

    // Intent: enospc_risk_advisory uses the scaled threshold on tiny pools.
    // Why it exists: a hard-coded 1 GiB threshold would warn forever on small
    //   VM fixtures that are otherwise healthy.
    // Scenario: two 256 MiB disks each have about 200 MiB unallocated.
    #[test]
    fn enospc_risk_advisory_silent_on_healthy_tiny_raid1() {
        let devices = vec![
            device(1, 256 * MIB, 200 * MIB),
            device(2, 256 * MIB, 200 * MIB),
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
            device(1, 12 * TIB, 5 * TIB),
            device(2, 12 * TIB, 5 * TIB),
            device(3, 12 * TIB, 5 * TIB),
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
            device(1, 100 * GIB, 10 * MIB),
            device(2, 100 * GIB, 10 * GIB),
        ];
        let advisories = enospc_risk_advisory(&devices, 0);

        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].starts_with("ENOSPC risk:"));
        assert!(advisories[0].contains("1 of 2 devices"));
        assert!(advisories[0].contains("1.00 GiB"));
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
            device(1, 4 * GIB, 3 * GIB),
            device(2, 4 * GIB, 3 * GIB),
            device(3, 4 * GIB, 700 * MIB),
        ];
        let advisories = enospc_risk_advisory(&devices, 0);

        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].starts_with("ENOSPC risk:"));
        assert!(advisories[0].contains("1 of 3 devices"));
    }

    // Intent: enospc_risk_advisory tolerates one low device when every
    //   single-disk-loss survivor set still has enough RAID1 capacity.
    // Why it exists: the predicate should be fault-tolerance aware, not a
    //   simple count of devices below threshold.
    // Scenario: four 100 GiB disks have unallocated [10 GiB, 10 GiB, 10 GiB, 50 MiB].
    #[test]
    fn enospc_risk_advisory_silent_on_4_disk_with_one_low() {
        let devices = vec![
            device(1, 100 * GIB, 10 * GIB),
            device(2, 100 * GIB, 10 * GIB),
            device(3, 100 * GIB, 10 * GIB),
            device(4, 100 * GIB, 50 * MIB),
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
            device(1, 4 * GIB, 3 * GIB),
            device(2, 4 * GIB, 3 * GIB),
            device(3, 4 * GIB, 900 * MIB),
        ];
        assert!(enospc_risk_advisory(&devices, 0).is_empty());
    }
}
