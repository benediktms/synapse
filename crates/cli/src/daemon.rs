use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use daemon_client::DaemonClient;

use crate::args::DaemonCommand;

const LABEL: &str = "com.benediktms.synapse";
const UNIT_NAME: &str = "synapse.service";
const PLIST_TEMPLATE: &str = include_str!("templates/launchd.plist");
const UNIT_TEMPLATE: &str = include_str!("templates/systemd.service");

pub fn run(cmd: DaemonCommand) -> Result<(), String> {
    match cmd {
        DaemonCommand::Install => install(),
        DaemonCommand::Uninstall => uninstall(),
        DaemonCommand::Start => start(),
        DaemonCommand::Stop => stop(Autostart::Disable),
        DaemonCommand::Restart => {
            stop(Autostart::Keep)?;
            start()
        }
        DaemonCommand::Logs { follow, lines } => logs(follow, lines),
    }
}

fn install() -> Result<(), String> {
    let state = daemon_client::state_dir()?;
    std::fs::create_dir_all(&state)
        .map_err(|e| format!("cannot create {}: {e}", state.display()))?;
    let config = daemon_client::config_path(&state);
    if !config.exists() {
        return Err(format!(
            "no daemon config at {}; run `syn setup` first, or the unit would crash-loop at boot",
            config.display()
        ));
    }
    let log = daemon_client::log_path(&state);
    let binary = installed_synd()?;

    let state_override = state_dir_override();
    quiesce()?;

    if cfg!(target_os = "macos") {
        let path = plist_path()?;
        let changed = write_if_changed(
            &path,
            &render_plist(&binary, &log, state_override.as_deref()),
        )?;
        let domain = gui_domain();
        let target = format!("{domain}/{LABEL}");
        let path_str = path_str(&path)?;
        tolerate_missing(
            "launchctl bootout",
            run_tool(&["launchctl", "bootout", &domain, &path_str])?,
        )?;
        require_success(
            "launchctl enable",
            run_tool(&["launchctl", "enable", &target])?,
        )?;
        require_success(
            "launchctl bootstrap",
            run_tool(&["launchctl", "bootstrap", &domain, &path_str])?,
        )?;
        report_install(&path, changed)
    } else if cfg!(target_os = "linux") {
        let path = unit_path()?;
        let changed = write_if_changed(
            &path,
            &render_unit(&binary, &log, state_override.as_deref()),
        )?;
        require_success(
            "systemctl --user daemon-reload",
            run_tool(&["systemctl", "--user", "daemon-reload"])?,
        )?;
        require_success(
            "systemctl --user enable --now",
            run_tool(&["systemctl", "--user", "enable", "--now", UNIT_NAME])?,
        )?;
        report_install(&path, changed)
    } else {
        Err(unsupported())
    }
}

fn report_install(path: &Path, changed: bool) -> Result<(), String> {
    let state = if changed { "written" } else { "unchanged" };
    println!(
        "installed and loaded {} (unit file {state})",
        path.display()
    );
    Ok(())
}

fn uninstall() -> Result<(), String> {
    quiesce()?;
    if cfg!(target_os = "macos") {
        let path = plist_path()?;
        let path_str = path_str(&path)?;
        tolerate_missing(
            "launchctl bootout",
            run_tool(&["launchctl", "bootout", &gui_domain(), &path_str])?,
        )?;
        remove_unit_file(&path)
    } else if cfg!(target_os = "linux") {
        let path = unit_path()?;
        tolerate_missing(
            "systemctl --user disable --now",
            run_tool(&["systemctl", "--user", "disable", "--now", UNIT_NAME])?,
        )?;
        remove_unit_file(&path)?;
        let _ = run_tool(&["systemctl", "--user", "daemon-reload"]);
        Ok(())
    } else {
        Err(unsupported())
    }
}

fn remove_unit_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => println!("removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("nothing installed at {}", path.display())
        }
        Err(e) => return Err(format!("cannot remove {}: {e}", path.display())),
    }
    Ok(())
}

fn start() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let path = plist_path()?;
        if !path.exists() {
            return Err(format!(
                "no unit installed at {}; run: syn daemon install",
                path.display()
            ));
        }
        let domain = gui_domain();
        let target = format!("{domain}/{LABEL}");
        let path_str = path_str(&path)?;
        let loaded = match run_tool(&["launchctl", "print", &target])? {
            Outcome::Success => true,
            Outcome::NotFound => false,
            Outcome::Failed { code, stderr } => {
                return Err(format!("launchctl print failed (exit {code}): {stderr}"));
            }
        };
        require_success(
            "launchctl enable",
            run_tool(&["launchctl", "enable", &target])?,
        )?;
        if !loaded {
            require_success(
                "launchctl bootstrap",
                run_tool(&["launchctl", "bootstrap", &domain, &path_str])?,
            )?;
        }
    } else if cfg!(target_os = "linux") {
        require_success(
            "systemctl --user enable --now",
            run_tool(&["systemctl", "--user", "enable", "--now", UNIT_NAME])?,
        )?;
    } else {
        return Err(unsupported());
    }
    println!("daemon unit started");
    Ok(())
}

