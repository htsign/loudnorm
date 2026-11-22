//! Applying a planned gain to one file, with tag preservation and backups.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::prelude::*;

use crate::core::{Action, Format, Track};
use crate::ffmpeg::Ffmpeg;
use crate::lossless;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mp3Encode {
    Cbr320,
    Cbr256,
    V0,
}

impl Mp3Encode {
    pub fn label(self) -> &'static str {
        match self {
            Mp3Encode::Cbr320 => "CBR 320 kbps",
            Mp3Encode::Cbr256 => "CBR 256 kbps",
            Mp3Encode::V0 => "VBR V0",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Output {
    /// Replace files in place; originals move to `backup` (same relative layout).
    InPlace { backup: PathBuf },
    /// Write changed files only to this folder (same relative layout).
    Folder(PathBuf),
}

#[derive(Clone, Debug)]
pub struct ApplyOptions {
    pub output: Output,
    pub keep_mtime: bool,
    pub mp3: Mp3Encode,
    /// Minimum AAC bitrate (kbps); the source bitrate is kept when higher.
    pub aac_kbps: u32,
    pub vorbis_q: u32,
}

/// Apply `t.action` to `t.path`. Returns a short status text.
pub fn apply(t: &Track, ff: Option<&Ffmpeg>, o: &ApplyOptions) -> Result<String> {
    let dest = match &o.output {
        Output::InPlace { .. } => t.path.clone(),
        Output::Folder(dir) => dir.join(&t.rel),
    };
    let dest_dir = dest.parent().ok_or_else(|| anyhow!("保存先が不正です"))?.to_path_buf();
    fs::create_dir_all(&dest_dir)?;
    let ext = t.path.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let stem = t.path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let tmp = dest_dir.join(format!("{stem}.loudnorm-tmp.{ext}"));
    let _ = fs::remove_file(&tmp);
    let mtime = fs::metadata(&t.path).and_then(|m| m.modified()).ok();

    let result = (|| -> Result<String> {
        let msg = match t.action {
            Action::Reencode => {
                let ff = ff.ok_or_else(|| anyhow!("再エンコードには ffmpeg が必要です"))?;
                reencode(t, &tmp, ff, o)?
            }
            Action::Mp3GlobalGain { steps } => {
                let mut d = fs::read(&t.path)?;
                let st = lossless::mp3_apply_gain(&mut d, steps)?;
                fs::write(&tmp, &d)?;
                if st.clamped > 0 {
                    format!("{} フレーム（{} 箇所で上限/下限に到達）", st.frames, st.clamped)
                } else {
                    format!("{} フレーム", st.frames)
                }
            }
            Action::OpusHeader => {
                let mut d = fs::read(&t.path)?;
                let g = lossless::opus_apply_gain(&mut d, t.gain)?;
                fs::write(&tmp, &d)?;
                format!("ヘッダゲイン {g:+.2} dB")
            }
            Action::ReplayGainTag => {
                fs::copy(&t.path, &tmp)?;
                write_replaygain(&tmp, t)?;
                format!("track gain {:+.2} dB", t.gain)
            }
            Action::None | Action::Skip => bail!("適用対象ではありません"),
        };
        Ok(msg)
    })();
    let msg = match result {
        Ok(m) => m,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    };

    if let Output::InPlace { backup } = &o.output {
        let b = backup.join(&t.rel);
        fs::create_dir_all(b.parent().unwrap())?;
        if b.exists() {
            let _ = fs::remove_file(&tmp);
            bail!("バックアップ先に同名ファイルがあります: {}", b.display());
        }
        move_file(&t.path, &b).context("元ファイルをバックアップへ移動できません")?;
        if let Err(e) = fs::rename(&tmp, &dest) {
            let _ = move_file(&b, &t.path);
            let _ = fs::remove_file(&tmp);
            return Err(e).context("新しいファイルを配置できません（元ファイルは戻しました）");
        }
    } else {
        if dest.exists() {
            fs::remove_file(&dest)?;
        }
        fs::rename(&tmp, &dest)?;
    }
    if o.keep_mtime {
        if let Some(m) = mtime {
            let _ = fs::File::options()
                .write(true)
                .open(&dest)
                .and_then(|f| f.set_times(fs::FileTimes::new().set_modified(m)));
        }
    }
    Ok(msg)
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(from, to)?;
            fs::remove_file(from)
        }
    }
}

