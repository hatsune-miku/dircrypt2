#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
compile_error!("Supported operating systems: Windows, macOS and Linux.");

mod engine;
mod native;
mod progress;
mod store;

pub use engine::{Options, Outcome, execute};

use anyhow::{Context, ensure};
use clap::Parser;
use std::io::Write;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Parser)]
#[command(
    version,
    about = "Reversibly rename a directory tree; DCDATA selects automatic restoration. This is obfuscation, not cryptographic encryption."
)]
struct Cli {
    /// Directory to process
    path: PathBuf,
    /// Also change the first 16 bytes of regular files larger than 16 bytes
    #[arg(long)]
    obfuscate: bool,
    /// ASCII suffix for mapped names, e.g. .bin (ignored when restoring)
    #[arg(long, default_value = "")]
    suffix: String,
    /// Number of independent directory workers; 0 selects a filesystem default
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u16).range(0..=16))]
    jobs: u16,
    /// Suppress periodic progress (errors and final summary remain visible)
    #[arg(long)]
    quiet: bool,
    /// Write JSON timings to a new file outside the target directory
    #[arg(long)]
    report: Option<PathBuf>,
}

pub fn run_cli() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut report = if let Some(path) = cli.report {
        let target = std::fs::canonicalize(&cli.path)?;
        let absolute = std::path::absolute(path)?;
        let parent = std::fs::canonicalize(
            absolute
                .parent()
                .context("Report needs a parent directory")?,
        )?;
        ensure!(
            !parent.starts_with(target),
            "Report must be outside the directory being processed"
        );
        Some(
            std::fs::File::create_new(
                parent.join(absolute.file_name().context("Report needs a filename")?),
            )
            .context("Creating report; existing files are never overwritten")?,
        )
    } else {
        None
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = cancelled.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
    })?;
    let options = Options {
        obfuscate: cli.obfuscate,
        suffix: cli.suffix,
        jobs: cli.jobs as usize,
        quiet: cli.quiet,
        cancelled,
    };
    let outcome = execute(&cli.path, options)?;
    eprintln!(
        "[INFO] {}: {} files, {} directories, {} links in {:.2}s",
        outcome.action, outcome.files, outcome.directories, outcome.links, outcome.seconds
    );
    if let Some(file) = &mut report {
        file.write_all(&serde_json::to_vec_pretty(&outcome)?)
            .context("Operation completed, but writing the timing report failed")?;
    }
    Ok(())
}
