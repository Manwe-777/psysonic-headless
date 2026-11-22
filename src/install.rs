//! `install`, `update` and `uninstall`: the player as a systemd service (after toolnet's installer).
//! The curl script only downloads and checks the binary and runs `install`, so running it again
//! is also how to update or change settings.
//!
//! Settings live in `/etc/psysonic-headless/config.env` (read by the unit), the server password
//! and hub token in files only root and the service's group can read. The service runs as the
//! `psysonic-headless` system user, in the `audio` group, with its track cache in systemd's cache
//! directory.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use sha2::{Digest, Sha256};

use crate::config::{InstallArgs, UninstallArgs, UpdateArgs, VERSION};
use crate::subsonic::{Quality, Subsonic};

const BIN: &str = "/usr/local/bin/psysonic-headless";
const ETC: &str = "/etc/psysonic-headless";
const CONFIG: &str = "/etc/psysonic-headless/config.env";
const PASSWORD: &str = "/etc/psysonic-headless/server-password";
const TOKEN: &str = "/etc/psysonic-headless/hub-token";
const LASTFM_SESSION: &str = "/etc/psysonic-headless/lastfm-session";
const UNIT: &str = "/etc/systemd/system/psysonic-headless.service";
const UNIT_TEXT: &str = include_str!("../packaging/psysonic-headless.service");
const CACHE_DIRS: [&str; 2] = ["/var/cache/psysonic-headless", "/var/cache/private/psysonic-headless"];
/// The service's system user and group.
const USER: &str = "psysonic-headless";
/// Where releases come from; PSYSONIC_DOWNLOAD overrides it (a URL or a local directory).
const RELEASES: &str = "https://github.com/Manwe-777/psysonic-headless/releases/latest/download";

pub async fn install(a: InstallArgs) -> Result<()> {
    preflight()?;
    let mut config = read_config(Path::new(CONFIG));
    let first = config.is_empty();

    let mut set = |key: &str, value: Option<String>| {
        if let Some(v) = value {
            if v.is_empty() {
                config.remove(key);
            } else {
                config.insert(key.to_string(), v);
            }
        }
    };
    set("PSYSONIC_SERVER_URL", a.server_url.clone());
    set("PSYSONIC_SERVER_USER", a.server_user.clone());
    set("PSYSONIC_HUB_URL", a.hub_url.clone());
    set("PSYSONIC_DEVICE_NAME", a.device_name.clone());
    set("PSYSONIC_AUDIO_DEVICE", a.audio_device.clone());
    set("PSYSONIC_MAX_BITRATE", a.max_bitrate.map(|n| n.to_string()));
    set("PSYSONIC_FORMAT", value_name(a.format));
    set("PSYSONIC_REPLAY_GAIN", value_name(a.replay_gain));
    set("PSYSONIC_CACHE", value_name(a.cache));
    set("PSYSONIC_CACHE_MAX_MB", a.cache_max_mb.map(|n| n.to_string()));
    set("PSYSONIC_VOLUME", a.volume.map(|n| n.to_string()));
    set("PSYSONIC_SCROBBLE", value_name(a.scrobble));
    set("PSYSONIC_SCROBBLE_PERCENT", a.scrobble_percent.map(|n| n.to_string()));
    for (key, question) in [
        ("PSYSONIC_SERVER_URL", "Navidrome server address (https://...)"),
        ("PSYSONIC_SERVER_USER", "Navidrome user name"),
        ("PSYSONIC_HUB_URL", "Psysonic Hub address (https://...)"),
    ] {
        if !config.contains_key(key) {
            config.insert(key.to_string(), ask(question)?);
        }
    }

    // Secrets: from a file, else kept, else asked for (without echo).
    let password = match (&a.server_password_file, Path::new(PASSWORD).exists() && !a.new_secrets) {
        (Some(file), _) => Some(read_secret(file)?),
        (None, true) => None,
        (None, false) => Some(ask_secret("Navidrome password")?),
    };
    let token = match (&a.hub_token_file, Path::new(TOKEN).exists() && !a.new_secrets) {
        (Some(file), _) => Some(read_secret(file)?),
        (None, true) => None,
        (None, false) => Some(ask_secret("Hub device token (the hub's Settings page)")?),
    };

    // A wrong address or password is caught now rather than in the service log.
    let server_changed = password.is_some() || a.server_url.is_some() || a.server_user.is_some() || first;
    if server_changed {
        let password = match &password {
            Some(p) => p.clone(),
            None => read_secret(Path::new(PASSWORD))?,
        };
        let quality = Quality {
            max_bitrate: 0,
            format: crate::config::StreamFormat::Auto,
        };
        Subsonic::new(
            &config["PSYSONIC_SERVER_URL"],
            &config["PSYSONIC_SERVER_USER"],
            &password,
            quality,
        )?
        .ping()
        .await
        .context("could not sign in to the server with these settings")?;
        println!("signed in to {}", config["PSYSONIC_SERVER_URL"]);
    }

    // Last.fm: asked on the first install, else only with --lastfm; access is allowed in a browser.
    // (Not offered by a build without the app's key.)
    let offer = first && !a.no_lastfm && crate::scrobble::lastfm::App::get().is_ok();
    let lastfm = a.lastfm || (offer && ask_yes_no("Scrobble to Last.fm? [y/N]"));
    let lastfm_session = if lastfm {
        let app = crate::scrobble::lastfm::App::get()?;
        let (session, name) = crate::scrobble::lastfm::sign_in(&app, |url| {
            println!("\nOpen this link on any device and allow access to Last.fm:\n\n  {url}\n\nWaiting...");
        })
        .await?;
        println!("signed in to Last.fm as {name}");
        config.insert("PSYSONIC_LASTFM_SESSION_FILE".into(), LASTFM_SESSION.into());
        Some(session)
    } else {
        None
    };
    if a.no_lastfm {
        config.remove("PSYSONIC_LASTFM_SESSION_FILE");
        remove(Path::new(LASTFM_SESSION))?;
    }

    ensure_audio_group()?;
    let gid = ensure_user()?;
    fs::create_dir_all(ETC)?;
    fs::set_permissions(ETC, fs::Permissions::from_mode(0o755))?;
    write(Path::new(CONFIG), write_config(&config).as_bytes(), 0o644, None)?;
    if let Some(p) = password {
        write(Path::new(PASSWORD), p.as_bytes(), 0o640, Some(gid))?;
    }
    if let Some(t) = token {
        write(Path::new(TOKEN), t.as_bytes(), 0o640, Some(gid))?;
    }
    if let Some(s) = lastfm_session {
        write(Path::new(LASTFM_SESSION), s.as_bytes(), 0o640, Some(gid))?;
    }

    let me = std::env::current_exe()?.canonicalize()?;
    copy_binary(&me, Path::new(BIN))?;
    write(Path::new(UNIT), UNIT_TEXT.as_bytes(), 0o644, None)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--quiet", "psysonic-headless"])?;
    if !a.no_start {
        systemctl(&["restart", "psysonic-headless"])?;
    }

    println!();
    println!("psysonic-headless {VERSION} is installed: {BIN}, settings in {ETC}");
    println!("  service: systemctl status psysonic-headless   (logs: journalctl -u psysonic-headless -f)");
    println!("  output:  psysonic-headless devices, then sudo psysonic-headless install --audio-device NAME");
    println!("  update:  sudo psysonic-headless update");
    if !config.contains_key("PSYSONIC_LASTFM_SESSION_FILE") {
        println!("  last.fm: sudo psysonic-headless install --lastfm");
    }
    Ok(())
}

