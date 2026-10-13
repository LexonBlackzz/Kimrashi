//! Update check and in-place self-update from GitHub releases.
//!
//! Trust, in two layers. The download must match the SHA-256 and size GitHub reports for
//! the asset (corrupt or truncated downloads, a swapped file on the CDN). It must also
//! carry a signature, in a `<name>.exe.sig` asset, that verifies against the release key
//! built into the program (see `sig.rs`): that holds even if someone can edit the release.
//!
//! A release is installable when it has an `.exe` asset with a digest and its `.sig`.
//! Otherwise the window only offers the release page.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};
use crossbeam_channel::Receiver;
use eframe::egui;
use crate::sig;

const REPO: &str = "LexonBlackzz/Kimrashi";
/// Refuse anything larger (the exe is about 11 MB).
const MAX_SIZE: u64 = 256 << 20;
const CHECK_EVERY_SECS: u64 = 24 * 3600;
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    body: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    size: u64,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub url: String,
    pub size: u64,
    pub sha256: [u8; 32],
    pub sig_url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    pub notes: String,
    pub page: String,
    /// `None`: nothing this program can install and verify; offer the page instead.
    pub asset: Option<Asset>,
}

/// `v0.4` and `0.4.0` are the same version; anything non-numeric is not comparable.
fn version(tag: &str) -> Option<[u64; 3]> {
    let mut out = [0; 3];
    let mut parts = tag.trim().trim_start_matches(['v', 'V']).split('.');
    for slot in &mut out {
        match parts.next() {
            Some(p) => *slot = p.parse().ok()?,
            None => break,
        }
    }
    parts.next().is_none().then_some(out)
}

fn newer(tag: &str, current: &str) -> bool {
    matches!((version(tag), version(current)), (Some(a), Some(b)) if a > b)
}

fn parse_digest(digest: &str) -> Option<[u8; 32]> {
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn parse_release(json: &str) -> Result<Release, String> {
    let r: ApiRelease = serde_json::from_str(json).map_err(|e| format!("unexpected reply from GitHub: {e}"))?;
    if r.draft || r.prerelease {
        return Err("the latest release is not a stable release".into());
    }
    let github = |url: &str| url.starts_with("https://github.com/");
    let asset = r.assets.iter()
        .find(|a| a.name.to_ascii_lowercase().ends_with(".exe"))
        .and_then(|a| Some(Asset {
            url: a.browser_download_url.clone(),
            size: a.size,
            sha256: parse_digest(a.digest.as_deref()?)?,
            sig_url: r.assets.iter().find(|s| s.name == format!("{}.sig", a.name))?.browser_download_url.clone(),
        }))
        .filter(|a| a.size > 0 && a.size <= MAX_SIZE && github(&a.url) && github(&a.sig_url));
    Ok(Release { tag: r.tag_name, notes: r.body.unwrap_or_default(), page: r.html_url, asset })
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .user_agent(&format!("kimrashi/{CURRENT}"))
        .build()
}

fn latest() -> Result<Release, String> {
    let reply = agent()
        .get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("could not reach GitHub: {e}"))?
        .into_string()
        .map_err(|e| e.to_string())?;
    parse_release(&reply)
}

/// Copy `reader` to `out`, hashing as it goes. Succeeds only for exactly `size` bytes
/// whose SHA-256 is `expected`.
fn copy_verified(
    mut reader: impl Read, size: u64, expected: &[u8; 32], mut out: impl Write, done: &AtomicU64, cancel: &AtomicBool,
) -> Result<(), String> {
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        if cancel.load(Relaxed) {
            return Err("cancelled".into());
        }
        let n = reader.read(&mut buf).map_err(|e| format!("download failed: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > size {
            return Err("the download is larger than GitHub announced".into());
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| format!("could not write the update: {e}"))?;
        done.store(total, Relaxed);
    }
    if total != size {
        return Err(format!("the download is incomplete ({total} of {size} bytes)"));
    }
    if hash.finalize().as_slice() != expected {
        return Err("the download does not match the checksum GitHub published".into());
    }
    out.flush().map_err(|e| e.to_string())
}

