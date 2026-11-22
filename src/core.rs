//! Scanning, loudness analysis and gain planning.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use ebur128::{EbuR128, Mode};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::ffmpeg::Ffmpeg;

/// Folder name used for backups; skipped when scanning.
pub const BACKUP_DIR: &str = "_loudnorm_backup";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mp3,
    Flac,
    M4a,
    OggVorbis,
    Opus,
    Wav,
}

impl Format {
    pub fn from_path(p: &Path) -> Option<Format> {
        let ext = p.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "mp3" => Format::Mp3,
            "flac" => Format::Flac,
            "m4a" | "mp4" | "m4b" => Format::M4a,
            "ogg" | "oga" => {
                if is_ogg_opus(p) {
                    Format::Opus
                } else {
                    Format::OggVorbis
                }
            }
            "opus" => Format::Opus,
            "wav" => Format::Wav,
            _ => return None,
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            Format::Mp3 => "MP3",
            Format::Flac => "FLAC",
            Format::M4a => "M4A",
            Format::OggVorbis => "Vorbis",
            Format::Opus => "Opus",
            Format::Wav => "WAV",
        }
    }
}

fn is_ogg_opus(p: &Path) -> bool {
    let mut buf = [0u8; 64];
    File::open(p)
        .and_then(|mut f| f.read(&mut buf))
        .map(|n| buf[..n].windows(8).any(|w| w == b"OpusHead"))
        .unwrap_or(false)
}

/// How a file that is outside the tolerance gets changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Decode, apply gain, encode again (tags copied from the original).
    Reencode,
    /// Prefer lossless changes (MP3 global_gain in 1.5 dB steps, Opus header gain).
    Lossless,
    /// Only write ReplayGain 2.0 tags; audio untouched.
    Tags,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Reencode,
    Mp3GlobalGain { steps: i32 },
    OpusHeader,
    ReplayGainTag,
    Skip,
}

impl Action {
    pub fn label(self) -> String {
        match self {
            Action::None => "変更なし".into(),
            Action::Reencode => "再エンコード".into(),
            Action::Mp3GlobalGain { steps } => format!("無劣化 ({steps:+} 段)"),
            Action::OpusHeader => "Opus ヘッダゲイン".into(),
            Action::ReplayGainTag => "ReplayGain タグ".into(),
            Action::Skip => "スキップ".into(),
        }
    }
    pub fn changes_file(self) -> bool {
        !matches!(self, Action::None | Action::Skip)
    }
}

#[derive(Clone, Debug)]
pub struct Track {
    pub path: PathBuf,
    pub rel: PathBuf,
    pub format: Format,
    pub include: bool,
    pub loudness: Option<f64>,
    pub peak_db: Option<f64>,
    pub has_rg: bool,
    pub error: Option<String>,
    /// Gain the plan wants (dB) and the gain actually applied by the chosen action.
    pub want: f64,
    pub gain: f64,
    pub action: Action,
    pub note: String,
    pub status: String,
}

pub fn scan(root: &Path) -> Vec<Track> {
    let mut v: Vec<Track> = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && e.file_name() == BACKUP_DIR))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| !e.file_name().to_string_lossy().contains(".loudnorm-tmp"))
        .filter_map(|e| {
            let p = e.into_path();
            let format = Format::from_path(&p)?;
            let rel = p.strip_prefix(root).unwrap_or(&p).to_path_buf();
            Some(Track {
                path: p,
                rel,
                format,
                include: true,
                loudness: None,
                peak_db: None,
                has_rg: false,
                error: None,
                want: 0.0,
                gain: 0.0,
                action: Action::None,
                note: String::new(),
                status: String::new(),
            })
        })
        .collect();
    v.sort_by(|a, b| a.rel.cmp(&b.rel));
    v
}

/// Integrated loudness (LUFS) and peak: true peak (dBTP, 4x oversampled) or, faster, sample peak (dBFS).
pub fn analyze(path: &Path, ff: Option<&Ffmpeg>, true_peak: bool) -> Result<(f64, f64)> {
    let mode = Mode::I | if true_peak { Mode::TRUE_PEAK } else { Mode::SAMPLE_PEAK };
    match analyze_symphonia(path, mode) {
        Ok(r) => Ok(r),
        Err(e) => match ff {
            Some(ff) => analyze_ffmpeg(path, ff, mode).with_context(|| format!("symphonia: {e:#}")),
            None => Err(e),
        },
    }
}

