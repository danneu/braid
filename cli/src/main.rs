use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};
use std::path::Path;
use std::time::{Duration, Instant};

use braid_cli::cmd::RealRunner;
use braid_cli::config::{DEFAULT_CONFIG_PATH, config_read};
use braid_cli::doctor::{DoctorOptions, cmd_doctor};
use braid_cli::membership::PoolMembership;
use braid_cli::online_state::{RealOnlineStateOps, mark_offline, mark_online, snapshot};
use braid_cli::pool_lock::{
    AcquirePoolLock, PoolLockError, RealPoolLock, RealStopCoordinator, StopCoordinatorError,
    StopCoordinatorPollResult,
};
use braid_cli::probe::RealFilesystem;
use braid_cli::progress::{ProgressMode, resolve_progress_output};
use braid_cli::state_paths::StatePaths;

#[derive(Debug, Parser)]
#[command(name = "braid", version)]
#[command(about = "braid -- encrypted NAS storage", long_about = None)]
#[command(disable_help_subcommand(true))]
struct Cli {
    #[arg(long, global = true, default_value = DEFAULT_CONFIG_PATH)]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Add disks to the pool
    Add(AddArgs),
    /// Remove a disk from the pool
    Remove(RemoveArgs),
    /// Clean up a missing/dead device entry from the pool (does not rebuild data -- use `replace` for that)
    RemoveMissing(RemoveMissingArgs),
    /// Replace a disk with a new one
    Replace(ReplaceArgs),
    /// Show pool health and disk info
    Status(StatusArgs),
    /// Check configuration for problems
    Doctor(DoctorArgs),
    /// Unlock LUKS volumes and mount the pool
    Unlock(UnlockArgs),
    /// Lock the pool: unmount and close LUKS volumes
    Lock(LockArgs),
    /// Enroll a binary keyfile into LUKS slot 1 on all pool disks
    #[command(name = "enroll")]
    EnrollKeyFile(EnrollKeyFileArgs),
    /// Check if pool is idle (no scrub or btrfs exclusive operation): exit 0 = idle, exit 1 = busy or probe failure, exit 2 = setup error
    Idle,
    /// Internal: invoked by `braid-scrub.service` ExecStop during lock/shutdown
    /// to cancel an in-flight scrub. Calls `btrfs scrub cancel` directly; the
    /// cancel ioctl is the kernel-authoritative test for whether a scrub is
    /// running. Exit 0 means a scrub was running and is now cancelled; exit 2
    /// means ENOTCONN/no scrub was running and is benign. Real failures
    /// propagate. Hidden from `braid --help`.
    #[command(hide = true)]
    ScrubCancel(ScrubCancelArgs),
    /// Internal: invoked by `braid-scrub-resume-trigger.service` to decide
    /// whether pool-online activation should start the shared scrub service.
    #[command(hide = true)]
    ScrubNeedsResume(ScrubMountArgs),
    /// Internal: invoked by `braid-scrub.service` for timer/manual scrubs to
    /// resume saved work or start a fresh scrub when nothing is resumable.
    #[command(hide = true)]
    ScrubResumeOrStart(ScrubMountArgs),
    /// Check disk health: exit 0 = ok/offline, exit 1 = alert (incl. probe/compute failure latched as ComputationError), exit 2 = setup error (config)
    Monitor,
    /// Acknowledge current alerts and silence notifications
    Ack,
    /// Interactive terminal dashboard
    Tui(TuiArgs),
    /// Scan for braid-labeled LUKS devices and display or rebuild pool membership
    Discover(DiscoverArgs),
    /// Recover from an interrupted operation by rebuilding pool.json from live pool state
    Recover(RecoverArgs),
    /// UPS (NUT) inspection commands
    Ups(UpsArgs),
    /// Print this message or the help of the given commands
    Help(HelpArgs),
}

/// Explicit help subcommand so braid owns the rendered wording while still
/// delegating per-command help output to Clap.
#[derive(Debug, Args)]
struct HelpArgs {
    /// Print help for the commands
    #[arg(value_name = "COMMAND", num_args(0..))]
    commands: Vec<String>,
}

#[derive(Debug, Args)]
struct UpsArgs {
    #[command(subcommand)]
    command: UpsCommand,
}

#[derive(Debug, Subcommand)]
enum UpsCommand {
    /// Show current UPS status, battery charge, runtime, load, and device info
    Status(UpsStatusArgs),
}

#[derive(Debug, Args)]
struct UpsStatusArgs {
    /// Emit the parsed `upsc` model as JSON instead of a human summary.
    /// Stable shape; distinct error sentinels for the not-enabled and
    /// query-failed branches.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RecoverArgs {
    #[command(flatten)]
    passphrase: PassphraseInputArgs,
    /// Allow mounting with missing devices (degraded mode -- new writes have no redundancy)
    #[arg(long)]
    allow_degraded: bool,
    /// Show what would be done without making changes
    #[arg(long)]
    dry_run: bool,
    /// Progress output for the post-mount remediation phase (replace resize
    /// replay and paused-balance resume). Defaults to auto, like the
    /// mutation commands. The resume itself can be many minutes on a loaded
    /// pool, so progress matters here even though `recover` is otherwise
    /// quiet.
    #[arg(long, value_enum, default_value_t = braid_cli::progress::ProgressMode::Auto)]
    progress: braid_cli::progress::ProgressMode,
}

