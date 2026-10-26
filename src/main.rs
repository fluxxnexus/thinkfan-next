mod logging;

use anyhow::{Result, ensure};
use clap::{ArgAction, Parser};
use std::{path::PathBuf, time::Duration};
use thinkfan_next::{
    config::Config,
    runtime::{self, Controller, Options},
};

#[derive(Parser, Debug)]
#[command(version, about = "Modern Linux fan control; YAML configuration only")]
struct Cli {
    #[arg(short, long, default_value = "/etc/thinkfan.yaml")]
    config: PathBuf,
    /// Maximum sampling interval in seconds (0.1..60)
    #[arg(short = 's', long, default_value_t = 5.0)]
    interval: f64,
    /// Signed rise prediction factor; horizon = bias/2 seconds (-10..30)
    #[arg(short, long, default_value_t = 0.0, allow_hyphen_values = true)]
    bias: f64,
    /// Validate YAML and policy without accessing hardware
    #[arg(long, conflicts_with = "check_hardware")]
    check: bool,
    /// Validate config and read hardware without changing fan state
    #[arg(long)]
    check_hardware: bool,
    /// Read sensors and print decisions, but never write to fans
    #[arg(long)]
    dry_run: bool,
    /// Run one control cycle and restore initial state
    #[arg(long)]
    once: bool,
    /// Stay in foreground (the default; provided for service migration)
    #[arg(short = 'n', long)]
    foreground: bool,
    #[arg(short, long, action = ArgAction::Count, conflicts_with = "quiet")]
    verbose: u8,
    #[arg(short, long)]
    quiet: bool,
    /// Log destination; auto detects stderr connected to journald
    #[arg(long, value_enum, default_value = "auto")]
    log_output: logging::LogOutput,
    /// Do not wake sleeping SMART disks
    #[arg(short = 'd', long)]
    no_disk_wakeup: bool,
    /// ThinkPad depulse duration, in seconds; -p defaults to 0.5
    #[arg(short = 'p', long, num_args = 0..=1, default_missing_value = "0.5", default_value = "0")]
    depulse: f64,
    /// ThinkPad kernel watchdog timeout, seconds
    #[arg(long, default_value_t = 120)]
    watchdog: u32,
    #[arg(long, default_value = "/run/thinkfan-next.lock")]
    lock_file: PathBuf,
}
fn execute(cli: Cli) -> Result<()> {
    ensure!(
        cli.interval.is_finite() && (0.1..=60.0).contains(&cli.interval),
        "interval must be 0.1..60 seconds"
    );
    ensure!(
        cli.depulse.is_finite() && (0.0..=10.0).contains(&cli.depulse),
        "depulse must be 0..10 seconds"
    );
    let options = Options {
        interval: Duration::from_secs_f64(cli.interval),
        bias: cli.bias,
        watchdog: cli.watchdog,
        depulse: Duration::from_secs_f64(cli.depulse),
        no_disk_wakeup: cli.no_disk_wakeup,
        dry_run: cli.dry_run,
        verbose: cli.verbose > 0,
    };
    options.validate()?;
    if cli.check {
        let config = Config::load(&cli.config)?;
        println!(
            "YAML configuration valid ({} sensors, {} fans)",
            config.sensors.len(),
            config.fans.len()
        );
        if config.sensors.iter().any(|s| s.width().is_none()) {
            println!("tpacpi channel count will be checked during hardware preparation");
        }
        return Ok(());
    }
    if cli.check_hardware {
        Controller::prepare(Config::load(&cli.config)?, options)?.check_hardware()?;
        println!("configuration and hardware checks passed; no fan writes");
        return Ok(());
    }
    let _lock = if cli.dry_run {
        None
    } else {
        Some(runtime::acquire_lock(&cli.lock_file)?)
    };
    if !cli.quiet {
        tracing::info!(version = env!("CARGO_PKG_VERSION"), config = %cli.config.display(),
            dry_run = cli.dry_run, "starting thinkfan-next");
    }
    runtime::run(&cli.config, options, cli.once)
}
fn main() {
    let cli = Cli::parse();
    logging::init(cli.verbose, cli.log_output);
    if let Err(error) = execute(cli) {
        tracing::error!(error = %format_args!("{error:#}"), "thinkfan-next failed");
        std::process::exit(1);
    }
}
