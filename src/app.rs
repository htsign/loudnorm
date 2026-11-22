//! egui front end.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, RichText, Stroke};
use egui_extras::{Column, TableBuilder};
use rayon::prelude::*;

use crate::apply::{self, ApplyOptions, Mp3Encode, Output};
use crate::core::{self, Action, Method, PlanOptions, Stats, Target, Track};
use crate::ffmpeg::Ffmpeg;
use crate::settings::Settings;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Analyzing,
    Applying,
}

struct Shared {
    tracks: Vec<Track>,
    done: usize,
    total: usize,
    phase: Phase,
    analyzed: bool,
    applied: bool,
    log: Vec<String>,
}

pub struct App {
    sh: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    root: String,
    ff_hint: String,
    /// The trimmed hint that `ff` was detected with; differs from `ff_hint` while a typed location is not yet applied.
    ff_located_hint: String,
    ff: Result<Ffmpeg, String>,
    po: PlanOptions,
    mp3: Mp3Encode,
    aac_kbps: u32,
    vorbis_q: u32,
    in_place: bool,
    out_dir: String,
    keep_mtime: bool,
    check_updates: bool,
    jobs: usize,
    true_peak: bool,
    last_folder: String,
    saved: Settings,
    last_save: std::time::Instant,
    save_error_logged: bool,
    win_w: f32,
    win_h: f32,
    win_max: bool,
    col_w: Vec<f32>,
    only_changes: bool,
    confirm: bool,
    stats: Option<Stats>,
    graph: GraphView,
    auto_start: bool,
    show_prefs: bool,
    show_about: bool,
    /// Frame number (`Context::cumulative_frame_nr`) from which the folder picker chosen in the menu may open.
    open_folder_at: Option<u64>,
}

const ACCENT: Color32 = Color32::from_rgb(64, 120, 242);
const WARN: Color32 = Color32::from_rgb(226, 122, 40);
const BAD: Color32 = Color32::from_rgb(214, 64, 64);

