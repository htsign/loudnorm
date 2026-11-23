//! Locating and running ffmpeg / ffprobe.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct Ffmpeg {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub has_libmp3lame: bool,
    pub has_libvorbis: bool,
    pub version: String,
}

#[derive(Clone, Debug, Default)]
pub struct StreamInfo {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub bit_rate: Option<u64>,
    pub bits: Option<u32>,
    pub sample_fmt: String,
}

pub fn command(exe: &Path) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// The folder searched for a portable `ffmpeg` / `ffprobe` next to the exe.
/// Inside an AppImage the running exe lives in a mount point or a temporary extraction that goes away,
/// so this is the folder holding the AppImage file instead.
fn exe_dir() -> Option<PathBuf> {
    let current = std::env::current_exe().ok();
    let (appimage, appdir) = if cfg!(target_os = "linux") {
        (std::env::var_os("APPIMAGE"), std::env::var_os("APPDIR"))
    } else {
        (None, None)
    };
    exe_dir_with(appimage.as_deref(), appdir.as_deref(), current.as_deref(), |d| std::fs::canonicalize(d).ok())
}

fn exe_dir_with(
    appimage: Option<&OsStr>,
    appdir: Option<&OsStr>,
    current_exe: Option<&Path>,
    canonicalize: impl Fn(&OsStr) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let dir = exe_dir_from(appimage, appdir, current_exe)?;
    // `APPDIR` may be spelled through a symlink (e.g. a symlinked `TMPDIR`) that `current_exe` has resolved.
    if Some(dir.as_path()) == current_exe.and_then(Path::parent)
        && let Some(real) = appdir.filter(|d| Path::new(d).is_absolute()).and_then(canonicalize)
    {
        return exe_dir_from(appimage, Some(real.as_os_str()), current_exe);
    }
    Some(dir)
}

fn exe_dir_from(appimage: Option<&OsStr>, appdir: Option<&OsStr>, current_exe: Option<&Path>) -> Option<PathBuf> {
    let exe = current_exe?;
    fn absolute(s: Option<&OsStr>) -> Option<&Path> {
        s.map(Path::new).filter(|p| p.is_absolute())
    }
    if let (Some(image), Some(dir)) = (absolute(appimage), absolute(appdir))
        && exe.starts_with(dir)
        && let Some(parent) = image.parent()
    {
        return Some(parent.to_path_buf());
    }
    exe.parent().map(Path::to_path_buf)
}

