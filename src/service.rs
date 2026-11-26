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
    /// Install the service and enable it at boot. Default: a system service
    /// (/etc/systemd/system, needs sudo). --rootless: a user service, no sudo.
    Install {
        /// Account the system service runs as (default: the user who ran sudo)
        #[arg(long, conflicts_with = "rootless")]
        user: Option<String>,
        /// Install as a systemd user service of your own account (~/.config/systemd/user)
        #[arg(long)]
        rootless: bool,
    },
    /// Stop, disable and remove the service (the database and config stay)
    Uninstall,
    /// Print the unit file that install would write
    Show {
        #[arg(long, conflicts_with = "rootless")]
        user: Option<String>,
        #[arg(long)]
        rootless: bool,
    },
}

/// Which kind of service is installed for the current account.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    System,
    User,
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn user_unit_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("systemd/user/ark-tbw.service"))
}

/// The installed service for this account: the user service when run as a normal user that
/// has one, else the system service.
pub fn installed() -> Option<Mode> {
    if !is_root() && user_unit_path().is_some_and(|p| p.exists()) {
        return Some(Mode::User);
    }
    Path::new(UNIT_PATH).exists().then_some(Mode::System)
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

/// A user unit runs as its owner already. Sandboxing options that need namespaces are left
/// out: unprivileged user managers cannot always set them up (AppArmor on Ubuntu 24.04+).
pub fn user_unit_text(exe: &Path, config: &Path) -> String {
    format!(
        "[Unit]
Description=ark-tbw true block weight reward sharing
Documentation=file://{config}
StartLimitIntervalSec=900
StartLimitBurst=5

[Service]
Type=simple
ExecStart={exe} run --config {config}
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=30
# exit code 2 = bad config or a chain reorganisation that needs a look: do not loop
RestartPreventExitStatus=2
TimeoutStopSec=120
UMask=0077
NoNewPrivileges=yes

[Install]
WantedBy=default.target
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

fn resolve_own(config: &Path) -> Result<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let config = if config.is_absolute() { config.to_path_buf() } else { std::env::current_dir()?.join(config) };
    Ok((exe, config))
}

fn sh(cmd: &str, args: &[&str]) -> Result<()> {
    let st = Command::new(cmd).args(args).status().with_context(|| format!("cannot run {cmd}"))?;
    if !st.success() {
        bail!("{cmd} {} failed", args.join(" "));
    }
    Ok(())
}

fn user_manager_hint() -> &'static str {
    "cannot reach your systemd user manager. Log in to this account directly (ssh user@host), \
     not through su or sudo -u, so XDG_RUNTIME_DIR is set"
}

/// After `su` / `sudo -u` XDG_RUNTIME_DIR is not set, but the user manager may still be
/// running (lingering): point at its runtime directory then.
fn find_runtime_dir() -> bool {
    if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
        return true;
    }
    let dir = PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() }));
    if dir.join("systemd/private").exists() {
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        return true;
    }
    false
}

fn user_systemctl(args: &[&str]) -> Result<()> {
    find_runtime_dir();
    let st = Command::new("systemctl").arg("--user").args(args).status().context("cannot run systemctl")?;
    if !st.success() {
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            bail!("systemctl --user {} failed: {}", args.join(" "), user_manager_hint());
        }
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

fn whoami() -> String {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        Command::new("id").arg("-un").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
    })
}

fn linger_enabled(user: &str) -> bool {
    Command::new("loginctl")
        .args(["show-user", user, "--property=Linger", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false)
}

fn install_rootless(config: &Path) -> Result<()> {
    if is_root() {
        bail!("--rootless installs a service for your own account: run it without sudo");
    }
    if Path::new(UNIT_PATH).exists() {
        bail!("a system service is already installed ({UNIT_PATH}); remove it first: sudo ark-tbw service uninstall");
    }
    let (exe, cfg) = resolve_own(config)?;
    if !cfg.exists() {
        bail!("config {} not found (create it first, see config.example.toml)", cfg.display());
    }
    if !find_runtime_dir() {
        bail!("{}", user_manager_hint());
    }
    let path = user_unit_path().ok_or_else(|| anyhow!("HOME is not set"))?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, user_unit_text(&exe, &cfg))?;
    user_systemctl(&["daemon-reload"])?;
    user_systemctl(&["enable", UNIT])?;
    println!("installed {} (runs as you, config {})", path.display(), cfg.display());

    // Without lingering, a user service only runs while you are logged in.
    let me = whoami();
    if linger_enabled(&me) {
        println!("lingering is on: the service starts at boot and keeps running after you log out");
    } else if Command::new("loginctl").args(["enable-linger", &me]).status().map(|s| s.success()).unwrap_or(false) && linger_enabled(&me) {
        println!("turned on lingering: the service starts at boot and keeps running after you log out");
    } else {
        println!();
        println!("IMPORTANT: lingering is off, so the service stops when you log out and does not start at boot.");
        println!("Turn it on once (needs an admin):   sudo loginctl enable-linger {me}");
        println!();
    }
    println!("start it:   ark-tbw start");
    println!("follow it:  ark-tbw logs -f");
    Ok(())
}

