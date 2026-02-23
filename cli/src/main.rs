use clap::{Args, Parser, Subcommand};
use std::path::Path;

use braid_cli::apply::{cmd_apply, ApplyFlags};
use braid_cli::cmd::RealRunner;
use braid_cli::config::config_read;
use braid_cli::plan::{compute_plan, format_plan_human, to_plan_report};
use braid_cli::probe::{probe_config_disk, probe_pool, RealFilesystem};
use braid_cli::types::PlanFlags;

#[derive(Debug, Parser)]
#[command(name = "braid")]
#[command(about = "braid Rust CLI", long_about = None)]
struct Cli {
    #[arg(long, global = true, default_value = "/etc/braid/config.json")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    InitDisk(InitDiskArgs),
    Plan(PlanArgs),
    Apply(ApplyArgs),
    Status(StatusArgs),
}

#[derive(Debug, Args)]
struct InitDiskArgs {
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
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(long)]
    verbose: bool,
    #[arg(long)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();

    let config_path = cli.config;

    match cli.command {
        Commands::InitDisk(_) => println!("not yet implemented"),
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
                .disks
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

            let pool = match probe_pool(&runner, &config.mount_point) {
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
            };
            if let Err(e) = cmd_apply(Path::new(&config_path), &flags) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Status(_) => println!("not yet implemented"),
    }
}
