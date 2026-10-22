//! SVG import: a drawing opens as a document of shape layers, and places as a vector smart
//! object that is re-rasterised at its placement scale ([`rasterize`]).
//!
//! Parsing is `usvg`'s (the crate the UI's icons already go through): it resolves CSS, `use`,
//! units, nested transforms and turns text into outlines, handing over a tree of groups, paths,
//! images and text. Each path becomes a [`ShapeLayer`] (cubic Bézier knots, fill, stroke) and
//! each group a layer group with its opacity and blend mode. What the shape model cannot hold,
//! clip paths, masks, filters, pattern paints and embedded images, is rasterised by `resvg` into
//! a pixel layer so the picture still looks right, with a warning; gradient paints keep their
//! stops and direction in the layer's own gradient fill, with a warning that the exact geometry
//! is not kept.

use std::sync::{Arc, Mutex, OnceLock};

use photocraft_color::blend::BlendMode;
use photocraft_color::{Color, ColorMode, PixelFormat, SampleType};
use photocraft_doc::effects::GradientStyle;
use photocraft_doc::vector::{FillRule, Knot, LineCap, LineJoin, Path, PathOp, ShapeLayer, ShapeStroke, StrokeAlign, Subpath, VectorMask};
use photocraft_doc::{Document, Fill, Group, Layer, LayerContent, MAX_GROUP_DEPTH};
use photocraft_geom::{Point, Rect, Size};
use photocraft_raster::Surface;
use resvg::tiny_skia;
use resvg::usvg;

use crate::{ImportResult, IoError};

/// Largest canvas side an SVG opens at; a larger drawing is scaled down to fit.
pub const MAX_SIDE: u32 = 16_384;
/// A rasterised fallback layer or a smart-object render never exceeds this many pixels.
const MAX_RASTER_PIXELS: u64 = 24_000_000;
/// Past this many drawable elements the whole drawing opens as one pixel layer.
const MAX_NODES: usize = 2_000;
/// Nesting past which a subtree is rasterised instead of walked (bounded recursion).
const MAX_RECURSION: usize = 200;
/// Deepest element nesting accepted. The XML parser recurses once per level, so a file nested
/// a few thousand deep (70 KB of `<g>`) overflows a 2 MB thread stack, an abort no guard can
/// catch; real drawings stay under a hundred levels.
pub const MAX_XML_DEPTH: usize = 512;
/// Largest `.svgz` accepted once inflated (a 1.5 MB gzip can hold 1.5 GB of whitespace).
pub const MAX_INFLATED_BYTES: u64 = 128 << 20;

/// Does this look like an SVG document (plain XML; `.svgz` is recognised by its extension)?
pub fn is_svg(bytes: &[u8]) -> bool {
    let head = bytes.get(..bytes.len().min(4096)).unwrap_or(bytes);
    let text = String::from_utf8_lossy(head);
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with("<svg") {
        return true;
    }
    (t.starts_with("<?xml") || t.starts_with("<!DOCTYPE") || t.starts_with("<!--")) && t.contains("<svg")
}

/// The fonts text runs are shaped with: the bundled craft fonts plus, on the desktop, the
/// system's. Loaded once per process.
fn fonts() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        for f in photocraft_text::CRAFT_FONTS {
            db.load_font_data(f.bytes.to_vec());
        }
        #[cfg(not(target_arch = "wasm32"))]
        db.load_system_fonts();
        configure_generic_fonts(&mut db);
        Arc::new(db)
    })
    .clone()
}

/// fontdb's defaults name common Windows fonts. If those families aren't installed,
/// usvg's missing-family fallback resolves to nothing and drops otherwise valid SVG text.
/// Keep existing mappings; fill only missing generics with a family actually in this database.
fn configure_generic_fonts(db: &mut usvg::fontdb::Database) {
    use usvg::fontdb::{Family, Query};
    for (generic, candidates) in [
        (Family::Serif, &["Noto Serif", "DejaVu Serif", "Liberation Serif", "Inter"][..]),
        (Family::SansSerif, &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Inter"][..]),
        (Family::Monospace, &["Noto Sans Mono", "DejaVu Sans Mono", "Liberation Mono", "JetBrains Mono"][..]),
    ] {
        if db.query(&Query { families: &[generic], ..Default::default() }).is_some() {
            continue;
        }
        let family = candidates
            .iter()
            .find(|name| db.query(&Query { families: &[Family::Name(name)], ..Default::default() }).is_some())
            .map(|name| (*name).to_string())
            .or_else(|| db.faces().find_map(|face| face.families.first().map(|(name, _)| name.clone())));
        if let Some(family) = family {
            match generic {
                Family::Serif => db.set_serif_family(family),
                Family::SansSerif => db.set_sans_serif_family(family),
                Family::Monospace => db.set_monospace_family(family),
                _ => {}
            }
        }
    }
}

/// Parse options for an untrusted drawing. `<image href>` resolves only `data:` URLs: a file path
/// or relative href is never read, so opening an SVG cannot pull local files (`/etc/...`,
/// `~/.ssh/...`) into the document. Embedded SVG images get no `<image>` resolution at all
/// (`usvg` already blanks it for sub-SVGs).
fn options(fontdb: Arc<usvg::fontdb::Database>) -> usvg::Options<'static> {
    usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver { resolve_data: usvg::ImageHrefResolver::default_data_resolver(), resolve_string: Box::new(|_, _| None) },
        fontdb,
        ..usvg::Options::default()
    }
}

/// A parsed drawing.
#[derive(Debug)]
struct Parsed {
    tree: usvg::Tree,
    /// The drawing has text but no font at all was available to shape it. `usvg` then drops the
    /// text while parsing, before any node reaches the conversion, so this is the only place
    /// that knows why the text is missing.
    fontless_text: bool,
}

/// Parses a drawing, `.svgz` included. The XML is inflated and checked here, before the parser
/// sees it: deeper nesting than [`MAX_XML_DEPTH`] is an error, not a stack overflow. The font
/// database (and its one-off scan of the system fonts) is only loaded when the drawing may
/// contain text.
fn parse(bytes: &[u8]) -> Result<Parsed, IoError> {
    parse_with(bytes, fonts)
}

/// [`parse`] with the font database `fonts` supplies (the tests hand in an empty one).
fn parse_with(bytes: &[u8], fonts: fn() -> Arc<usvg::fontdb::Database>) -> Result<Parsed, IoError> {
    let xml = inflate(bytes)?;
    if nesting_depth(&xml, MAX_XML_DEPTH) > MAX_XML_DEPTH {
        return Err(IoError::Svg(format!("elements are nested more than {MAX_XML_DEPTH} levels deep")));
    }
    let has_text = xml.windows(4).any(|w| w == b"text");
    let fontdb = if has_text { fonts() } else { Arc::new(usvg::fontdb::Database::new()) };
    let fontless_text = has_text && fontdb.faces().next().is_none();
    let opt = options(fontdb);
    let tree = usvg::Tree::from_data(&xml, &opt).map_err(|e| IoError::Svg(e.to_string()))?;
    Ok(Parsed { tree, fontless_text })
}

const MAX_BOUNDED_SVG_BYTES: u64 = 16 << 20;
const MAX_EMBEDDED_SVG_DEPTH: usize = 32;

struct ImageBudget {
    pixels: u64,
    xml_bytes: u64,
    svg_images: u64,
    fontless_text: bool,
    error: Option<String>,
}

fn image_error(budget: &Mutex<ImageBudget>, error: impl Into<String>) {
    let mut budget = budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if budget.error.is_none() {
        budget.error = Some(error.into());
    }
}

fn charge_image(budget: &Mutex<ImageBudget>, width: usize, height: usize) -> bool {
    let pixels = u64::try_from(width).ok().zip(u64::try_from(height).ok()).and_then(|(w, h)| w.checked_mul(h));
    let mut budget = budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match pixels.filter(|pixels| *pixels != 0).and_then(|pixels| budget.pixels.checked_sub(pixels)) {
        Some(remaining) => {
            budget.pixels = remaining;
            true
        }
        None => {
            budget.error.get_or_insert_with(|| "embedded SVG raster images exceed the decoded pixel budget".into());
            false
        }
    }
}