pub fn run(action: &ServiceCmd, config: &Path) -> Result<()> {
    match action {
        ServiceCmd::Show { user, rootless: true } => {
            let _ = user;
            let (exe, cfg) = resolve_own(config)?;
            print!("{}", user_unit_text(&exe, &cfg));
        }
        ServiceCmd::Show { user, rootless: false } => {
            let (u, exe, cfg) = resolve(user.as_ref().or(Some(&std::env::var("USER").unwrap_or_default())), config)?;
            print!("{}", unit_text(&u, &exe, &cfg));
        }
        ServiceCmd::Install { rootless: true, .. } => install_rootless(config)?,
        ServiceCmd::Install { user, rootless: false } => {
            if !is_root() {
                bail!(
                    "run it with sudo:  sudo {} service install\n(or, without sudo, as a user service:  ark-tbw service install --rootless)",
                    std::env::current_exe()?.display()
                );
            }
            let (u, exe, cfg) = resolve(user.as_ref(), config)?;
            if !cfg.exists() {
                bail!("config {} not found (create it first, see config.example.toml)", cfg.display());
            }
            if home_of(&u).map(|h| h.join(".config/systemd/user/ark-tbw.service").exists()).unwrap_or(false) {
                bail!("{u} already has a rootless user service; remove it first (as {u}): ark-tbw service uninstall");
            }
            if exe.starts_with("/home") || exe.starts_with("/root") {
                eprintln!("note: the service runs {} — consider  sudo install -m 755 {} /usr/local/bin/ark-tbw  and installing from there", exe.display(), exe.display());
            }
            std::fs::write(UNIT_PATH, unit_text(&u, &exe, &cfg))?;
            sh("systemctl", &["daemon-reload"])?;
            sh("systemctl", &["enable", UNIT])?;
            println!("installed {UNIT_PATH} (runs as {u}, config {})", cfg.display());
            println!("start it:   ark-tbw start");
            println!("follow it:  ark-tbw logs -f");
        }
        ServiceCmd::Uninstall => match installed() {
            Some(Mode::User) => {
                let _ = user_systemctl(&["disable", "--now", UNIT]);
                std::fs::remove_file(user_unit_path().unwrap())?;
                user_systemctl(&["daemon-reload"])?;
                println!("removed the ark-tbw user service; config and database are untouched");
                println!("(lingering is left as it is; turn it off with  loginctl disable-linger  if nothing else needs it)");
            }
            Some(Mode::System) => {
                if !is_root() {
                    bail!("the system service needs sudo:  sudo ark-tbw service uninstall");
                }
                let _ = sh("systemctl", &["disable", "--now", UNIT]);
                std::fs::remove_file(UNIT_PATH)?;
                sh("systemctl", &["daemon-reload"])?;
                println!("removed the ark-tbw service; config and database are untouched");
            }
            None => println!("no ark-tbw service is installed"),
        },
    }
    Ok(())
}

/// start / stop / restart / reload through systemctl: `--user` for a rootless service, sudo
/// for the system service when not root.
pub fn systemctl(verb: &str) -> Result<()> {
    match installed() {
        None => bail!("the service is not installed:  sudo ark-tbw service install   (or  ark-tbw service install --rootless)"),
        Some(Mode::User) => {
            user_systemctl(&[verb, UNIT])?;
            // find_runtime_dir() ran, so the status call below reaches the same manager
            let _ = Command::new("systemctl").args(["--user", "--no-pager", "--lines=0", "status", UNIT]).status();
        }
        Some(Mode::System) => {
            let st = if is_root() {
                Command::new("systemctl").args([verb, UNIT]).status().context("cannot run systemctl")?
            } else {
                Command::new("sudo").args(["systemctl", verb, UNIT]).status().context("cannot run sudo (needed for the system service; or use a rootless service)")?
            };
            if !st.success() {
                bail!("systemctl {verb} {UNIT} failed");
            }
            let _ = Command::new("systemctl").args(["--no-pager", "--lines=0", "status", UNIT]).status();
        }
    }
    Ok(())
}

/// The service log from journald. A rootless service's log is in your own user journal; the
/// system service's needs sudo unless you are in the adm or systemd-journal group.
pub fn logs(follow: bool, lines: u32, since: Option<&str>) -> Result<()> {
    let mode = installed().unwrap_or(Mode::System);
    if mode == Mode::User {
        find_runtime_dir();
    }
    let mut args: Vec<String> = match mode {
        Mode::User => vec!["--user".into(), "-u".into(), UNIT.into()],
        Mode::System => vec!["-u".into(), UNIT.into()],
    };
    args.extend(["-o".into(), "short-iso".into(), "--no-pager".into(), "-n".into(), lines.to_string()]);
    if follow {
        args.push("-f".into());
    }
    if let Some(s) = since {
        args.push("--since".into());
        args.push(s.into());
    }
    let can_read = mode == Mode::User
        || is_root()
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

    #[test]
    fn user_unit_runs_as_owner() {
        let u = super::user_unit_text(std::path::Path::new("/usr/local/bin/ark-tbw"), std::path::Path::new("/home/ark/.ark-tbw/config.toml"));
        assert!(u.contains("ExecStart=/usr/local/bin/ark-tbw run --config /home/ark/.ark-tbw/config.toml"));
        assert!(u.contains("RestartPreventExitStatus=2"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(!u.contains("User="));
        assert!(!u.contains("ProtectSystem"));
    }
}