pub fn uninstall(a: UninstallArgs) -> Result<()> {
    preflight()?;
    let _ = systemctl(&["disable", "--now", "--quiet", "psysonic-headless"]);
    for f in [UNIT, BIN] {
        remove(Path::new(f))?;
    }
    if a.purge {
        for dir in std::iter::once(ETC).chain(CACHE_DIRS) {
            if Path::new(dir).exists() {
                fs::remove_dir_all(dir)?;
            }
        }
        if group_id(USER).is_some() {
            let _ = Command::new("userdel").arg(USER).status();
        }
    }
    systemctl(&["daemon-reload"])?;
    if a.purge {
        println!("psysonic-headless removed, with its settings and track cache");
    } else {
        println!("psysonic-headless removed. The settings ({ETC}) and track cache stay, so a reinstall");
        println!("comes back set up the same; `psysonic-headless uninstall --purge` deletes them too.");
    }
    Ok(())
}

/// Compares with the latest release, downloads the binary for this machine, checks it against
/// SHA256SUMS and runs its `install` (which keeps the settings).
pub async fn update(a: UpdateArgs) -> Result<()> {
    if !a.check {
        preflight()?;
    }
    let base = std::env::var("PSYSONIC_DOWNLOAD").unwrap_or_else(|_| RELEASES.into());
    let dl = Download::new(&base)?;
    let latest = String::from_utf8(dl.get("VERSION").await?)?
        .trim()
        .trim_start_matches('v')
        .to_string();
    let newer = version(&latest)? > version(VERSION).unwrap_or_default();
    if a.check {
        if newer {
            println!("psysonic-headless {latest} is available (this is {VERSION}): sudo psysonic-headless update");
        } else {
            println!("psysonic-headless {VERSION} is the latest");
        }
        return Ok(());
    }
    if !newer && !a.force {
        println!("psysonic-headless {VERSION} is the latest (--force reinstalls it)");
        return Ok(());
    }
    let name = format!("psysonic-headless-linux-{}", arch()?);
    let sums = String::from_utf8(dl.get("SHA256SUMS").await?)?;
    let want = checksum_of(&sums, &name).with_context(|| format!("{name} isn't listed in SHA256SUMS"))?;
    println!("downloading {name} {latest}");
    let data = dl.get(&name).await?;
    if hex(&Sha256::digest(&data)) != want {
        bail!("{name} doesn't match its checksum: not installing");
    }
    let dir = temp_dir()?;
    let result = (|| {
        let new = dir.join("psysonic-headless");
        write(&new, &data, 0o755, None)?;
        let ok = Command::new(&new)
            .arg("install")
            .status()
            .context("running the new version")?
            .success();
        if !ok {
            bail!("the new version's install failed");
        }
        println!("updated psysonic-headless {VERSION} -> {latest}");
        Ok(())
    })();
    let _ = fs::remove_dir_all(&dir);
    result
}