// A GIF's first frame can exceed its logical screen. resvg allocates the frame's RGBA buffer,
// so inspect its descriptor too, without reading any LZW pixels.
fn gif_frame_size(bytes: &[u8]) -> Option<(usize, usize)> {
    let packed = *bytes.get(10)?;
    let table = if packed & 0x80 != 0 { 3usize.checked_mul(1usize.checked_shl(u32::from((packed & 7) + 1))?)? } else { 0 };
    let mut at = 13usize.checked_add(table)?;
    loop {
        match *bytes.get(at)? {
            0x2c => {
                let word = |offset: usize| -> Option<usize> {
                    let pair: [u8; 2] = bytes.get(at.checked_add(offset)?..at.checked_add(offset)?.checked_add(2)?)?.try_into().ok()?;
                    Some(usize::from(u16::from_le_bytes(pair)))
                };
                return Some((word(5)?, word(7)?));
            }
            0x21 => {
                at = at.checked_add(2)?;
                loop {
                    let length = usize::from(*bytes.get(at)?);
                    at = at.checked_add(1)?;
                    if length == 0 {
                        break;
                    }
                    bytes.get(at..at.checked_add(length)?)?;
                    at = at.checked_add(length)?;
                }
            }
            _ => return None,
        }
    }
}

// VP8X declares a container canvas, while a malformed lossy VP8 chunk may declare a larger
// frame which the downstream decoder allocates before comparing it with that canvas.
fn webp_frame_sizes(chunks: &[u8], dimensions: &mut (usize, usize), nested: bool) -> Option<()> {
    let mut at = 0usize;
    while at < chunks.len() {
        let header = chunks.get(at..at.checked_add(8)?)?;
        let length = usize::try_from(u32::from_le_bytes(header.get(4..8)?.try_into().ok()?)).ok()?;
        let from = at.checked_add(8)?;
        let end = from.checked_add(length)?;
        let payload = chunks.get(from..end)?;
        match header.get(..4)? {
            b"VP8 " | b"VP8L" => {
                let mut probe = b"RIFF\0\0\0\0WEBP".to_vec();
                probe.extend_from_slice(header);
                probe.extend_from_slice(payload.get(..payload.len().min(10))?);
                let size = imagesize::blob_size(&probe).ok()?;
                dimensions.0 = dimensions.0.max(size.width);
                dimensions.1 = dimensions.1.max(size.height);
            }
            b"ANMF" if !nested => {
                let word = |at: usize| -> Option<usize> {
                    let bytes: [u8; 3] = payload.get(at..at.checked_add(3)?)?.try_into().ok()?;
                    usize::try_from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]).checked_add(1)?).ok()
                };
                dimensions.0 = dimensions.0.max(word(6)?);
                dimensions.1 = dimensions.1.max(word(9)?);
                webp_frame_sizes(payload.get(16..)?, dimensions, true)?;
            }
            b"ANMF" => return None,
            _ => {}
        }
        at = end.checked_add(length & 1)?;
        if at > chunks.len() {
            return None;
        }
    }
    Some(())
}

fn bounded_data_image(mime: &str, data: Arc<Vec<u8>>, options: &usvg::Options, budget: &Arc<Mutex<ImageBudget>>, depth: usize) -> Option<usvg::ImageKind> {
    if budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner).error.is_some() {
        return None;
    }
    let format = photocraft_codecs::detect(&data);
    if mime == "image/svg+xml" || (mime == "text/plain" && format.is_none()) {
        if depth >= MAX_EMBEDDED_SVG_DEPTH {
            image_error(budget, "embedded SVG images exceed the nesting limit");
            return None;
        }
        match parse_image_bounded(&data, budget.clone(), depth + 1) {
            Ok(tree) => {
                let mut counter = budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                counter.svg_images = counter.svg_images.saturating_add(1);
                Some(usvg::ImageKind::SVG(tree))
            }
            Err(e) => {
                image_error(budget, format!("can't parse embedded SVG image: {e}"));
                None
            }
        }
    } else {
        let supported = matches!(
            format,
            Some(photocraft_codecs::Format::Png | photocraft_codecs::Format::Jpeg | photocraft_codecs::Format::Gif | photocraft_codecs::Format::WebP)
        );
        if !supported {
            image_error(budget, "embedded SVG image has unsupported or invalid raster data");
            return None;
        }
        let dimensions = match imagesize::blob_size(&data) {
            Ok(size) => (size.width, size.height),
            Err(e) => {
                image_error(budget, format!("can't read embedded SVG image dimensions: {e}"));
                return None;
            }
        };
        let mut dimensions = if format == Some(photocraft_codecs::Format::Gif) {
            let Some(frame) = gif_frame_size(&data) else {
                image_error(budget, "can't read embedded GIF frame dimensions");
                return None;
            };
            (dimensions.0.max(frame.0), dimensions.1.max(frame.1))
        } else {
            dimensions
        };
        if format == Some(photocraft_codecs::Format::WebP) && data.get(12..).and_then(|chunks| webp_frame_sizes(chunks, &mut dimensions, false)).is_none() {
            image_error(budget, "can't read embedded WebP frame dimensions");
            return None;
        }
        if !charge_image(budget, dimensions.0, dimensions.1) {
            return None;
        }
        let image = (usvg::ImageHrefResolver::default_data_resolver())(mime, data, options);
        if image.is_none() {
            image_error(budget, "embedded SVG raster image has an unsupported MIME type");
        }
        image
    }
}

fn parse_image_bounded(bytes: &[u8], budget: Arc<Mutex<ImageBudget>>, depth: usize) -> Result<usvg::Tree, IoError> {
    parse_image_bounded_with(bytes, budget, depth, fonts)
}

fn parse_image_bounded_with(
    bytes: &[u8],
    budget: Arc<Mutex<ImageBudget>>,
    depth: usize,
    fonts: fn() -> Arc<usvg::fontdb::Database>,
) -> Result<usvg::Tree, IoError> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BOUNDED_SVG_BYTES {
        return Err(IoError::Svg("embedded SVG source exceeds the 16 MiB byte limit".into()));
    }
    let xml = inflate_with_limit(bytes, MAX_BOUNDED_SVG_BYTES)?;
    if nesting_depth(&xml, MAX_XML_DEPTH) > MAX_XML_DEPTH {
        return Err(IoError::Svg(format!("elements are nested more than {MAX_XML_DEPTH} levels deep")));
    }
    {
        let mut counter = budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        counter.xml_bytes = counter
            .xml_bytes
            .checked_sub(u64::try_from(xml.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| IoError::Svg("embedded SVG XML exceeds the decoded byte budget".into()))?;
    }
    let has_text = xml.windows(4).any(|w| w == b"text");
    let fontdb = if has_text { fonts() } else { Arc::new(usvg::fontdb::Database::new()) };
    if has_text && fontdb.faces().next().is_none() {
        budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner).fontless_text = true;
    }
    let mut opt = options(fontdb);
    let shared = budget.clone();
    opt.image_href_resolver.resolve_data = Box::new(move |mime, data, options| bounded_data_image(mime, data, options, &shared, depth));
    let tree = usvg::Tree::from_data(&xml, &opt);
    if let Some(error) = budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner).error.clone() {
        return Err(IoError::Svg(error));
    }
    tree.map_err(|e| IoError::Svg(e.to_string()))
}

fn parse_bounded(bytes: &[u8], max_pixels: u64) -> Result<Parsed, IoError> {
    parse_bounded_inner(bytes, max_pixels).map(|(tree, _)| tree)
}

fn parse_bounded_inner(bytes: &[u8], max_pixels: u64) -> Result<(Parsed, u64), IoError> {
    parse_bounded_inner_with(bytes, max_pixels, fonts)
}

