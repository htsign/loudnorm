// GUI subsystem on Windows in every build, so no console window opens on start.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod apply;
mod core;
mod ffmpeg;
mod lossless;
mod settings;

use std::path::PathBuf;

use rayon::prelude::*;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        // A GUI-subsystem process has no console; reuse the one of the shell that started us, if any.
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
        }
        return cli(&args[2..]);
    }
    let (st, msg) = settings::Settings::load();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([st.window_width, st.window_height])
            .with_maximized(st.window_maximized)
            .with_min_inner_size([760.0, 480.0])
            .with_title("loudnorm"),
        ..Default::default()
    };
    eframe::run_native("loudnorm", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, st, msg)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Headless mode for scripting and tests:
/// `--cli <dir> [--method reencode|lossless|tags] [--tol dB] [--mp3 cbr320|cbr256|v0] [--out DIR] [--jobs N] [--sample-peak] [--target median|mean|trimmed_mean|min_change|custom] [--custom LUFS] [--apply]`
fn cli(args: &[String]) -> anyhow::Result<()> {
    let root = PathBuf::from(args.first().ok_or_else(|| anyhow::anyhow!("フォルダを指定してください"))?);
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let method = match get("--method").as_deref() {
        Some("lossless") => core::Method::Lossless,
        Some("tags") => core::Method::Tags,
        _ => core::Method::Reencode,
    };
    let po = core::PlanOptions {
        method,
        tolerance: get("--tol").and_then(|s| s.parse().ok()).unwrap_or(1.0),
        peak_ceiling: -1.0,
        limit_peak: true,
        lossless_fallback_reencode: args.iter().any(|a| a == "--fallback"),
        target: get("--target").as_deref().and_then(core::Target::from_key).unwrap_or(core::Target::Median),
        custom_target: get("--custom").and_then(|s| s.parse().ok()).unwrap_or(-16.0),
    };
    let ff = ffmpeg::Ffmpeg::locate(None).ok();
    let jobs = get("--jobs").and_then(|s| s.parse::<usize>().ok()).unwrap_or_else(settings::default_jobs);
    rayon::ThreadPoolBuilder::new().num_threads(jobs).build_global()?;
    let true_peak = !args.iter().any(|a| a == "--sample-peak");
    let t0 = std::time::Instant::now();
    let mut tracks = core::scan(&root);
    tracks.par_iter_mut().for_each(|t| {
        match core::analyze(&t.path, ff.as_ref(), true_peak) {
            Ok((l, p)) => {
                t.loudness = Some(l);
                t.peak_db = Some(p);
            }
            Err(e) => t.error = Some(format!("{e:#}")),
        }
        t.has_rg = core::has_replaygain(&t.path);
    });
    eprintln!("analysis: {} files in {:.2}s ({} threads)", tracks.len(), t0.elapsed().as_secs_f64(), rayon::current_num_threads());
    let st = core::plan(&mut tracks, &po);
    if let Some(st) = &st {
        println!(
            "target\t{:.2}\tmedian {:.2} mean {:.2} trimmed {:.2} min_change {:.2} (kept {}/{}) q1 {:.2} q3 {:.2} sd {:.2}",
            st.target, st.median, st.mean, st.trimmed_mean, st.min_change, st.min_change_kept, st.n, st.q1, st.q3, st.stdev
        );
    }
    let ao = apply::ApplyOptions {
        output: match get("--out") {
            Some(d) => apply::Output::Folder(PathBuf::from(d)),
            None => apply::Output::InPlace { backup: root.join(core::BACKUP_DIR) },
        },
        keep_mtime: true,
        mp3: match get("--mp3").as_deref() {
            Some("cbr256") => apply::Mp3Encode::Cbr256,
            Some("v0") => apply::Mp3Encode::V0,
            _ => apply::Mp3Encode::Cbr320,
        },
        aac_kbps: 256,
        vorbis_q: 8,
    };
    let doit = args.iter().any(|a| a == "--apply");
    let t1 = std::time::Instant::now();
    let statuses: Vec<String> = tracks
        .par_iter()
        .map(|t| {
            if doit && t.action.changes_file() {
                match apply::apply(t, ff.as_ref(), &ao) {
                    Ok(m) => m,
                    Err(e) => format!("ERROR {e:#}"),
                }
            } else {
                String::new()
            }
        })
        .collect();
    if doit {
        eprintln!("apply: {:.2}s", t1.elapsed().as_secs_f64());
    }
    for (t, status) in tracks.iter().zip(statuses) {
        println!(
            "{}\t{}\t{}\t{}\t{:+.2}\t{}\t{}\t{}{}",
            t.rel.display(),
            t.format.label(),
            t.loudness.map(|l| format!("{l:.2}")).unwrap_or("-".into()),
            t.peak_db.map(|l| format!("{l:.2}")).unwrap_or("-".into()),
            t.gain,
            t.action.label(),
            t.note,
            status,
            t.error.as_ref().map(|e| format!(" [{e}]")).unwrap_or_default()
        );
    }
    Ok(())
}