/// Whether a stop also revokes the unit's right to start at login.
#[derive(Clone, Copy, PartialEq)]
enum Autostart {
    Disable,
    Keep,
}

fn stop(autostart: Autostart) -> Result<(), String> {
    quiesce()?;
    if cfg!(target_os = "macos") {
        let domain = gui_domain();
        let target = format!("{domain}/{LABEL}");
        let path_str = path_str(&plist_path()?)?;
        tolerate_missing(
            "launchctl bootout",
            run_tool(&["launchctl", "bootout", &domain, &path_str])?,
        )?;
        if autostart == Autostart::Disable {
            tolerate_missing(
                "launchctl disable",
                run_tool(&["launchctl", "disable", &target])?,
            )?;
        }
    } else if cfg!(target_os = "linux") {
        let argv: &[&str] = match autostart {
            Autostart::Disable => &["systemctl", "--user", "disable", "--now", UNIT_NAME],
            Autostart::Keep => &["systemctl", "--user", "stop", UNIT_NAME],
        };
        tolerate_missing("systemctl --user stop", run_tool(argv)?)?;
    } else {
        return Err(unsupported());
    }
    println!("daemon stopped");
    Ok(())
}

const QUIESCE_DEADLINE: Duration = Duration::from_secs(10);
const QUIESCE_POLL: Duration = Duration::from_millis(100);

/// Ask whatever answers the socket to exit, and wait until nothing does. The unit does not own
/// the daemon's lifetime — any CLI call spawns one on demand — so stopping the unit alone can
/// leave the previous binary serving, and a newly installed unit would lose the single-instance
/// race to it. Failing here leaves the unit untouched on purpose. The shutdown call itself is
/// allowed to fail: the daemon may drop the connection as it goes, which is the wanted outcome.
fn quiesce() -> Result<(), String> {
    let state = daemon_client::state_dir()?;
    let socket = daemon_client::socket_path(&state);
    let client = DaemonClient::new(socket.clone(), Duration::from_secs(2));
    if client.ping().is_err() {
        return Ok(());
    }
    let _ = client.shutdown();
    let deadline = Instant::now() + QUIESCE_DEADLINE;
    while Instant::now() < deadline {
        if client.ping().is_err() {
            return Ok(());
        }
        std::thread::sleep(QUIESCE_POLL);
    }
    Err(format!(
        "a daemon still answers on {} after {}s, so nothing was changed; it may predate the \
         shutdown method — stop it with `pkill -x synd` and retry",
        socket.display(),
        QUIESCE_DEADLINE.as_secs()
    ))
}

fn logs(follow: bool, lines: usize) -> Result<(), String> {
    let state = daemon_client::state_dir()?;
    let path = daemon_client::log_path(&state);
    let mut offset = match std::fs::read_to_string(&path) {
        Ok(text) => {
            let tail: Vec<&str> = text.lines().rev().take(lines).collect();
            for line in tail.into_iter().rev() {
                println!("{line}");
            }
            text.len() as u64
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !follow {
                return Err(format!("no log at {}", path.display()));
            }
            0
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };

    if !follow {
        return Ok(());
    }
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let len = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot inspect {}: {e}", path.display())),
        };
        if len < offset {
            offset = 0;
        }
        if len == offset {
            continue;
        }
        let mut file = std::fs::File::open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut appended = String::new();
        file.read_to_string(&mut appended)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        offset += appended.len() as u64;
        print!("{appended}");
        std::io::stdout().flush().ok();
    }
}

enum Outcome {
    Success,
    NotFound,
    Failed { code: i32, stderr: String },
}

fn run_tool(argv: &[&str]) -> Result<Outcome, String> {
    let (program, args) = argv.split_first().expect("argv is never empty");
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if out.status.success() {
        return Ok(Outcome::Success);
    }
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // launchctl exits 113 ("Could not find service") for an unloaded label; systemctl
    // exits 4/5 for an unknown unit. Idempotent stop/uninstall treat those as done.
    let not_found = code == 113
        || code == 5
        || code == 4
        || stderr.contains("Could not find service")
        || stderr.contains("not loaded")
        || stderr.contains("not found")
        || stderr.contains("does not exist");
    if not_found {
        return Ok(Outcome::NotFound);
    }
    Ok(Outcome::Failed { code, stderr })
}

fn require_success(action: &str, outcome: Outcome) -> Result<(), String> {
    match outcome {
        Outcome::Success => Ok(()),
        Outcome::NotFound => Err(format!("{action}: the unit is not registered")),
        Outcome::Failed { code, stderr } => Err(format!("{action} failed (exit {code}): {stderr}")),
    }
}

