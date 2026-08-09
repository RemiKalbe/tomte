//! Self-updater: GitHub Releases → verified staged bundle → swap on restart.
//!
//! Trust chain: the release asset is a notarized, Developer-ID-signed
//! bundle fetched over HTTPS; before anything is staged we re-run
//! `codesign --verify --strict --deep` AND require the TeamIdentifier to
//! match the one baked into THIS binary at release-build time
//! (`TOMTE_TEAM_ID`). Dev builds (no baked team id, or not running from a
//! .app bundle) never self-update.
//!
//! The swap itself runs from the STAGED binary (`tomte --apply-update`) so
//! the bundle being replaced contains no running executable pages we care
//! about; the old daemon is shut down through the normal IPC path (a
//! protocol-mismatch takeover already handles downlevel daemons).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tomte_core::cmd::{CommandRequest, CommandRunner};

/// Repository the updater watches. Single distribution channel by design.
pub const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/RemiKalbe/tomte/releases/latest";

/// Team ID baked at release-build time; `None` in dev builds → updater off.
pub const BUILT_TEAM_ID: Option<&str> = option_env!("TOMTE_TEAM_ID");

/// The running binary's version (workspace-inherited).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: String,
    pub zip_url: String,
    /// Release notes (the GitHub release body — `--generate-notes` output,
    /// hand-editable on GitHub afterwards). Shown in Settings before the
    /// user restarts into the new version.
    pub notes: Option<String>,
}

/// A downloaded, signature-verified bundle waiting for its restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedUpdate {
    pub version: String,
    pub staged: PathBuf,
    pub notes: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("{0}")]
    Check(String),
    #[error("download failed: {0}")]
    Download(String),
    #[error("verification failed: {0}")]
    Verify(String),
}

/// Parse `x.y.z` (a leading `v` is tolerated) into comparable parts.
pub fn parse_semver(s: &str) -> Option<[u64; 3]> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut it = s.split('.');
    let out = [it.next()?, it.next()?, it.next()?];
    if it.next().is_some() {
        return None;
    }
    let mut parts = [0u64; 3];
    for (i, p) in out.iter().enumerate() {
        parts[i] = p.parse().ok()?;
    }
    Some(parts)
}

/// Latest-release JSON → an update STRICTLY newer than `current`, with its
/// `Tomte-<version>.zip` asset. `None` when current is up to date (or the
/// release carries no updater asset — a human-only release must not brick
/// the checker).
pub fn pick_update(latest_json: &str, current: &str) -> Result<Option<AvailableUpdate>, String> {
    let v: serde_json::Value =
        serde_json::from_str(latest_json).map_err(|e| format!("bad release JSON: {e}"))?;
    let tag = v["tag_name"]
        .as_str()
        .ok_or_else(|| "release JSON has no tag_name".to_string())?;
    let (theirs, ours) = match (parse_semver(tag), parse_semver(current)) {
        (Some(t), Some(o)) => (t, o),
        _ => {
            return Err(format!(
                "unparseable versions: tag {tag}, current {current}"
            ));
        }
    };
    if theirs <= ours {
        return Ok(None);
    }
    let version = tag.strip_prefix('v').unwrap_or(tag).to_string();
    let notes = v["body"]
        .as_str()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty());
    let want = format!("Tomte-{version}.zip");
    let zip_url = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(want.as_str()))
        .and_then(|a| a["browser_download_url"].as_str())
        .map(str::to_owned);
    Ok(zip_url.map(|zip_url| AvailableUpdate {
        version,
        zip_url,
        notes,
    }))
}

/// Extract `TeamIdentifier=XXXX` from `codesign -dvv` output.
pub fn team_identifier(codesign_dvv: &str) -> Option<&str> {
    codesign_dvv
        .lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier="))
        .map(str::trim)
}

/// The bundle this executable runs from, if it IS a bundle
/// (`…/Tomte.app/Contents/MacOS/tomte` → `…/Tomte.app`). Plain cargo
/// binaries return `None`, which disables the updater.
pub fn running_bundle(exe: &Path) -> Option<PathBuf> {
    let app = exe.ancestors().nth(3)?;
    (app.extension().is_some_and(|e| e == "app")).then(|| app.to_path_buf())
}