fn parse_bounded_inner_with(bytes: &[u8], max_pixels: u64, fonts: fn() -> Arc<usvg::fontdb::Database>) -> Result<(Parsed, u64), IoError> {
    let budget = Arc::new(Mutex::new(ImageBudget {
        pixels: max_pixels.min(MAX_RASTER_PIXELS),
        xml_bytes: MAX_BOUNDED_SVG_BYTES,
        svg_images: 0,
        fontless_text: false,
        error: None,
    }));
    let tree = parse_image_bounded_with(bytes, budget.clone(), 0, fonts)?;
    let counter = budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // resvg renders each embedded SVG through a temporary pixmap as large as its destination.
    if counter.svg_images != 0 {
        let area = canvas(&tree, &mut Vec::new()).size.area();
        if area.checked_mul(counter.svg_images.saturating_add(1)).is_none_or(|pixels| pixels > max_pixels.min(MAX_RASTER_PIXELS)) {
            return Err(IoError::Svg("embedded SVG intermediate images exceed the SVG cache budget".into()));
        }
    }
    Ok((Parsed { tree, fontless_text: counter.fontless_text }, counter.svg_images))
}

/// The plain XML of `bytes`: itself, or the inflated content of a gzip `.svgz`, refused past
/// [`MAX_INFLATED_BYTES`] (a decompression bomb) or when the stream is corrupt.
fn inflate(bytes: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>, IoError> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(std::borrow::Cow::Borrowed(bytes));
    }
    inflate_with_limit(bytes, MAX_INFLATED_BYTES)
}

fn inflate_with_limit(bytes: &[u8], limit: u64) -> Result<std::borrow::Cow<'_, [u8]>, IoError> {
    use std::io::Read;
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(IoError::Svg(format!("the SVG exceeds the {limit} byte limit")));
        }
        return Ok(std::borrow::Cow::Borrowed(bytes));
    }
    let mut out = Vec::new();
    let mut reader = flate2::read::MultiGzDecoder::new(bytes).take(limit.saturating_add(1));
    reader.read_to_end(&mut out).map_err(|e| IoError::Svg(format!("the compressed SVG couldn't be inflated: {e}")))?;
    if u64::try_from(out.len()).unwrap_or(u64::MAX) > limit {
        return Err(IoError::Svg(format!("the compressed SVG inflates past {} MB", limit >> 20)));
    }
    Ok(std::borrow::Cow::Owned(out))
}

/// The deepest element nesting in `xml`, stopping as soon as it passes `limit` (the result is
/// then `limit + 1`). Comments, CDATA sections, processing instructions and the DOCTYPE add
/// nothing; quoted attribute values may hold `>` or `/>`. Malformed markup is only counted
/// loosely: the parser rejects it afterwards.
fn nesting_depth(xml: &[u8], limit: usize) -> usize {
    let after = |from: usize, pat: &[u8]| -> usize {
        xml.get(from..).and_then(|s| s.windows(pat.len()).position(|w| w == pat)).map_or(xml.len(), |p| from.saturating_add(p).saturating_add(pat.len()))
    };
    // The byte after a tag's closing `>` (outside quotes) and whether the tag ended in `/>`.
    let tag_end = |from: usize| -> (usize, bool) {
        let (mut quote, mut last) = (None, b' ');
        for (k, &b) in xml.iter().enumerate().skip(from) {
            match quote {
                Some(q) if b == q => quote = None,
                Some(_) => {}
                None if b == b'"' || b == b'\'' => quote = Some(b),
                None if b == b'>' => return (k.saturating_add(1), last == b'/'),
                None => {
                    if !b.is_ascii_whitespace() {
                        last = b;
                    }
                }
            }
        }
        (xml.len(), false)
    };
    let (mut depth, mut deepest, mut i) = (0usize, 0usize, 0usize);
    while let Some(p) = xml.get(i..).and_then(|s| s.iter().position(|&b| b == b'<')) {
        let at = i.saturating_add(p);
        let rest = xml.get(at..).unwrap_or_default();
        i = if rest.starts_with(b"<!--") {
            after(at.saturating_add(4), b"-->")
        } else if rest.starts_with(b"<![CDATA[") {
            after(at.saturating_add(9), b"]]>")
        } else if rest.starts_with(b"<?") {
            after(at.saturating_add(2), b"?>")
        } else if rest.starts_with(b"<!") {
            tag_end(at.saturating_add(2)).0
        } else if rest.starts_with(b"</") {
            depth = depth.saturating_sub(1);
            tag_end(at.saturating_add(2)).0
        } else {
            let (end, self_closing) = tag_end(at.saturating_add(1));
            if !self_closing {
                depth = depth.saturating_add(1);
                deepest = deepest.max(depth);
                if deepest > limit {
                    return deepest;
                }
            }
            end
        };
    }
    deepest
}

/// The document canvas for a drawing: its size in px, scaled down to [`MAX_SIDE`] when needed.
struct Canvas {
    size: Size,
    /// Drawing units → canvas pixels (1 unless the drawing had to shrink).
    scale: f32,
}

fn canvas(tree: &usvg::Tree, warnings: &mut Vec<String>) -> Canvas {
    let (w, h) = (tree.size().width().max(1.0), tree.size().height().max(1.0));
    let limit = MAX_SIDE as f32;
    let scale = (limit / w).min(limit / h).min(1.0);
    if scale < 1.0 {
        warnings.push(format!("SVG: the {w:.0} × {h:.0} px drawing was scaled down to fit {MAX_SIDE} px"));
    }
    let side = |v: f32| ((v * scale).round() as u32).clamp(1, MAX_SIDE);
    Canvas { size: Size::new(side(w), side(h)), scale }
}

/// Opens an SVG as a document of shape layers (see the module docs for what is rasterised).
pub fn import_svg(name: &str, bytes: &[u8]) -> Result<ImportResult, IoError> {
    import_svg_with_group_depth(name, bytes, MAX_GROUP_DEPTH)
}

/// Opens an SVG while rasterising any subtrees deeper than `max_group_depth` into single pixel
/// layers. The document's hard nesting limit remains [`MAX_GROUP_DEPTH`].
pub fn import_svg_with_group_depth(name: &str, bytes: &[u8], max_group_depth: usize) -> Result<ImportResult, IoError> {
    import_svg_inner(name, bytes, max_group_depth, None, false)
}

/// Opens an SVG with a conservative budget for eagerly rendered layer caches. A canvas above
/// the budget (or the rasterisation limit) is rejected before any layer cache is allocated.
/// When the canvas fits but its elements' combined caches would exceed the budget, the whole
/// drawing becomes one pixel layer, with an explicit editability warning.
pub fn import_svg_bounded(name: &str, bytes: &[u8], max_cached_pixels: u64) -> Result<ImportResult, IoError> {
    import_svg_inner(name, bytes, MAX_GROUP_DEPTH, Some(max_cached_pixels), false)
}

/// The same cache bound without a lossy aggregate-budget fallback. Smart-object operations
/// use this variant so editing contents or changing Stack Mode fails before excessive caches
/// are rendered, rather than silently changing the source's layer structure.
pub fn import_svg_bounded_layers(name: &str, bytes: &[u8], max_cached_pixels: u64) -> Result<ImportResult, IoError> {
    import_svg_inner(name, bytes, MAX_GROUP_DEPTH, Some(max_cached_pixels), true)
}

fn import_svg_inner(
    name: &str,
    bytes: &[u8],
    max_group_depth: usize,
    max_cached_pixels: Option<u64>,
    reject_cache_overflow: bool,
) -> Result<ImportResult, IoError> {
    let parsed = match max_cached_pixels {
        Some(limit) => parse_bounded(bytes, limit)?,
        None => parse(bytes)?,
    };
    import_parsed(name, parsed, max_group_depth, max_cached_pixels, reject_cache_overflow)
}

