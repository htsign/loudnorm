//! Settings kept in a small INI file between runs.
//!
//! Location: `settings.ini` next to the exe if that file exists (portable use), otherwise
//! `%APPDATA%\loudnorm\settings.ini` on Windows and `$XDG_CONFIG_HOME/loudnorm/settings.ini`
//! (or `~/.config/...`) elsewhere. Lines starting with `;` or `#` are comments.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::apply::Mp3Encode;
use crate::core::{Method, Target};

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub ffmpeg: String,
    pub last_folder: String,
    pub method: Method,
    pub tolerance: f64,
    pub limit_peak: bool,
    pub peak_ceiling: f64,
    pub lossless_fallback_reencode: bool,
    pub target: Target,
    pub custom_target: f64,
    pub density: bool,
    pub mp3: Mp3Encode,
    pub aac_kbps: u32,
    pub vorbis_q: u32,
    pub jobs: usize,
    pub true_peak: bool,
    pub in_place: bool,
    pub output_folder: String,
    pub keep_mtime: bool,
    pub window_width: f32,
    pub window_height: f32,
    pub window_maximized: bool,
    /// Widths of the resizable table columns (folder … action), in logical pixels.
    pub column_widths: Vec<f32>,
}

/// Logical CPUs minus one (at least 1), leaving a core for the UI and the rest of the system.
/// フォルダ, ファイル名, 形式, LUFS, 中央値差, ピーク, 補正, 処理 (the last column takes the rest).
pub const DEFAULT_COLUMN_WIDTHS: [f32; 8] = [180.0, 220.0, 56.0, 64.0, 60.0, 58.0, 60.0, 120.0];

pub fn default_jobs() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).max(1)
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ffmpeg: String::new(),
            last_folder: String::new(),
            method: Method::Reencode,
            tolerance: 1.0,
            limit_peak: true,
            peak_ceiling: -1.0,
            lossless_fallback_reencode: false,
            target: Target::Median,
            custom_target: -16.0,
            density: false,
            mp3: Mp3Encode::Cbr320,
            aac_kbps: 256,
            vorbis_q: 8,
            jobs: default_jobs(),
            true_peak: true,
            in_place: true,
            output_folder: String::new(),
            keep_mtime: true,
            window_width: 1280.0,
            window_height: 820.0,
            window_maximized: false,
            column_widths: DEFAULT_COLUMN_WIDTHS.to_vec(),
        }
    }
}

pub fn path() -> Option<PathBuf> {
    let name = "settings.ini";
    if let Some(p) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name))) {
        if p.is_file() {
            return Some(p);
        }
    }
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("loudnorm").join(name))
}

fn method_str(m: Method) -> &'static str {
    match m {
        Method::Reencode => "reencode",
        Method::Lossless => "lossless",
        Method::Tags => "tags",
    }
}

fn mp3_str(m: Mp3Encode) -> &'static str {
    match m {
        Mp3Encode::Cbr320 => "cbr320",
        Mp3Encode::Cbr256 => "cbr256",
        Mp3Encode::V0 => "v0",
    }
}

impl Settings {
    pub fn to_ini(&self) -> String {
        format!(
            "; loudnorm の設定（終了時ではなく、変更するたびに保存されます）\n\
             [general]\n\
             ; ffmpeg のあるフォルダ、または ffmpeg 本体のパス（空なら PATH と exe のフォルダを探す）\n\
             ffmpeg = {}\n\
             last_folder = {}\n\
             \n\
             [plan]\n\
             ; reencode | lossless | tags\n\
             method = {}\n\
             tolerance_db = {}\n\
             limit_peak = {}\n\
             peak_ceiling_dbtp = {}\n\
             lossless_fallback_reencode = {}\n\
             ; median | mean | trimmed_mean | min_change | custom\n\
             target = {}\n\
             custom_target_lufs = {}\n\
             \n\
             [encode]\n\
             ; cbr320 | cbr256 | v0\n\
             mp3 = {}\n\
             aac_min_kbps = {}\n\
             vorbis_quality = {}\n\
             \n\
             [process]\n\
             jobs = {}\n\
             true_peak = {}\n\
             \n\
             [output]\n\
             in_place = {}\n\
             output_folder = {}\n\
             keep_mtime = {}\n\
             \n\
             [window]\n\
             ; 最大化していないときの大きさ（論理ピクセル）\n\
             width = {}\n\
             height = {}\n\
             maximized = {}\n\
             ; 上部のグラフをヒストグラムで表示するか\n\
             histogram = {}\n\
             \n\
             [table]\n\
             ; フォルダ, ファイル名, 形式, LUFS, 中央値差, ピーク, 補正, 処理 の列幅\n\
             column_widths = {}\n",
            self.ffmpeg,
            self.last_folder,
            method_str(self.method),
            self.tolerance,
            self.limit_peak,
            self.peak_ceiling,
            self.lossless_fallback_reencode,
            self.target.key(),
            self.custom_target,
            mp3_str(self.mp3),
            self.aac_kbps,
            self.vorbis_q,
            self.jobs,
            self.true_peak,
            self.in_place,
            self.output_folder,
            self.keep_mtime,
            self.window_width,
            self.window_height,
            self.window_maximized,
            self.density,
            self.column_widths.iter().map(|w| format!("{w}")).collect::<Vec<_>>().join(", "),
        )
    }

