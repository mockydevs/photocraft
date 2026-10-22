//! Memory-only SVG smart-object handoff. The shell owns editor selection and temporary files;
//! commands own optimistic concurrency, bounded rendering and one shared-content transaction.

use base64::Engine as _;
use std::sync::Arc;

use photocraft_doc::{Document, Layer, LayerContent, LayerId, SmartContentsId, SmartObject, SmartSource};
use photocraft_geom::{Rect, Size};
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

const EXPORT: &str = "layer.smartObjects.exportSvg";
const UPDATE: &str = "layer.smartObjects.updateSvg";
const MAX_PIXELS: u64 = 24_000_000;

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn layer(s: &Session, p: &Value, cmd: &str, required: bool) -> Result<LayerId> {
    if !p.is_object() {
        return Err(bad(cmd, "parameters must be an object"));
    }
    match p.get("layer") {
        Some(v) => v.as_u64().map(LayerId).ok_or_else(|| bad(cmd, "`layer` must be an integer layer id")),
        None if required => Err(bad(cmd, "missing `layer`")),
        None => crate::commands::layer_param(s, p),
    }
}

fn smart(doc: &Document, id: LayerId) -> Result<&SmartObject> {
    match &doc.layer(id).ok_or(EngineError::NoLayer(id))?.content {
        LayerContent::Smart(sm) => Ok(sm),
        _ => Err(EngineError::Other("the layer is not a smart object".into())),
    }
}

/// Borrow embedded bytes from the object or PSD table without following a linked disk path.
fn embedded<'a>(doc: &'a Document, sm: &'a SmartObject) -> Result<(String, &'a [u8])> {
    let (name, bytes) = match &sm.source {
        SmartSource::Embedded { file_name, bytes } => (file_name.clone(), bytes.as_slice()),
        SmartSource::Linked { path } => photocraft_io::linked::embedded_payload(&doc.metadata, path)
            .ok_or_else(|| EngineError::Other("external editing requires embedded vector contents".into()))?,
    };
    validate_source(bytes)?;
    Ok((name, bytes))
}

/// Reject unsupported magic bytes and oversized native-vector payloads before handoff.
fn validate_source(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > crate::vector_paste_cmds::MAX_VECTOR_CLIPBOARD_BYTES || source_extension(bytes).is_none() {
        return Err(EngineError::Other("external editing requires SVG, PDF-compatible AI, PDF or EPS contents within 16 MiB".into()));
    }
    Ok(())
}

/// Identify supported vector formats from their payload rather than a filename suffix.
fn source_extension(bytes: &[u8]) -> Option<&'static str> {
    if photocraft_io::svg::is_svg(bytes) {
        Some("svg")
    } else if bytes.starts_with(b"%PDF-") {
        Some("pdf")
    } else if bytes.starts_with(b"%!PS-Adobe-") || bytes.starts_with(&[0xc5, 0xd0, 0xd3, 0xc6]) {
        Some("eps")
    } else {
        None
    }
}

/// Recover the bounded original PSD source-pixel frame for the conversion viewport.
fn source_size(sm: &SmartObject) -> Option<(f64, f64)> {
    let placed = photocraft_io::smart_map::parse_sold(sm.psd_raw.as_ref()?.as_slice())?;
    photocraft_io::smart_map::stored_size(&placed.descriptor)
        .filter(|(w, h)| w.is_finite() && h.is_finite() && *w <= 16384.0 && *h <= 16384.0 && w * h <= MAX_PIXELS as f64)
}

fn enabled(s: &Session) -> std::result::Result<(), String> {
    let state = s.active().ok_or("no document open")?;
    if s.job_on(state.doc.id).is_some() {
        return Err("the document has a running background job".into());
    }
    if s.tools.mask_targeted || state.channel_view.layer_mask.is_some() || state.channel_view.target != crate::channel_cmds::ChannelTarget::Composite {
        return Err("SVG contents editing requires the layer composite target".into());
    }
    Ok(())
}

fn check_instances(s: &Session, id: LayerId) -> Result<Vec<LayerId>> {
    let doc = &s.active().ok_or(EngineError::NoDocument)?.doc;
    let contents = smart(doc, id)?.contents_id;
    if s.smart_links.iter().any(|link| link.parent == doc.id && link.contents_id == contents) {
        return Err(EngineError::Other("these smart-object contents are already open in PhotoCraft".into()));
    }
    let ids: Vec<_> =
        doc.walk().into_iter().filter_map(|(_, _, l)| matches!(&l.content, LayerContent::Smart(sm) if sm.contents_id == contents).then_some(l.id)).collect();
    for id in &ids {
        let locks = doc.effective_locks(*id);
        if locks.all || locks.pixels {
            return Err(EngineError::Other("a shared smart-object instance is locked".into()));
        }
        embedded(doc, smart(doc, *id)?)?;
    }
    Ok(ids)
}

