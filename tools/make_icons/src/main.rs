//! Regenerates assets/icon.ico, assets/icon.icns and assets/icon-256.png from assets/icon.svg and assets/icon-small.svg.
//!
//! Run from the repository root with `cargo run --release --locked --manifest-path tools/make_icons/Cargo.toml`.
//!
//! Every .ico entry is a PNG. The 256 px entry is byte-identical to icon-256.png, which is what the window uses.
//! Small sizes come from icon-small.svg (3 bars, legible at 16 px); larger ones come from icon.svg (4 bars).
//! The .icns (macOS app bundle icon) holds PNG entries ic07-ic14 (128-1024 px, plus the 32 and 64 px entries of the
//! 16 and 32 pt Retina sizes); ic11-ic14, the Retina entries, need macOS 10.8 or later. It follows the same rule for
//! choosing the SVG. An image of the same size and SVG is rendered once and shared, so the 256 px entries of the .ico and
//! the .icns are byte-identical to icon-256.png.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use resvg::{tiny_skia, usvg};

const FULL: &str = "icon.svg";
const SMALL: &str = "icon-small.svg";

/// Entries of the .ico: edge length in px and SVG.
const ICO_JOBS: [(u32, &str); 8] =
    [(16, SMALL), (20, SMALL), (24, SMALL), (32, SMALL), (40, FULL), (48, FULL), (64, FULL), (256, FULL)];

/// Entries of the .icns: OSType, edge length in px and SVG. 256 and 512 px appear twice (1x and 2x of the next size down).
const ICNS_JOBS: [(&str, u32, &str); 8] = [
    ("ic11", 32, SMALL),
    ("ic12", 64, FULL),
    ("ic07", 128, FULL),
    ("ic13", 256, FULL),
    ("ic08", 256, FULL),
    ("ic14", 512, FULL),
    ("ic09", 512, FULL),
    ("ic10", 1024, FULL),
];

/// Renders the SVGs of the assets folder to square, transparent PNGs, each size and SVG only once.
struct Renderer {
    assets: PathBuf,
    trees: HashMap<&'static str, usvg::Tree>,
    rendered: HashMap<(u32, &'static str), Vec<u8>>,
}

impl Renderer {
    fn new(assets: &Path) -> Self {
        Self { assets: assets.to_path_buf(), trees: HashMap::new(), rendered: HashMap::new() }
    }

    /// Returns the PNG of `file` at `size` px, rendering it on first use.
    fn png(&mut self, size: u32, file: &'static str) -> Result<Vec<u8>, String> {
        if let Some(png) = self.rendered.get(&(size, file)) {
            return Ok(png.clone());
        }
        if !self.trees.contains_key(file) {
            let path = self.assets.join(file);
            let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let text = std::str::from_utf8(&data).map_err(|e| format!("{}: {e}", path.display()))?;
            let tree = load_tree(text).map_err(|e| format!("{}: {e}", path.display()))?;
            self.trees.insert(file, tree);
        }
        let tree = &self.trees[file];
        let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or_else(|| format!("cannot allocate {size} px pixmap"))?;
        let scale = tiny_skia::Transform::from_scale(size as f32 / tree.size().width(), size as f32 / tree.size().height());
        resvg::render(tree, scale, &mut pixmap.as_mut());
        let png = pixmap.encode_png().map_err(|e| format!("{file} at {size} px: {e}"))?;
        self.rendered.insert((size, file), png.clone());
        Ok(png)
    }
}

/// Parses the SVG, rejects elements this build cannot render, and builds the render tree from the same XML document.
fn load_tree(svg: &str) -> Result<usvg::Tree, String> {
    let doc = parse_xml(svg)?;
    reject_unsupported_elements(&doc)?;
    usvg::Tree::from_xmltree(&doc, &usvg::Options::default()).map_err(|e| e.to_string())
}

/// Parses the XML the way usvg does, which allows a DTD (tools such as Illustrator write a DOCTYPE).
fn parse_xml(svg: &str) -> Result<usvg::roxmltree::Document<'_>, String> {
    let options = usvg::roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    usvg::roxmltree::Document::parse_with_options(svg, options).map_err(|e| format!("invalid SVG: {e}"))
}

/// Fails if the SVG contains an element that this build would silently leave out of the image.
///
/// Built without resvg's `text` and `raster-images` features, usvg drops `<text>` and `<image>` without an error, and it
/// never renders `<foreignObject>`. The icons are plain shapes, so any of these means the SVG needs a build with more features.
/// Elements of other namespaces are ignored by usvg as well, so they are not rejected.
fn reject_unsupported_elements(doc: &usvg::roxmltree::Document) -> Result<(), String> {
    for node in doc.descendants().filter(|n| n.is_element()) {
        let in_svg_namespace = matches!(node.tag_name().namespace(), None | Some("http://www.w3.org/2000/svg"));
        let name = node.tag_name().name();
        if in_svg_namespace && matches!(name, "text" | "image" | "foreignObject") {
            return Err(format!("<{name}> is not rendered by this build of make_icons; enable the matching resvg feature or draw it as paths"));
        }
    }
    Ok(())
}