impl Ffmpeg {
    /// `hint` is a user-given path to ffmpeg(.exe) or its folder; otherwise the exe's folder (see `exe_dir`) and PATH are tried.
    pub fn locate(hint: Option<&Path>) -> Result<Ffmpeg> {
        let exe = |n: &str| if cfg!(windows) { format!("{n}.exe") } else { n.to_string() };
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Some(h) = hint.filter(|h| !h.as_os_str().is_empty()) {
            if h.is_dir() { cands.push(h.join(exe("ffmpeg"))) } else { cands.push(h.to_path_buf()) }
        }
        if let Some(dir) = exe_dir() {
            cands.push(dir.join(exe("ffmpeg")));
        }
        cands.push(PathBuf::from(exe("ffmpeg")));
        for c in cands {
            let Ok(out) = command(&c).arg("-hide_banner").arg("-encoders").output() else { continue };
            if !out.status.success() {
                continue;
            }
            let enc = String::from_utf8_lossy(&out.stdout);
            let probe = match c.parent().filter(|p| !p.as_os_str().is_empty()) {
                Some(d) => d.join(exe("ffprobe")),
                None => PathBuf::from(exe("ffprobe")),
            };
            if command(&probe).arg("-version").output().map(|o| !o.status.success()).unwrap_or(true) {
                bail!("ffmpeg は見つかりましたが、同じ場所に ffprobe がありません: {}", probe.display());
            }
            let version = command(&c)
                .arg("-version")
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(str::to_string))
                .unwrap_or_default();
            return Ok(Ffmpeg {
                has_libmp3lame: enc.contains("libmp3lame"),
                has_libvorbis: enc.contains("libvorbis"),
                ffmpeg: c,
                ffprobe: probe,
                version,
            });
        }
        bail!("ffmpeg が見つかりません（PATH に追加するか、場所を指定してください）")
    }

    pub fn probe(&self, path: &Path) -> Result<StreamInfo> {
        let out = command(&self.ffprobe)
            .args(["-v", "error", "-select_streams", "a:0", "-show_entries"])
            .arg("stream=codec_name,channels,sample_rate,bit_rate,bits_per_raw_sample,bits_per_sample,sample_fmt")
            .args(["-of", "default=nw=1"])
            .arg(path)
            .output()
            .context("ffprobe を実行できません")?;
        if !out.status.success() {
            bail!("ffprobe: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let mut s = StreamInfo::default();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let num = v.parse::<u64>().ok().filter(|n| *n > 0);
            match k {
                "codec_name" => s.codec = v.to_string(),
                "channels" => s.channels = num.unwrap_or(0) as u32,
                "sample_rate" => s.sample_rate = num.unwrap_or(0) as u32,
                "bit_rate" => s.bit_rate = num,
                "bits_per_raw_sample" => s.bits = num.map(|n| n as u32).or(s.bits),
                "bits_per_sample" => s.bits = s.bits.or(num.map(|n| n as u32)),
                "sample_fmt" => s.sample_fmt = v.to_string(),
                _ => {}
            }
        }
        if s.channels == 0 || s.sample_rate == 0 {
            bail!("音声ストリームの情報を取得できません");
        }
        Ok(s)
    }

    pub fn spawn_decode(&self, path: &Path) -> Result<Child> {
        Ok(command(&self.ffmpeg)
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            .args(["-map", "0:a:0", "-f", "f32le", "-acodec", "pcm_f32le", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?)
    }

    pub fn run(&self, args: &[std::ffi::OsString]) -> Result<()> {
        let out = command(&self.ffmpeg).args(args).stdin(Stdio::null()).output()?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("ffmpeg: {}", err.lines().rev().take(3).collect::<Vec<_>>().join(" / "));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute path written in Unix form; on Windows it gets a drive so that it stays absolute.
    fn abs(unix: &str) -> PathBuf {
        if cfg!(windows) { PathBuf::from(format!("C:{unix}")) } else { PathBuf::from(unix) }
    }

    const APPIMAGE: &str = "/home/u/apps/loudnorm.AppImage";
    const APPDIR: &str = "/tmp/.mount_abc";
    const EXE_IN_APPDIR: &str = "/tmp/.mount_abc/usr/bin/loudnorm";

    #[test]
    fn exe_dir_is_the_appimage_folder_when_the_exe_runs_inside_appdir() {
        let (appimage, appdir, exe) = (abs(APPIMAGE), abs(APPDIR), abs(EXE_IN_APPDIR));

        let dir = exe_dir_from(Some(appimage.as_os_str()), Some(appdir.as_os_str()), Some(&exe));

        assert_eq!(dir, Some(abs("/home/u/apps")));
    }

    #[test]
    fn exe_dir_is_the_appimage_folder_when_appdir_has_redundant_separators() {
        let (appimage, appdir, exe) = (abs(APPIMAGE), abs("/tmp//.mount_abc/"), abs(EXE_IN_APPDIR));

        let dir = exe_dir_from(Some(appimage.as_os_str()), Some(appdir.as_os_str()), Some(&exe));

        assert_eq!(dir, Some(abs("/home/u/apps")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_without_appimage_variables() {
        let exe = abs("/opt/loudnorm/loudnorm");

        let dir = exe_dir_from(None, None, Some(&exe));

        assert_eq!(dir, Some(abs("/opt/loudnorm")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_only_appimage_is_missing() {
        let (appdir, exe) = (abs(APPDIR), abs(EXE_IN_APPDIR));

        let dir = exe_dir_from(None, Some(appdir.as_os_str()), Some(&exe));

        assert_eq!(dir, Some(abs("/tmp/.mount_abc/usr/bin")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_appimage_is_relative() {
        let (appdir, exe) = (abs(APPDIR), abs(EXE_IN_APPDIR));

        let dir = exe_dir_from(Some(OsStr::new("loudnorm.AppImage")), Some(appdir.as_os_str()), Some(&exe));

        assert_eq!(dir, Some(abs("/tmp/.mount_abc/usr/bin")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_appimage_is_empty() {
        let (appdir, exe) = (abs(APPDIR), abs(EXE_IN_APPDIR));

        let dir = exe_dir_from(Some(OsStr::new("")), Some(appdir.as_os_str()), Some(&exe));

        assert_eq!(dir, Some(abs("/tmp/.mount_abc/usr/bin")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_appimage_has_no_parent() {
        let (appimage, appdir, exe) = (abs("/"), abs(APPDIR), abs(EXE_IN_APPDIR));

        let dir = exe_dir_from(Some(appimage.as_os_str()), Some(appdir.as_os_str()), Some(&exe));

        assert_eq!(dir, Some(abs("/tmp/.mount_abc/usr/bin")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_without_appdir() {
        let (appimage, exe) = (abs(APPIMAGE), abs(EXE_IN_APPDIR));

        let dir = exe_dir_from(Some(appimage.as_os_str()), None, Some(&exe));

        assert_eq!(dir, Some(abs("/tmp/.mount_abc/usr/bin")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_appdir_is_empty() {
        let (appimage, exe) = (abs(APPIMAGE), abs(EXE_IN_APPDIR));

        let dir = exe_dir_from(Some(appimage.as_os_str()), Some(OsStr::new("")), Some(&exe));

        assert_eq!(dir, Some(abs("/tmp/.mount_abc/usr/bin")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_appdir_is_relative() {
        let appimage = abs(APPIMAGE);
        let exe = PathBuf::from("squashfs-root/usr/bin/loudnorm");

        let dir = exe_dir_from(Some(appimage.as_os_str()), Some(OsStr::new("squashfs-root")), Some(&exe));

        assert_eq!(dir, Some(PathBuf::from("squashfs-root/usr/bin")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_the_exe_is_outside_appdir() {
        let (appimage, appdir, exe) = (abs(APPIMAGE), abs(APPDIR), abs("/opt/loudnorm/loudnorm"));

        let dir = exe_dir_from(Some(appimage.as_os_str()), Some(appdir.as_os_str()), Some(&exe));

        assert_eq!(dir, Some(abs("/opt/loudnorm")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_the_exe_is_in_a_sibling_whose_name_starts_with_appdir() {
        let (appimage, appdir, exe) = (abs(APPIMAGE), abs(APPDIR), abs("/tmp/.mount_abcdef/usr/bin/loudnorm"));

        let dir = exe_dir_from(Some(appimage.as_os_str()), Some(appdir.as_os_str()), Some(&exe));

        assert_eq!(dir, Some(abs("/tmp/.mount_abcdef/usr/bin")));
    }

    #[test]
    fn exe_dir_is_the_appimage_folder_when_appdir_is_a_symlink_to_the_folder_holding_the_exe() {
        let (appimage, appdir, exe) = (abs(APPIMAGE), abs("/tmp/link/.mount_abc"), abs("/var/real/.mount_abc/usr/bin/loudnorm"));

        let dir = exe_dir_with(Some(appimage.as_os_str()), Some(appdir.as_os_str()), Some(&exe), |_| {
            Some(abs("/var/real/.mount_abc"))
        });

        assert_eq!(dir, Some(abs("/home/u/apps")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_the_resolved_appdir_does_not_hold_the_exe() {
        let (appimage, appdir, exe) = (abs(APPIMAGE), abs("/tmp/link/.mount_abc"), abs("/opt/loudnorm/loudnorm"));

        let dir = exe_dir_with(Some(appimage.as_os_str()), Some(appdir.as_os_str()), Some(&exe), |_| {
            Some(abs("/var/real/.mount_abc"))
        });

        assert_eq!(dir, Some(abs("/opt/loudnorm")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_appdir_cannot_be_resolved() {
        let (appimage, appdir, exe) = (abs(APPIMAGE), abs("/tmp/link/.mount_abc"), abs("/var/real/.mount_abc/usr/bin/loudnorm"));

        let dir = exe_dir_with(Some(appimage.as_os_str()), Some(appdir.as_os_str()), Some(&exe), |_| None);

        assert_eq!(dir, Some(abs("/var/real/.mount_abc/usr/bin")));
    }

    #[test]
    fn exe_dir_is_the_appimage_folder_when_the_given_appdir_holds_the_exe_even_if_it_resolves_elsewhere() {
        let (appimage, appdir, exe) = (abs(APPIMAGE), abs(APPDIR), abs(EXE_IN_APPDIR));

        let dir = exe_dir_with(Some(appimage.as_os_str()), Some(appdir.as_os_str()), Some(&exe), |_| {
            Some(abs("/elsewhere"))
        });

        assert_eq!(dir, Some(abs("/home/u/apps")));
    }

    #[test]
    fn exe_dir_is_the_exe_folder_when_a_relative_appdir_would_resolve_to_the_folder_holding_the_exe() {
        let (appimage, exe) = (abs(APPIMAGE), abs("/home/x/squashfs-root/usr/bin/loudnorm"));

        let dir = exe_dir_with(Some(appimage.as_os_str()), Some(OsStr::new("squashfs-root")), Some(&exe), |_| {
            Some(abs("/home/x/squashfs-root"))
        });

        assert_eq!(dir, Some(abs("/home/x/squashfs-root/usr/bin")));
    }

    #[test]
    fn exe_dir_is_none_without_current_exe_even_with_appimage_variables() {
        let (appimage, appdir) = (abs(APPIMAGE), abs(APPDIR));

        let dir = exe_dir_from(Some(appimage.as_os_str()), Some(appdir.as_os_str()), None);

        assert_eq!(dir, None);
    }
}
