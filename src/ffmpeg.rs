//! Locating and running ffmpeg / ffprobe.

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

impl Ffmpeg {
    /// `hint` is a user-given path to ffmpeg(.exe) or its folder; otherwise the program's folder and PATH are tried.
    pub fn locate(hint: Option<&Path>) -> Result<Ffmpeg> {
        let exe = |n: &str| if cfg!(windows) { format!("{n}.exe") } else { n.to_string() };
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Some(h) = hint.filter(|h| !h.as_os_str().is_empty()) {
            if h.is_dir() { cands.push(h.join(exe("ffmpeg"))) } else { cands.push(h.to_path_buf()) }
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
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
