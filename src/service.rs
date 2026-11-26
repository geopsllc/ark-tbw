//! systemd integration. Works with the systemd shipped by Ubuntu 22.04 (249), 24.04 (255)
//! and 26.04: only long-standing unit directives are used.

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const UNIT: &str = "ark-tbw";
const UNIT_PATH: &str = "/etc/systemd/system/ark-tbw.service";

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// Write /etc/systemd/system/ark-tbw.service and enable it at boot
    Install {
        /// Account the service runs as (default: the user who ran sudo)
        #[arg(long)]
        user: Option<String>,
    },
    /// Stop, disable and remove the unit (the database and config stay)
    Uninstall,
    /// Print the unit file that install would write
    Show {
        #[arg(long)]
        user: Option<String>,
    },
}

fn home_of(user: &str) -> Option<PathBuf> {
    let out = Command::new("getent").args(["passwd", user]).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).to_string();
    line.trim().split(':').nth(5).map(PathBuf::from)
}

pub fn unit_text(user: &str, exe: &Path, config: &Path) -> String {
    format!(
        "[Unit]
Description=ark-tbw true block weight reward sharing
Documentation=file://{config}
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=900
StartLimitBurst=5

[Service]
Type=simple
User={user}
ExecStart={exe} run --config {config}
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=30
# exit code 2 = bad config or a chain reorganisation that needs a look: do not loop
RestartPreventExitStatus=2
TimeoutStopSec=120
UMask=0077
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=full
ProtectKernelTunables=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes

[Install]
WantedBy=multi-user.target
",
        exe = exe.display(),
        config = config.display()
    )
}

fn resolve(user: Option<&String>, config: &Path) -> Result<(String, PathBuf, PathBuf)> {
    let user = match user {
        Some(u) => u.clone(),
        None => std::env::var("SUDO_USER").ok().filter(|u| u != "root").ok_or_else(|| anyhow!("run with sudo from the account that owns the config, or pass --user"))?,
    };
    let exe = std::env::current_exe()?.canonicalize()?;
    // a relative or default config path is resolved for the service user, not root
    let config = if config.is_absolute() && !config.starts_with("/root") {
        config.to_path_buf()
    } else {
        let home = home_of(&user).ok_or_else(|| anyhow!("unknown user {user}"))?;
        if config.is_absolute() {
            home.join(".ark-tbw/config.toml")
        } else {
            home.join(config)
        }
    };
    Ok((user, exe, config))
}

fn sh(cmd: &str, args: &[&str]) -> Result<()> {
    let st = Command::new(cmd).args(args).status().with_context(|| format!("cannot run {cmd}"))?;
    if !st.success() {
        bail!("{cmd} {} failed", args.join(" "));
    }
    Ok(())
}

pub fn run(action: &ServiceCmd, config: &Path) -> Result<()> {
    match action {
        ServiceCmd::Show { user } => {
            let (u, exe, cfg) = resolve(user.as_ref().or(Some(&std::env::var("USER").unwrap_or_default())), config)?;
            print!("{}", unit_text(&u, &exe, &cfg));
        }
        ServiceCmd::Install { user } => {
            if unsafe { libc::geteuid() } != 0 {
                bail!("run it with sudo:  sudo {} service install", std::env::current_exe()?.display());
            }
            let (u, exe, cfg) = resolve(user.as_ref(), config)?;
            if !cfg.exists() {
                bail!("config {} not found (create it first, see config.example.toml)", cfg.display());
            }
            if exe.starts_with("/home") || exe.starts_with("/root") {
                eprintln!("note: the service runs {} — consider  sudo install -m 755 {} /usr/local/bin/ark-tbw  and installing from there", exe.display(), exe.display());
            }
            std::fs::write(UNIT_PATH, unit_text(&u, &exe, &cfg))?;
            sh("systemctl", &["daemon-reload"])?;
            sh("systemctl", &["enable", UNIT])?;
            println!("installed {UNIT_PATH} (runs as {u}, config {})", cfg.display());
            println!("start it:   sudo systemctl start ark-tbw    (or: ark-tbw start)");
            println!("follow it:  ark-tbw logs -f");
        }
        ServiceCmd::Uninstall => {
            if unsafe { libc::geteuid() } != 0 {
                bail!("run it with sudo");
            }
            let _ = sh("systemctl", &["disable", "--now", UNIT]);
            if Path::new(UNIT_PATH).exists() {
                std::fs::remove_file(UNIT_PATH)?;
            }
            sh("systemctl", &["daemon-reload"])?;
            println!("removed the ark-tbw service; config and database are untouched");
        }
    }
    Ok(())
}

/// start / stop / restart / reload through systemctl, with sudo when not root.
pub fn systemctl(verb: &str) -> Result<()> {
    if !Path::new(UNIT_PATH).exists() {
        bail!("the service is not installed: sudo ark-tbw service install");
    }
    let root = unsafe { libc::geteuid() } == 0;
    let st = if root { Command::new("systemctl").args([verb, UNIT]).status() } else { Command::new("sudo").args(["systemctl", verb, UNIT]).status() }
        .context("cannot run systemctl")?;
    if !st.success() {
        bail!("systemctl {verb} {UNIT} failed");
    }
    let _ = Command::new("systemctl").args(["--no-pager", "--lines=0", "status", UNIT]).status();
    Ok(())
}

/// The service log from journald. Uses sudo only if the user cannot read the system journal.
pub fn logs(follow: bool, lines: u32, since: Option<&str>) -> Result<()> {
    let mut args: Vec<String> = vec!["-u".into(), UNIT.into(), "-o".into(), "short-iso".into(), "--no-pager".into(), "-n".into(), lines.to_string()];
    if follow {
        args.push("-f".into());
    }
    if let Some(s) = since {
        args.push("--since".into());
        args.push(s.into());
    }
    let can_read = unsafe { libc::geteuid() } == 0
        || Command::new("id").arg("-nG").output().map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().any(|g| g == "adm" || g == "systemd-journal")).unwrap_or(false);
    let st = if can_read {
        Command::new("journalctl").args(&args).status()
    } else {
        Command::new("sudo").arg("journalctl").args(&args).status()
    }
    .context("cannot run journalctl")?;
    if !st.success() {
        bail!("journalctl failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn unit_has_essentials() {
        let u = super::unit_text("ark", std::path::Path::new("/usr/local/bin/ark-tbw"), std::path::Path::new("/home/ark/.ark-tbw/config.toml"));
        assert!(u.contains("ExecStart=/usr/local/bin/ark-tbw run --config /home/ark/.ark-tbw/config.toml"));
        assert!(u.contains("ExecReload=/bin/kill -HUP $MAINPID"));
        assert!(u.contains("RestartPreventExitStatus=2"));
        assert!(u.contains("User=ark"));
    }
}
