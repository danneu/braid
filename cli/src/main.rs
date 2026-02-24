use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};
use std::path::Path;

use braid_cli::apply::{cmd_apply, ApplyFlags};
use braid_cli::cmd::RealRunner;
use braid_cli::config::config_read;
use braid_cli::doctor::cmd_doctor;
use braid_cli::plan::{compute_plan, format_plan_human, to_plan_report};
use braid_cli::probe::{probe_config_disk, probe_pool, RealFilesystem};
use braid_cli::progress::ProgressMode;
use braid_cli::types::PlanFlags;

#[derive(Debug, Parser)]
#[command(name = "braid", version)]
#[command(about = "braid Rust CLI", long_about = None)]
struct Cli {
    #[arg(long, global = true, default_value = "/etc/braid/config.json")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize and encrypt a new disk
    InitDisk(InitDiskArgs),
    /// Show what changes would be applied
    Plan(PlanArgs),
    /// Apply planned changes to the pool
    Apply(ApplyArgs),
    /// Show pool health and disk info
    Status(StatusArgs),
    /// Check configuration for problems
    Doctor(DoctorArgs),
}

#[derive(Debug, Args)]
struct InitDiskArgs {
    // Provide dynamic tab-completion candidates for disk paths, read from config.
    #[arg(add = ArgValueCandidates::new(disk_candidates))]
    by_id_path: String,
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct PlanArgs {
    #[arg(long)]
    json: bool,
    #[arg(long)]
    allow_remove_missing: bool,
    #[arg(long)]
    allow_remove_ambiguous: bool,
}

#[derive(Debug, Args)]
struct ApplyArgs {
    #[arg(long)]
    resume: bool,
    #[arg(long)]
    allow_remove_missing: bool,
    #[arg(long)]
    allow_remove_ambiguous: bool,
    #[arg(long, value_enum, default_value_t = ProgressMode::Auto)]
    progress: ProgressMode,
    #[arg(long)]
    json: bool,
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
    // Normal invocations pass through unchanged.
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
        Commands::InitDisk(args) => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            let runner = RealRunner;
            let fs = RealFilesystem;
            if let Err(e) = braid_cli::init_disk::cmd_init_disk(
                &runner, &fs, &config, &args.by_id_path, args.force,
            ) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Plan(args) => {
            let config = match config_read(Path::new(&config_path)) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };

            let runner = RealRunner;
            let fs = RealFilesystem;

            let config_disks: Vec<_> = match config
                .disks()
                .iter()
                .map(|d| probe_config_disk(&runner, &fs, d))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(disks) => disks,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };

            let pool = match probe_pool(&runner, config.mount_point()) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };

            let flags = PlanFlags {
                allow_remove_missing: args.allow_remove_missing,
                allow_remove_ambiguous: args.allow_remove_ambiguous,
            };

            let outcome = compute_plan(&config, &config_disks, &pool, &flags);
            let report = to_plan_report(&outcome, &config);

            if args.json {
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("error: failed to serialize plan: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                print!("{}", format_plan_human(&report));
            }
        }
        Commands::Apply(args) => {
            let flags = ApplyFlags {
                resume: args.resume,
                allow_remove_missing: args.allow_remove_missing,
                allow_remove_ambiguous: args.allow_remove_ambiguous,
                progress: args.progress,
                json: args.json,
            };
            if let Err(e) = cmd_apply(Path::new(&config_path), &flags) {
                eprintln!("error: {e}");
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
            if let Err(e) = braid_cli::status::cmd_status(&runner, &fs, &config, args.verbose, args.json) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Doctor(args) => {
            if let Err(e) = cmd_doctor(Path::new(&config_path), args.json) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Scan the in-flight command line for `--config <path>` or `--config=<path>`.
/// Fall back to the default `/etc/braid/config.json` if not found.
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

fn disk_candidates() -> Vec<CompletionCandidate> {
    let config_path = completion_config_path();
    let Ok(config) = config_read(Path::new(&config_path)) else {
        return Vec::new();
    };
    config
        .disks()
        .iter()
        .map(|d| CompletionCandidate::new(d.0.clone()))
        .collect()
}