#[derive(Debug, Args)]
struct LockArgs {
    /// Show what would be done without making changes
    #[arg(long)]
    dry_run: bool,
    /// Hidden: invoked from braid-online.service ExecStop with bounded wait.
    #[arg(long, hide = true, requires = "deadline_secs")]
    systemd_stop: bool,
    /// Hidden: maximum seconds to wait during --systemd-stop.
    #[arg(long, hide = true, requires = "systemd_stop", value_parser = clap::value_parser!(u64).range(1..))]
    deadline_secs: Option<u64>,
}

#[derive(Debug, Args)]
struct CommonArgs {
    /// Show what would happen without executing
    #[arg(long)]
    dry_run: bool,
    /// Skip interactive confirmations
    #[arg(long)]
    yes: bool,
    /// Progress display mode
    #[arg(long, value_enum, default_value_t = ProgressMode::Auto)]
    progress: ProgressMode,
}

/// Single owner for passphrase-source flags so every consumer inherits
/// the same stdin/file conflict declaration.
#[derive(Debug, Args)]
struct PassphraseInputArgs {
    /// Read passphrase from stdin
    #[arg(long)]
    passphrase_stdin: bool,
    /// Read passphrase from file instead of TTY prompt
    #[arg(long, conflicts_with = "passphrase_stdin")]
    passphrase_file: Option<std::path::PathBuf>,
}

#[derive(Debug, Args)]
struct LuksFormatArgs {
    /// Advanced: pass one raw argv element to cryptsetup luksFormat.
    ///
    /// Repeat for multiple arguments. Use the equals form for values that
    /// start with a hyphen, e.g. --luks-format-arg=--pbkdf.
    #[arg(
        long = "luks-format-arg",
        value_name = "ARG",
        action = ArgAction::Append,
        num_args = 1,
        require_equals = true,
        allow_hyphen_values = true
    )]
    luks_format_extra_opts: Vec<String>,
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Disk specs: NAME=/dev/disk/by-id/... (e.g. toshiba=/dev/disk/by-id/ata-TOSHIBA_MN07)
    #[arg(required = true, num_args(1..), add = ArgValueCandidates::new(disk_name_candidates))]
    disks: Vec<String>,
    /// Directory containing braid.key to enroll in the new disk (LUKS slot 1)
    #[arg(long = "enroll")]
    enroll_key_file: Option<std::path::PathBuf>,
    #[command(flatten)]
    luks_format: LuksFormatArgs,
    #[command(flatten)]
    passphrase: PassphraseInputArgs,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, Args)]
struct RemoveArgs {
    /// Disk name to remove
    #[arg(add = ArgValueCandidates::new(disk_name_candidates))]
    disk: String,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, Args)]
struct RemoveMissingArgs {
    /// Target missing device by btrfs devid (use 'braid status' to find it)
    #[arg(long)]
    missing_id: u64,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, Args)]
struct ReplaceArgs {
    /// Disk name of the disk to replace
    #[arg(long, add = ArgValueCandidates::new(disk_name_candidates))]
    old: String,
    /// Disk name of the new replacement disk
    #[arg(long, add = ArgValueCandidates::new(disk_name_candidates))]
    new: String,
    /// Target a specific missing device by btrfs devid (dead disk only)
    #[arg(long)]
    missing_id: Option<u64>,
    /// Directory containing braid.key to enroll in the new disk (LUKS slot 1)
    #[arg(long = "enroll")]
    enroll_key_file: Option<std::path::PathBuf>,
    #[command(flatten)]
    luks_format: LuksFormatArgs,
    #[command(flatten)]
    passphrase: PassphraseInputArgs,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Emit the parsed doctor report as JSON instead of human output.
    #[arg(long)]
    json: bool,
    /// Play the audible alert test beep when checking the alert path.
    #[arg(long)]
    beep: bool,
}

#[derive(Debug, Args)]
struct UnlockArgs {
    #[command(flatten)]
    passphrase: PassphraseInputArgs,
    /// Unlock with a binary keyfile instead of passphrase
    #[arg(long, conflicts_with_all = ["passphrase_stdin", "passphrase_file"])]
    key_file: Option<std::path::PathBuf>,
    /// Allow mounting with missing devices (degraded mode -- new writes have no redundancy)
    #[arg(long)]
    allow_degraded: bool,
    /// Show what would be done without making changes
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct EnrollKeyFileArgs {
    /// Directory containing (or to receive) braid.key
    dir: std::path::PathBuf,
    /// Generate a new 4096-byte random keyfile in DIR; DIR must already be a mount point
    #[arg(long)]
    generate: bool,
    #[command(flatten)]
    passphrase: PassphraseInputArgs,
    /// Show what would be done without making changes
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct TuiArgs {
    /// Run with fake data (no config or btrfs required)
    #[arg(long)]
    demo: bool,
}

#[derive(Debug, Args)]
struct DiscoverArgs {
    /// Write discovered membership to pool.json
    #[arg(long)]
    write: bool,
    /// Fail closed unless discovery produces exactly N members.
    /// Use as a guard for any discover --write rebuild where the
    /// expected member count is known ahead of time, so a momentarily
    /// detached disk (loose cable, USB power glitch, udev race) or
    /// extra braid-labeled disk cannot silently produce the wrong
    /// pool.json.
    /// Only honored alongside --write.
    #[arg(long = "expect-count", value_name = "N")]
    expect_count: Option<usize>,
}

#[derive(Debug, Args)]
struct ScrubCancelArgs {
    /// Mount point of the braid pool to check
    #[arg(long)]
    mount: String,
}

#[derive(Debug, Args)]
struct ScrubMountArgs {
    /// Mount point of the braid pool to scrub
    #[arg(long)]
    mount: String,
}

/// Renders help for a nested command path without reintroducing Clap's
/// auto-generated plural-marker wording.
fn print_help_for(commands: &[String]) {
    let mut command = Cli::command();
    command.build();
    match find_help_target(&mut command, commands) {
        Ok(target) => {
            if let Err(e) = target.print_long_help() {
                eprintln!("error: could not write help output: {e}");
                std::process::exit(1);
            }
            println!();
        }
        Err(e) => e.exit(),
    }
}

/// Resolves `braid help <path>` against Clap's command tree so nested help
/// remains generated by the same definitions as `--help`.
fn find_help_target<'a>(
    command: &'a mut clap::Command,
    commands: &[String],
) -> Result<&'a mut clap::Command, clap::Error> {
    let Some((name, rest)) = commands.split_first() else {
        return Ok(command);
    };
    if command.find_subcommand(name).is_none() {
        return Err(command.error(
            ErrorKind::InvalidSubcommand,
            format!("unrecognized command '{name}'"),
        ));
    }
    let subcommand = command
        .find_subcommand_mut(name)
        .expect("subcommand was checked above");
    find_help_target(subcommand, rest)
}

