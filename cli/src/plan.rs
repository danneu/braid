use crate::config::Config;
use crate::types::*;

// ---------------------------------------------------------------------------
// Plan ID generation
// ---------------------------------------------------------------------------

/// Generate a plan ID: UUID v7 (timestamp-ordered, globally unique).
pub fn generate_plan_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

// ---------------------------------------------------------------------------
// compute_plan — pure logic, no I/O
// ---------------------------------------------------------------------------

pub fn compute_plan(
    config: &Config,
    config_disks: &[ConfigDisk],
    pool: &PoolState,
    flags: &PlanFlags,
) -> PlanOutcome {
    let mut actions: Vec<Action> = Vec::new();
    let mut warnings: Vec<Warning> = Vec::new();
    let mut blocked_reasons: Vec<BlockedReason> = Vec::new();
    let mut confirmations: Vec<Confirmation> = Vec::new();
    let mut counter: u32 = 1;

    // Collect known config-disk UUIDs for matching against pool.
    let config_uuids: Vec<Option<&LuksUuid>> = config_disks
        .iter()
        .map(|cd| match &cd.state {
            ConfigDiskState::PresentLuks { uuid, .. } => Some(uuid),
            _ => None,
        })
        .collect();

    let has_absent = config_disks
        .iter()
        .any(|cd| matches!(cd.state, ConfigDiskState::Absent));

    // -------------------------------------------------------------------
    // Step 1: Classify config disks → disks to add
    // -------------------------------------------------------------------
    let mut disks_to_add_count: usize = 0;

    for cd in config_disks {
        match &cd.state {
            ConfigDiskState::Absent => {
                warnings.push(Warning {
                    code: WarningCode::DiskAbsentSkipped,
                    message: format!("{} is absent, skipping", cd.by_id_path),
                });
            }
            ConfigDiskState::PresentNotLuks => {
                warnings.push(Warning {
                    code: WarningCode::InitRequired,
                    message: format!(
                        "{} is not LUKS-formatted.\n    Run: braid init-disk {}",
                        cd.by_id_path, cd.by_id_path
                    ),
                });
            }
            ConfigDiskState::PresentLuks { uuid, mapper_open } => {
                let in_pool = pool.devices.iter().any(|pd| pd.luks_uuid == *uuid);
                if in_pool {
                    continue;
                }

                let mn = match mapper_name_for_by_id(&cd.by_id_path) {
                    Some(name) => name,
                    None => {
                        blocked_reasons.push(BlockedReason {
                            code: BlockedReasonCode::InvalidByIdPath,
                            disk: Some(cd.by_id_path.0.clone()),
                            message: format!(
                                "{} has no valid basename for mapper name",
                                cd.by_id_path
                            ),
                        });
                        continue;
                    }
                };

                disks_to_add_count += 1;

                let mut open_id = None;
                if !mapper_open {
                    let act = make_action(
                        &mut counter,
                        ActionType::OpenLuks,
                        cd.by_id_path.0.clone(),
                        vec![],
                    );
                    open_id = Some(act.id.clone());
                    actions.push(act);
                }

                let preconditions = open_id.into_iter().collect();
                actions.push(make_action(
                    &mut counter,
                    ActionType::AddDiskBtrfsAdd,
                    mapper_path(&mn.0),
                    preconditions,
                ));
            }
        }
    }

    // -------------------------------------------------------------------
    // Step 2: Identify disks to remove (pool devices not in config)
    // -------------------------------------------------------------------
    let mut disks_to_remove_count: usize = 0;
    let mut first_remove_id: Option<String> = None;

    if pool.mounted {
        for pd in &pool.devices {
            let in_config = config_uuids
                .iter()
                .any(|u| u.is_some_and(|cu| *cu == pd.luks_uuid));
            if !in_config {
                disks_to_remove_count += 1;
                let mp = mapper_path(&pd.mapper.0);

                let remove_act = make_action(
                    &mut counter,
                    ActionType::RemoveDiskGraceful,
                    mp.clone(),
                    vec![],
                );
                if first_remove_id.is_none() {
                    first_remove_id = Some(remove_act.id.clone());
                }
                let remove_id = remove_act.id.clone();
                actions.push(remove_act);

                actions.push(make_action(
                    &mut counter,
                    ActionType::CloseLuksMapper,
                    mp,
                    vec![remove_id],
                ));
            }
        }
    }

    // -------------------------------------------------------------------
    // Step 3: Identity ambiguity check
    // -------------------------------------------------------------------
    if disks_to_remove_count > 0 && has_absent {
        if flags.allow_remove_ambiguous {
            if let Some(ref rid) = first_remove_id {
                confirmations.push(Confirmation {
                    action_id: rid.clone(),
                    phrase: "remove despite ambiguous identity".to_owned(),
                });
            }
        } else {
            for cd in config_disks {
                if matches!(cd.state, ConfigDiskState::Absent) {
                    blocked_reasons.push(BlockedReason {
                        code: BlockedReasonCode::IdentityAmbiguousAbsentDisk,
                        disk: Some(cd.by_id_path.0.clone()),
                        message: format!(
                            "{} is absent — cannot verify identity of pool devices marked for removal",
                            cd.by_id_path
                        ),
                    });
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // Step 4: Missing device handling
    // -------------------------------------------------------------------
    let mut explicit_missing_removal = false;

    if pool.mounted && pool.missing_count > 0 {
        warnings.push(Warning {
            code: WarningCode::PoolDegradedMissingDevices,
            message: format!(
                "pool is degraded: {} missing device(s). To evict, run: braid apply --allow-remove-missing",
                pool.missing_count
            ),
        });

        if flags.allow_remove_missing {
            if pool.missing_count > 1 {
                blocked_reasons.push(BlockedReason {
                    code: BlockedReasonCode::AmbiguousMissing,
                    disk: None,
                    message: format!(
                        "{} devices are missing — cannot determine which to remove",
                        pool.missing_count
                    ),
                });
            } else {
                explicit_missing_removal = true;
                let act = make_action(
                    &mut counter,
                    ActionType::RemoveDiskMissingExplicit,
                    config.mount_point.clone(),
                    vec![],
                );
                confirmations.push(Confirmation {
                    action_id: act.id.clone(),
                    phrase: "remove missing device from pool".to_owned(),
                });
                actions.push(act);
            }
        }
    }

    // -------------------------------------------------------------------
    // Step 5: BALANCE_TO_RAID1
    // -------------------------------------------------------------------
    let future_size = pool.devices.len() as i64 + disks_to_add_count as i64
        - disks_to_remove_count as i64
        - if explicit_missing_removal { 1 } else { 0 };

    if disks_to_add_count > 0 && future_size >= 2 {
        let preconds: Vec<String> = actions.iter().map(|a| a.id.clone()).collect();
        actions.push(make_action(
            &mut counter,
            ActionType::BalanceToRaid1,
            config.mount_point.clone(),
            preconds,
        ));
    }

    // -------------------------------------------------------------------
    // Step 6: Redundancy confirmation
    // -------------------------------------------------------------------
    if disks_to_remove_count > 0 && future_size < 2 {
        if let Some(ref rid) = first_remove_id {
            confirmations.push(Confirmation {
                action_id: rid.clone(),
                phrase: "remove this disk without redundancy".to_owned(),
            });
        }
    }

    // -------------------------------------------------------------------
    // Step 7+8: Verify actions + assemble outcome
    // -------------------------------------------------------------------
    if !blocked_reasons.is_empty() {
        PlanOutcome::Blocked {
            plan_id: generate_plan_id(),
            warnings,
            blocked_reasons,
        }
    } else {
        if !actions.is_empty() {
            let all_ids: Vec<String> = actions.iter().map(|a| a.id.clone()).collect();
            let verify_health = make_action(
                &mut counter,
                ActionType::VerifyPoolHealth,
                config.mount_point.clone(),
                all_ids,
            );
            let verify_health_id = verify_health.id.clone();
            actions.push(verify_health);

            actions.push(make_action(
                &mut counter,
                ActionType::VerifyExpectedDiskSet,
                config.mount_point.clone(),
                vec![verify_health_id],
            ));
        }

        compute_commands(&mut actions, &config.mount_point, pool);

        PlanOutcome::Applicable {
            plan_id: generate_plan_id(),
            actions,
            warnings,
            confirmations,
        }
    }
}

// ---------------------------------------------------------------------------
// to_plan_report — convert PlanOutcome to JSON-serializable PlanReport
// ---------------------------------------------------------------------------

pub fn to_plan_report(outcome: &PlanOutcome, config: &Config) -> PlanReport {
    let (plan_id, actions, warnings, blocked_reasons, confirmations, status) = match outcome {
        PlanOutcome::Applicable {
            plan_id,
            actions,
            warnings,
            confirmations,
        } => (
            plan_id.clone(),
            actions.clone(),
            warnings.clone(),
            Vec::<BlockedReason>::new(),
            confirmations.clone(),
            PlanStatus::Applicable,
        ),
        PlanOutcome::Blocked {
            plan_id,
            warnings,
            blocked_reasons,
        } => (
            plan_id.clone(),
            Vec::<Action>::new(),
            warnings.clone(),
            blocked_reasons.clone(),
            Vec::<Confirmation>::new(),
            PlanStatus::Blocked,
        ),
    };

    let actions_verify = actions
        .iter()
        .filter(|a| {
            matches!(
                a.action_type,
                ActionType::VerifyPoolHealth | ActionType::VerifyExpectedDiskSet
            )
        })
        .count();
    let actions_mutation = actions.len() - actions_verify;
    let skipped_total = warnings
        .iter()
        .filter(|w| {
            matches!(
                w.code,
                WarningCode::DiskAbsentSkipped | WarningCode::InitRequired
            )
        })
        .count();

    PlanReport {
        schema_version: 1,
        plan_id,
        mount_point: config.mount_point.clone(),
        status,
        warning_count: warnings.len(),
        blocked_reasons: blocked_reasons.clone(),
        confirmations,
        summary: PlanSummary {
            actions_total: actions.len(),
            actions_mutation,
            actions_verify,
            warnings_total: warnings.len(),
            blocked_total: blocked_reasons.len(),
            skipped_total,
        },
        warnings,
        actions,
    }
}

// ---------------------------------------------------------------------------
// format_plan_human — human-readable plan output
// ---------------------------------------------------------------------------

pub fn format_plan_human(report: &PlanReport) -> String {
    let mut out = String::new();

    out.push_str(&format!("Plan ID: {}\n", report.plan_id));
    out.push_str(&format!("Mount:   {}\n", report.mount_point));

    let status_str = match report.status {
        PlanStatus::Applicable => "applicable",
        PlanStatus::Blocked => "blocked",
    };
    out.push_str(&format!("Status:  {}\n", status_str));
    out.push_str(&format!("Actions: {}\n", report.summary.actions_mutation));

    if !report.blocked_reasons.is_empty() {
        out.push('\n');
        out.push_str("BLOCKED — apply cannot proceed:\n");
        for br in &report.blocked_reasons {
            out.push_str(&format!("  - {}\n", br.message));
        }
    }

    if !report.actions.is_empty() {
        out.push('\n');
        for (i, action) in report.actions.iter().enumerate() {
            out.push_str(&format!(
                "[{}] {:<30} target={}\n",
                i + 1,
                action_type_str(&action.action_type),
                action.target
            ));
            for cmd in &action.commands {
                let suffix = if matches!(cmd.certainty, RunCertainty::MayRun) {
                    "  (may run)"
                } else {
                    ""
                };
                out.push_str(&format!("    $ {}{}\n", cmd.command, suffix));
            }
        }
    }

    out.push('\n');
    if report.warnings.is_empty() {
        out.push_str("Warnings: none\n");
    } else {
        out.push_str("Warnings:\n");
        for w in &report.warnings {
            out.push_str(&format!("  - {}\n", w.message));
        }
    }

    if !report.confirmations.is_empty() {
        out.push('\n');
        out.push_str(&format!(
            "Confirmations required: {}\n",
            report.confirmations.len()
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn compute_commands(actions: &mut [Action], mount_point: &str, pool: &PoolState) {
    let is_bootstrap = !pool.mounted && pool.total_devices == 0;
    let mut device_count = pool.devices.len();
    let mut first_add_seen = false;

    for action in actions.iter_mut() {
        match action.action_type {
            ActionType::OpenLuks => {
                let mapper = action
                    .target
                    .strip_prefix(BY_ID_PREFIX)
                    .unwrap_or(&action.target);
                action.commands = vec![PlannedCommand {
                    command: format!(
                        "cryptsetup luksOpen --key-file=- {} {}",
                        action.target, mapper
                    ),
                    certainty: RunCertainty::WillRun,
                }];
            }
            ActionType::AddDiskBtrfsAdd => {
                if is_bootstrap && !first_add_seen {
                    action.commands = vec![PlannedCommand {
                        command: format!("mkfs.btrfs -f {}", action.target),
                        certainty: RunCertainty::MayRun,
                    }];
                    first_add_seen = true;
                } else {
                    action.commands = vec![PlannedCommand {
                        command: format!(
                            "btrfs device add -f {} {}",
                            action.target, mount_point
                        ),
                        certainty: RunCertainty::WillRun,
                    }];
                }
                device_count += 1;
            }
            ActionType::BalanceToRaid1 => {
                action.commands = vec![PlannedCommand {
                    command: format!(
                        "btrfs balance start -dconvert=raid1 -mconvert=raid1 {}",
                        mount_point
                    ),
                    certainty: RunCertainty::WillRun,
                }];
            }
            ActionType::RemoveDiskGraceful => {
                let mut cmds = Vec::new();
                if device_count <= 2 {
                    cmds.push(PlannedCommand {
                        command: format!(
                            "btrfs balance start -dconvert=single -mconvert=single -f {}",
                            mount_point
                        ),
                        certainty: RunCertainty::WillRun,
                    });
                }
                cmds.push(PlannedCommand {
                    command: format!(
                        "btrfs device remove {} {}",
                        action.target, mount_point
                    ),
                    certainty: RunCertainty::WillRun,
                });
                action.commands = cmds;
                device_count -= 1;
            }
            ActionType::RemoveDiskMissingExplicit => {
                let mut cmds = Vec::new();
                if device_count <= 1 {
                    cmds.push(PlannedCommand {
                        command: format!(
                            "btrfs balance start -dconvert=single -mconvert=single -f {}",
                            mount_point
                        ),
                        certainty: RunCertainty::WillRun,
                    });
                }
                cmds.push(PlannedCommand {
                    command: format!("btrfs device remove missing {}", mount_point),
                    certainty: RunCertainty::WillRun,
                });
                action.commands = cmds;
                device_count -= 1;
            }
            ActionType::CloseLuksMapper => {
                let mapper = action
                    .target
                    .strip_prefix("/dev/mapper/")
                    .unwrap_or(&action.target);
                action.commands = vec![PlannedCommand {
                    command: format!("cryptsetup close {}", mapper),
                    certainty: RunCertainty::WillRun,
                }];
            }
            ActionType::VerifyPoolHealth | ActionType::VerifyExpectedDiskSet => {
                // Non-mutation actions — no commands.
            }
        }
    }
}

fn make_action(
    counter: &mut u32,
    action_type: ActionType,
    target: String,
    preconditions: Vec<String>,
) -> Action {
    let id = format!("a{}", counter);
    *counter += 1;
    Action {
        id,
        action_type,
        target,
        preconditions,
        state: ActionState::Pending,
        commands: vec![],
    }
}

fn mapper_path(name: &str) -> String {
    format!("/dev/mapper/{}", name)
}

pub(crate) const BY_ID_PREFIX: &str = "/dev/disk/by-id/";

pub(crate) fn mapper_name_for_by_id(path: &ByIdPath) -> Option<MapperName> {
    let basename = path.0.strip_prefix(BY_ID_PREFIX)?;

    if !is_valid_mapper_basename(basename) {
        return None;
    }

    Some(MapperName(basename.to_owned()))
}

fn is_valid_mapper_basename(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.chars().any(|c| c.is_whitespace() || c.is_ascii_control())
}

fn action_type_str(at: &ActionType) -> &'static str {
    match at {
        ActionType::OpenLuks => "OPEN_LUKS",
        ActionType::AddDiskBtrfsAdd => "ADD_DISK_BTRFS_ADD",
        ActionType::BalanceToRaid1 => "BALANCE_TO_RAID1",
        ActionType::RemoveDiskGraceful => "REMOVE_DISK_GRACEFUL",
        ActionType::RemoveDiskMissingExplicit => "REMOVE_DISK_MISSING_EXPLICIT",
        ActionType::CloseLuksMapper => "CLOSE_LUKS_MAPPER",
        ActionType::VerifyPoolHealth => "VERIFY_POOL_HEALTH",
        ActionType::VerifyExpectedDiskSet => "VERIFY_EXPECTED_DISK_SET",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use pretty_assertions::assert_eq;

    // -- Test helpers -------------------------------------------------------

    fn test_config() -> Config {
        Config {
            disks: vec![ByIdPath("/dev/disk/by-id/disk-1".into())],
            mount_point: "/mnt/storage".into(),
        }
    }

    fn pool_2disk() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName("disk-1".into()),
                    luks_uuid: LuksUuid("uuid-1".into()),
                    devid: 1,
                },
                PoolDevice {
                    mapper: MapperName("disk-2".into()),
                    luks_uuid: LuksUuid("uuid-2".into()),
                    devid: 2,
                },
            ],
            missing_count: 0,
            total_devices: 2,
        }
    }

    fn pool_3disk() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName("disk-1".into()),
                    luks_uuid: LuksUuid("uuid-1".into()),
                    devid: 1,
                },
                PoolDevice {
                    mapper: MapperName("disk-2".into()),
                    luks_uuid: LuksUuid("uuid-2".into()),
                    devid: 2,
                },
                PoolDevice {
                    mapper: MapperName("disk-3".into()),
                    luks_uuid: LuksUuid("uuid-3".into()),
                    devid: 3,
                },
            ],
            missing_count: 0,
            total_devices: 3,
        }
    }

    fn pool_unmounted() -> PoolState {
        PoolState {
            mounted: false,
            devices: vec![],
            missing_count: 0,
            total_devices: 0,
        }
    }

    fn config_disk_present(path: &str, uuid: &str) -> ConfigDisk {
        ConfigDisk {
            by_id_path: ByIdPath(path.into()),
            state: ConfigDiskState::PresentLuks {
                uuid: LuksUuid(uuid.into()),
                mapper_open: false,
            },
        }
    }

    fn config_disk_present_open(path: &str, uuid: &str) -> ConfigDisk {
        ConfigDisk {
            by_id_path: ByIdPath(path.into()),
            state: ConfigDiskState::PresentLuks {
                uuid: LuksUuid(uuid.into()),
                mapper_open: true,
            },
        }
    }

    fn config_disk_absent(path: &str) -> ConfigDisk {
        ConfigDisk {
            by_id_path: ByIdPath(path.into()),
            state: ConfigDiskState::Absent,
        }
    }

    fn config_disk_not_luks(path: &str) -> ConfigDisk {
        ConfigDisk {
            by_id_path: ByIdPath(path.into()),
            state: ConfigDiskState::PresentNotLuks,
        }
    }

    fn action_types(outcome: &PlanOutcome) -> Vec<&ActionType> {
        match outcome {
            PlanOutcome::Applicable { actions, .. } => {
                actions.iter().map(|a| &a.action_type).collect()
            }
            PlanOutcome::Blocked { .. } => vec![],
        }
    }

    fn warning_codes(outcome: &PlanOutcome) -> Vec<WarningCode> {
        match outcome {
            PlanOutcome::Applicable { warnings, .. }
            | PlanOutcome::Blocked { warnings, .. } => {
                warnings.iter().map(|w| w.code).collect()
            }
        }
    }

    fn blocked_codes(outcome: &PlanOutcome) -> Vec<BlockedReasonCode> {
        match outcome {
            PlanOutcome::Blocked {
                blocked_reasons, ..
            } => blocked_reasons.iter().map(|b| b.code).collect(),
            _ => vec![],
        }
    }

    fn action_targets(outcome: &PlanOutcome) -> Vec<(&ActionType, &str)> {
        match outcome {
            PlanOutcome::Applicable { actions, .. } => actions
                .iter()
                .map(|a| (&a.action_type, a.target.as_str()))
                .collect(),
            PlanOutcome::Blocked { .. } => vec![],
        }
    }

    fn confirmation_phrases(outcome: &PlanOutcome) -> Vec<&str> {
        match outcome {
            PlanOutcome::Applicable {
                confirmations, ..
            } => confirmations.iter().map(|c| c.phrase.as_str()).collect(),
            _ => vec![],
        }
    }

    // -- Tests --------------------------------------------------------------

    #[test]
    fn plan_noop() {
        let config = test_config();
        let pool = pool_2disk();
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        assert_eq!(action_types(&outcome), Vec::<&ActionType>::new());
        assert!(warning_codes(&outcome).is_empty());
        assert!(confirmation_phrases(&outcome).is_empty());
    }

    #[test]
    fn plan_add_single_disk() {
        let config = test_config();
        let pool = pool_2disk();
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
            config_disk_present("/dev/disk/by-id/disk-3", "uuid-3"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        assert_eq!(
            action_types(&outcome),
            vec![
                &ActionType::OpenLuks,
                &ActionType::AddDiskBtrfsAdd,
                &ActionType::BalanceToRaid1,
                &ActionType::VerifyPoolHealth,
                &ActionType::VerifyExpectedDiskSet,
            ]
        );
        // Add target uses by-id basename, not UUID.
        let targets = action_targets(&outcome);
        assert_eq!(targets[0], (&ActionType::OpenLuks, "/dev/disk/by-id/disk-3"));
        assert_eq!(targets[1], (&ActionType::AddDiskBtrfsAdd, "/dev/mapper/disk-3"));
    }

    #[test]
    fn plan_add_skip_open_when_mapper_open() {
        let config = test_config();
        let pool = pool_2disk();
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
            config_disk_present_open("/dev/disk/by-id/disk-3", "uuid-3"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        let types = action_types(&outcome);
        assert!(!types.contains(&&ActionType::OpenLuks));
        assert!(types.contains(&&ActionType::AddDiskBtrfsAdd));
    }

    #[test]
    fn plan_remove_single_disk() {
        let config = test_config();
        let pool = pool_3disk();
        // Config only knows about uuid-1 and uuid-2; uuid-3 is in pool but not config.
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        assert_eq!(
            action_types(&outcome),
            vec![
                &ActionType::RemoveDiskGraceful,
                &ActionType::CloseLuksMapper,
                &ActionType::VerifyPoolHealth,
                &ActionType::VerifyExpectedDiskSet,
            ]
        );
        // Remove target uses pool device's mapper name.
        let targets = action_targets(&outcome);
        assert_eq!(targets[0], (&ActionType::RemoveDiskGraceful, "/dev/mapper/disk-3"));
        assert_eq!(targets[1], (&ActionType::CloseLuksMapper, "/dev/mapper/disk-3"));
    }

    #[test]
    fn plan_replace_disk() {
        let config = test_config();
        let pool = pool_2disk();
        // uuid-1 stays, uuid-2 removed, uuid-3 added.
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present("/dev/disk/by-id/disk-3", "uuid-3"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        let types = action_types(&outcome);
        assert!(types.contains(&&ActionType::OpenLuks));
        assert!(types.contains(&&ActionType::AddDiskBtrfsAdd));
        assert!(types.contains(&&ActionType::RemoveDiskGraceful));
        assert!(types.contains(&&ActionType::CloseLuksMapper));
        assert!(types.contains(&&ActionType::BalanceToRaid1));
    }

    #[test]
    fn plan_absent_disk_skip_with_warning() {
        let config = test_config();
        let pool = pool_2disk();
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
            config_disk_absent("/dev/disk/by-id/disk-3"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        assert_eq!(action_types(&outcome), Vec::<&ActionType>::new());
        assert_eq!(warning_codes(&outcome), vec![WarningCode::DiskAbsentSkipped]);
    }

    #[test]
    fn plan_init_required_warning() {
        let config = test_config();
        let pool = pool_2disk();
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
            config_disk_not_luks("/dev/disk/by-id/disk-3"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        assert_eq!(action_types(&outcome), Vec::<&ActionType>::new());
        assert_eq!(warning_codes(&outcome), vec![WarningCode::InitRequired]);
    }

    #[test]
    fn plan_absent_blocks_removal() {
        let config = test_config();
        let pool = pool_2disk();
        // disk-1 has uuid-1 (in pool), disk-2 is absent.
        // uuid-2 in pool can't be matched to absent disk-2 → removal pending.
        // Absent disk + removal → blocked.
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_absent("/dev/disk/by-id/disk-2"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Blocked { .. }));
        assert_eq!(
            blocked_codes(&outcome),
            vec![BlockedReasonCode::IdentityAmbiguousAbsentDisk]
        );
        assert!(warning_codes(&outcome).contains(&WarningCode::DiskAbsentSkipped));
    }

    #[test]
    fn plan_absent_unblocked_with_flag() {
        let config = test_config();
        let pool = pool_3disk();
        // 3-disk pool. Config knows uuid-1, uuid-2, and disk-3 is absent.
        // uuid-3 in pool not matched → removal. Absent disk → ambiguity.
        // allow_remove_ambiguous unblocks with confirmation.
        // Future size: 3 - 1 = 2 (no redundancy issue).
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
            config_disk_absent("/dev/disk/by-id/disk-3"),
        ];
        let flags = PlanFlags {
            allow_remove_ambiguous: true,
            ..Default::default()
        };

        let outcome = compute_plan(&config, &disks, &pool, &flags);

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        assert_eq!(
            confirmation_phrases(&outcome),
            vec!["remove despite ambiguous identity"]
        );
    }

    #[test]
    fn plan_multiple_confirmations() {
        let config = test_config();
        let pool = pool_2disk();
        // disk-1 has uuid-1 (in pool), disk-2 is absent.
        // uuid-2 not matched → removal. Absent → ambiguity.
        // Future size: 2 - 1 = 1 < 2 → redundancy confirmation.
        // allow_remove_ambiguous → both confirmations.
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_absent("/dev/disk/by-id/disk-2"),
        ];
        let flags = PlanFlags {
            allow_remove_ambiguous: true,
            ..Default::default()
        };

        let outcome = compute_plan(&config, &disks, &pool, &flags);

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        let phrases = confirmation_phrases(&outcome);
        assert_eq!(phrases.len(), 2);
        assert!(phrases.contains(&"remove despite ambiguous identity"));
        assert!(phrases.contains(&"remove this disk without redundancy"));
    }

    #[test]
    fn plan_missing_device_warn_only() {
        let config = test_config();
        let pool = PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName("disk-1".into()),
                    luks_uuid: LuksUuid("uuid-1".into()),
                    devid: 1,
                },
                PoolDevice {
                    mapper: MapperName("disk-2".into()),
                    luks_uuid: LuksUuid("uuid-2".into()),
                    devid: 2,
                },
            ],
            missing_count: 1,
            total_devices: 3,
        };
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        assert_eq!(action_types(&outcome), Vec::<&ActionType>::new());
        assert_eq!(
            warning_codes(&outcome),
            vec![WarningCode::PoolDegradedMissingDevices]
        );
    }

    #[test]
    fn plan_missing_device_explicit_removal() {
        let config = test_config();
        let pool = PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName("disk-1".into()),
                    luks_uuid: LuksUuid("uuid-1".into()),
                    devid: 1,
                },
                PoolDevice {
                    mapper: MapperName("disk-2".into()),
                    luks_uuid: LuksUuid("uuid-2".into()),
                    devid: 2,
                },
            ],
            missing_count: 1,
            total_devices: 3,
        };
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
        ];
        let flags = PlanFlags {
            allow_remove_missing: true,
            ..Default::default()
        };

        let outcome = compute_plan(&config, &disks, &pool, &flags);

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        let types = action_types(&outcome);
        assert!(types.contains(&&ActionType::RemoveDiskMissingExplicit));
        assert!(types.contains(&&ActionType::VerifyPoolHealth));
        assert_eq!(
            warning_codes(&outcome),
            vec![WarningCode::PoolDegradedMissingDevices]
        );
        assert_eq!(
            confirmation_phrases(&outcome),
            vec!["remove missing device from pool"]
        );
    }

    #[test]
    fn plan_multiple_missing_blocked() {
        let config = test_config();
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("luks-uuid-1".into()),
                luks_uuid: LuksUuid("uuid-1".into()),
                devid: 1,
            }],
            missing_count: 2,
            total_devices: 3,
        };
        let disks = vec![config_disk_present_open(
            "/dev/disk/by-id/disk-1",
            "uuid-1",
        )];
        let flags = PlanFlags {
            allow_remove_missing: true,
            ..Default::default()
        };

        let outcome = compute_plan(&config, &disks, &pool, &flags);

        assert!(matches!(outcome, PlanOutcome::Blocked { .. }));
        assert_eq!(
            blocked_codes(&outcome),
            vec![BlockedReasonCode::AmbiguousMissing]
        );
        assert!(warning_codes(&outcome).contains(&WarningCode::PoolDegradedMissingDevices));
    }

    #[test]
    fn plan_redundancy_confirmation() {
        let config = test_config();
        let pool = pool_2disk();
        // Config only has disk-1. uuid-2 in pool → removal.
        // Future size: 2 - 1 = 1 < 2 → redundancy confirmation.
        let disks = vec![config_disk_present_open(
            "/dev/disk/by-id/disk-1",
            "uuid-1",
        )];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        assert!(action_types(&outcome).contains(&&ActionType::RemoveDiskGraceful));
        assert_eq!(
            confirmation_phrases(&outcome),
            vec!["remove this disk without redundancy"]
        );
    }

    #[test]
    fn plan_bootstrap_unmounted() {
        let config = test_config();
        let pool = pool_unmounted();
        let disks = vec![config_disk_present(
            "/dev/disk/by-id/disk-1",
            "uuid-1",
        )];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        let types = action_types(&outcome);
        assert!(types.contains(&&ActionType::OpenLuks));
        assert!(types.contains(&&ActionType::AddDiskBtrfsAdd));
        assert!(!types.contains(&&ActionType::BalanceToRaid1));
        assert!(types.contains(&&ActionType::VerifyPoolHealth));
    }

    #[test]
    fn plan_bootstrap_two_disks() {
        let config = test_config();
        let pool = pool_unmounted();
        let disks = vec![
            config_disk_present("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present("/dev/disk/by-id/disk-2", "uuid-2"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Applicable { .. }));
        let targets = action_targets(&outcome);
        // Two OPEN_LUKS + two ADD + BALANCE + two VERIFY.
        assert_eq!(
            targets,
            vec![
                (&ActionType::OpenLuks, "/dev/disk/by-id/disk-1"),
                (&ActionType::AddDiskBtrfsAdd, "/dev/mapper/disk-1"),
                (&ActionType::OpenLuks, "/dev/disk/by-id/disk-2"),
                (&ActionType::AddDiskBtrfsAdd, "/dev/mapper/disk-2"),
                (&ActionType::BalanceToRaid1, "/mnt/storage"),
                (&ActionType::VerifyPoolHealth, "/mnt/storage"),
                (&ActionType::VerifyExpectedDiskSet, "/mnt/storage"),
            ]
        );
    }

    #[test]
    fn plan_no_format_action_exists() {
        // ActionType enum has no format/luksFormat variant — verified at compile time.
        // This test documents the safety invariant: braid apply can never format a disk.
        let variants = [
            ActionType::OpenLuks,
            ActionType::AddDiskBtrfsAdd,
            ActionType::BalanceToRaid1,
            ActionType::RemoveDiskGraceful,
            ActionType::RemoveDiskMissingExplicit,
            ActionType::CloseLuksMapper,
            ActionType::VerifyPoolHealth,
            ActionType::VerifyExpectedDiskSet,
        ];
        for v in &variants {
            let s = action_type_str(v);
            assert!(
                !s.contains("FORMAT"),
                "ActionType must never include a format operation: {s}"
            );
        }
    }

    #[test]
    fn plan_blocked_not_convertible() {
        let config = test_config();
        let pool = pool_2disk();
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_absent("/dev/disk/by-id/disk-2"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

        assert!(matches!(outcome, PlanOutcome::Blocked { .. }));
        let result = ApplicablePlan::try_from(outcome);
        assert!(result.is_err());
    }

    #[test]
    fn plan_report_json_schema() {
        let config = test_config();
        let pool = pool_2disk();
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
            config_disk_present("/dev/disk/by-id/disk-3", "uuid-3"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());
        let report = to_plan_report(&outcome, &config);

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.mount_point, "/mnt/storage");
        assert_eq!(report.status, PlanStatus::Applicable);
        // OPEN_LUKS + ADD + BALANCE = 3 mutation, 2 verify = 5 total.
        assert_eq!(report.summary.actions_total, 5);
        assert_eq!(report.summary.actions_mutation, 3);
        assert_eq!(report.summary.actions_verify, 2);
        assert_eq!(report.summary.blocked_total, 0);
    }

    #[test]
    fn plan_report_skipped_total() {
        let config = test_config();
        let pool = pool_2disk();
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
            config_disk_absent("/dev/disk/by-id/disk-3"),
            config_disk_not_luks("/dev/disk/by-id/disk-4"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());
        let report = to_plan_report(&outcome, &config);

        assert_eq!(report.summary.skipped_total, 2);
        assert_eq!(report.summary.warnings_total, 2);
        assert_eq!(report.status, PlanStatus::Applicable);
    }

    #[test]
    fn plan_human_output_format() {
        let config = test_config();
        let pool = pool_2disk();
        let disks = vec![
            config_disk_present_open("/dev/disk/by-id/disk-1", "uuid-1"),
            config_disk_present_open("/dev/disk/by-id/disk-2", "uuid-2"),
            config_disk_absent("/dev/disk/by-id/disk-3"),
        ];

        let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());
        let report = to_plan_report(&outcome, &config);
        let human = format_plan_human(&report);

        assert!(human.contains("Plan ID:"));
        assert!(human.contains("Mount:   /mnt/storage"));
        assert!(human.contains("Status:  applicable"));
        assert!(human.contains("Actions: 0"));
        assert!(human.contains("Warnings:"));
        assert!(human.contains("is absent, skipping"));
    }

    #[test]
    fn plan_invalid_by_id_path_blocks() {
        let config = test_config();
        let pool = pool_unmounted();
        let bad_paths = &[
            "",                          // empty
            "/",                         // bare slash
            "/dev/disk/by-id/",          // prefix but empty basename
            "/dev/sda",                  // wrong prefix
            "disk-1",                    // relative, no prefix
            "/tmp/foo",                  // arbitrary absolute path
            "/dev/disk/by-id/.",         // dot
            "/dev/disk/by-id/..",        // dotdot
            "/dev/disk/by-id/a/b",       // embedded slash
            "/dev/disk/by-id//",         // empty basename via double slash
            "/dev/disk/by-id/ ",         // whitespace-only basename
            "/dev/disk/by-id/\t",        // tab-only basename
            "/dev/disk/by-id/\n",        // newline-only basename
            "/dev/disk/by-id/../disk-1", // parent traversal segment
            "/dev/disk/by-id/./disk-1",  // current-dir traversal segment
            "/DEV/disk/by-id/disk-1",    // case mismatch in prefix
            " /dev/disk/by-id/disk-1",   // leading space
            "/dev/disk/by-id/disk-1 ",   // trailing space
        ];
        for bad_path in bad_paths {
            let disks = vec![ConfigDisk {
                by_id_path: ByIdPath(bad_path.to_string()),
                state: ConfigDiskState::PresentLuks {
                    uuid: LuksUuid("uuid-bad".into()),
                    mapper_open: false,
                },
            }];

            let outcome = compute_plan(&config, &disks, &pool, &PlanFlags::default());

            assert!(
                matches!(outcome, PlanOutcome::Blocked { .. }),
                "expected Blocked for path {:?}, got {:?}",
                bad_path,
                outcome
            );
            assert_eq!(
                blocked_codes(&outcome),
                vec![BlockedReasonCode::InvalidByIdPath],
                "wrong blocked code for path {:?}",
                bad_path,
            );
        }
    }

    #[test]
    fn plan_id_is_valid_uuid_v7() {
        let id = generate_plan_id();
        let parsed = uuid::Uuid::parse_str(&id).expect("plan ID should be valid UUID");
        assert_eq!(parsed.get_version(), Some(uuid::Version::SortRand));
    }
}