fn arch() -> Result<&'static str> {
    match std::env::consts::ARCH {
        a @ ("x86_64" | "aarch64") => Ok(a),
        a => bail!("no build for {a}"),
    }
}

/// Release files from a URL or a local directory.
enum Download {
    Http(reqwest::Client, String),
    Dir(PathBuf),
}

impl Download {
    fn new(base: &str) -> Result<Self> {
        if base.starts_with('/') {
            return Ok(Self::Dir(base.into()));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(300))
            .user_agent(concat!("psysonic-headless/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self::Http(client, base.trim_end_matches('/').into()))
    }

    async fn get(&self, name: &str) -> Result<Vec<u8>> {
        match self {
            Self::Dir(dir) => fs::read(dir.join(name)).with_context(|| format!("reading {}", dir.join(name).display())),
            Self::Http(client, base) => {
                let url = format!("{base}/{name}");
                let response = client
                    .get(&url)
                    .send()
                    .await?
                    .error_for_status()
                    .with_context(|| format!("downloading {url}"))?;
                Ok(response.bytes().await?.to_vec())
            }
        }
    }
}

/// "0.2.0" -> (0, 2, 0); a dev build ("0.0.0-dev") is older than any release.
fn version(v: &str) -> Result<(u64, u64, u64)> {
    let mut parts = v.split('.').map(|p| p.parse::<u64>());
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(Ok(a)), Some(Ok(b)), Some(Ok(c)), None) => Ok((a, b, c)),
        _ => bail!("not a version: {v:?}"),
    }
}

/// The checksum SHA256SUMS lists for a file.
fn checksum_of(sums: &str, name: &str) -> Option<String> {
    sums.lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, f)| f.trim().trim_start_matches('*') == name)
        .map(|(h, _)| h.to_lowercase())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `KEY="value"` lines (what systemd's EnvironmentFile reads).
fn read_config(path: &Path) -> BTreeMap<String, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .map(|v| v.replace("\\\"", "\"").replace("\\\\", "\\"))
                .unwrap_or_else(|| v.to_string());
            (k.trim().to_string(), v)
        })
        .collect()
}

fn write_config(config: &BTreeMap<String, String>) -> String {
    let mut out = String::from(
        "# psysonic-headless settings: change them with `sudo psysonic-headless install --<setting> ...`\n",
    );
    for (k, v) in config {
        out.push_str(&format!("{k}=\"{}\"\n", v.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    out
}

/// A setting typed in at the terminal (also when the script comes through `curl | sh`).
fn ask(question: &str) -> Result<String> {
    let tty = fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("no terminal to ask on: pass the settings as flags (see --help)")?;
    let mut out = tty.try_clone()?;
    loop {
        write!(out, "{question}: ")?;
        out.flush()?;
        let mut line = String::new();
        std::io::BufReader::new(&tty).read_line(&mut line)?;
        let line = line.trim().to_string();
        if !line.is_empty() {
            return Ok(line);
        }
    }
}

/// A yes/no question at the terminal; no terminal (or no answer) is no.
fn ask_yes_no(question: &str) -> bool {
    let Ok(tty) = fs::File::options().read(true).write(true).open("/dev/tty") else {
        return false;
    };
    let Ok(mut out) = tty.try_clone() else {
        return false;
    };
    if write!(out, "{question} ").and_then(|()| out.flush()).is_err() {
        return false;
    }
    let mut line = String::new();
    if std::io::BufReader::new(&tty).read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Writes a file only its owner can read (a secret saved by hand, outside the service).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    write(path, data, 0o600, None)
}

fn ask_secret(question: &str) -> Result<String> {
    loop {
        let value = rpassword::prompt_password(format!("{question}: "))
            .context("no terminal to ask on: pass --server-password-file / --hub-token-file")?;
        if !value.is_empty() {
            return Ok(value);
        }
    }
}

fn read_secret(path: &Path) -> Result<String> {
    crate::config::read_secret_file(path)
}

/// A setting's name as typed on the command line (`original`, `track`...).
fn value_name<T: ValueEnum>(v: Option<T>) -> Option<String> {
    v.and_then(|v| v.to_possible_value()).map(|p| p.get_name().to_string())
}

fn preflight() -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("run as root (sudo)");
    }
    if !Path::new("/run/systemd/system").exists() {
        bail!("this system doesn't run systemd: run `psysonic-headless run` by hand instead (see --help)");
    }
    Ok(())
}

/// The service joins the `audio` group to reach the sound card; minimal systems may lack it.
fn ensure_audio_group() -> Result<()> {
    let groups = fs::read_to_string("/etc/group").unwrap_or_default();
    if groups.lines().any(|l| l.starts_with("audio:")) {
        return Ok(());
    }
    let ok = Command::new("groupadd")
        .args(["--system", "audio"])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        bail!("there is no `audio` group and it could not be created");
    }
    Ok(())
}

