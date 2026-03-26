use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};
use std::path::Path;

use braid_cli::cmd::RealRunner;
use braid_cli::config::config_read;
use braid_cli::doctor::cmd_doctor;
use braid_cli::probe::RealFilesystem;
use braid_cli::progress::{resolve_progress_output, ProgressMode};

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
    Lock,
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
    /// Disk name(s) (as defined in braid.disks)
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
    /// Target a specific missing device by btrfs devid
    #[arg(long)]
    missing_id: Option<u64>,
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
    /// Show what would be written without writing
    #[arg(long)]
    dry_run: bool,
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
                Path::new(&config_path),
                &args.disks,
                args.common.dry_run,
                args.common.yes,
                args.common.passphrase_stdin,
                args.common.passphrase_file.as_deref(),
                enroll_kf.as_deref(),
                progress,
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
            if let Err(e) = braid_cli::remove::cmd_remove(
                &runner,
                Path::new(&config_path),
                &args.disk,
                args.common.dry_run,
                args.common.yes,
                progress,
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
            if let Err(e) = braid_cli::remove_missing::cmd_remove_missing(
                &runner,
                Path::new(&config_path),
                args.missing_id,
                args.common.dry_run,
                args.common.yes,
                progress,
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
            if let Err(e) = braid_cli::replace::cmd_replace(
                &runner,
                &fs,
                Path::new(&config_path),
                &args.old,
                &args.new,
                args.missing_id,
                args.common.dry_run,
                args.common.yes,
                args.common.passphrase_stdin,
                args.common.passphrase_file.as_deref(),
                enroll_kf.as_deref(),
                progress,
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
            if let Err(e) = braid_cli::status::cmd_status(&runner, &fs, &config, args.json) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Doctor(args) => {
            if let Err(e) = cmd_doctor(Path::new(&config_path), args.json) {
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
            let membership = match braid_cli::membership::load_membership() {
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
                &config,
                &membership,
                args.passphrase_stdin,
                args.passphrase_file.as_deref(),
                args.key_file.as_deref(),
                args.allow_degraded,
            ) {
                Ok(()) => {}
                Err(braid_cli::unlock::UnlockError::DegradedRefused(msg)) => {
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
            let membership = match braid_cli::membership::load_membership() {
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
                &membership,
                &key_file_path,
                args.generate,
                args.passphrase_stdin,
                args.passphrase_file.as_deref(),
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Lock => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let membership = match braid_cli::membership::load_membership() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let runner = RealRunner;
            let fs = RealFilesystem;
            if let Err(e) = braid_cli::lock::cmd_lock(&runner, &fs, &config, &membership) {
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
            match braid_cli::monitor::cmd_monitor(&runner, config.mount_point().as_str()) {
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
            if let Err(e) = braid_cli::ack::cmd_ack(&runner, config.mount_point().as_str()) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Tui(args) => {
            let result = if args.demo {
                braid_cli::tui::run_demo()
            } else {
                braid_cli::tui::run(Path::new(&config_path))
            };
            if let Err(e) = result {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::Discover(args) => {
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
                        let m = braid_cli::membership::PoolMembership { disks: members };
                        if let Err(e) = braid_cli::membership::save_membership(&m) {
                            print_cli_error(&format!("failed to write pool membership: {e}"));
                            std::process::exit(1);
                        }
                        eprintln!("pool membership written to /var/lib/braid/pool.json");
                    } else if !args.dry_run {
                        eprintln!("pass --write to persist, or --dry-run to preview");
                    }
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
    let Ok(membership) = braid_cli::membership::load_membership() else {
        return Vec::new();
    };
    membership
        .disks
        .keys()
        .map(|name| CompletionCandidate::new(name.clone()))
        .collect()
}
