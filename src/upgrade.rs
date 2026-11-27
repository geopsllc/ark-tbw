//! `ark-tbw upgrade`: finds the newest release of this branch on GitHub and runs this branch's
//! install.sh, which downloads and verifies the binary, keeps config, secret and database, and
//! restarts the service. One install path for both a fresh install and an upgrade.

use crate::service::{self, Mode};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const REPO: &str = "geopsllc/ark-tbw";
const BRANCH: &str = "mainsail";
const TAG_PREFIX: &str = "mainsail-v";

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("ark-tbw/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// "1.2.3" -> (1, 2, 3); anything else sorts lowest
fn parse_ver(v: &str) -> (u64, u64, u64) {
    let mut it = v.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// The newest published release tag of this branch (same lookup as install.sh)
fn latest_tag() -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=50");
    let list: Value = agent()
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("cannot reach the GitHub release list")?
        .into_json()?;
    list.as_array()
        .ok_or_else(|| anyhow!("unexpected answer from GitHub"))?
        .iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(false))
        .filter_map(|r| r["tag_name"].as_str())
        .filter(|t| t.strip_prefix(TAG_PREFIX).is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit())))
        .max_by_key(|t| parse_ver(&t[TAG_PREFIX.len()..]))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no {TAG_PREFIX}* release published yet"))
}

pub fn run(config: &Path, check_only: bool, version: Option<&str>, from_source: bool, force: bool) -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        bail!("run  ark-tbw upgrade  as the account that owns ark-tbw, not as root (it uses sudo when needed)");
    }
    let current = env!("CARGO_PKG_VERSION");
    let tag = match version {
        Some(t) if t.starts_with(TAG_PREFIX) => t.to_string(),
        Some(t) => format!("{TAG_PREFIX}{}", t.trim_start_matches('v')),
        None => latest_tag()?,
    };
    let target = &tag[TAG_PREFIX.len()..];
    println!("installed  {current}");
    println!("{:<10} {target} ({tag})", if version.is_some() { "requested" } else { "latest" });
    let newer = parse_ver(target) > parse_ver(current);
    if check_only {
        println!("{}", if newer { "an upgrade is available: ark-tbw upgrade" } else { "up to date" });
        return Ok(());
    }
    if version.is_none() && !newer && !force {
        println!("up to date (ark-tbw upgrade --force reinstalls it)");
        return Ok(());
    }

    let cfg_dir = config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ".".into());
    let cfg_dir = std::fs::canonicalize(&cfg_dir).with_context(|| format!("no config folder {}", cfg_dir.display()))?;
    if config.file_name().is_some_and(|n| n != "config.toml") {
        bail!("the installer expects the config at <folder>/config.toml, not {}", config.display());
    }
    if !cfg_dir.join("config.toml").exists() {
        bail!("no config at {}: install with install.sh first", cfg_dir.join("config.toml").display());
    }

    // how it was installed: a user service or ~/.local/bin means a rootless install
    let mode = service::installed();
    let exe = std::env::current_exe().ok();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let in_local_bin = matches!((&exe, &home), (Some(e), Some(h)) if e.starts_with(h.join(".local/bin")));
    let rootless = matches!(mode, Some(Mode::User)) || (mode.is_none() && in_local_bin);

    let url = format!("https://raw.githubusercontent.com/{REPO}/{BRANCH}/install.sh");
    let script = agent().get(&url).call().context("cannot download install.sh")?.into_string()?;
    if !script.starts_with("#!") || !script.contains(&format!("TAG_PREFIX={TAG_PREFIX}")) {
        bail!("{url} does not look like this branch's installer");
    }
    let dir = std::env::temp_dir().join(format!("ark-tbw-upgrade-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("install.sh");
    std::fs::write(&path, script)?;

    let mut cmd = Command::new("bash");
    cmd.arg(&path).arg("--dir").arg(&cfg_dir).args(["--version", &tag]);
    if rootless {
        cmd.arg("--rootless");
    }
    if mode.is_none() {
        cmd.arg("--no-service");
    }
    if from_source {
        cmd.arg("--from-source");
    }
    println!("upgrading to {tag} with {url}\n");
    let st = cmd.status().context("cannot run bash");
    let _ = std::fs::remove_dir_all(&dir);
    if !st?.success() {
        bail!("the installer failed (the service keeps running the old version if it was not replaced)");
    }
    Ok(())
}
