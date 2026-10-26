//! An always-on storage node for this computer's user: started at login
//! (Linux: at boot, if lingering is allowed) and again if it stops.

use anyhow::{Context, Result, bail};
use std::path::Path;
#[cfg(not(windows))]
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let ok = Command::new(cmd).args(args).status()?.success();
    if !ok {
        bail!("{cmd} {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(not(windows))]
fn home(rest: &str) -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home")?.join(rest))
}

#[cfg(target_os = "linux")]
fn unit() -> Result<PathBuf> {
    home(".config/systemd/user/irch-serve.service")
}

#[cfg(target_os = "macos")]
fn plist() -> Result<PathBuf> {
    home("Library/LaunchAgents/dev.irch.serve.plist")
}

#[cfg(target_os = "windows")]
const TASK: &str = "irch storage node";

/// Run `exe --serve` from now on, then show its storage code.
pub fn install(exe: &Path) -> Result<()> {
    let exe = exe.to_str().context("an odd path")?;
    #[cfg(target_os = "linux")]
    {
        let f = unit()?;
        std::fs::create_dir_all(f.parent().context("no dir")?)?;
        std::fs::write(
            &f,
            format!(
                "[Unit]\nDescription=irch storage node\n\
             Wants=network-online.target\nAfter=network-online.target\n\n\
             [Service]\nExecStart=\"{exe}\" --serve\nRestart=always\n\
             RestartSec=5\n\n[Install]\nWantedBy=default.target\n"
            ),
        )?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", "irch-serve"])?;
        run("systemctl", &["--user", "restart", "irch-serve"])?;
        // At boot, not just at login: allowed for oneself on most systems.
        if run("loginctl", &["enable-linger"]).is_err() {
            println!("(It starts when you log in: `loginctl enable-linger`");
            println!(" as an admin makes that at boot.)");
        }
    }
    #[cfg(target_os = "macos")]
    {
        let f = plist()?;
        std::fs::create_dir_all(f.parent().context("no dir")?)?;
        std::fs::write(
            &f,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist \
             version=\"1.0\"><dict>\n<key>Label</key>\
             <string>dev.irch.serve</string>\n<key>ProgramArguments</key>\
             <array><string>{exe}</string><string>--serve</string></array>\n\
             <key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n\
             </dict></plist>\n"
            ),
        )?;
        let f = f.to_str().context("an odd path")?;
        let _ = run("launchctl", &["unload", f]);
        run("launchctl", &["load", "-w", f])?;
    }
    #[cfg(target_os = "windows")]
    {
        // conhost --headless: a console program with no window.
        let tr = format!("conhost.exe --headless \"{exe}\" --serve");
        run(
            "schtasks",
            &[
                "/Create", "/F", "/TN", TASK, "/SC", "ONLOGON", "/RL",
                "LIMITED", "/TR", &tr,
            ],
        )?;
        let _ = run("schtasks", &["/End", "/TN", TASK]);
        run("schtasks", &["/Run", "/TN", TASK])?;
    }
    let code = dirs::data_dir().context("no data dir")?;
    let code = code.join("irch-serve").join("code");
    for _ in 0..60 {
        if let Ok(c) = std::fs::read_to_string(&code) {
            println!("A storage node runs here from now on, kept up to date.");
            println!("Its code, for the people you store for (irch on this");
            println!("computer pairs with it by itself):\n\n{}", c.trim());
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    bail!("it was installed, but has not started yet")
}

pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = run("systemctl", &["--user", "disable", "--now", "irch-serve"]);
        std::fs::remove_file(unit()?)?;
    }
    #[cfg(target_os = "macos")]
    {
        let f = plist()?;
        let _ = run("launchctl", &["unload", f.to_str().unwrap_or_default()]);
        std::fs::remove_file(f)?;
    }
    #[cfg(target_os = "windows")]
    {
        let _ = run("schtasks", &["/End", "/TN", TASK]);
        run("schtasks", &["/Delete", "/F", "/TN", TASK])?;
    }
    println!("The storage node no longer runs here; its data stays.");
    Ok(())
}