/// Export a bounded source and optimistic identity entirely in memory.
fn export(s: &mut Session, p: &Value) -> Result<Value> {
    let id = layer(s, p, EXPORT, false)?;
    check_instances(s, id)?;
    let doc = &s.active().ok_or(EngineError::NoDocument)?.doc;
    let sm = smart(doc, id)?;
    let (name, bytes) = embedded(doc, sm)?;
    let mut extension = source_extension(bytes).ok_or_else(|| bad(EXPORT, "unsupported vector source"))?;
    if extension != "svg" && name.to_ascii_lowercase().ends_with(".ai") {
        extension = "ai";
    }
    if extension != "svg" && sm.psd_raw.is_some() && source_size(sm).is_none() {
        return Err(bad(EXPORT, "the embedded vector has no supported PSD source-pixel frame"));
    }
    let mut result = json!({"document":doc.id.0,"fingerprint":blake3::hash(bytes).to_hex().to_string(),"contentsId":sm.contents_id.0,"extension":extension,"sourceSize":source_size(sm)});
    if extension == "svg" {
        result["svg"] = json!(std::str::from_utf8(bytes).map_err(|_| bad(EXPORT, "SVG contents must be UTF-8"))?);
    } else {
        result["sourceBase64"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    Ok(result)
}

/// Mirror the renderer's mesh vertices before it allocates destination tiles. Without a warp,
/// a homography has corner extrema only when its denominator keeps one nonzero sign.
fn placed_bounds(sm: &SmartObject, size: Size) -> Result<Rect> {
    let (mut w, mut h) = (f64::from(size.width), f64::from(size.height));
    let warp = sm.warp.as_ref().filter(|warp| !warp.is_identity());
    if sm.warp.is_none() && sm.stack_mode.is_none() {
        let scale = crate::smart_cmds::vector_scale(sm);
        w = (w * scale).ceil() / scale;
        h = (h * scale).ceil() / scale;
    }
    let (nx, ny) = if warp.is_some() { (((w / 4.0).ceil() as usize).clamp(1, 512), ((h / 4.0).ceil() as usize).clamp(1, 512)) } else { (1, 1) };
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut sign = None;
    for j in 0..=ny {
        for i in 0..=nx {
            let (x, y) = (w * i as f64 / nx as f64, h * j as f64 / ny as f64);
            let (x, y) = warp.map_or((x, y), |warp| warp.map(x, y));
            let (x, y) = if let Some(m) = sm.perspective {
                let denominator = m[6] * x + m[7] * y + m[8];
                let positive = denominator > 0.0;
                if !denominator.is_finite() || denominator.abs() < 1e-9 || sign.is_some_and(|prev| prev != positive) {
                    return Err(bad(UPDATE, "SVG placement crosses an unbounded perspective"));
                }
                sign = Some(positive);
                ((m[0] * x + m[1] * y + m[2]) / denominator, (m[3] * x + m[4] * y + m[5]) / denominator)
            } else {
                let [a, b, c, d, e, f] = sm.transform.m;
                (a * x + c * y + e, b * x + d * y + f)
            };
            if !x.is_finite() || !y.is_finite() || x.abs() > 1_000_000.0 || y.abs() > 1_000_000.0 {
                return Err(bad(UPDATE, "SVG placement exceeds the supported coordinate range"));
            }
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    // Unwarped vectors rasterise at a quantised scale. Include its rounded source-pixel
    // edges and the inverse sampler's border, as well as the mesh renderer's one-pixel border.
    let padding = if warp.is_some() { 1 } else { 4 };
    Ok(Rect::new(x0.floor() as i32 - padding, y0.floor() as i32 - padding, x1.ceil() as i32 + padding, y1.ceil() as i32 + padding))
}

/// Validate identity and rendering limits before one undoable shared-content replacement.
fn update(s: &mut Session, p: &Value) -> Result<Value> {
    let id = layer(s, p, UPDATE, true)?;
    let document = p.get("document").and_then(Value::as_u64).ok_or_else(|| bad(UPDATE, "`document` must be an integer document id"))?;
    if s.active().map(|state| state.doc.id.0) != Some(document) {
        return Err(bad(UPDATE, "the smart object's parent document is no longer active"));
    }
    let svg = p.get("svg").and_then(Value::as_str).ok_or_else(|| bad(UPDATE, "`svg` must be an SVG string"))?;
    let expected = p
        .get("fingerprint")
        .and_then(Value::as_str)
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| bad(UPDATE, "`fingerprint` must be a BLAKE3 hex digest"))?;
    let contents = p.get("contentsId").and_then(Value::as_u64).map(SmartContentsId).ok_or_else(|| bad(UPDATE, "`contentsId` must be an integer"))?;
    let ids = check_instances(s, id)?;
    let doc = &s.active().ok_or(EngineError::NoDocument)?.doc;
    let target = smart(doc, id)?;
    let (name, original) = embedded(doc, target)?;
    if target.contents_id != contents || !blake3::hash(original).to_hex().as_str().eq_ignore_ascii_case(expected) {
        return Err(bad(UPDATE, "smart-object contents changed since external editing started"));
    }
    for instance in &ids {
        let (_, bytes) = embedded(doc, smart(doc, *instance)?)?;
        if !blake3::hash(bytes).to_hex().as_str().eq_ignore_ascii_case(expected) {
            return Err(bad(UPDATE, "shared smart-object instances have conflicting contents"));
        }
    }
    if original == svg.as_bytes() {
        return Ok(json!({"layer":id.0,"contentsId":contents.0,"fingerprint":expected,"updated":false}));
    }
    // Persisted smart filters are executable input too. A source replacement replays them
    // without Session::execute, so authorize every filter that render_in will apply, across
    // all shared instances, before parsing/rendering any replacement. Desktop sessions keep
    // their normal user-selected file access; untrusted automation must not inherit it.
    if let Some(authorize) = s.authorize {
        for instance in &ids {
            let sm = smart(doc, *instance)?;
            if sm.filters_enabled {
                for filter in sm.smart_filters.iter().filter(|filter| filter.visible) {
                    authorize(&filter.command, &filter.params)?;
                }
            }
        }
    }
    if svg.is_empty() || svg.len() > crate::vector_paste_cmds::MAX_VECTOR_CLIPBOARD_BYTES || !photocraft_io::svg::is_svg(svg.as_bytes()) {
        return Err(bad(UPDATE, "SVG contents must contain a valid SVG document within 16 MiB"));
    }
    // Save-back can intentionally remove the last drawable object. Use the same bounded
    // importer as clipboard preparation without its paste-only nonempty-layer requirement.
    let imported = photocraft_io::svg::import_svg_bounded(&name, svg.as_bytes(), MAX_PIXELS)
        .map_err(|error| bad(UPDATE, format!("can't import edited SVG contents: {error}")))?;
    let size = imported.document.size;
    if size.area() > MAX_PIXELS {
        return Err(bad(UPDATE, "SVG contents exceed the 24 million pixel limit"));
    }
    let source = SmartSource::Embedded {
        file_name: if photocraft_io::svg::is_svg(original) { name } else { "Artwork.svg".into() },
        bytes: Arc::new(svg.as_bytes().to_vec()),
    };
    let mut replacements: Vec<Layer> = Vec::with_capacity(ids.len());
    let mut pixels = 0u64;
    for instance in ids {
        let mut replacement = doc.layer(instance).cloned().ok_or(EngineError::NoLayer(instance))?;
        let LayerContent::Smart(sm) = &mut replacement.content else { return Err(EngineError::NoLayer(instance)) };
        let bounds = placed_bounds(sm, size)?;
        let extent = if sm.filters_enabled && sm.smart_filters.iter().any(|filter| filter.visible) { bounds.union(&doc.bounds()) } else { bounds };
        pixels = pixels.checked_add(extent.size().area()).ok_or_else(|| bad(UPDATE, "SVG placement cache budget exceeded"))?;
        if pixels > MAX_PIXELS {
            return Err(bad(UPDATE, "SVG placement caches exceed the 24 million pixel limit"));
        }
        sm.source = source.clone();
        replacements.push(replacement);
    }
    // Finish every fallible render before committing any document state.
    for replacement in &mut replacements {
        if !crate::smart_cmds::refresh_layer(doc, replacement)? {
            return Err(bad(UPDATE, "SVG contents could not be rendered"));
        }
        crate::smart_cmds::detach_psd(replacement);
    }
    s.edit("Update SVG Contents", |doc, _| {
        for replacement in replacements {
            let id = replacement.id;
            *doc.layer_mut(id).ok_or(EngineError::NoLayer(id))? = replacement;
        }
        Ok(())
    })?;
    Ok(json!({"layer":id.0,"contentsId":contents.0,"fingerprint":blake3::hash(svg.as_bytes()).to_hex().to_string(),"updated":true}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: EXPORT,
            label: "Export SVG Contents",
            menu: &[],
            shortcut: None,
            params: r#"{"layer":id?} → {document,svg?|sourceBase64?,extension,sourceSize,fingerprint,contentsId}"#,
            enabled,
            run: export,
            journal: false,
        },
        CommandSpec {
            id: UPDATE,
            label: "Update SVG Contents",
            menu: &[],
            shortcut: None,
            params: r#"{"document":id,"layer":id,"svg":"<svg …>","fingerprint":"original BLAKE3 hex","contentsId":id} → {layer,contentsId,fingerprint,updated}"#,
            enabled,
            run: update,
            journal: true,
        },
    ]
}

#[cfg(test)]
mod tests;
