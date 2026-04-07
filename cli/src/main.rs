use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};
use std::path::Path;

use braid_cli::cmd::RealRunner;
use braid_cli::config::config_read;
use braid_cli::doctor::cmd_doctor;
use braid_cli::probe::RealFilesystem;
use braid_cli::progress::{resolve_progress_output, ProgressMode};
use braid_cli::state_paths::StatePaths;

#[derive(Debug, Parser)]
#[command(name = "braid", version)]
#[command(about = "braid — encrypted NAS storage", long_about = None)]
struct Cli {
    #[arg(long, global = true, default_value = "/etc/braid/config.json")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Add disk(s) to the pool
    Add(AddArgs),
    /// Remove a disk from the pool
    Remove(RemoveArgs),
    /// Clean up a missing/dead device entry from the pool (does not rebuild data — use `replace` for that)
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
    /// Check if pool is idle (no scrub/balance/replace): exit 0 = idle, exit 1 = busy, exit 2 = error
    Idle,
    /// Check disk health: exit 0 = ok/offline, exit 1 = alert, exit 2 = error
    Monitor,
    /// Acknowledge current alerts and silence notifications
    Ack,
    /// Interactive terminal dashboard
    Tui(TuiArgs),
    /// Browse raw btrfs command output in a tabbed TUI
    Browse(BrowseArgs),
    /// Scan for braid-labeled LUKS devices and display or rebuild pool membership
    Discover(DiscoverArgs),
    /// Recover from an interrupted operation by rebuilding pool.json from live pool state
    Recover(RecoverArgs),
}