fn main() {
    // When COMPLETE=bash|zsh|fish is set, produce shell completion output and exit.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    // Parse before root gate so --help/--version work without sudo.
    let cli = Cli::parse();

    // Allow --demo without root; everything else needs root.
    let needs_root = match &cli.command {
        Commands::Tui(args) if args.demo => false,
        Commands::Help(_) => false,
        _ => true,
    };

    // SAFETY: geteuid() is a trivial syscall with no arguments, always safe to call.
    if needs_root && unsafe { libc::geteuid() } != 0 {
        eprintln!("error: braid must be run as root (try: sudo braid ...)");
        std::process::exit(1);
    }

    let config_path = cli.config;
    let paths = StatePaths::production();
    let pool_lock = RealPoolLock::production();
    let stop_coordinator = RealStopCoordinator::production();

    // Hoisted once: shared by add/remove/remove-missing/replace. Each command's
    // cmd_* function holds the inhibitor only across its irreversible mutation
    // window — see docs/decisions/019-inhibit-sleep.md for the boundary rule.
    let sleep_inhibitor = braid_cli::inhibit::RealSleepInhibitor;

    match cli.command {
        Commands::Help(args) => {
            print_help_for(&args.commands);
        }
        Commands::Add(args) => {
            let progress = resolve_progress_output(
                args.common.progress,
                std::io::IsTerminal::is_terminal(&std::io::stderr()),
                false,
            );
            let _pool_guard = (!args.common.dry_run).then(|| acquire_pool_or_exit(&pool_lock));
            let runner = RealRunner;
            let online_ops = RealOnlineStateOps::new(&runner);
            let online_snapshot = (!args.common.dry_run).then(|| snapshot(&online_ops));
            let online_config =
                (!args.common.dry_run).then(|| load_config_or_exit(Path::new(&config_path), 1));
            let fs = RealFilesystem;
            let backing_path_resolver = braid_cli::luks::RealBackingPathResolver;
            let enroll_kf = args
                .enroll_key_file
                .as_ref()
                .map(|dir| dir.join(braid_cli::luks::KEYFILE_NAME));
            if let Err(e) = braid_cli::add::cmd_add(
                &runner,
                &fs,
                &braid_cli::add::AddParams {
                    config_path: Path::new(&config_path),
                    disk_specs: &args.disks,
                    dry_run: args.common.dry_run,
                    yes: args.common.yes,
                    passphrase_stdin: args.passphrase.passphrase_stdin,
                    passphrase_file: args.passphrase.passphrase_file.as_deref(),
                    enroll_key_file: enroll_kf.as_deref(),
                    luks_format_extra_opts: &args.luks_format.luks_format_extra_opts,
                    progress,
                    paths: &paths,
                    sleep_inhibitor: &sleep_inhibitor,
                    passphrase_reader: &braid_cli::luks::RealTty,
                    backing_path_resolver: &backing_path_resolver,
                },
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
            if let (Some(snap), Some(cfg)) = (online_snapshot.as_ref(), online_config.as_ref()) {
                let _ = mark_online(snap, cfg, &online_ops);
            }
        }
        Commands::Remove(args) => {
            let progress = resolve_progress_output(
                args.common.progress,
                std::io::IsTerminal::is_terminal(&std::io::stderr()),
                false,
            );
            let _pool_guard = (!args.common.dry_run).then(|| acquire_pool_or_exit(&pool_lock));
            let runner = RealRunner;
            let fs = RealFilesystem;
            if let Err(e) = braid_cli::remove::cmd_remove(
                &runner,
                &fs,
                &braid_cli::remove::RemoveParams {
                    config_path: Path::new(&config_path),
                    name: &args.disk,
                    dry_run: args.common.dry_run,
                    yes: args.common.yes,
                    progress,
                    paths: &paths,
                    sleep_inhibitor: &sleep_inhibitor,
                    sleeper: &braid_cli::progress::RealSleeper,
                },
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::RemoveMissing(args) => {
            let progress = resolve_progress_output(
                args.common.progress,
                std::io::IsTerminal::is_terminal(&std::io::stderr()),
                false,
            );
            let _pool_guard = (!args.common.dry_run).then(|| acquire_pool_or_exit(&pool_lock));
            let runner = RealRunner;
            let fs = RealFilesystem;
            if let Err(e) = braid_cli::remove_missing::cmd_remove_missing(
                &runner,
                &fs,
                &braid_cli::remove_missing::RemoveMissingParams {
                    config_path: Path::new(&config_path),
                    missing_id: args.missing_id,
                    dry_run: args.common.dry_run,
                    yes: args.common.yes,
                    progress,
                    paths: &paths,
                    sleep_inhibitor: &sleep_inhibitor,
                    sleeper: &braid_cli::progress::RealSleeper,
                },
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Replace(args) => {
            let progress = resolve_progress_output(
                args.common.progress,
                std::io::IsTerminal::is_terminal(&std::io::stderr()),
                false,
            );
            let _pool_guard = (!args.common.dry_run).then(|| acquire_pool_or_exit(&pool_lock));
            let runner = RealRunner;
            let fs = RealFilesystem;
            let backing_path_resolver = braid_cli::luks::RealBackingPathResolver;
            let enroll_kf = args
                .enroll_key_file
                .as_ref()
                .map(|dir| dir.join(braid_cli::luks::KEYFILE_NAME));
            if let Err(e) = braid_cli::replace::cmd_replace(
                &runner,
                &fs,
                &braid_cli::replace::ReplaceParams {
                    config_path: Path::new(&config_path),
                    old_name: &args.old,
                    new_name: &args.new,
                    missing_id: args.missing_id,
                    dry_run: args.common.dry_run,
                    yes: args.common.yes,
                    passphrase_stdin: args.passphrase.passphrase_stdin,
                    passphrase_file: args.passphrase.passphrase_file.as_deref(),
                    enroll_key_file: enroll_kf.as_deref(),
                    luks_format_extra_opts: &args.luks_format.luks_format_extra_opts,
                    progress,
                    paths: &paths,
                    sleep_inhibitor: &sleep_inhibitor,
                    sleeper: &braid_cli::progress::RealSleeper,
                    backing_path_resolver: &backing_path_resolver,
                },
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Status(args) => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let runner = RealRunner;
            let fs = RealFilesystem;
            let backing_path_resolver = braid_cli::luks::RealBackingPathResolver;
            if let Err(e) = braid_cli::status::cmd_status(
                &runner,
                &fs,
                &config,
                args.json,
                &paths,
                &backing_path_resolver,
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Doctor(args) => {
            if let Err(e) = cmd_doctor(
                Path::new(&config_path),
                &paths,
                DoctorOptions {
                    json: args.json,
                    beep: args.beep,
                },
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Unlock(args) => {
            let _pool_guard = (!args.dry_run).then(|| acquire_pool_or_exit(&pool_lock));
            let runner = RealRunner;
            let online_ops = RealOnlineStateOps::new(&runner);
            let online_snapshot = (!args.dry_run).then(|| snapshot(&online_ops));
            let config = load_config_or_exit(Path::new(&config_path), 1);
            let membership = load_membership_or_exit(&paths, 1);
            let fs = RealFilesystem;
            let backing_path_resolver = braid_cli::luks::RealBackingPathResolver;
            match braid_cli::unlock::cmd_unlock(
                &runner,
                &fs,
                &braid_cli::unlock::UnlockParams {
                    config: &config,
                    membership: &membership,
                    paths: &paths,
                    passphrase_stdin: args.passphrase.passphrase_stdin,
                    passphrase_file: args.passphrase.passphrase_file.as_deref(),
                    key_file: args.key_file.as_deref(),
                    allow_degraded: args.allow_degraded,
                    dry_run: args.dry_run,
                    backing_path_resolver: &backing_path_resolver,
                },
            ) {
                Ok(()) => {
                    if let Some(snap) = online_snapshot.as_ref() {
                        let _ = mark_online(snap, &config, &online_ops);
                    }
                }
                Err(braid_cli::unlock::UnlockError::Mount(
                    braid_cli::mount::MountError::DegradedRefused(msg),
                )) => {
                    print_cli_error(&msg);
                    std::process::exit(2);
                }
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(1);
                }
            }
        }
        Commands::EnrollKeyFile(args) => {
            let _pool_guard = (!args.dry_run).then(|| acquire_pool_or_exit(&pool_lock));
            let membership = load_membership_or_exit(&paths, 1);
            let runner = RealRunner;
            let fs = RealFilesystem;
            let backing_path_resolver = braid_cli::luks::RealBackingPathResolver;
            let key_file_path = args.dir.join(braid_cli::luks::KEYFILE_NAME);
            if let Err(e) = braid_cli::enroll_key_file::cmd_enroll_key_file(
                &runner,
                &fs,
                &braid_cli::enroll_key_file::EnrollKeyFileParams {
                    membership: &membership,
                    key_file_path: &key_file_path,
                    generate: args.generate,
                    passphrase_stdin: args.passphrase.passphrase_stdin,
                    passphrase_file: args.passphrase.passphrase_file.as_deref(),
                    dry_run: args.dry_run,
                    paths: &paths,
                    backing_path_resolver: &backing_path_resolver,
                },
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Lock(args) => {
            if args.dry_run {
                let config = load_config_or_exit(Path::new(&config_path), 1);
                let membership = load_membership_or_exit(&paths, 1);
                let runner = RealRunner;
                let fs = RealFilesystem;
                if let Err(e) = braid_cli::lock::cmd_lock(&runner, &fs, &config, &membership, true)
                {
                    print_cli_error(&e.to_string());
                    std::process::exit(1);
                }
            } else if args.systemd_stop {
                run_systemd_stop_lock(
                    &pool_lock,
                    &stop_coordinator,
                    Duration::from_secs(args.deadline_secs.expect("clap requires deadline")),
                    Path::new(&config_path),
                    &paths,
                );
            } else {
                run_plain_lock(
                    &pool_lock,
                    &stop_coordinator,
                    Path::new(&config_path),
                    &paths,
                );
            }
        }
        Commands::Idle => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };
            let runner = RealRunner;
            let fs = braid_cli::probe::RealFilesystem;
            match braid_cli::idle::cmd_idle(&runner, &fs, config.mount_point()) {
                braid_cli::idle::IdleResult::PoolOffline => {
                    println!("idle: pool is offline");
                    std::process::exit(0);
                }
                braid_cli::idle::IdleResult::Idle => {
                    println!("idle: pool is idle");
                    std::process::exit(0);
                }
                braid_cli::idle::IdleResult::Busy(reason) => {
                    println!("busy: {reason}");
                    std::process::exit(1);
                }
            }
        }
        Commands::ScrubCancel(args) => {
            // Mount comes from --mount, NOT config_read. ExecStop must have zero
            // filesystem dependencies beyond the binary itself — see
            // docs/decisions/018-systemd-lifecycle.md (thin-systemd-layer principle).
            let runner = RealRunner;
            let mount_point = braid_cli::types::MountPoint(args.mount.clone());
            match braid_cli::scrub_cancel::cmd_scrub_cancel(&runner, &mount_point) {
                Ok(_) => std::process::exit(0),
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(1);
                }
            }
        }
        Commands::ScrubNeedsResume(args) => {
            let runner = RealRunner;
            let mount_point = braid_cli::types::MountPoint(args.mount.clone());
            match braid_cli::scrub_needs_resume::cmd_scrub_needs_resume(&runner, &mount_point) {
                Ok(braid_cli::scrub_needs_resume::ScrubNeedsResumeResult::Yes) => {
                    std::process::exit(0)
                }
                Ok(braid_cli::scrub_needs_resume::ScrubNeedsResumeResult::No) => {
                    std::process::exit(1)
                }
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(2);
                }
            }
        }
        Commands::ScrubResumeOrStart(args) => {
            let runner = RealRunner;
            let mount_point = braid_cli::types::MountPoint(args.mount.clone());
            match braid_cli::scrub_resume_or_start::cmd_scrub_resume_or_start(&runner, &mount_point)
            {
                Ok(braid_cli::scrub_resume_or_start::ScrubResumeOrStartResult::Resumed {
                    uncorrectable_errors: false,
                })
                | Ok(braid_cli::scrub_resume_or_start::ScrubResumeOrStartResult::Started {
                    uncorrectable_errors: false,
                }) => std::process::exit(0),
                Ok(braid_cli::scrub_resume_or_start::ScrubResumeOrStartResult::Resumed {
                    uncorrectable_errors: true,
                })
                | Ok(braid_cli::scrub_resume_or_start::ScrubResumeOrStartResult::Started {
                    uncorrectable_errors: true,
                }) => std::process::exit(3),
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(1);
                }
            }
        }
        Commands::Monitor => {
            let _pool_guard = match pool_lock.acquire() {
                Ok(guard) => Some(guard),
                Err(PoolLockError::AlreadyHeld) => std::process::exit(0),
                Err(e) => {
                    handle_pool_lock_error(e);
                    std::process::exit(2);
                }
            };
            let config = load_config_or_exit(Path::new(&config_path), 2);
            let runner = RealRunner;
            let fs = RealFilesystem;
            match braid_cli::monitor::cmd_monitor(&runner, &fs, config.mount_point(), &paths) {
                braid_cli::monitor::MonitorResult::PoolOffline => {
                    std::process::exit(0);
                }
                braid_cli::monitor::MonitorResult::Ok => {
                    std::process::exit(0);
                }
                braid_cli::monitor::MonitorResult::Alert(_) => {
                    std::process::exit(1);
                }
            }
        }
        Commands::Ack => {
            let _pool_guard =
                acquire_pool_with_timeout_or_exit(&pool_lock, Duration::from_secs(10));
            let config = load_config_or_exit(Path::new(&config_path), 1);
            let runner = RealRunner;
            let fs = RealFilesystem;
            if let Err(e) = braid_cli::ack::cmd_ack(&runner, &fs, config.mount_point(), &paths) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Tui(args) => {
            let result = if args.demo {
                braid_cli::tui::run_demo()
            } else {
                braid_cli::tui::run(Path::new(&config_path), &paths)
            };
            if let Err(e) = result {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Discover(args) => {
            let _pool_guard = args.write.then(|| acquire_pool_or_exit(&pool_lock));
            // Note: the pre-save fail-closed gates for `--write`
            // (pending-op presence + pool.json shape check that refuses
            // `ValidUuidKeyed`; `Corrupt` is the documented rebuild path
            // per decision 017) live inside
            // `discover::write_discovered_membership`. The bare read-only
            // path reuses the shape classifier so corrupt or unreadable
            // state fails closed with rebuild guidance.
            let pool_json = paths.pool_json();
            let shape = braid_cli::discover::classify_pool_json(&pool_json);
            if !args.write {
                match shape {
                    braid_cli::discover::PoolJsonShape::Missing => {}
                    braid_cli::discover::PoolJsonShape::ValidUuidKeyed => {
                        print_cli_error(&format!(
                            "pool.json already exists at {} -- use 'braid add' to add disks",
                            pool_json.display()
                        ));
                        std::process::exit(1);
                    }
                    braid_cli::discover::PoolJsonShape::Corrupt => {
                        print_cli_error(&format!(
                            "pool.json at {} is corrupt or unreadable -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)",
                            pool_json.display()
                        ));
                        std::process::exit(1);
                    }
                }
            }
            let runner = RealRunner;
            let scan = braid_cli::discover::discover_pool_members(&runner);
            let members = match braid_cli::discover::drain_warnings(scan, &mut std::io::stderr()) {
                Ok(members) => members,
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(1);
                }
            };
            if members.is_empty() {
                eprintln!("no braid-labeled LUKS devices found");
                std::process::exit(1);
            }
            for line in braid_cli::discover::render_preview_lines(&members) {
                eprintln!("{line}");
            }
            if args.write {
                match braid_cli::discover::write_discovered_membership(
                    members,
                    &paths,
                    args.expect_count,
                ) {
                    Ok(_) => {
                        eprintln!("pool membership written to {}", pool_json.display());
                    }
                    Err(e) => {
                        print_cli_error(&e.to_string());
                        std::process::exit(1);
                    }
                }
            } else {
                eprintln!("pass --write to save to {}", pool_json.display());
            }
        }
        Commands::Recover(args) => {
            let progress = resolve_progress_output(
                args.progress,
                std::io::IsTerminal::is_terminal(&std::io::stderr()),
                false,
            );
            let _pool_guard = (!args.dry_run).then(|| acquire_pool_or_exit(&pool_lock));
            let runner = RealRunner;
            let online_ops = RealOnlineStateOps::new(&runner);
            let online_snapshot = (!args.dry_run).then(|| snapshot(&online_ops));
            let config = load_config_or_exit(Path::new(&config_path), 1);
            let fs = RealFilesystem;
            let by_id_resolver = braid_cli::recover::RealByIdResolver;
            let backing_path_resolver = braid_cli::luks::RealBackingPathResolver;
            match braid_cli::recover::cmd_recover(
                &runner,
                &fs,
                &by_id_resolver,
                &braid_cli::recover::RecoverParams {
                    config: &config,
                    paths: &paths,
                    passphrase_stdin: args.passphrase.passphrase_stdin,
                    passphrase_file: args.passphrase.passphrase_file.as_deref(),
                    allow_degraded: args.allow_degraded,
                    dry_run: args.dry_run,
                    progress,
                    sleep_inhibitor: &sleep_inhibitor,
                    sleeper: &braid_cli::progress::RealSleeper,
                    backing_path_resolver: &backing_path_resolver,
                },
            ) {
                Ok(()) => {
                    if let Some(snap) = online_snapshot.as_ref() {
                        let _ = mark_online(snap, &config, &online_ops);
                    }
                }
                Err(braid_cli::recover::RecoverError::Mount(
                    braid_cli::mount::MountError::DegradedRefused(msg),
                )) => {
                    print_cli_error(&msg);
                    std::process::exit(2);
                }
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(1);
                }
            }
        }
        Commands::Ups(args) => match args.command {
            UpsCommand::Status(status_args) => {
                let runner = RealRunner;
                match braid_cli::ups::cmd_ups_status(
                    &runner,
                    Path::new(&config_path),
                    status_args.json,
                ) {
                    Ok(()) => {}
                    Err(braid_cli::ups::UpsError::QueryFailedJsonReported) => {
                        std::process::exit(1);
                    }
                    Err(e) => {
                        print_cli_error(&e.to_string());
                        std::process::exit(1);
                    }
                }
            }
        },
    }
}

fn acquire_pool_or_exit(pool_lock: &RealPoolLock) -> Box<dyn braid_cli::pool_lock::PoolLockGuard> {
    match pool_lock.acquire() {
        Ok(guard) => guard,
        Err(e) => {
            handle_pool_lock_error(e);
            std::process::exit(1);
        }
    }
}

fn acquire_pool_with_timeout_or_exit(
    pool_lock: &RealPoolLock,
    timeout: Duration,
) -> Box<dyn braid_cli::pool_lock::PoolLockGuard> {
    match pool_lock.acquire_with_timeout(timeout) {
        Ok(guard) => guard,
        Err(e) => {
            handle_pool_lock_error(e);
            std::process::exit(1);
        }
    }
}

fn handle_pool_lock_error(error: PoolLockError) {
    match error {
        PoolLockError::AlreadyHeld | PoolLockError::DeadlineExpired { .. } => {
            eprintln!("{error}");
        }
        PoolLockError::Io(e) => print_cli_error(&e.to_string()),
    }
}

fn load_config_or_exit(path: &Path, exit_code: i32) -> braid_cli::config::Config {
    match config_read(path) {
        Ok(config) => config,
        Err(e) => {
            print_cli_error(&e.to_string());
            std::process::exit(exit_code);
        }
    }
}

fn load_membership_or_exit(paths: &StatePaths, exit_code: i32) -> PoolMembership {
    match braid_cli::membership::load_membership(paths) {
        Ok(membership) => membership,
        Err(e) => {
            print_cli_error(&e.to_string());
            std::process::exit(exit_code);
        }
    }
}

fn run_plain_lock(
    pool_lock: &RealPoolLock,
    stop_coordinator: &RealStopCoordinator,
    config_path: &Path,
    paths: &StatePaths,
) {
    let coordinator_guard = match stop_coordinator.acquire() {
        Ok(guard) => guard,
        Err(StopCoordinatorError::Held) => {
            eprintln!("{}", PoolLockError::AlreadyHeld);
            std::process::exit(1);
        }
        Err(StopCoordinatorError::Io(e)) => {
            print_cli_error(&e.to_string());
            std::process::exit(1);
        }
    };
    let _pool_guard = acquire_pool_or_exit(pool_lock);
    let config = load_config_or_exit(config_path, 1);
    let membership = load_membership_or_exit(paths, 1);
    let runner = RealRunner;
    let fs = RealFilesystem;
    let online_ops = RealOnlineStateOps::new(&runner);

    if let Err(e) = braid_cli::lock::cmd_lock(&runner, &fs, &config, &membership, false) {
        print_cli_error(&e.to_string());
        std::process::exit(1);
    }
    if let Err(e) = coordinator_guard.mark_done() {
        print_cli_error(&format!("failed to mark lock cleanup done: {e}"));
        std::process::exit(1);
    }
    let _ = mark_offline(config.mount_point(), &online_ops);
}

fn run_systemd_stop_lock(
    pool_lock: &RealPoolLock,
    stop_coordinator: &RealStopCoordinator,
    deadline: Duration,
    config_path: &Path,
    paths: &StatePaths,
) {
    let start = Instant::now();
    let _coordinator_guard = match stop_coordinator.acquire() {
        Ok(guard) => guard,
        Err(StopCoordinatorError::Held) => {
            match stop_coordinator.poll_for_done_or_release(deadline) {
                StopCoordinatorPollResult::Done => return,
                StopCoordinatorPollResult::Acquired(guard) => guard,
                StopCoordinatorPollResult::Deadline => {
                    eprintln!("{}", PoolLockError::DeadlineExpired { waited: deadline });
                    std::process::exit(1);
                }
            }
        }
        Err(StopCoordinatorError::Io(e)) => {
            print_cli_error(&e.to_string());
            std::process::exit(1);
        }
    };
    let Some(remaining) = deadline.checked_sub(start.elapsed()) else {
        eprintln!("{}", PoolLockError::DeadlineExpired { waited: deadline });
        std::process::exit(1);
    };
    if remaining.is_zero() {
        eprintln!("{}", PoolLockError::DeadlineExpired { waited: deadline });
        std::process::exit(1);
    }
    let _pool_guard = match pool_lock.acquire_with_systemd_stop_deadline(remaining) {
        Ok(guard) => guard,
        Err(PoolLockError::DeadlineExpired { .. }) => {
            eprintln!("{}", PoolLockError::DeadlineExpired { waited: deadline });
            std::process::exit(1);
        }
        Err(e) => {
            handle_pool_lock_error(e);
            std::process::exit(1);
        }
    };
    let config = load_config_or_exit(config_path, 1);
    let membership = load_membership_or_exit(paths, 1);
    let runner = RealRunner;
    let fs = RealFilesystem;
    if let Err(e) = braid_cli::lock::cmd_lock(&runner, &fs, &config, &membership, false) {
        print_cli_error(&e.to_string());
        std::process::exit(1);
    }
}

fn print_cli_error(message: &str) {
    if message.starts_with("error[") {
        eprintln!("{message}");
    } else {
        eprintln!("error: {message}");
    }
}

/// Tab completion returns disk names from membership in operator-visible order.
fn disk_name_candidates() -> Vec<CompletionCandidate> {
    let paths = StatePaths::production();
    let Ok(membership) = braid_cli::membership::load_membership(&paths) else {
        return Vec::new();
    };
    membership
        .iter_by_name()
        .into_iter()
        .map(|(_, member)| CompletionCandidate::new(member.name.as_str().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_luks_format_args() -> Vec<String> {
        [
            "--pbkdf",
            "pbkdf2",
            "--iter-time",
            "1",
            "--label",
            "ignored-by-cli-test",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn add_accepts_repeated_luks_format_arg_values_starting_with_hyphen() {
        let cli = Cli::try_parse_from([
            "braid",
            "add",
            "disk2=/dev/disk/by-id/x",
            "--luks-format-arg=--pbkdf",
            "--luks-format-arg=pbkdf2",
            "--luks-format-arg=--iter-time",
            "--luks-format-arg=1",
            "--luks-format-arg=--label",
            "--luks-format-arg=ignored-by-cli-test",
        ])
        .expect("add should accept repeated raw LUKS format argv elements");

        let Commands::Add(args) = cli.command else {
            panic!("expected add command");
        };
        assert_eq!(
            args.luks_format.luks_format_extra_opts,
            expected_luks_format_args()
        );
    }

    #[test]
    fn replace_accepts_repeated_luks_format_arg_values_starting_with_hyphen() {
        let cli = Cli::try_parse_from([
            "braid",
            "replace",
            "--luks-format-arg=--pbkdf",
            "--luks-format-arg=pbkdf2",
            "--luks-format-arg=--iter-time",
            "--luks-format-arg=1",
            "--luks-format-arg=--label",
            "--luks-format-arg=ignored-by-cli-test",
            "--old",
            "disk1",
            "--new",
            "disk2=/dev/disk/by-id/x",
        ])
        .expect("replace should accept repeated raw LUKS format argv elements");

        let Commands::Replace(args) = cli.command else {
            panic!("expected replace command");
        };
        assert_eq!(
            args.luks_format.luks_format_extra_opts,
            expected_luks_format_args()
        );
    }

    #[test]
    fn remove_does_not_accept_luks_format_arg() {
        let err = Cli::try_parse_from(["braid", "remove", "disk1", "--luks-format-arg=--pbkdf"])
            .expect_err("remove must not expose LUKS format options");

        assert!(err.to_string().contains("unexpected argument"));
    }

    // Intent: remove-missing rejects --luks-format-arg because it never
    // formats a fresh LUKS volume.
    // Why it exists: RemoveMissingArgs structurally does not flatten
    // LuksFormatArgs, but only `remove` had a parse-rejection regression test
    // before. A future reflexive flatten would regress this silently.
    // Scenario: an operator copy-pastes a --luks-format-arg=... flag from add
    // or replace into a remove-missing invocation.
    #[test]
    fn remove_missing_does_not_accept_luks_format_arg() {
        let err = Cli::try_parse_from([
            "braid",
            "remove-missing",
            "--missing-id",
            "1",
            "--luks-format-arg=--pbkdf",
        ])
        .expect_err("remove-missing must not expose LUKS format options");

        assert!(err.to_string().contains("unexpected argument"));
    }

    #[test]
    fn luks_format_arg_rejects_space_form_for_hyphen_value() {
        let err = Cli::try_parse_from([
            "braid",
            "add",
            "disk2=/dev/disk/by-id/x",
            "--luks-format-arg",
            "--pbkdf",
        ])
        .expect_err("luks format args must use --luks-format-arg=ARG");

        assert!(err.to_string().contains("equal sign is needed"));
    }

    // Intent: clap rejects conflicting passphrase inputs on every command
    // that reads one, including unlock's key-file conflicts.
    // Why it exists: the read path used to silently prefer passphrase files
    // over stdin when both flags were present.
    // Scenario: an operator updates a script from one input source to another
    // and leaves the old flag in place.
    #[test]
    fn passphrase_input_conflicts_are_rejected() {
        let cases: &[&[&str]] = &[
            &[
                "braid",
                "add",
                "disk1=/dev/disk/by-id/x",
                "--passphrase-stdin",
                "--passphrase-file",
                "/dev/null",
            ],
            &[
                "braid",
                "replace",
                "--old",
                "a",
                "--new",
                "b=/dev/disk/by-id/x",
                "--passphrase-stdin",
                "--passphrase-file",
                "/dev/null",
            ],
            &[
                "braid",
                "unlock",
                "--passphrase-stdin",
                "--passphrase-file",
                "/dev/null",
            ],
            &[
                "braid",
                "enroll",
                "/mnt/usb",
                "--passphrase-stdin",
                "--passphrase-file",
                "/dev/null",
            ],
            &[
                "braid",
                "recover",
                "--passphrase-stdin",
                "--passphrase-file",
                "/dev/null",
            ],
            &[
                "braid",
                "unlock",
                "--key-file",
                "/dev/null",
                "--passphrase-stdin",
            ],
            &[
                "braid",
                "unlock",
                "--key-file",
                "/dev/null",
                "--passphrase-file",
                "/dev/null",
            ],
        ];

        for argv in cases {
            let err = Cli::try_parse_from(argv.iter().copied())
                .expect_err(&format!("expected ArgumentConflict for {argv:?}"));
            assert_eq!(
                err.kind(),
                ErrorKind::ArgumentConflict,
                "wrong error kind for {argv:?}: {err}"
            );
        }
    }

    // Intent: remove commands reject passphrase flags because they never read
    // a passphrase.
    // Why it exists: those flags used to arrive through CommonArgs and parse
    // successfully even though the command bodies ignored them.
    // Scenario: an operator copy-pastes a passphrase-input flag from add or
    // unlock into a remove workflow.
    #[test]
    fn remove_commands_reject_passphrase_flags() {
        let cases: &[&[&str]] = &[
            &["braid", "remove", "disk1", "--passphrase-stdin"],
            &["braid", "remove", "disk1", "--passphrase-file", "/dev/null"],
            &[
                "braid",
                "remove-missing",
                "--missing-id",
                "1",
                "--passphrase-stdin",
            ],
            &[
                "braid",
                "remove-missing",
                "--missing-id",
                "1",
                "--passphrase-file",
                "/dev/null",
            ],
        ];

        for argv in cases {
            let err = Cli::try_parse_from(argv.iter().copied())
                .expect_err(&format!("expected UnknownArgument for {argv:?}"));
            assert_eq!(
                err.kind(),
                ErrorKind::UnknownArgument,
                "wrong error kind for {argv:?}: {err}"
            );
        }
    }

    #[test]
    fn lock_plain_parses_without_systemd_stop() {
        let cli = Cli::try_parse_from(["braid", "lock"]).expect("plain lock parses");
        let Commands::Lock(args) = cli.command else {
            panic!("expected lock command");
        };
        assert!(!args.dry_run);
        assert!(!args.systemd_stop);
        assert_eq!(args.deadline_secs, None);
    }

    #[test]
    fn lock_dry_run_parses() {
        let cli = Cli::try_parse_from(["braid", "lock", "--dry-run"]).expect("lock dry-run parses");
        let Commands::Lock(args) = cli.command else {
            panic!("expected lock command");
        };
        assert!(args.dry_run);
    }

    #[test]
    fn lock_systemd_stop_with_deadline_parses() {
        let cli =
            Cli::try_parse_from(["braid", "lock", "--systemd-stop", "--deadline-secs", "270"])
                .expect("systemd-stop lock parses with deadline");
        let Commands::Lock(args) = cli.command else {
            panic!("expected lock command");
        };
        assert!(args.systemd_stop);
        assert_eq!(args.deadline_secs, Some(270));
    }

    #[test]
    fn lock_systemd_stop_without_deadline_rejected() {
        let err = Cli::try_parse_from(["braid", "lock", "--systemd-stop"])
            .expect_err("systemd-stop requires deadline");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn lock_deadline_without_systemd_stop_rejected() {
        let err = Cli::try_parse_from(["braid", "lock", "--deadline-secs", "270"])
            .expect_err("deadline requires systemd-stop");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn lock_deadline_zero_rejected() {
        let err = Cli::try_parse_from(["braid", "lock", "--systemd-stop", "--deadline-secs", "0"])
            .expect_err("deadline must be positive");
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }
}