/// Everything the updater needs to run blocking subprocess work off-main.
pub struct Updater {
    pub runner: Arc<dyn CommandRunner>,
    /// `…/Application Support/Tomte/updates`
    pub stage_dir: PathBuf,
}

impl Updater {
    /// Query GitHub for a strictly-newer release. Network-cheap (one JSON).
    pub fn check(&self) -> Result<Option<AvailableUpdate>, UpdateError> {
        let out = self
            .runner
            .run(
                CommandRequest::new("curl")
                    .args([
                        "-fsSL",
                        "--max-time",
                        "30",
                        "-H",
                        "Accept: application/vnd.github+json",
                        RELEASES_LATEST_URL,
                    ])
                    .timeout(Duration::from_secs(35)),
            )
            .map_err(|e| UpdateError::Check(format!("curl: {e}")))?;
        if !out.success() {
            return Err(UpdateError::Check(format!(
                "release check failed: {}",
                out.stderr_utf8().lines().last().unwrap_or("curl error")
            )));
        }
        pick_update(&out.stdout_utf8(), CURRENT_VERSION).map_err(UpdateError::Check)
    }

    /// Download + unpack + verify; returns the staged, verified `Tomte.app`.
    pub fn download_and_stage(&self, update: &AvailableUpdate) -> Result<PathBuf, UpdateError> {
        let team = BUILT_TEAM_ID.ok_or_else(|| {
            UpdateError::Verify("dev build (no baked Team ID) — updater disabled".into())
        })?;
        std::fs::remove_dir_all(&self.stage_dir).ok();
        std::fs::create_dir_all(&self.stage_dir)
            .map_err(|e| UpdateError::Download(format!("stage dir: {e}")))?;
        let zip = self.stage_dir.join("Tomte.zip");
        let out = self
            .runner
            .run(
                CommandRequest::new("curl")
                    .args(["-fSL", "--max-time", "300", "-o"])
                    .arg(zip.display().to_string())
                    .arg(&update.zip_url)
                    .timeout(Duration::from_secs(320)),
            )
            .map_err(|e| UpdateError::Download(format!("curl: {e}")))?;
        if !out.success() {
            return Err(UpdateError::Download(
                out.stderr_utf8()
                    .lines()
                    .last()
                    .unwrap_or("curl error")
                    .into(),
            ));
        }
        let unpacked = self.stage_dir.join("unpacked");
        let out = self
            .runner
            .run(
                CommandRequest::new("ditto")
                    .args(["-x", "-k"])
                    .arg(zip.display().to_string())
                    .arg(unpacked.display().to_string())
                    .timeout(Duration::from_secs(120)),
            )
            .map_err(|e| UpdateError::Download(format!("ditto: {e}")))?;
        if !out.success() {
            return Err(UpdateError::Download(format!(
                "unpack: {}",
                out.stderr_utf8()
            )));
        }
        let app = unpacked.join("Tomte.app");
        self.verify(&app, team)?;
        Ok(app)
    }

    /// Signature must be intact AND from our team — a MITM'd or corrupted
    /// bundle dies here, before it can ever be swapped in.
    fn verify(&self, app: &Path, team: &str) -> Result<(), UpdateError> {
        let out = self
            .runner
            .run(
                CommandRequest::new("codesign")
                    .args(["--verify", "--strict", "--deep"])
                    .arg(app.display().to_string())
                    .timeout(Duration::from_secs(60)),
            )
            .map_err(|e| UpdateError::Verify(format!("codesign: {e}")))?;
        if !out.success() {
            return Err(UpdateError::Verify(format!(
                "signature invalid: {}",
                out.stderr_utf8()
            )));
        }
        let out = self
            .runner
            .run(
                CommandRequest::new("codesign")
                    .args(["-dvv"])
                    .arg(app.display().to_string())
                    .timeout(Duration::from_secs(60)),
            )
            .map_err(|e| UpdateError::Verify(format!("codesign -dvv: {e}")))?;
        // codesign prints details to stderr.
        let details = out.stderr_utf8();
        match team_identifier(&details) {
            Some(t) if t == team => Ok(()),
            Some(t) => Err(UpdateError::Verify(format!(
                "bundle signed by team {t}, expected {team}"
            ))),
            None => Err(UpdateError::Verify("no TeamIdentifier in signature".into())),
        }
    }
}

