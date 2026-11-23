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

/// Silverman's rule-of-thumb kernel bandwidth (dB) for `values` (ascending): 0.9 * min(stdev, IQR / 1.34) * n^(-1/5).
/// A zero spread measure is skipped in the min; 0.5 dB when no spread is left (fewer than two values or all equal).
pub fn silverman_bandwidth(values: &[f64]) -> f64 {
    const FALLBACK: f64 = 0.5;
    debug_assert!(values.is_sorted(), "values must be ascending");
    let n = values.len();
    if n < 2 || values[0] == values[n - 1] {
        return FALLBACK;
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    let stdev = (values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt();
    let iqr = (percentile(values, 0.75) - percentile(values, 0.25)) / 1.34;
    match [stdev, iqr].into_iter().filter(|s| *s > 0.0).min_by(f64::total_cmp) {
        Some(spread) => 0.9 * spread * (n as f64).powf(-0.2),
        None => FALLBACK,
    }
}

/// Gaussian kernel density estimate at `x`: the mean of N(v, h) pdfs over `values` (ascending, for the cutoff search).
/// Values farther than 8 bandwidths from `x` are ignored (they contribute less than 1e-13 of a kernel peak);
/// values exactly 8 bandwidths away are included. Empty `values` give 0. `h` must be positive.
pub fn kde_density(values: &[f64], h: f64, x: f64) -> f64 {
    debug_assert!(h > 0.0, "bandwidth must be positive");
    if values.is_empty() {
        return 0.0;
    }
    let reach = 8.0 * h;
    let from = values.partition_point(|v| *v < x - reach);
    let to = values.partition_point(|v| *v <= x + reach);
    let sum: f64 = values[from..to].iter().map(|v| (-0.5 * ((x - v) / h).powi(2)).exp()).sum();
    sum / (values.len() as f64 * h * (2.0 * std::f64::consts::PI).sqrt())
}

/// Bandwidth used to draw the density curve over [lo, hi] sampled at `steps` intervals: the estimate `h`,
/// but never narrower than one sample interval so a peak cannot fall between samples.
pub fn display_bandwidth(h: f64, lo: f64, hi: f64, steps: usize) -> f64 {
    debug_assert!(steps > 0, "steps must be positive");
    h.max((hi - lo) / steps as f64)
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

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() <= 1e-9 * expected.abs(), "actual {actual}, expected {expected}");
    }

    #[test]
    fn silverman_uses_iqr_when_it_is_smaller_than_stdev() {
        // Type 7 quartiles -19.75 / -18.125 (IQR 1.625, /1.34 = 1.21); sample stdev sqrt(40.775) = 6.39.
        let v = [-30.0, -20.0, -19.0, -18.5, -18.0, -10.0];

        let h = silverman_bandwidth(&v);

        assert_close(h, 0.9 * (1.625 / 1.34) * 6f64.powf(-0.2));
    }

    #[test]
    fn silverman_uses_stdev_when_it_is_smaller_than_iqr() {
        // Type 7 quartiles -20 / -10 (IQR 10, /1.34 = 7.46); sample stdev sqrt(100 / 3) = 5.77.
        let v = [-20.0, -20.0, -10.0, -10.0];

        let h = silverman_bandwidth(&v);

        assert_close(h, 0.9 * (100.0f64 / 3.0).sqrt() * 4f64.powf(-0.2));
    }

    #[test]
    fn silverman_uses_stdev_when_iqr_is_zero() {
        // Both quartiles are -20; sample stdev sqrt(80 / 4) = sqrt(20).
        let v = [-20.0, -20.0, -20.0, -20.0, -10.0];

        let h = silverman_bandwidth(&v);

        assert_close(h, 0.9 * 20f64.sqrt() * 5f64.powf(-0.2));
    }

    #[test]
    fn silverman_falls_back_to_half_db_for_fewer_than_two_values() {
        assert_eq!(silverman_bandwidth(&[]), 0.5);
        assert_eq!(silverman_bandwidth(&[-14.0]), 0.5);
    }

    #[test]
    fn silverman_computes_bandwidth_from_two_distinct_values() {
        // Type 7 quartiles -17.5 / -12.5 (IQR 5, /1.34 = 3.73); sample stdev sqrt(50) = 7.07.
        let v = [-20.0, -10.0];

        let h = silverman_bandwidth(&v);

        assert_close(h, 0.9 * (5.0 / 1.34) * 2f64.powf(-0.2));
    }

    #[test]
    fn silverman_falls_back_to_half_db_when_all_values_are_equal() {
        // -29.9 is not exact in binary, so a naive mean leaves a stdev of about 4e-15 instead of 0.
        let v = [-29.9, -29.9, -29.9];

        let h = silverman_bandwidth(&v);

        assert_eq!(h, 0.5);
    }

    fn normal_pdf(mean: f64, sd: f64, x: f64) -> f64 {
        (-0.5 * ((x - mean) / sd).powi(2)).exp() / (sd * (2.0 * std::f64::consts::PI).sqrt())
    }

    #[test]
    fn kde_of_one_value_is_the_normal_pdf_around_it() {
        let (v, h) = (-14.0, 0.5);

        let at_center = kde_density(&[v], h, v);
        let off_center = kde_density(&[v], h, -13.2);

        assert_close(at_center, normal_pdf(v, h, v));
        assert_close(off_center, normal_pdf(v, h, -13.2));
    }

    #[test]
    fn kde_of_two_values_is_the_mean_of_their_normal_pdfs() {
        let (a, b, h, x) = (-16.0, -13.0, 0.5, -14.2);

        let d = kde_density(&[a, b], h, x);

        assert_close(d, (normal_pdf(a, h, x) + normal_pdf(b, h, x)) / 2.0);
    }

    #[test]
    fn kde_of_several_values_integrates_to_one() {
        let v = [-20.0, -15.0, -14.5, -9.0];
        let h = 0.8;
        let (a, b, steps) = (-20.0 - 6.0 * h, -9.0 + 6.0 * h, 20_000usize);
        let dx = (b - a) / steps as f64;

        let ys: Vec<f64> = (0..=steps).map(|i| kde_density(&v, h, a + i as f64 * dx)).collect();

        let area = dx * (ys.iter().sum::<f64>() - (ys[0] + ys[steps]) / 2.0);
        assert!((area - 1.0).abs() < 1e-3, "area {area}");
    }

    #[test]
    fn kde_includes_a_value_exactly_eight_bandwidths_away() {
        let d = kde_density(&[0.0], 1.0, 8.0);

        assert_close(d, normal_pdf(0.0, 1.0, 8.0));
    }

    #[test]
    fn kde_ignores_a_value_just_beyond_eight_bandwidths() {
        let d = kde_density(&[0.0], 1.0, 8.0 + 1e-9);

        assert_eq!(d, 0.0);
    }

    #[test]
    fn kde_of_no_values_is_zero() {
        assert_eq!(kde_density(&[], 1.0, 0.0), 0.0);
    }

    #[test]
    fn display_bandwidth_keeps_an_estimate_wider_than_the_grid() {
        // Grid spacing (-5 - -30) / 500 = 0.05.
        let h = display_bandwidth(0.9, -30.0, -5.0, 500);

        assert_close(h, 0.9);
    }

    #[test]
    fn display_bandwidth_widens_an_estimate_narrower_than_the_grid() {
        // Grid spacing (-5 - -30) / 500 = 0.05.
        let h = display_bandwidth(0.0005, -30.0, -5.0, 500);

        assert_close(h, 0.05);
    }

    #[test]
    fn display_bandwidth_keeps_an_estimate_equal_to_the_grid() {
        // Grid spacing (1 - 0) / 4 = 0.25, exact in binary.
        let h = display_bandwidth(0.25, 0.0, 1.0, 4);

        assert_close(h, 0.25);
    }
}