/// Pick a Japanese UI font and make it the *primary* font, so Latin and Japanese text share one set of metrics
/// (mixing egui's default Latin font with a CJK fallback misaligns baselines, notably with Yu Gothic).
/// Returns a description of the chosen face.
fn install_fonts(ctx: &egui::Context) -> Option<String> {
    // (file, family to look for), in order of preference. Faces with a non-regular weight or italics are skipped,
    // e.g. YuGothR.ttc also holds "Yu Gothic UI Semilight".
    let cands: &[(&str, &str)] = &[
        ("C:\\Windows\\Fonts\\YuGothM.ttc", "Yu Gothic UI"),
        ("C:\\Windows\\Fonts\\YuGothR.ttc", "Yu Gothic UI"),
        ("C:\\Windows\\Fonts\\meiryo.ttc", "Meiryo UI"),
        ("C:\\Windows\\Fonts\\msgothic.ttc", "MS UI Gothic"),
        ("/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc", "Hiragino"),
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", "Noto Sans CJK JP"),
        ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", "Noto Sans CJK JP"),
    ];
    const NOT_REGULAR: [&str; 9] = ["Light", "Semilight", "Bold", "Semibold", "Black", "Heavy", "Thin", "Italic", "Oblique"];
    let names = |data: &[u8], i: u32| -> Vec<String> {
        ttf_parser::Face::parse(data, i)
            .map(|f| {
                f.names()
                    .into_iter()
                    .filter(|n| n.name_id == ttf_parser::name_id::FULL_NAME || n.name_id == ttf_parser::name_id::FAMILY)
                    .filter_map(|n| n.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut chosen: Option<(Vec<u8>, u32, String)> = None;
    'files: for (path, family) in cands {
        let Ok(data) = std::fs::read(path) else { continue };
        let count = ttf_parser::fonts_in_collection(&data).unwrap_or(1);
        for i in 0..count {
            let ns = names(&data, i);
            if ns.iter().any(|n| n.contains(family)) && !ns.iter().any(|n| NOT_REGULAR.iter().any(|w| n.contains(w))) {
                let label = format!("{} (#{i}, {path})", ns.iter().find(|n| n.contains(family)).unwrap());
                chosen = Some((data, i, label));
                break 'files;
            }
        }
    }
    let (data, index, label) = chosen?;
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("jp".into(), Arc::new(egui::FontData::from_blob(Arc::new(data), index)));
    fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "jp".into());
    fonts.families.entry(egui::FontFamily::Monospace).or_default().push("jp".into());
    ctx.set_fonts(fonts);
    Some(label)
}

/// Shorten `text` to the cell width by cutting the middle: "abcde…vwxyz".
fn elide_middle(ui: &egui::Ui, text: &str) -> String {
    let max = ui.available_width() - 2.0;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let chars: Vec<char> = text.chars().collect();
    let (widths, ell) = ui.ctx().fonts_mut(|f| {
        (chars.iter().map(|&c| f.glyph_width(&font, c)).collect::<Vec<f32>>(), f.glyph_width(&font, '…'))
    });
    if widths.iter().sum::<f32>() <= max || chars.len() < 3 {
        return text.to_string();
    }
    let budget = (max - ell).max(0.0);
    // Head gets up to half the budget, the tail gets the rest (file names keep their ending and extension).
    let (mut head, mut used) = (0usize, 0.0f32);
    while head < chars.len() && used + widths[head] <= budget / 2.0 {
        used += widths[head];
        head += 1;
    }
    let mut tail = chars.len();
    while tail > head && used + widths[tail - 1] <= budget {
        used += widths[tail - 1];
        tail -= 1;
    }
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[tail..]);
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GraphView {
    Dots,
    Histogram,
}

fn short_label(t: Target) -> &'static str {
    match t {
        Target::TrimmedMean => "トリム平均",
        Target::MinChange => "変更最少",
        _ => t.label(),
    }
}

/// Loudness graph: one dot per track (with its planned move) or a histogram, the target line and the
/// tolerance band, and a Q1–median–Q3 box. Returns the LUFS value the user clicked, if any.
fn loudness_graph(ui: &mut egui::Ui, tracks: &[Track], st: Option<&Stats>, po: &PlanOptions, view: GraphView) -> Option<f64> {
    let pts: Vec<(f64, f64, Action, bool)> = tracks
        .iter()
        .filter(|t| t.include)
        .filter_map(|t| t.loudness.map(|l| (l, t.gain, t.action, t.error.is_some())))
        .collect();
    let h = if view == GraphView::Histogram { 140.0 } else { 84.0 };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0, ui.visuals().extreme_bg_color);
    let (Some(st), false) = (st, pts.is_empty()) else { return None };
    let weak = ui.visuals().weak_text_color();
    let tags = po.method == Method::Tags;
    let lo = pts.iter().map(|x| x.0.min(x.0 + x.1)).fold(st.target, f64::min).min(st.min) - 1.0;
    let hi = pts.iter().map(|x| x.0.max(x.0 + x.1)).fold(st.target, f64::max).max(st.max) + 1.0;
    let (lo, hi) = if tags { (st.min - 1.0, st.max + 1.0) } else { (lo, hi) };
    let pad = 14.0;
    let w = rect.width() - 2.0 * pad;
    let x = |v: f64| rect.left() + pad + ((v - lo) / (hi - lo).max(1e-6)) as f32 * w;
    let inv = |px: f32| lo + ((px - rect.left() - pad) / w) as f64 * (hi - lo);
    let top = rect.top() + 6.0;
    let axis_y = rect.bottom() - 18.0;
    let box_y = axis_y - 8.0;
    let plot_bottom = box_y - 8.0;

    if !tags {
        let band = egui::Rect::from_x_y_ranges(x(st.target - po.tolerance)..=x(st.target + po.tolerance), top..=plot_bottom);
        p.rect_filled(band, 3.0, ACCENT.gamma_multiply(0.15));
    }
    match view {
        GraphView::Dots => {
            let mid_y = (top + plot_bottom) / 2.0;
            let n = pts.len().max(1);
            for (i, (l, g, a, err)) in pts.iter().enumerate() {
                let y = mid_y + ((i * 7919) % 17) as f32 - 8.0; // light jitter so dots don't stack
                let c = if *err || *a == Action::Skip {
                    BAD
                } else if a.changes_file() {
                    WARN
                } else {
                    weak
                };
                if a.changes_file() && *a != Action::ReplayGainTag {
                    p.line_segment([egui::pos2(x(*l), y), egui::pos2(x(l + g), y)], Stroke::new(1.0, c.gamma_multiply(0.6)));
                }
                p.circle_filled(egui::pos2(x(*l), y), if n > 400 { 2.0 } else { 3.0 }, c);
            }
        }
        GraphView::Histogram => {
            // Bin width: 0.5 LU for narrow spreads, otherwise 1 LU.
            let bw = if st.max - st.min <= 12.0 { 0.5 } else { 1.0 };
            let first = (lo / bw).floor() * bw;
            let nb = (((hi - first) / bw).ceil() as usize).max(1);
            let mut bins = vec![(0usize, 0usize); nb]; // (kept, changed)
            for (l, _, a, _) in &pts {
                let i = (((l - first) / bw).floor() as usize).min(nb - 1);
                if a.changes_file() { bins[i].1 += 1 } else { bins[i].0 += 1 }
            }
            let maxc = bins.iter().map(|b| b.0 + b.1).max().unwrap_or(1).max(1);
            let unit = (plot_bottom - top - 12.0) / maxc as f32;
            let font = egui::FontId::proportional(10.0);
            for (i, (k, c)) in bins.iter().enumerate() {
                if k + c == 0 {
                    continue;
                }
                let x0 = x(first + i as f64 * bw) + 0.5;
                let x1 = x(first + (i + 1) as f64 * bw) - 0.5;
                let yk = plot_bottom - *k as f32 * unit;
                let yc = yk - *c as f32 * unit;
                p.rect_filled(egui::Rect::from_x_y_ranges(x0..=x1, yk..=plot_bottom), 1.0, weak.gamma_multiply(0.8));
                p.rect_filled(egui::Rect::from_x_y_ranges(x0..=x1, yc..=yk), 1.0, WARN);
                if x1 - x0 > 9.0 {
                    p.text(egui::pos2((x0 + x1) / 2.0, yc - 1.0), egui::Align2::CENTER_BOTTOM, format!("{}", k + c), font.clone(), weak);
                }
            }
        }
    }
    // Box: Q1–Q3 with the median tick, and the mean as a small diamond.
    let bstroke = Stroke::new(1.0, weak);
    p.line_segment([egui::pos2(x(st.min), box_y), egui::pos2(x(st.max), box_y)], bstroke);
    p.rect_stroke(egui::Rect::from_x_y_ranges(x(st.q1)..=x(st.q3), box_y - 4.0..=box_y + 4.0), 1.0, bstroke, egui::StrokeKind::Middle);
    p.line_segment([egui::pos2(x(st.median), box_y - 4.0), egui::pos2(x(st.median), box_y + 4.0)], Stroke::new(2.0, weak));
    let m = egui::pos2(x(st.mean), box_y);
    p.add(egui::Shape::convex_polygon(
        vec![m + egui::vec2(0.0, -3.5), m + egui::vec2(3.5, 0.0), m + egui::vec2(0.0, 3.5), m + egui::vec2(-3.5, 0.0)],
        ui.visuals().text_color(),
        Stroke::NONE,
    ));
    if !tags {
        p.line_segment([egui::pos2(x(st.target), top - 2.0), egui::pos2(x(st.target), plot_bottom)], Stroke::new(1.5, ACCENT));
    }
    // Axis labels.
    let font = egui::FontId::proportional(11.0);
    let step = if hi - lo > 24.0 { 6.0 } else if hi - lo > 10.0 { 2.0 } else { 1.0 };
    let mut v = (lo / step).ceil() * step;
    while v <= hi {
        p.text(egui::pos2(x(v), rect.bottom() - 3.0), egui::Align2::CENTER_BOTTOM, format!("{v:.0}"), font.clone(), weak);
        v += step;
    }
    p.text(egui::pos2(rect.right() - 6.0, rect.top() + 4.0), egui::Align2::RIGHT_TOP, "LUFS", font, weak);
    // Hover guide and click-to-set.
    if tags {
        return None;
    }
    if let Some(pos) = resp.hover_pos() {
        let v = inv(pos.x);
        p.line_segment([egui::pos2(pos.x, top), egui::pos2(pos.x, plot_bottom)], Stroke::new(1.0, ui.visuals().text_color().gamma_multiply(0.4)));
        let kept = pts.iter().filter(|q| (q.0 - v).abs() <= po.tolerance).count();
        resp.clone().on_hover_text(format!("{v:.1} LUFS を目標にする（変更なし {kept} / {} 曲）", pts.len()));
    }
    if resp.clicked() {
        resp.interact_pointer_pos().map(|pos| inv(pos.x))
    } else {
        None
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, st: Settings, settings_msg: String) -> Self {
        let font = install_fonts(&cc.egui_ctx);
        let ff_hint = st.ffmpeg.clone();
        let graph = if st.histogram { GraphView::Histogram } else { GraphView::Dots };
        let ff = if ff_hint.trim().is_empty() {
            Ffmpeg::locate(None)
        } else {
            Ffmpeg::locate(Some(std::path::Path::new(ff_hint.trim())))
        }
        .map_err(|e| e.to_string());
        // A folder passed on the command line (or dropped on the exe) is analysed right away.
        let arg = std::env::args().nth(1).filter(|a| std::path::Path::new(a).is_dir());
        App {
            auto_start: arg.is_some(),
            sh: Arc::new(Mutex::new(Shared {
                tracks: Vec::new(),
                done: 0,
                total: 0,
                phase: Phase::Idle,
                analyzed: false,
                applied: false,
                log: vec![
                    match font {
                        Some(f) => format!("フォント: {f}"),
                        None => "日本語フォントが見つかりません".into(),
                    },
                    settings_msg,
                ],
            })),
            cancel: Arc::new(AtomicBool::new(false)),
            root: arg.unwrap_or_else(|| st.last_folder.clone()),
            ff_located_hint: ff_hint.trim().to_string(),
            ff_hint,
            ff,
            po: PlanOptions {
                method: st.method,
                tolerance: st.tolerance,
                peak_ceiling: st.peak_ceiling,
                limit_peak: st.limit_peak,
                lossless_fallback_reencode: st.lossless_fallback_reencode,
                target: st.target,
                custom_target: st.custom_target,
            },
            mp3: st.mp3,
            aac_kbps: st.aac_kbps,
            vorbis_q: st.vorbis_q,
            in_place: st.in_place,
            out_dir: st.output_folder.clone(),
            keep_mtime: st.keep_mtime,
            check_updates: st.check_updates,
            jobs: st.jobs,
            true_peak: st.true_peak,
            last_folder: st.last_folder.clone(),
            last_save: std::time::Instant::now(),
            save_error_logged: false,
            win_w: st.window_width,
            win_h: st.window_height,
            win_max: st.window_maximized,
            col_w: st.column_widths.clone(),
            saved: st,
            only_changes: false,
            confirm: false,
            stats: None,
            graph,
            show_prefs: false,
            show_about: false,
            open_folder_at: None,
        }
    }

    fn current_settings(&self) -> Settings {
        Settings {
            ffmpeg: self.ff_hint.trim().to_string(),
            last_folder: self.last_folder.clone(),
            method: self.po.method,
            tolerance: self.po.tolerance,
            limit_peak: self.po.limit_peak,
            peak_ceiling: self.po.peak_ceiling,
            lossless_fallback_reencode: self.po.lossless_fallback_reencode,
            target: self.po.target,
            custom_target: self.po.custom_target,
            histogram: self.graph == GraphView::Histogram,
            mp3: self.mp3,
            aac_kbps: self.aac_kbps,
            vorbis_q: self.vorbis_q,
            jobs: self.jobs,
            true_peak: self.true_peak,
            in_place: self.in_place,
            output_folder: self.out_dir.trim().to_string(),
            keep_mtime: self.keep_mtime,
            check_updates: self.check_updates,
            window_width: self.win_w,
            window_height: self.win_h,
            window_maximized: self.win_max,
            column_widths: self.col_w.clone(),
        }
    }

    /// Write the settings file when something changed (at most once a second).
    fn autosave(&mut self, ctx: &egui::Context) {
        let cur = self.current_settings();
        if cur == self.saved {
            return;
        }
        let wait = std::time::Duration::from_secs(1);
        if self.last_save.elapsed() < wait {
            ctx.request_repaint_after(wait);
            return;
        }
        self.last_save = std::time::Instant::now();
        match cur.save() {
            Ok(()) => self.saved = cur,
            Err(e) => {
                if !self.save_error_logged {
                    self.log(format!("設定を保存できません: {e}"));
                    self.save_error_logged = true;
                }
                self.saved = cur;
            }
        }
    }

    fn log(&self, s: impl Into<String>) {
        self.sh.lock().unwrap().log.push(s.into());
    }

    fn start_analyze(&mut self, ctx: &egui::Context) {
        let root = PathBuf::from(self.root.trim());
        if !root.is_dir() {
            self.log("フォルダが見つかりません");
            return;
        }
        self.last_folder = root.display().to_string();
        let (sh, cancel, ctx) = (self.sh.clone(), self.cancel.clone(), ctx.clone());
        let ff = self.ff.as_ref().ok().cloned();
        let (jobs, true_peak) = (self.jobs, self.true_peak);
        cancel.store(false, Ordering::SeqCst);
        {
            let mut s = sh.lock().unwrap();
            s.phase = Phase::Analyzing;
            s.tracks.clear();
            s.done = 0;
            s.total = 0;
            s.analyzed = false;
            s.applied = false;
            s.log.push(format!("走査: {}", root.display()));
        }
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build().expect("thread pool");
            let tracks = core::scan(&root);
            let n = tracks.len();
            {
                let mut s = sh.lock().unwrap();
                s.tracks = tracks;
                s.total = n;
            }
            ctx.request_repaint();
            pool.install(|| (0..n).into_par_iter().for_each(|i| {
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                let path = sh.lock().unwrap().tracks[i].path.clone();
                let r = core::analyze(&path, ff.as_ref(), true_peak);
                let rg = core::has_replaygain(&path);
                let mut s = sh.lock().unwrap();
                let t = &mut s.tracks[i];
                match r {
                    Ok((l, p)) => {
                        t.loudness = Some(l);
                        t.peak_db = Some(p);
                    }
                    Err(e) => t.error = Some(format!("{e:#}")),
                }
                t.has_rg = rg;
                s.done += 1;
                drop(s);
                ctx.request_repaint();
            }));
            let mut s = sh.lock().unwrap();
            let errs = s.tracks.iter().filter(|t| t.error.is_some()).count();
            let msg = if cancel.load(Ordering::SeqCst) {
                "解析を中止しました".to_string()
            } else {
                format!("解析完了: {} 曲（エラー {} 曲）、{:.1} 秒、{jobs} 並列", s.done, errs, t0.elapsed().as_secs_f64())
            };
            s.log.push(msg);
            s.phase = Phase::Idle;
            s.analyzed = true;
            ctx.request_repaint();
        });
    }

    fn apply_options(&self) -> ApplyOptions {
        let root = PathBuf::from(self.root.trim());
        ApplyOptions {
            output: if self.in_place {
                Output::InPlace { backup: root.join(core::BACKUP_DIR) }
            } else {
                Output::Folder(PathBuf::from(self.out_dir.trim()))
            },
            keep_mtime: self.keep_mtime,
            mp3: self.mp3,
            aac_kbps: self.aac_kbps,
            vorbis_q: self.vorbis_q,
        }
    }

    fn start_apply(&mut self, ctx: &egui::Context) {
        let (sh, cancel, ctx) = (self.sh.clone(), self.cancel.clone(), ctx.clone());
        let ff = self.ff.as_ref().ok().cloned();
        let ao = self.apply_options();
        let jobs = self.jobs;
        cancel.store(false, Ordering::SeqCst);
        let idx: Vec<usize> = {
            let mut s = sh.lock().unwrap();
            s.phase = Phase::Applying;
            let idx: Vec<usize> =
                s.tracks.iter().enumerate().filter(|(_, t)| t.include && t.action.changes_file()).map(|(i, _)| i).collect();
            s.done = 0;
            s.total = idx.len();
            for &i in &idx {
                s.tracks[i].status = "待機中".into();
            }
            s.log.push(format!("適用開始: {} 曲", idx.len()));
            idx
        };
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build().expect("thread pool");
            pool.install(|| idx.par_iter().for_each(|&i| {
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                let t = sh.lock().unwrap().tracks[i].clone();
                let r = apply::apply(&t, ff.as_ref(), &ao);
                let mut s = sh.lock().unwrap();
                s.tracks[i].status = match &r {
                    Ok(m) => format!("✔ {m}"),
                    Err(e) => format!("✖ {e:#}"),
                };
                if let Err(e) = r {
                    let line = format!("失敗: {} — {e:#}", t.rel.display());
                    s.log.push(line);
                }
                s.done += 1;
                drop(s);
                ctx.request_repaint();
            }));
            let mut s = sh.lock().unwrap();
            let ok = s.tracks.iter().filter(|t| t.status.starts_with('✔')).count();
            let ng = s.tracks.iter().filter(|t| t.status.starts_with('✖')).count();
            s.log.push(format!("適用完了: 成功 {ok} 曲、失敗 {ng} 曲、{:.1} 秒、{jobs} 並列。結果を確かめるには再解析してください。", t0.elapsed().as_secs_f64()));
            s.phase = Phase::Idle;
            s.applied = true;
            ctx.request_repaint();
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, busy: bool) {
        ui.add_enabled_ui(!busy, |ui| {
            ui.heading("調整方法");
            ui.radio_value(&mut self.po.method, Method::Reencode, "再エンコード（精密）")
                .on_hover_text("ずれた曲だけを 0.01 dB 単位で補正して再エンコード。タグ・歌詞・ジャケットは元ファイルから引き継ぐ");
            ui.radio_value(&mut self.po.method, Method::Lossless, "無劣化を優先")
                .on_hover_text("MP3 はフレームのゲイン値を 1.5 dB 単位で書き換え（再エンコードなし・タグ無変更）。FLAC/WAV は可逆なので再エンコード");
            ui.radio_value(&mut self.po.method, Method::Tags, "ReplayGain タグだけ書く")
                .on_hover_text("音声は変えず、全曲に ReplayGain 2.0（基準 −18 LUFS）のトラックゲインを書き込む。再生側の対応が必要");
            if self.po.method == Method::Lossless {
                ui.checkbox(&mut self.po.lossless_fallback_reencode, "AAC / Vorbis は再エンコードで補正");
            }
            ui.add_space(8.0);
            ui.heading("揃え方");
            ui.add_enabled_ui(self.po.method != Method::Tags, |ui| {
                ui.horizontal(|ui| {
                    ui.label("許容幅 ±");
                    ui.add(egui::DragValue::new(&mut self.po.tolerance).range(0.0..=10.0).speed(0.05).suffix(" dB"));
                });
                ui.label(RichText::new("目標からこの幅に収まる曲は変更しません").small().weak());
                ui.label("目標");
                let prev = self.po.target;
                for t in Target::ALL {
                    let val = self.stats.as_ref().map(|st| match t {
                        Target::Median => st.median,
                        Target::Mean => st.mean,
                        Target::TrimmedMean => st.trimmed_mean,
                        Target::MinChange => st.min_change,
                        Target::Custom => self.po.custom_target,
                    });
                    let text = match val {
                        Some(v) if t != Target::Custom => format!("{}：{:.1}", t.label(), v),
                        _ => t.label().to_string(),
                    };
                    let r = ui.radio_value(&mut self.po.target, t, text);
                    if t == Target::MinChange {
                        if let Some(st) = &self.stats {
                            r.on_hover_text(format!("許容幅に収まる曲が最も多くなる値（{} / {} 曲がそのまま）。候補が複数あるときは中央値に近いもの", st.min_change_kept, st.n));
                        }
                    }
                }
                if self.po.target == Target::Custom && prev != Target::Custom {
                    // Start from the target in use so switching to custom does not jump.
                    if let Some(st) = &self.stats {
                        self.po.custom_target = (st.target * 10.0).round() / 10.0;
                    }
                }
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    let r = ui.add(egui::DragValue::new(&mut self.po.custom_target).range(-60.0..=0.0).speed(0.05).fixed_decimals(1).suffix(" LUFS"));
                    if r.changed() {
                        self.po.target = Target::Custom;
                    }
                });
                ui.checkbox(&mut self.po.limit_peak, "上げるときはピークを制限");
                ui.add_enabled_ui(self.po.limit_peak, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("上限");
                        ui.add(egui::DragValue::new(&mut self.po.peak_ceiling).range(-6.0..=0.0).speed(0.05).suffix(" dBTP"));
                    });
                });
            });
            ui.add_space(8.0);
            ui.heading("再エンコード設定");
            egui::ComboBox::from_label("MP3")
                .selected_text(self.mp3.label())
                .show_ui(ui, |ui| {
                    for m in [Mp3Encode::Cbr320, Mp3Encode::Cbr256, Mp3Encode::V0] {
                        ui.selectable_value(&mut self.mp3, m, m.label());
                    }
                });
            egui::ComboBox::from_label("AAC（最低）")
                .selected_text(format!("{} kbps", self.aac_kbps))
                .show_ui(ui, |ui| {
                    for k in [192, 256, 320] {
                        ui.selectable_value(&mut self.aac_kbps, k, format!("{k} kbps"));
                    }
                });
            ui.add(egui::Slider::new(&mut self.vorbis_q, 3..=10).text("Vorbis 品質"));
            ui.label(RichText::new("FLAC / ALAC / WAV は元のビット深度のまま可逆で書き出し").small().weak());
            ui.add_space(8.0);
            ui.heading("処理");
            ui.horizontal(|ui| {
                ui.label("同時に処理する曲数");
                ui.add(egui::DragValue::new(&mut self.jobs).range(1..=256));
            })
            .response
            .on_hover_text("解析も適用も、曲単位で並列に処理します。既定は CPU の論理コア数");
            ui.checkbox(&mut self.true_peak, "真のピークを測る（解析が遅くなる）")
                .on_hover_text("オフにするとサンプルピークで代用し、解析が速くなります。変更後は再解析が必要です");
            ui.add_space(8.0);
            ui.heading("出力");
            ui.radio_value(&mut self.in_place, true, "上書き（元ファイルはバックアップへ）");
            ui.label(RichText::new(format!("バックアップ先: 対象フォルダ\\{}", core::BACKUP_DIR)).small().weak());
            ui.radio_value(&mut self.in_place, false, "変更した曲だけ別フォルダへ");
            if !self.in_place {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.out_dir).desired_width(170.0).hint_text("出力フォルダ"));
                    if ui.button("選択…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.out_dir = p.display().to_string();
                        }
                    }
                });
            }
            ui.checkbox(&mut self.keep_mtime, "更新日時を元のまま保つ");
        });
    }

    /// Ask for a folder and analyse it.
    fn pick_folder_and_analyze(&mut self, ctx: &egui::Context) {
        if let Some(p) = rfd::FileDialog::new().pick_folder() {
            self.root = p.display().to_string();
            self.start_analyze(ctx);
        }
    }

    /// ffmpeg location: status, hint field, folder / file pickers and re-detection.
    fn ffmpeg_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            match &self.ff {
                Ok(f) => ui.label(RichText::new(format!("ffmpeg: {}", f.ffmpeg.display())).small().weak()).on_hover_text(&f.version),
                Err(e) => ui.label(RichText::new(format!("⚠ {e}")).small().color(WARN)),
            };
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.ff_hint).desired_width(260.0).hint_text("ffmpeg の場所（任意）"));
            let mut relocate = false;
            if ui.small_button("フォルダ…").on_hover_text("ffmpeg と ffprobe があるフォルダを選ぶ").clicked() {
                if let Some(p) = rfd::FileDialog::new().set_title("ffmpeg のあるフォルダ").pick_folder() {
                    self.ff_hint = p.display().to_string();
                    relocate = true;
                }
            }
            if ui.small_button("ファイル…").on_hover_text("ffmpeg の実行ファイルを選ぶ").clicked() {
                let mut d = rfd::FileDialog::new().set_title("ffmpeg の実行ファイル");
                if cfg!(windows) {
                    d = d.add_filter("ffmpeg.exe", &["exe"]);
                }
                if let Some(p) = d.pick_file() {
                    self.ff_hint = p.display().to_string();
                    relocate = true;
                }
            }
            if ui.small_button("再検出").clicked() || relocate {
                self.relocate_ffmpeg();
            }
        });
    }

    /// Detect ffmpeg again from the current hint (an empty hint falls back to the default search).
    fn relocate_ffmpeg(&mut self) {
        let hint = self.ff_hint.trim().to_string();
        self.ff = Ffmpeg::locate(Some(std::path::Path::new(&hint))).map_err(|e| e.to_string());
        self.ff_located_hint = hint;
        let msg = match &self.ff {
            Ok(f) => format!("ffmpeg: {}", f.ffmpeg.display()),
            Err(e) => e.clone(),
        };
        self.log(msg);
    }

    /// Draws the menu bar; returns true when フォルダを開く… was chosen (the blocking picker is opened by the caller after the menu is drawn).
    fn menu_bar_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, busy: bool) -> bool {
        let mut open_folder = false;
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("ファイル", |ui| {
                if ui.add_enabled(!busy, egui::Button::new("フォルダを開く…")).clicked() {
                    ui.close();
                    open_folder = true;
                }
                ui.separator();
                if ui.add_enabled(!busy, egui::Button::new("終了")).clicked() {
                    ui.close();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("設定", |ui| {
                if ui.button("環境設定…").clicked() {
                    ui.close();
                    self.show_prefs = true;
                }
            });
            ui.menu_button("ヘルプ", |ui| {
                if ui.button("バージョン情報…").clicked() {
                    ui.close();
                    self.show_about = true;
                }
            });
        });
        open_folder
    }

    fn dialogs_ui(&mut self, ctx: &egui::Context) {
        if self.show_prefs {
            let resp = egui::Modal::new(egui::Id::unique("loudnorm-prefs")).show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.heading("環境設定");
                ui.add_space(4.0);
                ui.label("ffmpeg");
                self.ffmpeg_ui(ui);
                ui.add_space(8.0);
                if ui.button("閉じる").clicked() {
                    ui.close();
                }
            });
            if resp.should_close() {
                self.show_prefs = false;
                // Whichever way the dialog is closed, a typed location must not stay saved but unused this session.
                if self.ff_hint.trim() != self.ff_located_hint {
                    self.relocate_ffmpeg();
                }
            }
        }
        if self.show_about {
            let resp = egui::Modal::new(egui::Id::unique("loudnorm-about")).show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.heading("loudnorm");
                ui.label(format!("バージョン {}", env!("CARGO_PKG_VERSION")));
                ui.add_space(8.0);
                if ui.button("閉じる").clicked() {
                    ui.close();
                }
            });
            if resp.should_close() {
                self.show_about = false;
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Track the window size; while maximized or minimized keep the last normal size.
        let (rect, maxd, mind) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().maximized, i.viewport().minimized));
        if mind != Some(true) {
            if let Some(m) = maxd {
                self.win_max = m;
            }
            if let (Some(r), false) = (rect, self.win_max) {
                self.win_w = r.width().round();
                self.win_h = r.height().round();
            }
        }
        self.autosave(&ctx);
        // The picker blocks the UI thread, so open it only after the menu has been painted closed; otherwise the open menu stays frozen on screen.
        // Count painted frames, not ui() calls: a frame may run ui() more than once (request_discard).
        if let Some(at) = self.open_folder_at {
            if ctx.cumulative_frame_nr() >= at {
                self.open_folder_at = None;
                // Something may have started in the frame in between (e.g. 解析 was clicked); do not open the picker while busy.
                if self.sh.lock().unwrap().phase == Phase::Idle {
                    self.pick_folder_and_analyze(&ctx);
                }
            } else {
                ctx.request_repaint();
            }
        }
        if std::mem::take(&mut self.auto_start) {
            self.start_analyze(&ctx);
        }
        let (phase, analyzed, applied, done, total) = {
            let s = self.sh.lock().unwrap();
            (s.phase, s.analyzed, s.applied, s.done, s.total)
        };
        let busy = phase != Phase::Idle;
        if !busy && analyzed {
            let mut s = self.sh.lock().unwrap();
            self.stats = core::plan(&mut s.tracks, &self.po);
        }

        egui::Panel::top("menubar").show(ui, |ui| {
            if self.menu_bar_ui(ui, &ctx, busy) {
                self.open_folder_at = Some(ctx.cumulative_frame_nr() + 2);
                ctx.request_repaint();
            }
        });

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("フォルダ");
                ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut self.root).desired_width(ui.available_width() - 260.0).hint_text("曲が入ったフォルダ（サブフォルダも対象）"),
                );
                if ui.add_enabled(!busy, egui::Button::new("選択…")).clicked() {
                    self.pick_folder_and_analyze(&ctx);
                }
                if ui.add_enabled(!busy && !self.root.trim().is_empty(), egui::Button::new(RichText::new("解析").strong())).clicked() {
                    self.start_analyze(&ctx);
                }
                if busy && ui.button("中止").clicked() {
                    self.cancel.store(true, Ordering::SeqCst);
                }
            });
            if let Err(e) = &self.ff {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("⚠ {e}")).small().color(WARN));
                    if ui.link(RichText::new("環境設定を開く").small()).clicked() {
                        self.show_prefs = true;
                    }
                });
            }
            if busy {
                let label = if phase == Phase::Analyzing { "解析中" } else { "適用中" };
                ui.add(egui::ProgressBar::new(if total > 0 { done as f32 / total as f32 } else { 0.0 }).text(format!("{label} {done} / {total}")));
            }
            ui.add_space(4.0);
        });

        egui::Panel::left("settings").resizable(false).default_size(290.0).show(ui, |ui| {
            // The apply button stays visible at the bottom; the settings above scroll.
            egui::Panel::bottom("apply_bar").show_separator_line(false).show(ui, |ui| {
                ui.add_space(8.0);
                let s = self.sh.lock().unwrap();
                let n_change = s.tracks.iter().filter(|t| t.include && t.action.changes_file()).count();
                drop(s);
                let out_ok = self.in_place || !self.out_dir.trim().is_empty();
                let can = !busy && analyzed && !applied && n_change > 0 && out_ok;
                let btn = egui::Button::new(RichText::new(format!("{n_change} 曲に適用…")).strong().size(16.0)).min_size(egui::vec2(260.0, 40.0));
                if ui.add_enabled(can, btn).clicked() {
                    self.confirm = true;
                }
                if applied {
                    ui.label(RichText::new("適用済みです。続けるには再解析してください。").small().color(WARN));
                } else if !out_ok {
                    ui.label(RichText::new("出力フォルダを指定してください").small().color(WARN));
                }
                ui.add_space(6.0);
            });
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(6.0);
                self.settings_ui(ui, busy);
                ui.add_space(12.0);
            });
        });

        egui::Panel::bottom("log").resizable(true).default_size(90.0).show(ui, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for l in &self.sh.lock().unwrap().log {
                    ui.label(RichText::new(l).small());
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let mut s = self.sh.lock().unwrap();
            if s.tracks.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("フォルダを選ぶと、サブフォルダまで含めて MP3 / FLAC / M4A / Ogg / Opus / WAV を解析します。\n解析だけではファイルは変更されません。").weak());
                });
                return;
            }
            let ok: Vec<&Track> = s.tracks.iter().filter(|t| t.include && t.loudness.is_some()).collect();
            let changed = ok.iter().filter(|t| t.action.changes_file()).count();
            let skipped = s.tracks.iter().filter(|t| t.include && t.action == Action::Skip && t.error.is_none()).count();
            let errs = s.tracks.iter().filter(|t| t.error.is_some()).count();
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(match &self.stats {
                    _ if self.po.method == Method::Tags => "ReplayGain 基準 −18 LUFS".to_string(),
                    Some(st) => format!("目標 {:.1} LUFS（{}）", st.target, short_label(self.po.target)),
                    None => "目標 —".into(),
                }).strong().size(18.0).color(ACCENT));
                ui.label(format!("解析済み {} 曲", ok.len()));
                ui.label(format!("変更なし {} 曲", ok.len() - changed));
                ui.label(RichText::new(format!("変更 {changed} 曲")).color(WARN));
                if skipped > 0 {
                    ui.label(RichText::new(format!("スキップ {skipped} 曲")).color(BAD));
                }
                if errs > 0 {
                    ui.label(RichText::new(format!("エラー {errs} 曲")).color(BAD));
                }
                ui.checkbox(&mut self.only_changes, "変更する曲だけ表示");
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.graph, GraphView::Dots, "分布（各曲）");
                ui.selectable_value(&mut self.graph, GraphView::Histogram, "ヒストグラム");
                if let Some(st) = &self.stats {
                    ui.label(RichText::new(format!(
                        "n={}　平均 {:.1}　中央値 {:.1}　Q1 {:.1}　Q3 {:.1}　σ {:.1}　範囲 {:.1}〜{:.1}　（クリックで目標を設定）",
                        st.n, st.mean, st.median, st.q1, st.q3, st.stdev, st.min, st.max
                    )).small().weak());
                }
            });
            let pick = loudness_graph(ui, &s.tracks, self.stats.as_ref(), &self.po, self.graph);
            if let Some(v) = pick {
                if self.po.method != Method::Tags {
                    self.po.target = Target::Custom;
                    self.po.custom_target = (v * 10.0).round() / 10.0;
                }
            }
            ui.add_space(6.0);

            let vis: Vec<usize> = (0..s.tracks.len())
                .filter(|&i| !self.only_changes || s.tracks[i].action.changes_file() || s.tracks[i].action == Action::Skip)
                .collect();
            let target = self.stats.as_ref().map(|st| st.target);
            let tol = self.po.tolerance;
            const MIN_W: [f32; 8] = [60.0, 80.0, 40.0, 48.0, 48.0, 48.0, 48.0, 60.0];
            let mut measured = self.col_w.clone();
            let mut tb = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(22.0).resizable(false));
            for (w, min) in self.col_w.iter().zip(MIN_W) {
                tb = tb.column(Column::initial(w.max(min)).at_least(min).clip(true));
            }
            tb.column(Column::remainder().clip(true))
                .header(22.0, |mut h| {
                    let titles = ["", "フォルダ", "ファイル名", "形式", "LUFS", "目標差", "ピーク", "補正", "処理", "備考 / 結果"];
                    for (i, t) in titles.into_iter().enumerate() {
                        h.col(|ui| {
                            // Remember the current width of each resizable column (saved to the settings file).
                            if (1..=measured.len()).contains(&i) {
                                measured[i - 1] = ui.max_rect().width().round();
                            }
                            ui.strong(t);
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, vis.len(), |mut row| {
                        let t = &mut s.tracks[vis[row.index()]];
                        row.col(|ui| {
                            ui.add_enabled(!busy, egui::Checkbox::without_text(&mut t.include)).on_hover_text("対象に含める");
                        });
                        let dir = t.rel.parent().map(|p| p.display().to_string()).filter(|d| !d.is_empty()).unwrap_or_else(|| ".".into());
                        let name = t.rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        row.col(|ui| {
                            let shown = elide_middle(ui, &dir);
                            ui.add(egui::Label::new(RichText::new(shown).weak()).truncate()).on_hover_text(&dir);
                        });
                        row.col(|ui| {
                            let shown = elide_middle(ui, &name);
                            ui.add(egui::Label::new(shown).truncate()).on_hover_text(&name);
                        });
                        row.col(|ui| {
                            ui.label(t.format.label());
                        });
                        row.col(|ui| {
                            ui.label(t.loudness.map(|l| format!("{l:.1}")).unwrap_or_else(|| if t.error.is_some() { "—".into() } else { "…".into() }));
                        });
                        row.col(|ui| {
                            if let (Some(l), Some(m)) = (t.loudness, target) {
                                let d = l - m;
                                let txt = RichText::new(format!("{d:+.1}"));
                                ui.label(if d.abs() > tol { txt.color(WARN) } else { txt.weak() });
                            }
                        });
                        row.col(|ui| {
                            if let Some(p) = t.peak_db {
                                let txt = RichText::new(format!("{p:.1}"));
                                ui.label(if p > 0.0 { txt.color(BAD) } else { txt });
                            }
                        });
                        row.col(|ui| {
                            if t.action.changes_file() {
                                ui.label(RichText::new(format!("{:+.2}", t.gain)).strong());
                            }
                        });
                        row.col(|ui| {
                            let txt = RichText::new(t.action.label());
                            ui.label(match t.action {
                                Action::None => txt.weak(),
                                Action::Skip => txt.color(BAD),
                                _ => txt.color(WARN),
                            });
                        });
                        row.col(|ui| {
                            let mut parts: Vec<String> = Vec::new();
                            if !t.status.is_empty() {
                                parts.push(t.status.clone());
                            }
                            if let Some(e) = &t.error {
                                parts.push(format!("解析エラー: {e}"));
                            }
                            if !t.note.is_empty() {
                                parts.push(t.note.clone());
                            }
                            let joined = parts.join(" ／ ");
                            ui.label(RichText::new(&joined).small()).on_hover_text(&joined);
                        });
                    });
                });
            if measured.iter().all(|w| *w > 0.0) {
                self.col_w = measured;
            }
        });

        self.dialogs_ui(&ctx);

        if self.confirm {
            let (n, dest) = {
                let s = self.sh.lock().unwrap();
                let n = s.tracks.iter().filter(|t| t.include && t.action.changes_file()).count();
                let dest = if self.in_place {
                    format!("元のファイルを置き換えます（元は {} に移動）", PathBuf::from(self.root.trim()).join(core::BACKUP_DIR).display())
                } else {
                    format!("{} に書き出します（元のファイルは変更しません）", self.out_dir.trim())
                };
                (n, dest)
            };
            let resp = egui::Modal::new(egui::Id::unique("loudnorm-confirm")).show(&ctx, |ui| {
                ui.set_max_width(460.0);
                ui.heading(format!("{n} 曲に適用します"));
                ui.label(dest);
                ui.label(match self.po.method {
                    Method::Reencode => format!("方法: 再エンコード（MP3 は {}）", self.mp3.label()),
                    Method::Lossless => "方法: 無劣化を優先".into(),
                    Method::Tags => "方法: ReplayGain タグのみ".into(),
                });
                ui.add_space(8.0);
                let mut r = None;
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("実行").strong()).clicked() {
                        r = Some(true);
                    }
                    if ui.button("キャンセル").clicked() {
                        r = Some(false);
                    }
                });
                r
            });
            match resp.inner {
                Some(true) => {
                    self.confirm = false;
                    self.start_apply(&ctx);
                }
                Some(false) => self.confirm = false,
                None if resp.should_close() => self.confirm = false,
                None => {}
            }
        }
    }
}