/// `tomte --apply-update <staged.app> <target.app> <parent-pid>` — runs FROM
/// the staged bundle. Waits for the old app to exit, retires the old bundle,
/// moves the staged one into place, and relaunches. Never returns.
pub fn apply_update_main(staged: &Path, target: &Path, parent_pid: u32) -> ! {
    // Wait (bounded) for the calling app to exit so its bundle is quiescent.
    for _ in 0..100 {
        // SAFETY: kill(pid, 0) only probes liveness.
        if unsafe { libc::kill(parent_pid as i32, 0) } != 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Old bundle out of the way first: a half-swapped state must never leave
    // BOTH bundles at the target path. The retired copy is kept beside the
    // stage as a manual escape hatch until the next successful update.
    let retired = staged.with_file_name("Tomte.app.retired");
    std::fs::remove_dir_all(&retired).ok();
    if target.exists()
        && let Err(e) = std::fs::rename(target, &retired)
    {
        eprintln!("tomte --apply-update: cannot retire old bundle: {e}");
        std::process::exit(1);
    }
    if let Err(e) = std::fs::rename(staged, target) {
        eprintln!("tomte --apply-update: cannot install new bundle: {e}");
        // Roll back so the user still has a working app.
        let _ = std::fs::rename(&retired, target);
        std::process::exit(1);
    }
    let _ = std::process::Command::new("open").arg(target).status();
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_json(tag: &str, assets: &[&str]) -> String {
        let assets: Vec<String> = assets
            .iter()
            .map(|name| format!(r#"{{"name":"{name}","browser_download_url":"https://x/{name}"}}"#))
            .collect();
        format!(
            r#"{{"tag_name":"{tag}","body":"- faster scans\n- bug fixes","assets":[{}]}}"#,
            assets.join(",")
        )
    }

    #[test]
    fn semver_parses_and_orders() {
        assert_eq!(parse_semver("v1.2.3"), Some([1, 2, 3]));
        assert_eq!(parse_semver("0.10.0"), Some([0, 10, 0]));
        assert!(parse_semver("0.10.0") > parse_semver("0.9.9"));
        assert_eq!(parse_semver("1.2"), None);
        assert_eq!(parse_semver("1.2.x"), None);
    }

    #[test]
    fn pick_update_wants_strictly_newer_with_zip_asset() {
        let json = release_json("v0.2.0", &["Tomte-0.2.0.zip", "Tomte-0.2.0.dmg"]);
        let up = pick_update(&json, "0.1.0").unwrap().unwrap();
        assert_eq!(up.version, "0.2.0");
        assert_eq!(up.zip_url, "https://x/Tomte-0.2.0.zip");
        assert_eq!(up.notes.as_deref(), Some("- faster scans\n- bug fixes"));
        // same or older → no update
        assert_eq!(pick_update(&json, "0.2.0").unwrap(), None);
        assert_eq!(pick_update(&json, "0.3.0").unwrap(), None);
        // newer release without the updater asset → quietly nothing
        let human_only = release_json("v0.2.0", &["Tomte-0.2.0.dmg"]);
        assert_eq!(pick_update(&human_only, "0.1.0").unwrap(), None);
    }

    #[test]
    fn team_identifier_reads_codesign_output() {
        let dvv = "Executable=/x\nIdentifier=com.remikalbe.tomte\nTeamIdentifier=ABC1234567\n";
        assert_eq!(team_identifier(dvv), Some("ABC1234567"));
        assert_eq!(team_identifier("Identifier=x\n"), None);
    }

    #[test]
    fn running_bundle_detects_app_layout() {
        assert_eq!(
            running_bundle(Path::new("/Applications/Tomte.app/Contents/MacOS/tomte")),
            Some(PathBuf::from("/Applications/Tomte.app"))
        );
        assert_eq!(running_bundle(Path::new("/repo/target/debug/tomte")), None);
    }
}
