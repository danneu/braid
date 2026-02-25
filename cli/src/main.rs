use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};
use std::path::Path;

use braid_cli::checkpoint::CHECKPOINT_FILE;
use braid_cli::cmd::RealRunner;
use braid_cli::config::config_read;
use braid_cli::doctor::cmd_doctor;
use braid_cli::probe::RealFilesystem;
use braid_cli::progress::{ProgressMode, resolve_progress_output};

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
    /// Add a disk to the pool
    Add(AddArgs),
    /// Remove a disk from the pool
    Remove(RemoveArgs),
    /// Remove a missing/dead device from the pool
    RemoveMissing(RemoveMissingArgs),
    /// Replace a disk with a new one
    Replace(ReplaceArgs),
    /// Show pool health and disk info
    Status(StatusArgs),
    /// Check configuration for problems
    Doctor(DoctorArgs),
    /// Interactive terminal dashboard
    Tui,
}

#[derive(Debug, Args)]
struct CommonArgs {
    /// Show what would happen without executing
    #[arg(long)]
    dry_run: bool,
    /// Skip interactive confirmations (requires BRAID_PASSPHRASE or --passphrase-file)
    #[arg(long)]
    yes: bool,
    /// Read passphrase from file instead of TTY prompt
    #[arg(long)]
    passphrase_file: Option<std::path::PathBuf>,
    /// Progress display mode
    #[arg(long, value_enum, default_value_t = ProgressMode::Auto)]
    progress: ProgressMode,
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Disk key (as defined in braid.disks)
    #[arg(add = ArgValueCandidates::new(disk_key_candidates))]
    key: String,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, Args)]
struct RemoveArgs {
    /// Disk key to remove
    #[arg(add = ArgValueCandidates::new(disk_key_candidates))]
    key: String,
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
    /// Disk key of the disk to replace
    #[arg(long, add = ArgValueCandidates::new(disk_key_candidates))]
    old: String,
    /// Disk key of the new replacement disk
    #[arg(long, add = ArgValueCandidates::new(disk_key_candidates))]
    new: String,
    /// Target a specific missing device by btrfs devid (dead disk only)
    #[arg(long)]
    missing_id: Option<u64>,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(long)]
    verbose: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    #[arg(long)]
    json: bool,
}

fn main() {
    // When COMPLETE=bash|zsh|fish is set, produce shell completion output and exit.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    // Parse before root gate so --help/--version work without sudo.
    let cli = Cli::parse();

    // SAFETY: geteuid() is a trivial syscall with no arguments, always safe to call.
    if unsafe { libc::geteuid() } != 0 {
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
            if let Err(e) = braid_cli::add::cmd_add(
                &runner,
                &fs,
                Path::new(&config_path),
                &args.key,
                args.common.dry_run,
                args.common.yes,
                args.common.passphrase_file.as_deref(),
                progress,
                Path::new(CHECKPOINT_FILE),
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
                &args.key,
                args.common.dry_run,
                args.common.yes,
                progress,
                Path::new(CHECKPOINT_FILE),
            ) {
                print_cli_error(&e.to_string());
                std::process::exit(1);
            }
        }
        Commands::RemoveMissing(args) => {
            let runner = RealRunner;
            if let Err(e) = braid_cli::remove_missing::cmd_remove_missing(
                &runner,
                Path::new(&config_path),
                args.missing_id,
                args.common.dry_run,
                args.common.yes,
                Path::new(CHECKPOINT_FILE),
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
            if let Err(e) = braid_cli::replace::cmd_replace(
                &runner,
                &fs,
                Path::new(&config_path),
                &args.old,
                &args.new,
                args.missing_id,
                args.common.dry_run,
                args.common.yes,
                args.common.passphrase_file.as_deref(),
                progress,
                Path::new(CHECKPOINT_FILE),
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
            if let Err(e) =
                braid_cli::status::cmd_status(&runner, &fs, &config, args.verbose, args.json)
            {
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
        Commands::Tui => {
            if let Err(e) = braid_cli::tui::run(Path::new(&config_path)) {
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

/// Scan the in-flight command line for `--config <path>` or `--config=<path>`.
fn completion_config_path() -> String {
    let args: Vec<String> = std::env::args().collect();
    for pair in args.windows(2) {
        if pair[0] == "--config" {
            return pair[1].clone();
        }
    }
    for arg in &args {
        if let Some(val) = arg.strip_prefix("--config=") {
            return val.to_string();
        }
    }
    "/etc/braid/config.json".to_string()
}

/// Tab completion returns disk keys from config.json keys.
fn disk_key_candidates() -> Vec<CompletionCandidate> {
    let config_path = completion_config_path();
    let Ok(config) = config_read(Path::new(&config_path)) else {
        return Vec::new();
    };
    config
        .keys()
        .into_iter()
        .map(|name| CompletionCandidate::new(name.clone()))
        .collect()
}