#[derive(Debug, Args)]
struct RecoverArgs {
    /// Read passphrase from stdin
    #[arg(long)]
    passphrase_stdin: bool,
    /// Read passphrase from file instead of TTY prompt
    #[arg(long)]
    passphrase_file: Option<std::path::PathBuf>,
    /// Allow mounting with missing devices (degraded mode — new writes have no redundancy)
    #[arg(long)]
    allow_degraded: bool,
    /// Show what would be done without making changes
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct LockArgs {
    /// Show what would be done without making changes
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct CommonArgs {
    /// Show what would happen without executing
    #[arg(long)]
    dry_run: bool,
    /// Skip interactive confirmations
    #[arg(long)]
    yes: bool,
    /// Read passphrase from stdin
    #[arg(long)]
    passphrase_stdin: bool,
    /// Read passphrase from file instead of TTY prompt
    #[arg(long)]
    passphrase_file: Option<std::path::PathBuf>,
    /// Progress display mode
    #[arg(long, value_enum, default_value_t = ProgressMode::Auto)]
    progress: ProgressMode,
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Disk spec(s): NAME=/dev/disk/by-id/... (e.g. toshiba=/dev/disk/by-id/ata-TOSHIBA_MN07)
    #[arg(num_args(1..), add = ArgValueCandidates::new(disk_name_candidates))]
    disks: Vec<String>,
    /// Directory containing braid.key to enroll in the new disk (LUKS slot 1)
    #[arg(long = "enroll")]
    enroll_key_file: Option<std::path::PathBuf>,
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
    common: CommonArgs,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct UnlockArgs {
    /// Read passphrase from stdin
    #[arg(long)]
    passphrase_stdin: bool,
    /// Read passphrase from file instead of TTY prompt
    #[arg(long)]
    passphrase_file: Option<std::path::PathBuf>,
    /// Unlock with a binary keyfile instead of passphrase
    #[arg(long, conflicts_with_all = ["passphrase_stdin", "passphrase_file"])]
    key_file: Option<std::path::PathBuf>,
    /// Allow mounting with missing devices (degraded mode — new writes have no redundancy)
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
    /// Generate a new 4096-byte random keyfile before enrolling
    #[arg(long)]
    generate: bool,
    /// Read passphrase from stdin
    #[arg(long)]
    passphrase_stdin: bool,
    /// Read passphrase from file instead of TTY prompt
    #[arg(long)]
    passphrase_file: Option<std::path::PathBuf>,
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
struct BrowseArgs {
    /// Mount point to inspect (defaults to config mount_point)
    #[arg(long)]
    mount_point: Option<String>,

    /// Non-interactive: run key commands and exit 0/1
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Args)]
struct DiscoverArgs {
    /// Write discovered membership to pool.json
    #[arg(long)]
    write: bool,
}

fn main() {
    // When COMPLETE=bash|zsh|fish is set, produce shell completion output and exit.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    // Parse before root gate so --help/--version work without sudo.
    let cli = Cli::parse();

    // Allow --demo without root; everything else needs root.
    let needs_root = !matches!(&cli.command, Commands::Tui(args) if args.demo);

    // SAFETY: geteuid() is a trivial syscall with no arguments, always safe to call.
    if needs_root && unsafe { libc::geteuid() } != 0 {
        eprintln!("error: braid must be run as root (try: sudo braid ...)");
        std::process::exit(1);
    }

    let config_path = cli.config;
    let paths = StatePaths::production();

    match cli.command {
        Commands::Add(args) => {
            let progress = resolve_progress_output(
                args.common.progress,
                std::io::IsTerminal::is_terminal(&std::io::stderr()),
                false,
            );
            let runner = RealRunner;
            let fs = RealFilesystem;
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
                    passphrase_stdin: args.common.passphrase_stdin,
                    passphrase_file: args.common.passphrase_file.as_deref(),
                    enroll_key_file: enroll_kf.as_deref(),
                    progress,
                    paths: &paths,
                },
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Remove(args) => {
            let progress = resolve_progress_output(
                args.common.progress,
                std::io::IsTerminal::is_terminal(&std::io::stderr()),
                false,
            );
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
            let runner = RealRunner;
            let fs = RealFilesystem;
            let enroll_kf = args
                .enroll_key_file
                .as_ref()
                .map(|dir| dir.join(braid_cli::luks::KEYFILE_NAME));
            let sleep_inhibitor = braid_cli::inhibit::RealSleepInhibitor;
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
                    passphrase_stdin: args.common.passphrase_stdin,
                    passphrase_file: args.common.passphrase_file.as_deref(),
                    enroll_key_file: enroll_kf.as_deref(),
                    progress,
                    paths: &paths,
                    sleep_inhibitor: &sleep_inhibitor,
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
            if let Err(e) = braid_cli::status::cmd_status(&runner, &fs, &config, args.json, &paths)
            {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Doctor(args) => {
            if let Err(e) = cmd_doctor(Path::new(&config_path), &paths, args.json) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Unlock(args) => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let membership = match braid_cli::membership::load_membership(&paths) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let runner = RealRunner;
            let fs = RealFilesystem;
            match braid_cli::unlock::cmd_unlock(
                &runner,
                &fs,
                &braid_cli::unlock::UnlockParams {
                    config: &config,
                    membership: &membership,
                    paths: &paths,
                    passphrase_stdin: args.passphrase_stdin,
                    passphrase_file: args.passphrase_file.as_deref(),
                    key_file: args.key_file.as_deref(),
                    allow_degraded: args.allow_degraded,
                    dry_run: args.dry_run,
                },
            ) {
                Ok(()) => {}
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
            let membership = match braid_cli::membership::load_membership(&paths) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let runner = RealRunner;
            let fs = RealFilesystem;
            let key_file_path = args.dir.join(braid_cli::luks::KEYFILE_NAME);
            if let Err(e) = braid_cli::enroll_key_file::cmd_enroll_key_file(
                &runner,
                &fs,
                &braid_cli::enroll_key_file::EnrollKeyFileParams {
                    membership: &membership,
                    key_file_path: &key_file_path,
                    generate: args.generate,
                    passphrase_stdin: args.passphrase_stdin,
                    passphrase_file: args.passphrase_file.as_deref(),
                    dry_run: args.dry_run,
                    paths: &paths,
                },
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Lock(args) => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let membership = match braid_cli::membership::load_membership(&paths) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let runner = RealRunner;
            let fs = RealFilesystem;
            if let Err(e) =
                braid_cli::lock::cmd_lock(&runner, &fs, &config, &membership, args.dry_run)
            {
                print_cli_error(&e.to_string());
                std::process::exit(1);
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
            match braid_cli::idle::cmd_idle(&runner, config.mount_point().as_str()) {
                Ok(braid_cli::idle::IdleResult::PoolOffline) => {
                    println!("idle: pool is offline");
                    std::process::exit(0);
                }
                Ok(braid_cli::idle::IdleResult::Idle) => {
                    println!("idle: pool is idle");
                    std::process::exit(0);
                }
                Ok(braid_cli::idle::IdleResult::Busy(reason)) => {
                    println!("busy: {reason}");
                    std::process::exit(1);
                }
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(2);
                }
            }
        }
        Commands::Monitor => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };
            let runner = RealRunner;
            match braid_cli::monitor::cmd_monitor(&runner, config.mount_point().as_str(), &paths) {
                Ok(braid_cli::monitor::MonitorResult::PoolOffline) => {
                    std::process::exit(0);
                }
                Ok(braid_cli::monitor::MonitorResult::Ok) => {
                    std::process::exit(0);
                }
                Ok(braid_cli::monitor::MonitorResult::Alert(_)) => {
                    std::process::exit(1);
                }
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(2);
                }
            }
        }
        Commands::Ack => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let runner = RealRunner;
            if let Err(e) = braid_cli::ack::cmd_ack(&runner, config.mount_point().as_str(), &paths)
            {
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
            if let Err(e) = braid_cli::preflight::check_no_pending_operation(&paths) {
                print_cli_error(&e);
                std::process::exit(1);
            }
            let pool_json = paths.pool_json();
            if pool_json.exists() {
                print_cli_error(&format!(
                    "pool.json already exists at {} — use 'braid add' to add disks",
                    pool_json.display()
                ));
                std::process::exit(1);
            }
            let runner = RealRunner;
            match braid_cli::discover::discover_pool_members(&runner) {
                Ok(members) => {
                    if members.is_empty() {
                        eprintln!("no braid-labeled LUKS devices found");
                        std::process::exit(1);
                    }
                    for (name, by_id) in &members {
                        eprintln!("  {} = {}", name, by_id);
                    }
                    if args.write {
                        let m = braid_cli::membership::PoolMembership {
                            disks: members
                                .into_iter()
                                .map(|(name, by_id)| {
                                    (name, braid_cli::membership::DiskMember::from_by_id(by_id))
                                })
                                .collect(),
                        };
                        if let Err(e) = braid_cli::membership::save_membership(&m, &paths) {
                            print_cli_error(&format!("failed to write pool membership: {e}"));
                            std::process::exit(1);
                        }
                        eprintln!("pool membership written to {}", pool_json.display());
                    } else {
                        eprintln!("pass --write to save to {}", pool_json.display());
                    }
                }
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(1);
                }
            }
        }
        Commands::Recover(args) => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    print_cli_error(&e.to_string());
                    std::process::exit(1);
                }
            };
            let runner = RealRunner;
            let fs = RealFilesystem;
            let by_id_resolver = braid_cli::recover::RealByIdResolver;
            match braid_cli::recover::cmd_recover(
                &runner,
                &fs,
                &by_id_resolver,
                &braid_cli::recover::RecoverParams {
                    config: &config,
                    paths: &paths,
                    passphrase_stdin: args.passphrase_stdin,
                    passphrase_file: args.passphrase_file.as_deref(),
                    allow_degraded: args.allow_degraded,
                    dry_run: args.dry_run,
                },
            ) {
                Ok(()) => {}
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
        Commands::Browse(args) => {
            let mount_point = match args.mount_point {
                Some(mp) => mp,
                None => {
                    let config = match config_read(Path::new(&config_path)) {
                        Ok(c) => c,
                        Err(e) => {
                            print_cli_error(&e.to_string());
                            std::process::exit(1);
                        }
                    };
                    config.mount_point().0.clone()
                }
            };
            let result = if args.check {
                braid_cli::browse::run_check(&mount_point)
            } else {
                braid_cli::browse::run(&mount_point)
            };
            if let Err(e) = result {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
    }
}

fn print_cli_error(message: &str) {
    if message.starts_with("error[") {
        eprintln!("{message}");
    } else {
        eprintln!("error: {message}");
    }
}

/// Tab completion returns disk names from membership.
fn disk_name_candidates() -> Vec<CompletionCandidate> {
    let paths = StatePaths::production();
    let Ok(membership) = braid_cli::membership::load_membership(&paths) else {
        return Vec::new();
    };
    membership
        .disks
        .keys()
        .map(|name| CompletionCandidate::new(name.clone()))
        .collect()
}