    /// Parse an INI text; unknown keys are ignored and bad values keep their defaults.
    /// Returns the settings and the keys whose values could not be read.
    pub fn from_ini(text: &str) -> (Settings, Vec<String>) {
        let mut kv: BTreeMap<String, String> = BTreeMap::new();
        let mut section = String::new();
        for line in text.lines() {
            let line = line.trim().trim_start_matches('\u{feff}');
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some(sec) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = sec.trim().to_ascii_lowercase();
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let v = v.trim();
                let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v);
                kv.insert(format!("{section}.{}", k.trim().to_ascii_lowercase()), v.to_string());
            }
        }
        let mut s = Settings::default();
        let mut bad = Vec::new();
        macro_rules! take {
            ($key:literal, $field:expr, $parse:expr) => {
                if let Some(v) = kv.get($key) {
                    match $parse(v.as_str()) {
                        Some(x) => $field = x,
                        None => bad.push($key.to_string()),
                    }
                }
            };
        }
        let b = |v: &str| match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        };
        let f = |v: &str| v.parse::<f64>().ok().filter(|x| x.is_finite());
        take!("general.ffmpeg", s.ffmpeg, |v: &str| Some(v.to_string()));
        take!("general.last_folder", s.last_folder, |v: &str| Some(v.to_string()));
        take!("plan.method", s.method, |v: &str| match v {
            "reencode" => Some(Method::Reencode),
            "lossless" => Some(Method::Lossless),
            "tags" => Some(Method::Tags),
            _ => None,
        });
        take!("plan.tolerance_db", s.tolerance, |v: &str| f(v).filter(|x| (0.0..=10.0).contains(x)));
        take!("plan.limit_peak", s.limit_peak, b);
        take!("plan.peak_ceiling_dbtp", s.peak_ceiling, |v: &str| f(v).filter(|x| (-6.0..=0.0).contains(x)));
        take!("plan.lossless_fallback_reencode", s.lossless_fallback_reencode, b);
        take!("plan.target", s.target, Target::from_key);
        take!("plan.custom_target_lufs", s.custom_target, |v: &str| f(v).filter(|x| (-60.0..=0.0).contains(x)));
        take!("encode.mp3", s.mp3, |v: &str| match v {
            "cbr320" => Some(Mp3Encode::Cbr320),
            "cbr256" => Some(Mp3Encode::Cbr256),
            "v0" => Some(Mp3Encode::V0),
            _ => None,
        });
        take!("encode.aac_min_kbps", s.aac_kbps, |v: &str| v.parse::<u32>().ok().filter(|x| [192, 256, 320].contains(x)));
        take!("encode.vorbis_quality", s.vorbis_q, |v: &str| v.parse::<u32>().ok().filter(|x| (3..=10).contains(x)));
        take!("process.jobs", s.jobs, |v: &str| v.parse::<usize>().ok().filter(|x| (1..=256).contains(x)));
        take!("process.true_peak", s.true_peak, b);
        take!("output.in_place", s.in_place, b);
        take!("output.output_folder", s.output_folder, |v: &str| Some(v.to_string()));
        take!("output.keep_mtime", s.keep_mtime, b);
        let size = |v: &str| v.parse::<f32>().ok().filter(|x| (400.0..=20000.0).contains(x));
        take!("window.width", s.window_width, size);
        take!("window.height", s.window_height, size);
        take!("window.maximized", s.window_maximized, b);
        take!("window.histogram", s.density, b);
        take!("table.column_widths", s.column_widths, |v: &str| {
            let ws: Option<Vec<f32>> = v.split(',').map(|x| x.trim().parse::<f32>().ok().filter(|w| (20.0..=4000.0).contains(w))).collect();
            ws.filter(|ws| ws.len() == DEFAULT_COLUMN_WIDTHS.len())
        });
        (s, bad)
    }

    /// Load from the settings file. Returns the settings and a message for the log.
    pub fn load() -> (Settings, String) {
        let Some(p) = path() else { return (Settings::default(), "設定ファイルの場所を決められません".into()) };
        match std::fs::read_to_string(&p) {
            Ok(t) => {
                let (s, bad) = Settings::from_ini(&t);
                let msg = if bad.is_empty() {
                    format!("設定を読み込みました: {}", p.display())
                } else {
                    format!("設定を読み込みました（不正な値は既定値を使用: {}）: {}", bad.join(", "), p.display())
                };
                (s, msg)
            }
            Err(_) => (Settings::default(), format!("設定ファイルはまだありません（変更すると作成）: {}", p.display())),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = path().ok_or_else(|| std::io::Error::other("設定ファイルの場所を決められません"))?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("ini.tmp");
        std::fs::write(&tmp, self.to_ini())?;
        std::fs::rename(&tmp, &p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = Settings {
            ffmpeg: "C:\\tools\\ffmpeg\\bin".into(),
            last_folder: "D:\\Music; 2026".into(),
            method: Method::Lossless,
            tolerance: 1.5,
            limit_peak: false,
            peak_ceiling: -0.5,
            lossless_fallback_reencode: true,
            target: Target::MinChange,
            custom_target: -14.5,
            density: true,
            mp3: Mp3Encode::V0,
            aac_kbps: 320,
            vorbis_q: 6,
            jobs: 7,
            true_peak: false,
            in_place: false,
            output_folder: "E:\\out".into(),
            keep_mtime: false,
            window_width: 1500.0,
            window_height: 900.0,
            window_maximized: true,
            column_widths: vec![200.5, 300.0, 50.0, 70.0, 66.0, 58.0, 60.0, 140.0],
        };
        let (r, bad) = Settings::from_ini(&s.to_ini());
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(r, s);
    }

    #[test]
    fn bad_values_keep_defaults() {
        let (r, bad) = Settings::from_ini("[plan]\ntolerance_db = abc\nmethod = lossless\n[process]\njobs = 0\nUNKNOWN = 1\n");
        assert_eq!(r.method, Method::Lossless);
        assert_eq!(r.tolerance, Settings::default().tolerance);
        assert_eq!(r.jobs, default_jobs());
        assert_eq!(bad, vec!["plan.tolerance_db".to_string(), "process.jobs".to_string()]);
    }
}
