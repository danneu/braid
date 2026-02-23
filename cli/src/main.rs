use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "braid")]
#[command(about = "braid Rust CLI", long_about = None)]
struct Cli {
    #[arg(long, default_value = "/etc/braid/config.json")]
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

    let _config_path = cli.config;

    match cli.command {
        Commands::InitDisk(_) => println!("not yet implemented"),
        Commands::Plan(_) => println!("not yet implemented"),
        Commands::Apply(_) => println!("not yet implemented"),
        Commands::Status(_) => println!("not yet implemented"),
    }
}