fn finish(m: &EbuR128) -> Result<(f64, f64)> {
    let l = m.loudness_global()?;
    if !l.is_finite() {
        bail!("無音のため測定できません");
    }
    let mut pk = 0.0f64;
    for c in 0..m.channels() {
        pk = pk.max(if m.mode().contains(Mode::TRUE_PEAK) { m.true_peak(c)? } else { m.sample_peak(c)? });
    }
    Ok((l, 20.0 * pk.max(1e-9).log10()))
}

fn analyze_symphonia(path: &Path, mode: Mode) -> Result<(f64, f64)> {
    let src = Box::new(File::open(path)?);
    let mss = MediaSourceStream::new(src, Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("音声トラックがありません"))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("音声パラメータがありません"))?;
    let mut dec = symphonia::default::get_codecs().make_audio_decoder(params, &AudioDecoderOptions::default())?;
    let tid = track.id;
    let mut meter: Option<EbuR128> = None;
    let mut buf: Vec<f32> = Vec::new();
    let mut errors = 0usize;
    loop {
        let pkt = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::IoError(_)) => break,
            Err(e) => return Err(e.into()),
        };
        if pkt.track_id != tid {
            continue;
        }
        match dec.decode(&pkt) {
            Ok(ab) => {
                let ch = ab.spec().channels().count() as u32;
                let rate = ab.spec().rate();
                let m = match &mut meter {
                    Some(m) => m,
                    None => meter.insert(EbuR128::new(ch, rate, mode)?),
                };
                if m.channels() != ch || m.rate() != rate {
                    bail!("途中でチャンネル数かサンプルレートが変わります");
                }
                buf.resize(ab.samples_interleaved(), 0.0);
                ab.copy_to_slice_interleaved(&mut buf);
                m.add_frames_f32(&buf)?;
            }
            Err(SymError::DecodeError(_)) => {
                errors += 1;
                if errors > 50 {
                    bail!("デコードエラーが多すぎます");
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    finish(meter.as_ref().ok_or_else(|| anyhow!("音声がありません"))?)
}

fn analyze_ffmpeg(path: &Path, ff: &Ffmpeg, mode: Mode) -> Result<(f64, f64)> {
    let info = ff.probe(path)?;
    let ch = info.channels.max(1);
    let mut child = ff.spawn_decode(path)?;
    let mut out = child.stdout.take().unwrap();
    let mut m = EbuR128::new(ch, info.sample_rate, mode)?;
    let frame_bytes = 4 * ch as usize;
    let mut raw = vec![0u8; frame_bytes * 16384];
    let mut fill = 0usize;
    let mut samples: Vec<f32> = Vec::new();
    loop {
        let n = out.read(&mut raw[fill..])?;
        if n == 0 {
            break;
        }
        fill += n;
        let usable = fill - fill % frame_bytes;
        samples.clear();
        samples.extend(raw[..usable].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        m.add_frames_f32(&samples)?;
        raw.copy_within(usable..fill, 0);
        fill -= usable;
    }
    let st = child.wait()?;
    if !st.success() {
        bail!("ffmpeg でのデコードに失敗しました");
    }
    finish(&m)
}

pub fn has_replaygain(path: &Path) -> bool {
    use lofty::prelude::*;
    lofty::read_from_path(path)
        .map(|f| {
            f.tags().iter().any(|t| {
                t.get(ItemKey::ReplayGainTrackGain).is_some() || t.get(ItemKey::ReplayGainAlbumGain).is_some()
            })
        })
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub struct PlanOptions {
    pub method: Method,
    /// Files whose loudness is within ±tolerance of the target stay untouched.
    pub tolerance: f64,
    /// Upper limit for the true peak after a gain increase (dBTP).
    pub peak_ceiling: f64,
    pub limit_peak: bool,
    /// In lossless mode, re-encode lossy formats that have no lossless method (AAC, Vorbis).
    pub lossless_fallback_reencode: bool,
    /// How the target loudness is chosen.
    pub target: Target,
    /// Target (LUFS) used when `target` is `Custom`.
    pub custom_target: f64,
}

/// How the target loudness is derived from the analysed tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Median,
    Mean,
    /// Mean after dropping the quietest and loudest 10 %.
    TrimmedMean,
    /// The value that leaves the most tracks within the tolerance (fewest files changed).
    MinChange,
    Custom,
}

impl Target {
    pub const ALL: [Target; 5] = [Target::Median, Target::Mean, Target::TrimmedMean, Target::MinChange, Target::Custom];
    pub fn label(self) -> &'static str {
        match self {
            Target::Median => "中央値",
            Target::Mean => "平均値",
            Target::TrimmedMean => "トリム平均（上下 10% 除外）",
            Target::MinChange => "変更が最少になる値",
            Target::Custom => "カスタム",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Target::Median => "median",
            Target::Mean => "mean",
            Target::TrimmedMean => "trimmed_mean",
            Target::MinChange => "min_change",
            Target::Custom => "custom",
        }
    }
    pub fn from_key(k: &str) -> Option<Target> {
        Target::ALL.into_iter().find(|t| t.key() == k)
    }
}

/// Summary statistics of the analysed loudness values (LUFS).
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub n: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stdev: f64,
    pub q1: f64,
    pub median: f64,
    pub q3: f64,
    pub trimmed_mean: f64,
    pub min_change: f64,
    /// Tracks left within the tolerance by `min_change`.
    pub min_change_kept: usize,
    /// The target actually used for planning.
    pub target: f64,
}

/// Percentile with linear interpolation between closest ranks (the common "type 7" definition).
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let h = (sorted.len() - 1) as f64 * p;
    let (lo, hi) = (h.floor() as usize, h.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (h - lo as f64)
}

/// Statistics of `sorted` (ascending, non-empty) for the given tolerance.
pub fn stats(sorted: &[f64], tolerance: f64) -> Stats {
    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    let var = if n > 1 { sorted.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64 } else { 0.0 };
    let cut = n / 10;
    let trimmed = &sorted[cut..n - cut];
    let median = percentile(sorted, 0.5);
    // Sliding window of width 2*tolerance: the window holding the most values gives the targets that keep
    // the most tracks untouched (any T in [hi - tol, lo + tol]); prefer the one closest to the median.
    let (mut best_kept, mut best_t, mut j) = (0usize, median, 0usize);
    for i in 0..n {
        if j < i {
            j = i;
        }
        while j + 1 < n && sorted[j + 1] - sorted[i] <= 2.0 * tolerance + 1e-9 {
            j += 1;
        }
        let kept = j - i + 1;
        let t = median.clamp(sorted[j] - tolerance, sorted[i] + tolerance);
        if kept > best_kept || (kept == best_kept && (t - median).abs() < (best_t - median).abs()) {
            best_kept = kept;
            best_t = t;
        }
    }
    Stats {
        n,
        min: sorted[0],
        max: sorted[n - 1],
        mean,
        stdev: var.sqrt(),
        q1: percentile(sorted, 0.25),
        median,
        q3: percentile(sorted, 0.75),
        trimmed_mean: trimmed.iter().sum::<f64>() / trimmed.len() as f64,
        min_change: best_t,
        min_change_kept: best_kept,
        target: median,
    }
}

pub const RG2_REFERENCE: f64 = -18.0;

/// Fill `want`, `gain`, `action`, `note` for every track. Returns the loudness statistics (with the target used).
pub fn plan(tracks: &mut [Track], o: &PlanOptions) -> Option<Stats> {
    let mut ls: Vec<f64> = tracks.iter().filter(|t| t.include).filter_map(|t| t.loudness).collect();
    if ls.is_empty() {
        for t in tracks.iter_mut() {
            t.action = Action::None;
            t.gain = 0.0;
            t.want = 0.0;
            t.note.clear();
        }
        return None;
    }
    ls.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut st = stats(&ls, o.tolerance);
    st.target = match o.target {
        Target::Median => st.median,
        Target::Mean => st.mean,
        Target::TrimmedMean => st.trimmed_mean,
        Target::MinChange => st.min_change,
        Target::Custom => o.custom_target,
    };
    let target = st.target;

    for t in tracks.iter_mut() {
        t.note.clear();
        t.want = 0.0;
        t.gain = 0.0;
        t.action = Action::None;
        let (Some(l), true) = (t.loudness, t.include) else {
            if t.include && t.error.is_some() {
                t.action = Action::Skip;
            }
            continue;
        };
        if o.method == Method::Tags {
            // ReplayGain 2.0 track gain (reference -18 LUFS); the player applies it.
            if t.format == Format::Opus {
                t.action = Action::Skip;
                t.note = "Opus は R128 系タグのため対象外（無劣化ヘッダゲインを使ってください）".into();
            } else {
                t.want = RG2_REFERENCE - l;
                t.gain = t.want;
                t.action = Action::ReplayGainTag;
            }
            continue;
        }
        let diff = target - l;
        if diff.abs() <= o.tolerance {
            continue;
        }
        let mut g = diff;
        if o.limit_peak && g > 0.0 {
            if let Some(pk) = t.peak_db {
                let room = (o.peak_ceiling - pk).max(0.0);
                if g > room {
                    g = room;
                    t.note = format!("ピーク制限（{:.1} dBTP まで）", o.peak_ceiling);
                }
            }
        }
        t.want = g;
        if g.abs() < 0.05 {
            continue;
        }
        let action = match (o.method, t.format) {
            (_, Format::Opus) => Action::OpusHeader,
            (Method::Reencode, _) => Action::Reencode,
            (Method::Lossless, Format::Mp3) => {
                let mut steps = (g / MP3_STEP_DB).round() as i32;
                // Never push a raised track over the peak ceiling because of rounding.
                if o.limit_peak && steps > 0 && steps as f64 * MP3_STEP_DB > g + 1e-9 && t.note.starts_with("ピーク") {
                    steps -= 1;
                }
                if steps == 0 {
                    t.note = append(&t.note, "1.5 dB 単位では 0 段");
                    Action::None
                } else {
                    Action::Mp3GlobalGain { steps }
                }
            }
            (Method::Lossless, Format::Flac | Format::Wav) => Action::Reencode,
            (Method::Lossless, _) => {
                if o.lossless_fallback_reencode {
                    t.note = append(&t.note, "無劣化手段がないため再エンコード");
                    Action::Reencode
                } else {
                    t.note = append(&t.note, "無劣化で変更できない形式");
                    Action::Skip
                }
            }
            (Method::Tags, _) => unreachable!(),
        };
        t.gain = match action {
            Action::Mp3GlobalGain { steps } => steps as f64 * MP3_STEP_DB,
            Action::Reencode | Action::OpusHeader => g,
            _ => 0.0,
        };
        t.action = action;
        if t.has_rg && t.action.changes_file() {
            t.note = append(&t.note, "既存の ReplayGain 値は補正前のまま");
        }
    }
    Some(st)
}

/// One MP3 global_gain step: 2^(1/4) in amplitude.
pub const MP3_STEP_DB: f64 = 1.505_149_978_319_906;

fn append(a: &str, b: &str) -> String {
    if a.is_empty() { b.to_string() } else { format!("{a}、{b}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics() {
        let v = [-30.0, -20.0, -19.0, -18.5, -18.0, -10.0];
        let s = stats(&v, 1.0);
        assert!((s.median - -18.75).abs() < 1e-9);
        assert!((s.mean - -19.25).abs() < 1e-9);
        assert!((s.q1 - -19.75).abs() < 1e-9 && (s.q3 - -18.125).abs() < 1e-9);
        // -20 .. -18 fits in a 2 dB window: 4 tracks kept, target in [-19, -19] -> -19.
        assert_eq!(s.min_change_kept, 4);
        assert!((s.min_change - -19.0).abs() < 1e-9);
    }

    #[test]
    fn min_change_prefers_median_side() {
        // Windows around -20 and -10 both keep 3 and are equally far from the median (-15); the first one wins.
        let v = [-21.0, -20.0, -19.0, -15.0, -11.0, -10.0, -9.0];
        let s = stats(&v, 1.0);
        assert_eq!(s.min_change_kept, 3);
        assert!(s.min_change < -18.0);
    }
}
