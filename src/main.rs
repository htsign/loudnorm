// GUI subsystem on Windows in every build, so no console window opens on start.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod apply;
mod core;
mod ffmpeg;
mod lossless;
mod settings;
mod update;

use std::path::PathBuf;

use rayon::prelude::*;

/// Window icon; also the 256 px entry of `assets/icon.ico`.
const ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");

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
    let icon = eframe::icon_data::from_png_bytes(ICON_PNG)?;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([st.window_width, st.window_height])
            .with_maximized(st.window_maximized)
            .with_min_inner_size([760.0, 480.0])
            .with_title("loudnorm")
            // Wayland compositors ignore the window icon and look the icon up from the .desktop file named after this ID.
            .with_app_id("loudnorm")
            .with_icon(icon),
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

#[cfg(test)]
mod tests {
    use super::*;

    const ICON_ICO: &[u8] = include_bytes!("../assets/icon.ico");
    const ICON_ICNS: &[u8] = include_bytes!("../assets/icon.icns");

    /// Splits an .ico into (size in px, image data) per directory entry; a width byte of 0 means 256.
    fn ico_entries(ico: &[u8]) -> Vec<(u32, &[u8])> {
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        (0..count)
            .map(|i| {
                let e = &ico[6 + 16 * i..6 + 16 * (i + 1)];
                let size = if e[0] == 0 { 256 } else { u32::from(e[0]) };
                let len = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
                let off = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
                (size, &ico[off..off + len])
            })
            .collect()
    }

    /// Splits an .icns into (OSType, image data) per entry. Numbers are big-endian and lengths include the 8-byte header.
    fn icns_entries(icns: &[u8]) -> Vec<(String, &[u8])> {
        assert_eq!(&icns[..4], b"icns");
        let total = u32::from_be_bytes([icns[4], icns[5], icns[6], icns[7]]) as usize;
        assert_eq!(total, icns.len(), "the header length must cover the whole file");
        let mut entries = Vec::new();
        let mut at = 8;
        while at < icns.len() {
            let len = u32::from_be_bytes([icns[at + 4], icns[at + 5], icns[at + 6], icns[at + 7]]) as usize;
            entries.push((String::from_utf8(icns[at..at + 4].to_vec()).unwrap(), &icns[at + 8..at + len]));
            at += len;
        }
        assert_eq!(at, icns.len(), "the last entry must end exactly at the end of the file");
        entries
    }

    #[test]
    fn embedded_window_icon_decodes_to_256px_square() {
        let icon = eframe::icon_data::from_png_bytes(ICON_PNG).expect("icon-256.png must decode");

        assert_eq!((icon.width, icon.height), (256, 256));
    }

    #[test]
    fn exe_icon_has_every_size_windows_picks_from() {
        let mut sizes: Vec<u32> = ico_entries(ICON_ICO).iter().map(|(size, _)| *size).collect();
        sizes.sort_unstable();

        assert_eq!(sizes, [16, 20, 24, 32, 40, 48, 64, 256]);
    }

    #[test]
    fn exe_icon_entries_are_png_of_their_listed_size() {
        for (size, data) in ico_entries(ICON_ICO) {
            assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n", "{size} px entry must be a PNG");
            let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
            let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
            assert_eq!((width, height), (size, size), "{size} px entry has a different PNG size");
        }
    }

    #[test]
    fn exe_icon_256px_entry_is_the_window_icon() {
        let entries = ico_entries(ICON_ICO);

        let (_, png) = entries.iter().find(|(size, _)| *size == 256).expect("256 px entry");
        assert_eq!(*png, ICON_PNG);
    }

    #[test]
    fn app_bundle_icon_has_png_entries_of_the_size_their_type_names() {
        let mut got: Vec<(String, u32)> = icns_entries(ICON_ICNS)
            .iter()
            .map(|(kind, data)| {
                assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n", "{kind} must be a PNG");
                let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
                let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
                assert_eq!(width, height, "{kind} must be square");
                (kind.clone(), width)
            })
            .collect();
        got.sort();

        // ic07-ic10 are 128-512 pt at 1x (ic10 is 512 pt at 2x); ic11-ic14 are 16-256 pt at 2x.
        let want = [("ic07", 128), ("ic08", 256), ("ic09", 512), ("ic10", 1024), ("ic11", 32), ("ic12", 64), ("ic13", 256), ("ic14", 512)];
        assert_eq!(got, want.map(|(kind, px)| (kind.to_string(), px)));
    }

    #[test]
    fn app_bundle_icon_256px_entries_are_the_window_icon() {
        let entries = icns_entries(ICON_ICNS);

        for want in ["ic08", "ic13"] {
            let (_, png) = entries.iter().find(|(kind, _)| kind == want).unwrap_or_else(|| panic!("{want} entry"));
            assert_eq!(*png, ICON_PNG, "{want} must be the window icon");
        }
    }
}