/// The service's system user (created once); returns its group id.
fn ensure_user() -> Result<u32> {
    if group_id(USER).is_none() {
        let shell = ["/usr/sbin/nologin", "/sbin/nologin"]
            .into_iter()
            .find(|s| Path::new(s).exists())
            .unwrap_or("/bin/false");
        let ok = Command::new("useradd")
            .args([
                "--system",
                "--user-group",
                "--no-create-home",
                "--home-dir",
                CACHE_DIRS[0],
                "--shell",
                shell,
                USER,
            ])
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            bail!("could not create the {USER} system user");
        }
    }
    group_id(USER).with_context(|| format!("no {USER} group"))
}

/// A group's id from /etc/group.
fn group_id(name: &str) -> Option<u32> {
    fs::read_to_string("/etc/group").ok()?.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next() == Some(name)).then(|| f.nth(1)?.parse().ok())?
    })
}

/// Replaces `to` atomically, so the running service keeps working until it restarts.
fn copy_binary(from: &Path, to: &Path) -> Result<()> {
    if to.exists() && from == to.canonicalize()? {
        return Ok(());
    }
    let tmp = to.with_extension("new");
    fs::copy(from, &tmp).with_context(|| format!("copying {} to {}", from.display(), tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(&tmp, to)?;
    Ok(())
}

/// Writes a file, root-only until its group (`gid`) and `mode` are set: a secret is never readable
/// by anyone else, even for a moment.
fn write(path: &Path, data: &[u8], mode: u32, gid: Option<u32>) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", path.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    if let Some(gid) = gid {
        std::os::unix::fs::chown(&tmp, Some(0), Some(gid))?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn remove(p: &Path) -> Result<()> {
    match fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", p.display()))
        }
        _ => Ok(()),
    }
}

/// A fresh root-only directory for a download.
fn temp_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("psysonic-headless-update-{:016x}", rand::random::<u64>()));
    fs::create_dir(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn systemctl(args: &[&str]) -> Result<()> {
    let ok = Command::new("systemctl")
        .args(args)
        .status()
        .context("running systemctl")?
        .success();
    if !ok {
        bail!("systemctl {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number_and_dev_builds_are_older() {
        assert!(version("0.10.0").unwrap() > version("0.9.9").unwrap());
        assert!(version("0.0.0-dev").is_err(), "a dev build counts as 0.0.0");
        assert!(version("1.0").is_err());
    }

    #[test]
    fn settings_with_quotes_and_backslashes_survive_a_reinstall() {
        let mut config = BTreeMap::new();
        config.insert("PSYSONIC_DEVICE_NAME".to_string(), r#"Living "room" \ Pi"#.to_string());
        config.insert(
            "PSYSONIC_SERVER_URL".to_string(),
            "https://music.example.com".to_string(),
        );
        let dir = std::env::temp_dir().join(format!("psh-install-{}", rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.env");
        fs::write(&path, write_config(&config)).unwrap();
        assert_eq!(read_config(&path), config);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_checksum_is_found_by_exact_file_name() {
        let sums = "aaa  psysonic-headless-linux-x86_64\nbbb *psysonic-headless-linux-aarch64\n";
        assert_eq!(
            checksum_of(sums, "psysonic-headless-linux-aarch64").as_deref(),
            Some("bbb")
        );
        assert_eq!(checksum_of(sums, "psysonic-headless-linux"), None);
    }
}