fn os(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

fn reencode(t: &Track, tmp: &Path, ff: &Ffmpeg, o: &ApplyOptions) -> Result<String> {
    let info = ff.probe(&t.path)?;
    let mut a: Vec<OsString> = os(&["-hide_banner", "-nostdin", "-v", "error", "-y", "-i"]);
    a.push(t.path.clone().into());
    a.extend(os(&[
        "-map", "0:a:0", "-vn", "-sn", "-dn", "-map_metadata", "-1", "-map_chapters", "-1", "-fflags", "+bitexact",
        "-flags:a", "+bitexact", "-af",
    ]));
    a.push(format!("volume={:.4}dB", t.gain).into());
    let desc: String;
    match t.format {
        Format::Mp3 => {
            if !ff.has_libmp3lame {
                bail!("この ffmpeg には libmp3lame（LAME）がありません");
            }
            a.extend(os(&["-c:a", "libmp3lame"]));
            a.extend(os(match o.mp3 {
                Mp3Encode::Cbr320 => &["-b:a", "320k"][..],
                Mp3Encode::Cbr256 => &["-b:a", "256k"][..],
                Mp3Encode::V0 => &["-q:a", "0"][..],
            }));
            a.extend(os(&["-id3v2_version", "0", "-write_id3v1", "0", "-f", "mp3"]));
            desc = format!("LAME {}", o.mp3.label());
        }
        Format::Flac => {
            let bits = info.bits.unwrap_or(16);
            a.extend(os(&["-c:a", "flac", "-compression_level", "8"]));
            if bits <= 16 {
                a.extend(os(&["-sample_fmt", "s16"]));
            } else {
                a.extend(os(&["-sample_fmt", "s32", "-bits_per_raw_sample", "24"]));
            }
            a.extend(os(&["-f", "flac"]));
            desc = format!("FLAC {}-bit", bits.min(24));
        }
        Format::M4a => {
            if info.codec == "alac" {
                let bits = info.bits.unwrap_or(16);
                a.extend(os(&["-c:a", "alac", "-sample_fmt", if bits <= 16 { "s16p" } else { "s32p" }]));
                desc = "ALAC".into();
            } else {
                let kbps = (info.bit_rate.unwrap_or(0) / 1000).max(o.aac_kbps as u64);
                a.extend(os(&["-c:a", "aac", "-b:a"]));
                a.push(format!("{kbps}k").into());
                desc = format!("AAC {kbps} kbps");
            }
            a.extend(os(&["-f", "ipod"]));
        }
        Format::OggVorbis => {
            if !ff.has_libvorbis {
                bail!("この ffmpeg には libvorbis がありません");
            }
            a.extend(os(&["-c:a", "libvorbis", "-q:a"]));
            a.push(o.vorbis_q.to_string().into());
            a.extend(os(&["-f", "ogg"]));
            desc = format!("Vorbis q{}", o.vorbis_q);
        }
        Format::Wav => {
            let codec = match info.codec.as_str() {
                c @ ("pcm_s16le" | "pcm_s24le" | "pcm_s32le" | "pcm_f32le" | "pcm_f64le" | "pcm_u8") => c.to_string(),
                _ => "pcm_s16le".to_string(),
            };
            a.extend(os(&["-c:a", &codec, "-f", "wav"]));
            desc = codec;
        }
        Format::Opus => bail!("Opus は再エンコードしません"),
    }
    a.push(tmp.as_os_str().to_owned());
    ff.run(&a)?;
    copy_tags(&t.path, tmp, t.format)?;
    Ok(format!("{desc}、{:+.2} dB", t.gain))
}

/// Copy every tag (incl. lyrics and pictures) from `src` to the freshly encoded `dst`.
pub fn copy_tags(src: &Path, dst: &Path, format: Format) -> Result<()> {
    fn copy<F: AudioFile>(src: &Path, dst: &Path) -> Result<()> {
        let mut f = fs::File::open(src)?;
        let file = F::read_from(&mut f, ParseOptions::new()).map_err(|e| anyhow!("タグを読めません: {e}"))?;
        if file.contains_tag() {
            file.save_to_path(dst, WriteOptions::new().preferred_padding(0))
                .map_err(|e| anyhow!("タグを書き込めません: {e}"))?;
        }
        Ok(())
    }
    match format {
        Format::Mp3 => {
            // Byte-exact: keep the original ID3v2 block(s) in front and APEv2/ID3v1 at the end.
            let orig = fs::read(src)?;
            let (s, e) = lossless::mp3_layout(&orig);
            let audio = fs::read(dst)?;
            let (as_, ae) = lossless::mp3_layout(&audio);
            let mut out = Vec::with_capacity(orig.len() + audio.len());
            out.extend_from_slice(&orig[..s]);
            out.extend_from_slice(&audio[as_..ae]);
            out.extend_from_slice(&orig[e..]);
            fs::write(dst, out)?;
            Ok(())
        }
        Format::Flac => copy::<lofty::flac::FlacFile>(src, dst),
        Format::M4a => copy::<lofty::mp4::Mp4File>(src, dst),
        Format::OggVorbis => copy::<lofty::ogg::VorbisFile>(src, dst),
        Format::Wav => copy::<lofty::iff::wav::WavFile>(src, dst),
        Format::Opus => Ok(()),
    }
}

fn write_replaygain(path: &Path, t: &Track) -> Result<()> {
    use lofty::tag::Tag;
    let gain = format!("{:+.2} dB", t.gain);
    let peak = t.peak_db.map(|pk| format!("{:.6}", 10f64.powf(pk / 20.0)));
    // FLAC / Vorbis: edit the concrete Vorbis comments so pictures and other fields are written back unchanged.
    let set = |vc: &mut lofty::ogg::tag::VorbisComments| {
        vc.insert("REPLAYGAIN_TRACK_GAIN".into(), gain.clone());
        if let Some(p) = &peak {
            vc.insert("REPLAYGAIN_TRACK_PEAK".into(), p.clone());
        }
    };
    let open = || -> Result<fs::File> { Ok(fs::File::open(path)?) };
    match t.format {
        Format::Flac => {
            let mut f = lofty::flac::FlacFile::read_from(&mut open()?, ParseOptions::new()).map_err(|e| anyhow!("タグを読めません: {e}"))?;
            if f.vorbis_comments().is_none() {
                f.set_vorbis_comments(lofty::ogg::tag::VorbisComments::default());
            }
            set(f.vorbis_comments_mut().unwrap());
            return f.save_to_path(path, WriteOptions::new()).map_err(|e| anyhow!("タグを書き込めません: {e}"));
        }
        Format::OggVorbis => {
            let mut f = lofty::ogg::VorbisFile::read_from(&mut open()?, ParseOptions::new()).map_err(|e| anyhow!("タグを読めません: {e}"))?;
            set(f.vorbis_comments_mut());
            return f.save_to_path(path, WriteOptions::new()).map_err(|e| anyhow!("タグを書き込めません: {e}"));
        }
        _ => {}
    }
    let mut tf = lofty::read_from_path(path).map_err(|e| anyhow!("タグを読めません: {e}"))?;
    let tt = tf.primary_tag_type();
    if tf.tag(tt).is_none() {
        tf.insert_tag(Tag::new(tt));
    }
    let tag = tf.tag_mut(tt).unwrap();
    tag.insert_text(ItemKey::ReplayGainTrackGain, format!("{:+.2} dB", t.gain));
    if let Some(pk) = t.peak_db {
        tag.insert_text(ItemKey::ReplayGainTrackPeak, format!("{:.6}", 10f64.powf(pk / 20.0)));
    }
    let mut wo = WriteOptions::new();
    if t.format == Format::Mp3 {
        // Keep ID3v2.3 files as v2.3.
        let head = fs::read(path).map(|d| d.get(3).copied()).ok().flatten();
        if head == Some(3) {
            wo = wo.use_id3v23(true);
        }
    }
    tag.save_to_path(path, wo).map_err(|e| anyhow!("タグを書き込めません: {e}"))?;
    Ok(())
}