fn tolerate_missing(action: &str, outcome: Outcome) -> Result<(), String> {
    match outcome {
        Outcome::Success | Outcome::NotFound => Ok(()),
        Outcome::Failed { code, stderr } => Err(format!("{action} failed (exit {code}): {stderr}")),
    }
}

fn unsupported() -> String {
    "syn daemon supports macOS (launchd) and Linux (systemd --user) only".to_string()
}

fn gui_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

fn plist_path() -> Result<PathBuf, String> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn unit_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map_or_else(|| home().map(|home| home.join(".config")), Ok)?;
    Ok(base.join("systemd/user").join(UNIT_NAME))
}

/// The unit must point at a path that survives rebuilds, so the `~/.local/bin`
/// symlink wins over the synd sitting next to the running syn.
fn installed_synd() -> Result<PathBuf, String> {
    let canonical = home()?.join(".local/bin/synd");
    if canonical.exists() {
        return Ok(canonical);
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("synd")))
        .filter(|candidate| candidate.exists());
    sibling.ok_or_else(|| {
        format!(
            "synd not found at {} or next to syn; run: just install",
            canonical.display()
        )
    })
}

fn state_dir_override() -> Option<String> {
    std::env::var("SYNAPSE_STATE_DIR")
        .ok()
        .filter(|value| !value.is_empty())
}

fn render_plist(binary: &Path, log: &Path, state_override: Option<&str>) -> String {
    let env_block = state_override
        .map(|dir| {
            format!(
                "\n    <key>EnvironmentVariables</key>\n    <dict>\n        <key>SYNAPSE_STATE_DIR</key>\n        <string>{}</string>\n    </dict>",
                xml_escape(dir)
            )
        })
        .unwrap_or_default();
    PLIST_TEMPLATE
        .replace("{{LABEL}}", LABEL)
        .replace("{{BINARY_PATH}}", &xml_escape(&binary.to_string_lossy()))
        .replace("{{LOG_PATH}}", &xml_escape(&log.to_string_lossy()))
        .replace("{{ENV_BLOCK}}", &env_block)
}

fn render_unit(binary: &Path, log: &Path, state_override: Option<&str>) -> String {
    let env_block = state_override
        .map(|dir| format!("Environment=\"SYNAPSE_STATE_DIR={}\"\n", unit_escape(dir)))
        .unwrap_or_default();
    UNIT_TEMPLATE
        .replace("{{BINARY_PATH}}", &unit_escape(&binary.to_string_lossy()))
        .replace("{{LOG_PATH}}", &unit_escape(&log.to_string_lossy()))
        .replace("{{ENV_BLOCK}}", &env_block)
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `%` is a systemd specifier and would be expanded inside the unit file.
fn unit_escape(text: &str) -> String {
    text.replace('%', "%%")
}

fn path_str(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(String::from)
        .ok_or_else(|| format!("path is not valid UTF-8: {path:?}"))
}

fn write_if_changed(path: &Path, desired: &str) -> Result<bool, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    match std::fs::read(path) {
        Ok(current) if current == desired.as_bytes() => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, desired).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_substitutes_every_placeholder_and_escapes_xml() {
        let rendered = render_plist(
            Path::new("/Users/a & b/.local/bin/synd"),
            Path::new("/Users/a & b/state/daemon.log"),
            None,
        );
        assert!(rendered.contains("<string>com.benediktms.synapse</string>"));
        assert!(rendered.contains("<string>/Users/a &amp; b/.local/bin/synd</string>"));
        assert!(rendered.contains("<string>/Users/a &amp; b/state/daemon.log</string>"));
        assert!(rendered.contains("SuccessfulExit"));
        assert!(!rendered.contains("{{"));
    }

    #[test]
    fn unit_substitutes_every_placeholder_and_escapes_percent() {
        let rendered = render_unit(
            Path::new("/home/x/.local/bin/synd"),
            Path::new("/home/x/100%/daemon.log"),
            None,
        );
        assert!(rendered.contains("ExecStart=\"/home/x/.local/bin/synd\""));
        assert!(rendered.contains("StandardError=append:/home/x/100%%/daemon.log"));
        assert!(rendered.contains("Restart=on-failure"));
        assert!(!rendered.contains("{{"));
    }

    #[test]
    fn a_state_dir_override_is_pinned_into_both_units() {
        let plist = render_plist(
            Path::new("/b/synd"),
            Path::new("/l/d.log"),
            Some("/custom state"),
        );
        assert!(plist.contains("<key>SYNAPSE_STATE_DIR</key>"));
        assert!(plist.contains("<string>/custom state</string>"));

        let unit = render_unit(
            Path::new("/b/synd"),
            Path::new("/l/d.log"),
            Some("/custom state"),
        );
        assert!(unit.contains("Environment=\"SYNAPSE_STATE_DIR=/custom state\"\n"));
    }

    #[test]
    fn write_if_changed_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("unit");
        assert!(write_if_changed(&path, "one").unwrap());
        assert!(!write_if_changed(&path, "one").unwrap());
        assert!(write_if_changed(&path, "two").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
    }
}