/// Converts a length or offset to the u32 the .ico and .icns headers store.
fn to_u32(n: usize, what: &str) -> Result<u32, String> {
    u32::try_from(n).map_err(|_| format!("{what} ({n}) does not fit in 32 bits"))
}

/// Packs PNG images into an .ico: ICONDIR + ICONDIRENTRY[] + image data. A width/height byte of 0 means 256.
fn pack_ico(entries: &[(u32, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let count = u16::try_from(entries.len()).map_err(|_| format!("too many .ico entries ({})", entries.len()))?;
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    let mut offset = 6 + 16 * entries.len();
    for (size, png) in entries {
        let dim = if *size >= 256 { 0 } else { u8::try_from(*size).map_err(|_| format!("bad .ico size {size}"))? };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&to_u32(png.len(), "PNG length")?.to_le_bytes());
        out.extend_from_slice(&to_u32(offset, ".ico image offset")?.to_le_bytes());
        offset += png.len();
    }
    for (_, png) in entries {
        out.extend_from_slice(png);
    }
    Ok(out)
}

/// Packs PNG images into an .icns: the header "icns" + total length, then per image its OSType + length + PNG data.
/// All numbers are big-endian and every length includes its own 8-byte header.
fn pack_icns(entries: &[(&str, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let total = 8 + entries.iter().map(|(_, png)| 8 + png.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&to_u32(total, ".icns length")?.to_be_bytes());
    for (kind, png) in entries {
        out.extend_from_slice(kind.as_bytes());
        out.extend_from_slice(&to_u32(8 + png.len(), "icns entry length")?.to_be_bytes());
        out.extend_from_slice(png);
    }
    Ok(out)
}

fn run() -> Result<(), String> {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    let mut renderer = Renderer::new(&assets);

    let mut ico_entries = Vec::new();
    for (size, file) in ICO_JOBS {
        ico_entries.push((size, renderer.png(size, file)?));
    }
    let mut icns_entries = Vec::new();
    for (kind, size, file) in ICNS_JOBS {
        icns_entries.push((kind, renderer.png(size, file)?));
    }

    // Build every file before writing any, so a rendering or packing failure cannot leave the three assets out of step.
    let ico = pack_ico(&ico_entries)?;
    let png_256 = renderer.png(256, FULL)?;
    let icns = pack_icns(&icns_entries)?;

    let write = |name: &str, data: &[u8]| std::fs::write(assets.join(name), data).map_err(|e| format!("{name}: {e}"));
    write("icon.ico", &ico)?;
    write("icon-256.png", &png_256)?;
    write("icon.icns", &icns)?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("make_icons: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(svg: &str) -> Result<(), String> {
        reject_unsupported_elements(&parse_xml(svg)?)
    }

    fn svg_with(body: &str) -> String {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">{body}</svg>"#)
    }

    #[test]
    fn svg_of_shapes_only_is_accepted() {
        let svg = svg_with(r#"<rect width="8" height="8"/>"#);

        assert_eq!(check(&svg), Ok(()));
    }

    #[test]
    fn svg_with_text_is_rejected_naming_the_element() {
        let svg = svg_with(r#"<text x="0" y="8">A</text>"#);

        let err = check(&svg).unwrap_err();

        assert!(err.contains("<text>"), "{err}");
    }

    #[test]
    fn svg_with_image_is_rejected_naming_the_element() {
        let svg = svg_with(r#"<image href="a.png" width="8" height="8"/>"#);

        let err = check(&svg).unwrap_err();

        assert!(err.contains("<image>"), "{err}");
    }

    #[test]
    fn svg_with_foreign_object_is_rejected_naming_the_element() {
        let svg = svg_with(r#"<foreignObject width="8" height="8"/>"#);

        let err = check(&svg).unwrap_err();

        assert!(err.contains("<foreignObject>"), "{err}");
    }

    #[test]
    fn malformed_svg_is_rejected() {
        let err = check("<svg").unwrap_err();

        assert!(err.contains("invalid SVG"), "{err}");
    }

    #[test]
    fn svg_with_doctype_is_accepted() {
        let svg = format!(
            r#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">{}"#,
            svg_with(r#"<rect width="8" height="8"/>"#)
        );

        assert_eq!(check(&svg), Ok(()));
    }

    #[test]
    fn svg_with_doctype_builds_a_tree() {
        let svg = format!(
            r#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">{}"#,
            svg_with(r#"<rect width="8" height="8"/>"#)
        );

        let tree = load_tree(&svg).unwrap();

        assert_eq!(tree.size().width(), 16.0);
    }

    #[test]
    fn element_in_a_foreign_namespace_is_accepted() {
        let svg = svg_with(r#"<foo:text xmlns:foo="urn:x">A</foo:text>"#);

        assert_eq!(check(&svg), Ok(()));
    }
}