fn download(asset: &Asset, to: &Path, done: &AtomicU64, cancel: &AtomicBool) -> Result<(), String> {
    let reply = agent().get(&asset.url).call().map_err(|e| format!("could not download the update: {e}"))?;
    let file = std::fs::File::create(to).map_err(|e| format!("could not write next to the program: {e}"))?;
    let result = copy_verified(reply.into_reader(), asset.size, &asset.sha256, std::io::BufWriter::new(file), done, cancel);
    if result.is_err() {
        let _ = std::fs::remove_file(to);
    }
    result
}

/// The release signature (128 hex characters) for the file whose hash is `asset.sha256`.
fn check_signature(asset: &Asset, version: [u64; 3], key: &[u8; 32]) -> Result<(), String> {
    let text = agent().get(&asset.sig_url).call().map_err(|e| format!("could not download the signature: {e}"))?
        .into_string().map_err(|e| e.to_string())?;
    if text.len() > 4096 {
        return Err("the signature file is too large".into());
    }
    sig::verify(key, version, &asset.sha256, &text)
}

fn with_suffix(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Put `new` in place of the running `exe`. Windows lets a running program be renamed
/// but not overwritten: move it aside, move the new one in, undo if that fails.
fn replace_exe(new: &Path, exe: &Path) -> Result<(), String> {
    let mut magic = [0u8; 2];
    std::fs::File::open(new).and_then(|mut f| f.read_exact(&mut magic)).map_err(|e| e.to_string())?;
    if &magic != b"MZ" {
        return Err("the downloaded file is not a Windows program".into());
    }
    let old = with_suffix(exe, ".old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("could not replace the program (is it in a protected folder?): {e}"))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("could not put the update in place: {e}"));
    }
    Ok(())
}

/// Delete what a previous update left beside the exe. The old exe may still be shutting
/// down when this runs, so keep trying for a few seconds.
pub fn cleanup_old() {
    let Ok(exe) = std::env::current_exe() else { return };
    std::thread::spawn(move || {
        for _ in 0..10 {
            let gone = [".old", ".new"].iter().all(|s| {
                let p = with_suffix(&exe, s);
                !p.exists() || std::fs::remove_file(&p).is_ok()
            });
            if gone {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// Start the (new) program again with the same arguments and end this one.
pub fn relaunch() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).args(std::env::args_os().skip(1)).spawn();
    }
    std::process::exit(0)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `kimrashi --check-update [--download]`: print what the updater sees; with `--download`
/// fetch and verify the latest asset (to the temp folder, then delete it) without installing.
pub fn run_from_args() -> bool {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--check-update") {
        return false;
    }
    match latest() {
        Err(e) => println!("check failed: {e}"),
        Ok(r) => {
            println!("installed {CURRENT}, latest {} ({})", r.tag, if newer(&r.tag, CURRENT) { "newer" } else { "not newer" });
            match (&r.asset, args.iter().any(|a| a == "--download")) {
                (None, _) => println!("no installable asset (needs an .exe with a SHA-256 digest and a .exe.sig); would open {}", r.page),
                (Some(a), false) => println!("asset {} bytes, sha256 {}", a.size, a.sha256.iter().map(|b| format!("{b:02x}")).collect::<String>()),
                (Some(a), true) => {
                    let to = std::env::temp_dir().join("kimrashi-update-check.exe");
                    let result = download(a, &to, &AtomicU64::new(0), &AtomicBool::new(false))
                        .and_then(|()| check_signature(a, version(&r.tag).ok_or("unreadable version")?, &sig::PUBLIC_KEY));
                    println!("download and verify: {}", result.map_or_else(|e| e, |()| format!("ok, {} bytes match the SHA-256 and the signature verifies", a.size)));
                    let _ = std::fs::remove_file(to);
                }
            }
        }
    }
    true
}

enum State {
    Idle,
    Checking { rx: Receiver<Result<Release, String>>, manual: bool },
    UpToDate,
    Available(Release),
    Downloading { release: Release, rx: Receiver<Result<(), String>>, done: Arc<AtomicU64>, cancel: Arc<AtomicBool> },
    Failed { release: Option<Release>, error: String },
}

/// What the app should do after `poll`.
pub enum Outcome {
    Nothing,
    /// A check finished: persist `last_check`.
    Checked,
    /// The new exe is in place: save settings and call `relaunch`.
    Restart,
}

pub struct Updater {
    /// Check once a day when the program starts.
    pub enabled: bool,
    /// Unix seconds of the last finished check.
    pub last_check: u64,
    state: State,
    window: bool,
}

impl Updater {
    pub fn new(enabled: bool, last_check: u64) -> Self {
        Self { enabled, last_check, state: State::Idle, window: false }
    }

    /// Start a silent check if one is due.
    pub fn start_if_due(&mut self) {
        if self.enabled && now().saturating_sub(self.last_check) >= CHECK_EVERY_SECS {
            self.check(false);
        }
    }

    pub fn check_now(&mut self) {
        self.check(true);
    }

    fn check(&mut self, manual: bool) {
        if matches!(self.state, State::Checking { .. } | State::Downloading { .. }) {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || { let _ = tx.send(latest()); });
        self.state = State::Checking { rx, manual };
    }

    fn install(&mut self) {
        let (State::Available(release) | State::Failed { release: Some(release), .. }) = &self.state else { return };
        let (Some(asset), Ok(exe)) = (release.asset.clone(), std::env::current_exe()) else { return };
        let release = release.clone();
        let Some(version) = version(&release.tag) else { return };
        let (done, cancel) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (d, c) = (done.clone(), cancel.clone());
        std::thread::spawn(move || {
            let new = with_suffix(&exe, ".new");
            // The file is on disk and matches the hash; it is installed only if the signature holds.
            let result = download(&asset, &new, &d, &c)
                .and_then(|()| check_signature(&asset, version, &sig::PUBLIC_KEY))
                .and_then(|()| replace_exe(&new, &exe))
                .inspect_err(|_| { let _ = std::fs::remove_file(&new); });
            let _ = tx.send(result);
        });
        self.state = State::Downloading { release, rx, done, cancel };
    }

    pub fn poll(&mut self) -> Outcome {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Checking { rx, manual } => match rx.try_recv() {
                Err(_) => { self.state = State::Checking { rx, manual }; Outcome::Nothing }
                Ok(result) => {
                    self.last_check = now();
                    self.state = match result {
                        Ok(r) if newer(&r.tag, CURRENT) => State::Available(r),
                        Ok(_) => State::UpToDate,
                        Err(error) if manual => State::Failed { release: None, error },
                        Err(_) => State::Idle,
                    };
                    Outcome::Checked
                }
            },
            State::Downloading { release, rx, done, cancel } => match rx.try_recv() {
                Err(_) => { self.state = State::Downloading { release, rx, done, cancel }; Outcome::Nothing }
                Ok(Ok(())) => Outcome::Restart,
                Ok(Err(error)) => { self.state = State::Failed { release: Some(release), error }; Outcome::Nothing }
            },
            other => { self.state = other; Outcome::Nothing }
        }
    }

    /// A button in the toolbar while an update is waiting or installing.
    pub fn toolbar(&mut self, ui: &mut egui::Ui) {
        match &self.state {
            State::Available(r) => {
                if ui.button(format!("Update to {}", r.tag)).on_hover_text("Release notes and install").clicked() {
                    self.window = true;
                }
            }
            State::Downloading { .. } => { ui.label("Updating…"); }
            _ => {}
        }
    }

    /// The Settings rows: the on/off switch, a manual check, and its result.
    pub fn settings(&mut self, ui: &mut egui::Ui) {
        ui.label("Updates");
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.enabled, "Check on startup")
                .on_hover_text("Asks GitHub for the latest release once a day. Nothing else is sent.");
            let busy = matches!(self.state, State::Checking { .. } | State::Downloading { .. });
            if ui.add_enabled(!busy, egui::Button::new("Check now")).clicked() {
                self.check_now();
            }
            match &self.state {
                State::Checking { .. } => { ui.label("Checking…"); }
                State::UpToDate => { ui.label(format!("Up to date ({CURRENT})")); }
                State::Available(r) => {
                    if ui.link(format!("{} is available", r.tag)).clicked() { self.window = true; }
                }
                State::Failed { error, .. } => { ui.colored_label(egui::Color32::LIGHT_RED, "Check failed").on_hover_text(error); }
                _ => {}
            }
        });
        ui.end_row();
    }

    /// The release notes window, with the install button and progress.
    pub fn window(&mut self, ctx: &egui::Context) {
        if !self.window {
            return;
        }
        if matches!(self.state, State::Checking { .. } | State::Downloading { .. }) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let mut open = true;
        let mut install = false;
        let mut cancel = false;
        let mut dismiss = false;
        egui::Window::new("Update").open(&mut open).collapsible(false).resizable(true).default_width(460.0).show(ctx, |ui| {
            match &self.state {
                State::Available(r) | State::Failed { release: Some(r), .. } | State::Downloading { release: r, .. } => {
                    ui.heading(format!("Kimrashi {} is available", r.tag.trim_start_matches(['v', 'V'])));
                    ui.label(format!("You have {CURRENT}."));
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| { ui.label(&r.notes); });
                    ui.add_space(6.0);
                    if let State::Failed { error, .. } = &self.state {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                    if let State::Downloading { release, done, .. } = &self.state {
                        let total = release.asset.as_ref().map_or(1, |a| a.size.max(1));
                        let fraction = (done.load(Relaxed) as f32 / total as f32).min(1.0);
                        ui.add(egui::ProgressBar::new(fraction).text(format!("{:.0}%", fraction * 100.0)));
                        cancel = ui.button("Cancel").clicked();
                    } else {
                        ui.horizontal(|ui| {
                            if r.asset.is_some() {
                                install = ui.button("Update now").on_hover_text(
                                    "Downloads the new program, checks its SHA-256 and release signature, replaces this one and restarts.",
                                ).clicked();
                            }
                            if ui.button("Open release page").clicked() {
                                ctx.open_url(egui::OpenUrl::new_tab(&r.page));
                            }
                            dismiss = ui.button("Later").clicked();
                        });
                        if r.asset.is_none() {
                            ui.label("This release can't be installed from here; use the release page.");
                        }
                    }
                }
                _ => { ui.label("No update is waiting."); }
            }
        });
        if install {
            self.install();
        }
        if cancel {
            if let State::Downloading { cancel, .. } = &self.state { cancel.store(true, Relaxed); }
        }
        self.window = open && !dismiss && matches!(self.state, State::Available(_) | State::Downloading { .. } | State::Failed { release: Some(_), .. });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const SAMPLE: &str = r#"{
        "url": "https://api.github.com/repos/LexonBlackzz/Kimrashi/releases/1",
        "tag_name": "v0.4.1", "draft": false, "prerelease": false,
        "html_url": "https://github.com/LexonBlackzz/Kimrashi/releases/tag/v0.4.1",
        "body": "Fixes.\r\n- one\r\n- two",
        "assets": [
          {"name": "notes.txt", "size": 10, "browser_download_url": "https://github.com/x/notes.txt", "digest": null},
          {"name": "kimrashi.exe", "size": 10870784,
           "browser_download_url": "https://github.com/LexonBlackzz/Kimrashi/releases/download/v0.4.1/kimrashi.exe",
           "digest": "sha256:b8457be768b857f19e34265d1f31f91a0d8b256d9d52a709cc1fd790d53bed59"},
          {"name": "kimrashi.exe.sig", "size": 129,
           "browser_download_url": "https://github.com/LexonBlackzz/Kimrashi/releases/download/v0.4.1/kimrashi.exe.sig",
           "digest": null}
        ]}"#;

    #[test]
    fn versions_compare_the_way_tags_are_written() {
        assert_eq!(version("v0.4"), Some([0, 4, 0]));
        assert_eq!(version("0.4.1"), Some([0, 4, 1]));
        assert_eq!(version("v1"), Some([1, 0, 0]));
        assert_eq!(version("0.4.1-beta"), None);
        assert_eq!(version("1.2.3.4"), None);
        assert_eq!(version("nightly"), None);
        assert!(newer("v0.4.1", "0.4.0"));
        assert!(newer("v0.10.0", "0.9.9"));
        assert!(!newer("v0.4", "0.4.0"));
        assert!(!newer("v0.3", "0.4.0"));
        assert!(!newer("garbage", "0.4.0"));
    }

    #[test]
    fn release_json_gives_the_exe_and_its_digest() {
        let r = parse_release(SAMPLE).unwrap();
        assert_eq!(r.tag, "v0.4.1");
        assert!(r.page.ends_with("/v0.4.1"));
        let a = r.asset.unwrap();
        assert_eq!(a.size, 10_870_784);
        assert!(a.url.ends_with("/v0.4.1/kimrashi.exe"));
        assert_eq!(a.sha256[0], 0xb8);
        assert_eq!(a.sha256[31], 0x59);
        assert!(a.sig_url.ends_with("/v0.4.1/kimrashi.exe.sig"));
    }

    #[test]
    fn a_release_without_a_usable_asset_is_page_only() {
        let no_digest = SAMPLE.replace("\"digest\": \"sha256:b845", "\"digest\": \"md5:b845");
        assert_eq!(parse_release(&no_digest).unwrap().asset, None);
        let elsewhere = SAMPLE.replace("https://github.com/LexonBlackzz/Kimrashi/releases/download/v0.4.1/kimrashi.exe\"", "https://evil.example/kimrashi.exe\"");
        assert_eq!(parse_release(&elsewhere).unwrap().asset, None);
        let unsigned = SAMPLE.replace("kimrashi.exe.sig", "other.txt");
        assert_eq!(parse_release(&unsigned).unwrap().asset, None, "no signature, no install");
        let no_exe = SAMPLE.replace("kimrashi.exe", "kimrashi.zip");
        assert_eq!(parse_release(&no_exe).unwrap().asset, None);
        let draft = SAMPLE.replace("\"draft\": false", "\"draft\": true");
        assert!(parse_release(&draft).is_err());
        assert!(parse_release("{}").is_err());
    }

    fn sha(bytes: &[u8]) -> [u8; 32] { Sha256::digest(bytes).into() }

    #[test]
    fn a_download_must_match_size_and_hash() {
        let data = vec![7u8; 200_000];
        let (done, cancel) = (AtomicU64::new(0), AtomicBool::new(false));
        let mut out = Vec::new();
        copy_verified(Cursor::new(&data), data.len() as u64, &sha(&data), &mut out, &done, &cancel).unwrap();
        assert_eq!(out, data);
        assert_eq!(done.load(Relaxed), data.len() as u64);
        let mut bad = data.clone();
        bad[100_000] ^= 1;
        let err = copy_verified(Cursor::new(&bad), data.len() as u64, &sha(&data), Vec::new(), &done, &cancel).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        let err = copy_verified(Cursor::new(&data[..1000]), data.len() as u64, &sha(&data), Vec::new(), &done, &cancel).unwrap_err();
        assert!(err.contains("incomplete"), "{err}");
        let err = copy_verified(Cursor::new(&data), 1000, &sha(&data), Vec::new(), &done, &cancel).unwrap_err();
        assert!(err.contains("larger"), "{err}");
        cancel.store(true, Relaxed);
        assert!(copy_verified(Cursor::new(&data), data.len() as u64, &sha(&data), Vec::new(), &done, &cancel).is_err());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kimrashi-update-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replacing_the_exe_keeps_the_old_one_aside() {
        let dir = scratch("replace");
        let (exe, new) = (dir.join("kimrashi.exe"), dir.join("kimrashi.exe.new"));
        std::fs::write(&exe, b"MZ old").unwrap();
        std::fs::write(&new, b"MZ new").unwrap();
        std::fs::write(dir.join("settings.txt"), b"vsync=true").unwrap();
        replace_exe(&new, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ new");
        assert_eq!(std::fs::read(dir.join("kimrashi.exe.old")).unwrap(), b"MZ old");
        assert!(!new.exists());
        assert_eq!(std::fs::read(dir.join("settings.txt")).unwrap(), b"vsync=true");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_replace_leaves_the_program_as_it_was() {
        let dir = scratch("rollback");
        let exe = dir.join("kimrashi.exe");
        std::fs::write(&exe, b"MZ old").unwrap();
        // Not a program: rejected before anything moves.
        let junk = dir.join("junk.new");
        std::fs::write(&junk, b"<html>").unwrap();
        assert!(replace_exe(&junk, &exe).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ old");
        // Missing: also rejected up front.
        assert!(replace_exe(&dir.join("absent.new"), &exe).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ old");
        assert!(!dir.join("kimrashi.exe.old").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Serve `routes` (path -> body) on localhost until `requests` have been answered.
    fn serve(routes: Vec<(&'static str, Vec<u8>)>, requests: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let n = stream.read(&mut request).unwrap();
                let path = String::from_utf8_lossy(&request[..n]).split_whitespace().nth(1).unwrap_or("/").to_owned();
                let body = routes.iter().find(|(p, _)| *p == path).map(|(_, b)| b.clone());
                let head = if body.is_some() { "200 OK" } else { "404 Not Found" };
                let body = body.unwrap_or_default();
                let _ = write!(stream, "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(&body);
            }
        });
        base
    }

    #[test]
    fn an_update_installs_only_when_hash_and_signature_both_hold() {
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[5u8; 32]);
        let public = key.verifying_key().to_bytes();
        let mut exe_bytes = b"MZ new program".to_vec();
        exe_bytes.extend(vec![1u8; 300_000]);
        let hash = sha(&exe_bytes);
        let version = [0, 4, 1];
        let good_sig = sig::hex(&key.sign(&sig::message(version, &hash)).to_bytes());
        let other_sig = sig::hex(&SigningKey::from_bytes(&[6u8; 32]).sign(&sig::message(version, &hash)).to_bytes());
        let old_version_sig = sig::hex(&key.sign(&sig::message([0, 4, 0], &hash)).to_bytes());
        let mut tampered = exe_bytes.clone();
        tampered[5000] ^= 1;
        let base = serve(vec![
            ("/kimrashi.exe", exe_bytes.clone()), ("/tampered.exe", tampered), ("/good.sig", good_sig.into_bytes()),
            ("/other.sig", other_sig.into_bytes()), ("/old.sig", old_version_sig.into_bytes()),
        ], 8);
        let asset = |exe: &str, sig: &str| Asset { url: format!("{base}/{exe}"), size: exe_bytes.len() as u64, sha256: hash, sig_url: format!("{base}/{sig}") };
        let (done, cancel) = (AtomicU64::new(0), AtomicBool::new(false));
        let dir = scratch("e2e");
        let (target, new) = (dir.join("kimrashi.exe"), dir.join("kimrashi.exe.new"));
        std::fs::write(&target, b"MZ old program").unwrap();

        // A file that differs from the published hash never reaches the signature check.
        let err = download(&asset("tampered.exe", "good.sig"), &new, &done, &cancel).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert!(!new.exists());
        // Right file, signature from another key / for another version: rejected.
        download(&asset("kimrashi.exe", "other.sig"), &new, &done, &cancel).unwrap();
        assert!(check_signature(&asset("kimrashi.exe", "other.sig"), version, &public).is_err());
        assert!(check_signature(&asset("kimrashi.exe", "old.sig"), version, &public).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"MZ old program", "nothing was installed yet");
        // The real thing.
        check_signature(&asset("kimrashi.exe", "good.sig"), version, &public).unwrap();
        replace_exe(&new, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), exe_bytes);
        assert_eq!(std::fs::read(dir.join("kimrashi.exe.old")).unwrap(), b"MZ old program");
        let _ = std::fs::remove_dir_all(dir);
    }
}