fn import_parsed(
    name: &str,
    parsed: Parsed,
    max_group_depth: usize,
    max_cached_pixels: Option<u64>,
    reject_cache_overflow: bool,
) -> Result<ImportResult, IoError> {
    let Parsed { tree, fontless_text } = parsed;
    let mut warnings = Vec::new();
    if fontless_text {
        warnings.push("SVG: the text needs a font and none is installed, so it was dropped".to_string());
    }
    let cv = canvas(&tree, &mut warnings);
    let nodes = count_nodes(tree.root(), max_cached_pixels.is_some());
    let over_cache_budget = if let Some(budget) = max_cached_pixels {
        let area = cv.size.area();
        if area > budget.min(MAX_RASTER_PIXELS) {
            return Err(IoError::Svg(format!(
                "{} × {} px exceeds the SVG layer cache budget of {} pixels",
                cv.size.width,
                cv.size.height,
                budget.min(MAX_RASTER_PIXELS)
            )));
        }
        let count = u64::try_from(nodes.min(MAX_NODES)).unwrap_or(u64::MAX);
        area.checked_mul(count).is_none_or(|pixels| pixels > budget)
    } else {
        false
    };
    if over_cache_budget && reject_cache_overflow {
        return Err(IoError::Svg("the source's editable layer caches exceed the SVG cache budget".into()));
    }
    let mut doc = Document::new(name, cv.size, ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();
    let global = tiny_skia::Transform::from_scale(cv.scale, cv.scale);
    let max_group_depth = (max_group_depth != 0).then_some(max_group_depth.clamp(1, MAX_GROUP_DEPTH));
    let mut conv = Conv { fmt, canvas: doc.bounds(), global, max_group_depth, warnings, noted: Vec::new(), counts: [0; 4] };
    if nodes > MAX_NODES || over_cache_budget {
        if over_cache_budget {
            conv.warnings.push("SVG: combined layer caches exceed the clipboard cache budget; opened as one pixel layer instead of editable shapes".into());
        } else {
            conv.warnings.push(format!("SVG: more than {MAX_NODES} elements; opened as one pixel layer instead of shapes"));
        }
        let (w, h) = (cv.size.width, cv.size.height);
        if let Some(mut pixmap) = pixmap(w, h) {
            resvg::render(&tree, global, &mut pixmap.as_mut());
            doc.layers.push(Layer::new("Background", LayerContent::Raster(surface(&pixmap, doc.bounds(), fmt))));
        }
    } else {
        doc.layers = conv.convert_children(tree.root().children(), 0, 0, tree.root().abs_transform());
    }
    if doc.layers.is_empty() {
        conv.warnings.push("SVG: nothing to draw (an empty drawing)".to_string());
    }
    Ok(ImportResult { document: doc, warnings: conv.warnings, source_read_only: false, preview_only: false })
}

/// The whole drawing rendered at `scale` (1 = its own size), as a straight-alpha RGBA buffer at
/// the origin. Smart objects use it to stay sharp at any placement.
pub fn rasterize(bytes: &[u8], scale: f32) -> Result<photocraft_compose::Buffer, IoError> {
    let tree = parse(bytes)?.tree;
    rasterize_tree(&tree, scale)
}

/// Render untrusted embedded SVG with intrinsic raster-image and aggregate decode budgets.
pub fn rasterize_bounded(bytes: &[u8], scale: f32, max_decoded_pixels: u64) -> Result<photocraft_compose::Buffer, IoError> {
    let (parsed, images) = parse_bounded_inner(bytes, max_decoded_pixels)?;
    let tree = parsed.tree;
    if images != 0 {
        let cv = canvas(&tree, &mut Vec::new());
        let k = cv.scale * scale.clamp(1.0 / 64.0, 64.0);
        let width = ((tree.size().width() * k).ceil() as u32).max(1);
        let height = ((tree.size().height() * k).ceil() as u32).max(1);
        if (u64::from(width) * u64::from(height)).checked_mul(images.saturating_add(1)).is_none_or(|pixels| pixels > max_decoded_pixels.min(MAX_RASTER_PIXELS))
        {
            return Err(IoError::Svg("embedded SVG intermediate images exceed the rendered pixel budget".into()));
        }
    }
    rasterize_tree(&tree, scale)
}

fn rasterize_tree(tree: &usvg::Tree, scale: f32) -> Result<photocraft_compose::Buffer, IoError> {
    let cv = canvas(tree, &mut Vec::new());
    let k = cv.scale * scale.clamp(1.0 / 64.0, 64.0);
    let side = |v: f32| ((v * k).ceil() as u32).max(1);
    let (w, h) = (side(tree.size().width()), side(tree.size().height()));
    let mut pixmap = pixmap(w, h).ok_or_else(|| IoError::Svg(format!("{w} × {h} px is too large to rasterise")))?;
    resvg::render(tree, tiny_skia::Transform::from_scale(k, k), &mut pixmap.as_mut());
    let rect = Rect::new(0, 0, w as i32, h as i32);
    let px = pixmap.pixels().iter().map(|p| rgba(p.demultiply())).collect();
    Ok(photocraft_compose::Buffer { rect, px })
}

/// A transparent pixmap, or `None` past [`MAX_RASTER_PIXELS`].
fn pixmap(w: u32, h: u32) -> Option<tiny_skia::Pixmap> {
    if u64::from(w) * u64::from(h) > MAX_RASTER_PIXELS {
        return None;
    }
    tiny_skia::Pixmap::new(w, h)
}

fn rgba(c: tiny_skia::ColorU8) -> [f32; 4] {
    [f32::from(c.red()) / 255.0, f32::from(c.green()) / 255.0, f32::from(c.blue()) / 255.0, f32::from(c.alpha()) / 255.0]
}

/// A pixmap as a surface covering `rect` in `fmt`.
fn surface(pixmap: &tiny_skia::Pixmap, rect: Rect, fmt: PixelFormat) -> Surface {
    let n = fmt.channels();
    let mut data = vec![0.0f32; pixmap.pixels().len() * n];
    for (p, out) in pixmap.pixels().iter().zip(data.chunks_exact_mut(n)) {
        photocraft_raster::from_rgba_into(&fmt, rgba(p.demultiply()), out);
    }
    let mut s = Surface::new(fmt);
    if !rect.is_empty() {
        s.write_region(rect, &data);
    }
    s.prune();
    s
}

/// Drawable elements in the tree (iterative: the nesting can be anything).
fn count_nodes(root: &usvg::Group, include_text: bool) -> usize {
    let mut n = 0;
    let mut stack: Vec<&usvg::Group> = vec![root];
    while let Some(g) = stack.pop() {
        for node in g.children() {
            n += 1;
            if let usvg::Node::Group(child) = node {
                stack.push(child);
            }
            if include_text && let usvg::Node::Text(text) = node {
                stack.push(text.flattened());
            }
            if n > MAX_NODES {
                return n;
            }
        }
    }
    n
}

/// Converts a parsed tree into layers.
struct Conv {
    fmt: PixelFormat,
    canvas: Rect,
    /// Drawing units → canvas pixels.
    global: tiny_skia::Transform,
    max_group_depth: Option<usize>,
    warnings: Vec<String>,
    /// Features already warned about (one warning each).
    noted: Vec<&'static str>,
    /// Unnamed shapes, groups, images and texts so far, for "Shape 3"-style names.
    counts: [usize; 4],
}

impl Conv {
    fn note(&mut self, what: &'static str, msg: String) {
        if !self.noted.contains(&what) {
            self.noted.push(what);
            self.warnings.push(msg);
        }
    }

    /// The element's `id`, or a numbered name by kind (0 shape, 1 group, 2 image, 3 text).
    fn name(&mut self, id: &str, kind: usize) -> String {
        if !id.is_empty() {
            return id.to_string();
        }
        let label = ["Shape", "Group", "Image", "Text"].get(kind).copied().unwrap_or("Layer");
        let n = self.counts.get_mut(kind).map(|c| {
            *c += 1;
            *c
        });
        format!("{label} {}", n.unwrap_or(0))
    }

    /// `nodes` in paint order (first = bottom), as layers in the same order. `parent` is the
    /// absolute transform of the group holding them (what resvg needs to render one of them
    /// alone, since a node's own data is in its parent's space).
    fn convert_children(&mut self, nodes: &[usvg::Node], depth: usize, rec: usize, parent: tiny_skia::Transform) -> Vec<Layer> {
        let mut out = Vec::new();
        for node in nodes {
            self.convert_node(node, &mut out, depth, rec, parent);
        }
        out
    }

    fn convert_node(&mut self, node: &usvg::Node, out: &mut Vec<Layer>, depth: usize, rec: usize, parent: tiny_skia::Transform) {
        match node {
            usvg::Node::Group(g) => {
                // A clip-path of one plain path becomes the group's vector mask (#4266); others
                // are rasterised with the group.
                let clip = g.clip_path().and_then(|cp| self.clip_mask(g, cp));
                let dropped: [(bool, &'static str); 3] =
                    [(!g.filters().is_empty(), "filter"), (g.clip_path().is_some() && clip.is_none(), "clip-path"), (g.mask().is_some(), "mask")];
                let too_deep = self.max_group_depth.is_some_and(|limit| depth >= limit) || depth >= MAX_GROUP_DEPTH || rec >= MAX_RECURSION;
                if dropped.iter().any(|d| d.0) || too_deep {
                    for (yes, what) in dropped {
                        if yes {
                            self.note(what, format!("SVG: a group with a {what} was rasterised, so it is not editable as shapes"));
                        }
                    }
                    if too_deep {
                        self.note("depth", "SVG: groups nested too deeply were rasterised".to_string());
                    }
                    let name = self.name(g.id(), 1);
                    out.extend(self.raster_layer(node, name, parent));
                    return;
                }
                // A group that only carries a transform (baked into its paths by usvg) or that
                // usvg inserted for its own bookkeeping adds nothing to the layer tree.
                let plain = clip.is_none() && g.id().is_empty() && g.opacity().get() >= 1.0 && g.blend_mode() == usvg::BlendMode::Normal;
                let children = self.convert_children(g.children(), if plain { depth } else { depth + 1 }, rec + 1, g.abs_transform());
                if children.is_empty() {
                    return;
                }
                if plain {
                    out.extend(children);
                    return;
                }
                let only_id = g.opacity().get() >= 1.0 && g.blend_mode() == usvg::BlendMode::Normal;
                // A lone child takes the group's name (and its clip, when it has no vector mask of
                // its own).
                if only_id && children.len() == 1 && (clip.is_none() || children.iter().all(|c| c.vector_mask.is_none())) {
                    if let Some(mut child) = children.into_iter().next() {
                        if !g.id().is_empty() {
                            child.name = g.id().to_string();
                        }
                        child.vector_mask = clip.or(child.vector_mask);
                        out.push(child);
                    }
                    return;
                }
                let mut l = Layer::new(self.name(g.id(), 1), LayerContent::Group(Group { children, expanded: false, artboard: None }));
                l.opacity = g.opacity().get();
                l.blend = blend(g.blend_mode());
                l.vector_mask = clip;
                out.push(l);
            }
            usvg::Node::Path(p) => out.extend(self.shape_layer(node, p, parent)),
            usvg::Node::Image(i) => {
                self.note("image", "SVG: embedded images are pixel layers".to_string());
                let name = self.name(i.id(), 2);
                out.extend(self.raster_layer(node, name, parent));
            }
            usvg::Node::Text(t) => {
                let children = self.convert_children(t.flattened().children(), depth + 1, rec + 1, t.flattened().abs_transform());
                if children.is_empty() {
                    self.note("text", "SVG: text needs a font the drawing can use; none matched, so it was dropped".to_string());
                    return;
                }
                let name = if t.id().is_empty() {
                    let content: String = t.chunks().iter().map(|c| c.text()).collect::<Vec<_>>().join("");
                    let content = content.trim();
                    if content.is_empty() { self.name("", 3) } else { content.chars().take(31).collect() }
                } else {
                    t.id().to_string()
                };
                out.push(Layer::new(name, LayerContent::Group(Group { children, expanded: false, artboard: None })));
            }
        }
    }

    /// The vector mask for group `g` clipped by `cp`, when the clip is one visible path with no
    /// clip of its own: what resvg draws for the clip (the group's transform, then the clip's,
    /// then the path), in canvas pixels, with the clip rule as its fill rule.
    fn clip_mask(&self, g: &usvg::Group, cp: &usvg::ClipPath) -> Option<VectorMask> {
        if cp.clip_path().is_some() {
            return None;
        }
        let [usvg::Node::Path(p)] = cp.root().children() else { return None };
        if !p.is_visible() {
            return None;
        }
        let ts = g.abs_transform().pre_concat(cp.transform()).post_concat(self.global);
        let subpaths = knots(&(*p.data()).clone().transform(ts)?);
        if subpaths.is_empty() {
            return None;
        }
        let fill_rule = match p.fill().map(|f| f.rule()) {
            Some(usvg::FillRule::EvenOdd) => FillRule::EvenOdd,
            _ => FillRule::NonZero,
        };
        Some(VectorMask::new(Path { subpaths, fill_rule, inverted: false }))
    }

    /// A path as a shape layer, or a pixel layer when its paint is a pattern.
    fn shape_layer(&mut self, node: &usvg::Node, p: &usvg::Path, parent: tiny_skia::Transform) -> Option<Layer> {
        if !p.is_visible() {
            return None;
        }
        let is_pattern = |paint: &usvg::Paint| matches!(paint, usvg::Paint::Pattern(_));
        if p.fill().is_some_and(|f| is_pattern(f.paint())) || p.stroke().is_some_and(|s| is_pattern(s.paint())) {
            self.note("pattern", "SVG: pattern paints were rasterised, so those shapes are not editable".to_string());
            let name = self.name(p.id(), 0);
            return self.raster_layer(node, name, parent);
        }
        let ts = p.abs_transform().post_concat(self.global);
        let data = (*p.data()).clone().transform(ts)?;
        let subpaths = knots(&data);
        if subpaths.is_empty() {
            return None;
        }
        let fill = p.fill().map(|f| self.paint(f.paint(), f.opacity().get(), ts));
        let stroke = p.stroke().map(|s| {
            // Stroke widths are in drawing units; under a non-uniform scale they take the
            // geometric mean, the one width the shape model has.
            let (kx, ky) = ((ts.sx * ts.sx + ts.ky * ts.ky).sqrt(), (ts.kx * ts.kx + ts.sy * ts.sy).sqrt());
            if (kx - ky).abs() > 0.01 * kx.max(ky) {
                self.note("stroke-scale", "SVG: strokes under a non-uniform scale keep one averaged width".to_string());
            }
            let k = (ts.sx * ts.sy - ts.kx * ts.ky).abs().sqrt().max(1e-6);
            let width = (s.width().get() * k).max(0.01);
            ShapeStroke {
                width,
                paint: self.paint(s.paint(), 1.0, ts),
                opacity: s.opacity().get(),
                align: StrokeAlign::Center,
                cap: match s.linecap() {
                    usvg::LineCap::Butt => LineCap::Butt,
                    usvg::LineCap::Round => LineCap::Round,
                    usvg::LineCap::Square => LineCap::Square,
                },
                join: match s.linejoin() {
                    usvg::LineJoin::Round => LineJoin::Round,
                    usvg::LineJoin::Bevel => LineJoin::Bevel,
                    usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => LineJoin::Miter,
                },
                miter_limit: s.miterlimit().get(),
                dashes: s.dasharray().map(|d| d.iter().map(|v| v * k / width).collect()).unwrap_or_default(),
                dash_offset: s.dashoffset() * k / width,
            }
        });
        let fill_rule = match p.fill().map(|f| f.rule()) {
            Some(usvg::FillRule::EvenOdd) => FillRule::EvenOdd,
            _ => FillRule::NonZero,
        };
        let mut sh = ShapeLayer { path: Path { subpaths, fill_rule, inverted: false }, fill, stroke, ..Default::default() };
        sh.cache = Some(photocraft_vector::render_shape(&sh, self.fmt, self.canvas));
        Some(Layer::new(self.name(p.id(), 0), LayerContent::Shape(sh)))
    }

    /// A paint as a layer fill. `ts` maps the paint's space onto the canvas.
    fn paint(&mut self, paint: &usvg::Paint, opacity: f32, ts: tiny_skia::Transform) -> Fill {
        let stops = |g: &usvg::BaseGradient| -> Vec<(f32, Color)> {
            g.stops().iter().map(|s| (s.offset().get(), color(s.color(), s.opacity().get() * opacity))).collect()
        };
        let gradient = |stops: Vec<(f32, Color)>, angle: f32, style: GradientStyle| Fill::Gradient {
            stops,
            angle,
            scale: 1.0,
            style,
            reverse: false,
            opacity_stops: Vec::new(),
            midpoints: Vec::new(),
            offset: (0.0, 0.0),
            dither: false,
            align: true,
        };
        match paint {
            usvg::Paint::Color(c) => Fill::Solid(color(*c, opacity)),
            usvg::Paint::LinearGradient(g) => {
                self.note("gradient", "SVG: gradient paints keep their stops and direction, not their exact geometry".to_string());
                let gt = g.transform().post_concat(ts);
                let mut a = tiny_skia::Point::from_xy(g.x1(), g.y1());
                let mut b = tiny_skia::Point::from_xy(g.x2(), g.y2());
                gt.map_point(&mut a);
                gt.map_point(&mut b);
                // Photoshop's angle: 0° left → right, counter-clockwise positive (y points down).
                let angle = (-(b.y - a.y)).atan2(b.x - a.x).to_degrees();
                gradient(stops(g), if angle.is_finite() { angle } else { 0.0 }, GradientStyle::Linear)
            }
            usvg::Paint::RadialGradient(g) => {
                self.note("gradient", "SVG: gradient paints keep their stops and direction, not their exact geometry".to_string());
                gradient(stops(g), 0.0, GradientStyle::Radial)
            }
            // Pattern paints are rasterised before we get here.
            usvg::Paint::Pattern(_) => Fill::Solid(Color::rgba(0.5, 0.5, 0.5, opacity)),
        }
    }

    /// The node rendered by resvg into a pixel layer over its bounding box, clipped to the canvas.
    /// `parent` is the absolute transform of the node's parent group.
    fn raster_layer(&mut self, node: &usvg::Node, name: String, parent: tiny_skia::Transform) -> Option<Layer> {
        let bb = node.abs_layer_bounding_box()?;
        let k = self.global.sx;
        let x0 = ((bb.x() * k).floor() as i32).max(self.canvas.x0);
        let y0 = ((bb.y() * k).floor() as i32).max(self.canvas.y0);
        let x1 = ((bb.right() * k).ceil() as i32).min(self.canvas.x1);
        let y1 = ((bb.bottom() * k).ceil() as i32).min(self.canvas.y1);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let Some(mut pixmap) = pixmap((x1 - x0) as u32, (y1 - y0) as u32) else {
            self.note("raster-size", "SVG: an element too large to rasterise was dropped".to_string());
            return None;
        };
        // resvg renders the node in its parent's space after shifting its absolute bounding box
        // to the origin: undo that shift, apply the parent's transform, then scale to the canvas
        // and shift so the pixmap starts at (x0, y0).
        let canvas = tiny_skia::Transform::from_row(k, 0.0, 0.0, k, -(x0 as f32), -(y0 as f32));
        let ts = canvas.pre_concat(parent).pre_translate(bb.x(), bb.y());
        resvg::render_node(node, ts, &mut pixmap.as_mut())?;
        Some(Layer::new(name, LayerContent::Raster(surface(&pixmap, Rect::new(x0, y0, x1, y1), self.fmt))))
    }
}

fn color(c: usvg::Color, alpha: f32) -> Color {
    Color::rgba(f32::from(c.red) / 255.0, f32::from(c.green) / 255.0, f32::from(c.blue) / 255.0, alpha.clamp(0.0, 1.0))
}

fn blend(b: usvg::BlendMode) -> BlendMode {
    use usvg::BlendMode as B;
    match b {
        B::Normal => BlendMode::Normal,
        B::Multiply => BlendMode::Multiply,
        B::Screen => BlendMode::Screen,
        B::Overlay => BlendMode::Overlay,
        B::Darken => BlendMode::Darken,
        B::Lighten => BlendMode::Lighten,
        B::ColorDodge => BlendMode::ColorDodge,
        B::ColorBurn => BlendMode::ColorBurn,
        B::HardLight => BlendMode::HardLight,
        B::SoftLight => BlendMode::SoftLight,
        B::Difference => BlendMode::Difference,
        B::Exclusion => BlendMode::Exclusion,
        B::Hue => BlendMode::Hue,
        B::Saturation => BlendMode::Saturation,
        B::Color => BlendMode::Color,
        B::Luminosity => BlendMode::Luminosity,
    }
}

fn pt(p: tiny_skia::Point) -> Point {
    Point::new(f64::from(p.x), f64::from(p.y))
}

/// A flattened-transform path as subpaths of knots: lines keep retracted handles, quadratics
/// are raised to cubics, and a closing segment that returns to the start merges into it. Every
/// subpath after the first joins its component, so the fill rule counts winding across the whole
/// path as SVG does (a second subpath drawn inside the first is a hole under even-odd).
fn knots(path: &tiny_skia::Path) -> Vec<Subpath> {
    use tiny_skia::PathSegment as S;
    let mut out: Vec<Subpath> = Vec::new();
    let mut cur: Option<Subpath> = None;
    fn flush(cur: &mut Option<Subpath>, out: &mut Vec<Subpath>, closed: bool) {
        let Some(mut sp) = cur.take() else { return };
        sp.closed = closed;
        if closed && sp.knots.len() > 1 {
            let (first, last) = (sp.knots.first().map(|k| k.anchor), sp.knots.last().map(|k| (k.anchor, k.in_ctrl)));
            if let (Some(a), Some((b, in_ctrl))) = (first, last)
                && (a.x - b.x).abs() < 1e-6
                && (a.y - b.y).abs() < 1e-6
            {
                sp.knots.pop();
                if let Some(k) = sp.knots.first_mut() {
                    k.in_ctrl = in_ctrl;
                }
            }
        }
        if !sp.knots.is_empty() {
            if !out.is_empty() {
                sp.op = PathOp::Join;
            }
            out.push(sp);
        }
    }
    fn curve(sp: &mut Subpath, c1: Point, c2: Point, p: Point) {
        if let Some(last) = sp.knots.last_mut() {
            last.out_ctrl = c1;
        }
        sp.knots.push(Knot { anchor: p, in_ctrl: c2, out_ctrl: p, smooth: false });
    }
    for seg in path.segments() {
        match seg {
            S::MoveTo(p) => {
                flush(&mut cur, &mut out, false);
                cur = Some(Subpath { knots: vec![Knot::corner(f64::from(p.x), f64::from(p.y))], ..Default::default() });
            }
            S::LineTo(p) => {
                if let Some(sp) = cur.as_mut() {
                    sp.knots.push(Knot::corner(f64::from(p.x), f64::from(p.y)));
                }
            }
            S::QuadTo(c, p) => {
                if let Some(sp) = cur.as_mut() {
                    let (c, p) = (pt(c), pt(p));
                    let p0 = sp.knots.last().map_or(p, |k| k.anchor);
                    let c1 = Point::new(p0.x + 2.0 / 3.0 * (c.x - p0.x), p0.y + 2.0 / 3.0 * (c.y - p0.y));
                    let c2 = Point::new(p.x + 2.0 / 3.0 * (c.x - p.x), p.y + 2.0 / 3.0 * (c.y - p.y));
                    curve(sp, c1, c2, p);
                }
            }
            S::CubicTo(c1, c2, p) => {
                if let Some(sp) = cur.as_mut() {
                    curve(sp, pt(c1), pt(c2), pt(p));
                }
            }
            S::Close => flush(&mut cur, &mut out, true),
        }
    }
    flush(&mut cur, &mut out, false);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded_image(mime: &str, bytes: &[u8]) -> String {
        let data = bytes.iter().map(|byte| format!("%{byte:02X}")).collect::<String>();
        format!("<image width=\"1\" height=\"1\" href=\"data:{mime},{data}\"/>")
    }

    fn image_svg(images: &str) -> String {
        format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\">{images}</svg>")
    }

    fn png_dimensions(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    #[test]
    fn embedded_raster_intrinsic_sizes_are_rejected_before_pixel_decode() {
        let png = png_dimensions(32768, 32768);
        let jpeg = vec![0xff, 0xd8, 0xff, 0xc0, 0, 17, 8, 0x80, 0, 0x80, 0, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0];
        let mut gif = b"GIF89a\x01\0\x01\0\0\0\0\x2c\0\0\0\0".to_vec();
        gif.extend_from_slice(&[0, 0x80, 0, 0x80, 0]);
        let mut webp = b"RIFF\x16\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        webp.extend_from_slice(&[0xff, 0x7f, 0, 0xff, 0x7f, 0]);
        for (mime, bytes) in [("image/png", png), ("image/jpeg", jpeg), ("image/gif", gif), ("image/webp", webp)] {
            // These are dimension headers, not valid compressed pixel streams. The budget
            // error proves no downstream image decoder was asked to allocate their pixels.
            let svg = image_svg(&embedded_image(mime, &bytes));
            let error = import_svg_bounded("clipboard", svg.as_bytes(), MAX_RASTER_PIXELS).unwrap_err();
            assert!(error.to_string().contains("decoded pixel budget"), "{mime}: {error}");
            assert!(rasterize_bounded(svg.as_bytes(), 1.0, MAX_RASTER_PIXELS).unwrap_err().to_string().contains("decoded pixel budget"));
        }
    }

    #[test]
    fn embedded_images_share_budget_and_nested_svg_cannot_hide_raster_bombs() {
        let image = embedded_image("image/png", &png_dimensions(2, 2));
        let aggregate = image_svg(&image.repeat(2));
        assert!(parse_bounded(aggregate.as_bytes(), 7).unwrap_err().to_string().contains("decoded pixel budget"));
        let oversized = image_svg(&embedded_image("image/png", &png_dimensions(u32::MAX, u32::MAX)));
        let nested = image_svg(&embedded_image("image/svg+xml", oversized.as_bytes()));
        assert!(parse_bounded(nested.as_bytes(), MAX_RASTER_PIXELS).unwrap_err().to_string().contains("decoded pixel budget"));
        let malformed = image_svg(&embedded_image("image/png", b"\x89PNG\r\n\x1a\n"));
        assert!(parse_bounded(malformed.as_bytes(), MAX_RASTER_PIXELS).unwrap_err().to_string().contains("dimensions"));
        assert_eq!(gif_frame_size(b"GIF89a\x01\0\x01\0\0\0\0\x21\xfe\xff"), None, "truncated extension has no frame");
    }

    #[test]
    fn webp_container_cannot_hide_a_larger_lossy_frame() {
        let mut webp = b"RIFF\x28\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0\0\0\0\0\0\0".to_vec();
        webp.extend_from_slice(b"VP8 \x0a\0\0\0\0\0\0\x9d\x01\x2a");
        webp.extend_from_slice(&8000u16.to_le_bytes());
        webp.extend_from_slice(&8000u16.to_le_bytes());
        assert_eq!(imagesize::blob_size(&webp).unwrap().width, 1, "container claims one pixel");
        let svg = image_svg(&embedded_image("image/webp", &webp));
        assert!(parse_bounded(svg.as_bytes(), MAX_RASTER_PIXELS).unwrap_err().to_string().contains("decoded pixel budget"));
    }

    #[test]
    fn bounded_images_keep_small_pixels_and_external_references_stay_disabled() {
        let image = photocraft_codecs::Image::from_u8(1, 1, photocraft_codecs::ChannelLayout::Rgba, vec![255, 0, 0, 255]).unwrap();
        let bytes = photocraft_codecs::encode(&image, photocraft_codecs::Format::Png, &Default::default()).unwrap();
        let inner = image_svg(&embedded_image("image/png", &bytes));
        let nested = image_svg(&embedded_image("image/svg+xml", inner.as_bytes()));
        for svg in [&inner, &nested] {
            let rendered = rasterize_bounded(svg.as_bytes(), 1.0, 16).unwrap();
            assert_eq!(rendered.px, vec![[1.0, 0.0, 0.0, 1.0]]);
        }
        assert!(rasterize_bounded(nested.as_bytes(), 32.0, 16).unwrap_err().to_string().contains("rendered pixel budget"));
        let external = image_svg("<rect width=\"1\" height=\"1\" fill=\"blue\"/><image href=\"file:///never-read.png\" width=\"1\" height=\"1\"/>");
        assert_eq!(rasterize_bounded(external.as_bytes(), 1.0, 16).unwrap().px, vec![[0.0, 0.0, 1.0, 1.0]]);
    }

    #[test]
    fn sniffing_accepts_svg_and_rejects_other_xml() {
        assert!(is_svg(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"));
        assert!(is_svg("\u{feff}<?xml version=\"1.0\"?>\n<!-- hi -->\n<svg/>".as_bytes()));
        assert!(is_svg(b"<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"x\"><svg/>"));
        assert!(!is_svg(b"<?xml version=\"1.0\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>"));
        assert!(!is_svg(b"\x89PNG\r\n"));
        assert!(!is_svg(b""));
    }

    #[test]
    fn quadratics_become_cubics_and_closing_merges_the_start_knot() {
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(0.0, 0.0);
        pb.quad_to(5.0, 10.0, 10.0, 0.0);
        pb.line_to(0.0, 0.0);
        pb.close();
        pb.move_to(20.0, 20.0);
        pb.line_to(30.0, 20.0);
        let path = pb.finish().unwrap();
        let sp = knots(&path);
        assert_eq!(sp.len(), 2);
        assert!(sp[0].closed && !sp[1].closed);
        // The explicit line back to the start was merged into the first knot.
        assert_eq!(sp[0].knots.len(), 2);
        let (a, b) = (&sp[0].knots[0], &sp[0].knots[1]);
        assert!((a.out_ctrl.x - 10.0 / 3.0).abs() < 1e-9 && (a.out_ctrl.y - 20.0 / 3.0).abs() < 1e-9);
        assert!((b.in_ctrl.x - 20.0 / 3.0).abs() < 1e-9 && (b.in_ctrl.y - 20.0 / 3.0).abs() < 1e-9);
        assert_eq!(b.anchor, Point::new(10.0, 0.0));
        assert_eq!(sp[1].knots.len(), 2);
    }

    #[test]
    fn text_without_any_font_is_reported_not_silently_dropped() {
        // No font at all (a bare CI image): usvg drops the <text> while parsing, so nothing
        // reaches the conversion; the reader still has to say why the text is missing.
        fn no_fonts() -> Arc<usvg::fontdb::Database> {
            Arc::new(usvg::fontdb::Database::new())
        }
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"120\" height=\"60\"><text x=\"5\" y=\"45\" font-size=\"48\">II</text></svg>";
        let parsed = parse_with(svg, no_fonts).unwrap();
        assert!(parsed.fontless_text);
        let r = import_parsed("t", parsed, MAX_GROUP_DEPTH, None, false).unwrap();
        assert!(r.document.layers.is_empty());
        assert!(r.warnings.iter().any(|w| w.contains("font")), "{:?}", r.warnings);
        // Clipboard/import paths use the bounded parser and retain the same diagnostic.
        let (parsed, _) = parse_bounded_inner_with(svg, MAX_RASTER_PIXELS, no_fonts).unwrap();
        assert!(parsed.fontless_text);
        let r = import_parsed("t", parsed, MAX_GROUP_DEPTH, Some(MAX_RASTER_PIXELS), false).unwrap();
        assert!(r.document.layers.is_empty());
        assert!(r.warnings.iter().any(|w| w.contains("font")), "{:?}", r.warnings);
        // A drawing without text never loads or misses fonts.
        let parsed =
            parse_with(b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>", no_fonts).unwrap();
        assert!(!parsed.fontless_text);
        let r = import_parsed("t", parsed, MAX_GROUP_DEPTH, None, false).unwrap();
        assert!(r.warnings.iter().all(|w| !w.contains("font")), "{:?}", r.warnings);
    }

    /// #4266: a group clipped by one path (the PhotoCraft icon's rounded square) stays editable
    /// shapes with the clip as its vector mask, and looks as resvg draws the drawing; a clip of
    /// two shapes is still rasterised, with the warning.
    #[test]
    fn a_single_path_clip_becomes_the_groups_vector_mask() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64">
            <defs><clipPath id="c"><rect x="4" y="4" width="40" height="40" rx="10"/></clipPath></defs>
            <g transform="translate(8 6)" clip-path="url(#c)">
              <rect width="64" height="64" fill="#2f7bf5"/>
              <circle cx="40" cy="40" r="20" fill="#efe9dc"/>
            </g></svg>"##;
        let r = import_svg("t", svg).unwrap();
        assert!(r.warnings.iter().all(|w| !w.contains("clip-path")), "{:?}", r.warnings);
        let [group] = r.document.layers.as_slice() else { panic!("one group: {:?}", r.document.layers.len()) };
        let LayerContent::Group(g) = &group.content else { panic!("a group") };
        assert_eq!(g.children.len(), 2);
        assert!(g.children.iter().all(|c| matches!(c.content, LayerContent::Shape(_))), "editable shapes");
        let mask = group.vector_mask.as_ref().expect("the clip as a vector mask");
        let xs: Vec<f64> = mask.path.subpaths.iter().flat_map(|s| s.knots.iter().map(|k| k.anchor.x)).collect();
        let (lo, hi) = (xs.iter().copied().fold(f64::MAX, f64::min), xs.iter().copied().fold(f64::MIN, f64::max));
        assert!((lo - 12.0).abs() < 1e-3 && (hi - 52.0).abs() < 1e-3, "in the group's space: {lo}..{hi}");
        // Pixel for pixel what resvg draws, but for anti-aliasing along the edges.
        let ours = photocraft_compose::flatten(&r.document).to_rgba8().pixels;
        let tree = parse(svg).unwrap().tree;
        let mut pm = tiny_skia::Pixmap::new(64, 64).unwrap();
        resvg::render(&tree, tiny_skia::Transform::default(), &mut pm.as_mut());
        let theirs: Vec<u8> = pm
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        let off = ours.chunks(4).zip(theirs.chunks(4)).filter(|(a, b)| a.iter().zip(*b).any(|(x, y)| x.abs_diff(*y) > 48)).count();
        assert!(off < 64, "{off} pixels differ");
        // The report's file: PhotoCraft's own icon opens without the clip-path warning.
        let icon = import_svg("photocraft.svg", include_bytes!("../../../assets/app-icon/photocraft.svg")).unwrap();
        assert!(icon.warnings.iter().all(|w| !w.contains("clip-path")), "{:?}", icon.warnings);
        assert!(icon.document.layers.iter().any(|l| l.vector_mask.is_some()), "its rounded square is a vector mask");
        // Two shapes in the clip: rasterised, and said so.
        let two = br##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64">
            <defs><clipPath id="c"><rect width="20" height="20"/><rect x="30" width="20" height="20"/></clipPath></defs>
            <g clip-path="url(#c)"><rect width="64" height="64"/><rect width="9" height="9" fill="red"/></g></svg>"##;
        let r = import_svg("t", two).unwrap();
        assert!(r.warnings.iter().any(|w| w.contains("clip-path")), "{:?}", r.warnings);
    }

    #[test]
    fn huge_drawings_scale_to_the_side_limit() {
        let tree = parse(b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100000 50000\"/>").unwrap().tree;
        let mut w = Vec::new();
        let cv = canvas(&tree, &mut w);
        assert_eq!((cv.size.width, cv.size.height), (MAX_SIDE, MAX_SIDE / 2));
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn bounded_import_preserves_shapes_at_budget_and_rasterises_aggregate_overflow() {
        let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="red"/><rect x="4" width="4" height="8" fill="blue"/></svg>"#;
        let at_limit = import_svg_bounded("bounded", bytes, 128).unwrap();
        assert_eq!(at_limit.document.layers.len(), 2);
        assert!(at_limit.document.layers.iter().all(|l| matches!(l.content, LayerContent::Shape(_))));
        let over = import_svg_bounded("bounded", bytes, 127).unwrap();
        assert!(import_svg_bounded_layers("bounded", bytes, 127).unwrap_err().to_string().contains("cache budget"));
        assert_eq!(over.document.layers.len(), 1);
        assert!(matches!(over.document.layers[0].content, LayerContent::Raster(_)));
        assert!(over.warnings.iter().any(|w| w.contains("cache budget") && w.contains("editable shapes")));
        let surface = over.document.layers[0].surface().unwrap();
        assert_eq!(surface.read_region(Rect::new(1, 1, 2, 2)), vec![1.0, 0.0, 0.0, 1.0]);
        assert_eq!(surface.read_region(Rect::new(6, 1, 7, 2)), vec![0.0, 0.0, 1.0, 1.0]);
        // Ordinary file imports retain their existing editable-layer behavior.
        assert_eq!(import_svg("unbounded", bytes).unwrap().document.layers.len(), 2);
    }

    #[test]
    fn bounded_import_rejects_canvas_before_rendering_and_respects_raster_limit() {
        let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8"/></svg>"#;
        assert!(import_svg_bounded("bounded", bytes, 0).is_err());
        assert!(import_svg_bounded("bounded", bytes, 63).is_err());
        assert!(import_svg_bounded("bounded", bytes, 64).is_ok());
        let oversized = br#"<svg xmlns="http://www.w3.org/2000/svg" width="5000" height="5000"><rect width="5000" height="5000"/></svg>"#;
        assert!(import_svg_bounded("bounded", oversized, u64::MAX).unwrap_err().to_string().contains("cache budget"));
    }
    /// A one-font database reproduces Linux hosts without Times New Roman. A missing authored
    /// family must still produce text geometry; it must not silently disappear on save-back.
    #[test]
    fn missing_svg_font_uses_an_available_generic_instead_of_dropping_text() {
        fn inter() -> Arc<usvg::fontdb::Database> {
            let mut db = usvg::fontdb::Database::new();
            db.load_font_data(include_bytes!("../../../assets/fonts/Inter-Regular.ttf").to_vec());
            configure_generic_fonts(&mut db);
            Arc::new(db)
        }
        let svg=br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="30"><text x="2" y="24" font-size="20" font-family="Unavailable Authored Font">LOGO</text></svg>"#;
        let parsed = parse_with(svg, inter).unwrap();
        assert!(!parsed.fontless_text);
        assert!(!parsed.tree.root().children().is_empty(), "missing-family fallback must preserve text geometry");
        let mut db = (*inter()).clone();
        let before = db.query(&usvg::fontdb::Query { families: &[usvg::fontdb::Family::Serif], ..Default::default() });
        configure_generic_fonts(&mut db);
        assert_eq!(db.query(&usvg::fontdb::Query { families: &[usvg::fontdb::Family::Serif], ..Default::default() }), before);
    }
}
