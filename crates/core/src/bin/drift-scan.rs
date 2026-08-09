use std::process::ExitCode;
use std::sync::Arc;

use tomte_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use tomte_core::cmd::SystemRunner;
use tomte_core::git::GitClient;
use tomte_core::scanner::DriftScanner;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let fetch = args.iter().any(|a| a == "--fetch");

    let runner = Arc::new(SystemRunner);
    let chezmoi = ChezmoiClient::new(runner.clone(), ChezmoiOptions::default());
    let source_dir = match chezmoi.source_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot locate chezmoi source dir: {e}");
            return ExitCode::FAILURE;
        }
    };
    let git = GitClient::new(runner, source_dir);
    let branch = git.head_branch().unwrap_or_else(|_| "main".into());
    let remote_ref = format!("origin/{branch}");
    if fetch && let Err(e) = git.fetch("origin") {
        eprintln!("warning: fetch failed, remote info may be stale: {e}");
    }
    let scanner = DriftScanner::new(chezmoi, git, remote_ref);
    match scanner.scan() {
        Ok(report) => {
            if json {
                // hand-rolled to avoid serde derives on domain types for now
                println!(
                    "{{\"drifted\":{},\"in_sync\":{},\"degraded\":{}}}",
                    report.drifted.len(),
                    report.in_sync_count,
                    report.degraded.is_some()
                );
            }
            if let Some(f) = &report.degraded {
                eprintln!(
                    "degraded scan: {} — {}",
                    f.hint,
                    f.raw_stderr.lines().next().unwrap_or("")
                );
            }
            for d in &report.drifted {
                println!("{:<22} {}", format!("{:?}", d.class), d.target.display());
            }
            println!(
                "-- {} drifted, {} in sync",
                report.drifted.len(),
                report.in_sync_count
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("scan failed: {e}");
            ExitCode::FAILURE
        }
    }
}
