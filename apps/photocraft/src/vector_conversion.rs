//! Bounded, private VectorCraft conversion before an embedded AI/PDF/EPS editing session.
//! No document metadata supplies a process, command line or output path.

use super::native::OwnedSvg;
use photocraft_ui_egui::ExternalVectorSource;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// Prepare a bounded SVG working copy, converting native vectors inside private MCP roots.
pub(super) fn prepare(editor: &Path, source: &ExternalVectorSource, stop: &Receiver<()>) -> Result<(OwnedSvg, Vec<u8>), String> {
    if !matches!(source.extension.as_str(), "svg" | "pdf" | "ai" | "eps") {
        return Err("Unsupported embedded vector source type".into());
    }
    let owned = OwnedSvg::create(&source.bytes)?;
    if source.extension == "svg" {
        return Ok((owned, source.bytes.as_ref().clone()));
    }
    let cli_name = if cfg!(windows) { "vectorcraft-cli.exe" } else { "vectorcraft-cli" };
    let cli = editor.parent().ok_or("The editor has no installation directory")?.join(cli_name);
    // Selecting an editor explicitly also selects its adjacent VectorCraft CLI; PATH and the
    // embedded file name never participate in executable discovery.
    let metadata =
        std::fs::symlink_metadata(&cli).map_err(|_| "Embedded AI/PDF/EPS editing needs vectorcraft-cli next to the selected VectorCraft executable")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("VectorCraft CLI must be a regular file".into());
    }
    let input = owned.root.join(format!("Source.{}", source.extension));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&input).map_err(|e| format!("Cannot create vector input: {e}"))?;
    file.write_all(&source.bytes).map_err(|e| format!("Cannot write vector input: {e}"))?;
    drop(file);
    let commands = conversion_commands(&input, &owned.file)?;
    // MCP's headless roots apply to every thread and file reader, including references in
    // hostile embedded artwork. Ordinary `convert` does not establish those restrictions.
    let responses_path = owned.root.join("Responses.jsonl");
    let mut responses_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&responses_path)
        .map_err(|e| format!("Cannot create conversion response file: {e}"))?;
    let responses_output = responses_file.try_clone().map_err(|e| format!("Cannot prepare conversion response handle: {e}"))?;
    let mut child = Command::new(cli)
        .args(["mcp", "--headless", "--automation-read-root"])
        .arg(&owned.root)
        .arg("--automation-write-root")
        .arg(&owned.root)
        .current_dir(&owned.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(responses_output))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot start VectorCraft conversion: {e}"))?;
    let result = (|| {
        let mut stdin = child.stdin.take().ok_or("VectorCraft conversion input is unavailable")?;
        for (id, command, params) in commands {
            let request = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"run_command","arguments":{"command":command,"params":params}}});
            writeln!(stdin, "{request}").map_err(|e| format!("Cannot send vector conversion request: {e}"))?;
        }
        drop(stdin);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if stop.try_recv().is_ok() {
                return Err("Vector editing cancelled".into());
            }
            if let Some(status) = child.try_wait().map_err(|e| format!("Cannot wait for vector conversion: {e}"))? {
                if !status.success() {
                    return Err("VectorCraft could not convert the embedded vector".into());
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err("VectorCraft conversion timed out".into());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let mut responses = String::new();
        // Read the retained handle, never reopen a pathname an input processor could replace.
        responses_file.rewind().map_err(|e| format!("Cannot seek conversion responses: {e}"))?;
        responses_file.take(1024 * 1024 + 1).read_to_string(&mut responses).map_err(|e| format!("Invalid conversion responses: {e}"))?;
        validate_responses(&responses)?;
        let converted = owned.read(&owned.stamp()?)?;
        let svg = source_svg(&converted, source.size)?;
        // Validate bounded rendering before the interactive editor sees the working copy.
        photocraft_io::svg::import_svg_bounded("Artwork.svg", &svg, 24_000_000).map_err(|e| format!("Converted SVG is unsupported: {e}"))?;
        owned.overwrite(&svg)?;
        Ok(svg)
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result.map(|svg| (owned, svg))
}

/// Build requests only from UTF-8 paths: serializing a non-UTF-8 `Path` inside `json!` panics.
fn conversion_commands(input: &Path, output: &Path) -> Result<[(u8, &'static str, serde_json::Value); 2], String> {
    let input = input.to_str().ok_or("The temporary vector input path is not UTF-8")?;
    let output = output.to_str().ok_or("The temporary vector output path is not UTF-8")?;
    Ok([(1, "document.open", serde_json::json!({"path": input})), (2, "document.export", serde_json::json!({"path": output, "format": "svg"}))])
}

/// Keep the original PSD source-pixel frame: PDF points and EPS bounds need not equal the
/// placed object's stored pixel dimensions. The nested viewport scales vector geometry once.
fn source_svg(bytes: &[u8], size: Option<(f64, f64)>) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "Converted SVG is not UTF-8")?;
    photocraft_io::svg::import_svg_bounded("Artwork.svg", bytes, 24_000_000).map_err(|e| format!("Converted SVG is unsupported: {e}"))?;
    let doc = roxmltree::Document::parse(text).map_err(|e| format!("Invalid converted SVG: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "svg" {
        return Err("VectorCraft did not return SVG".into());
    }
    let Some((w, h)) = size else {
        return Ok(bytes.to_vec());
    };
    if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 || w > 16384.0 || h > 16384.0 || w * h > 24_000_000.0 {
        return Err("The original vector source dimensions exceed the editing limit".into());
    }
    let dimension = |key| root.attribute(key).and_then(|v| v.trim_end_matches("pt").parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0);
    let (iw, ih) = dimension("width").zip(dimension("height")).ok_or("Converted SVG has no finite dimensions")?;
    let inner = text.get(root.range()).ok_or("Invalid SVG root range")?;
    let result =
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {iw} {ih}" preserveAspectRatio="none">{inner}</svg>"#)
            .into_bytes();
    if result.len() > 16 * 1024 * 1024 {
        return Err("Converted SVG exceeds 16 MiB".into());
    }
    Ok(result)
}

/// Require successful replies for both opening the source and exporting the SVG.
fn validate_responses(text: &str) -> Result<(), String> {
    if text.len() > 1024 * 1024 {
        return Err("VectorCraft conversion responses exceeded the limit".into());
    }
    let replies: Vec<serde_json::Value> =
        text.lines().map(serde_json::from_str).collect::<Result<_, _>>().map_err(|_| "Invalid VectorCraft conversion response")?;
    for id in [1, 2] {
        let reply = replies.iter().find(|r| r.get("id").and_then(serde_json::Value::as_u64) == Some(id)).ok_or("Missing VectorCraft conversion response")?;
        if reply.get("error").is_some() || reply.get("result").and_then(|r| r.get("isError")).and_then(serde_json::Value::as_bool) != Some(false) {
            return Err("VectorCraft could not open or export the embedded vector; original contents were retained".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn non_utf8_temporary_paths_return_errors_instead_of_panicking() {
        use std::os::unix::ffi::OsStrExt;
        let invalid = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/vector-\xff"));
        let valid = Path::new("/tmp/Artwork.svg");
        assert!(conversion_commands(invalid, valid).is_err());
        assert!(conversion_commands(valid, invalid).is_err());
        let commands = conversion_commands(valid, valid).unwrap();
        assert_eq!(commands[0].2["path"], "/tmp/Artwork.svg");
        assert_eq!(commands[1].2["format"], "svg");
    }

    #[test]
    fn converted_vectors_use_the_psd_source_pixel_frame() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="6"><rect width="8" height="6" fill="red"/></svg>"#;
        let normalized = source_svg(svg, Some((16.0, 12.0))).unwrap();
        let parsed = photocraft_io::svg::import_svg_bounded("Artwork.svg", &normalized, 24_000_000).unwrap();
        assert_eq!(parsed.document.size, photocraft_geom::Size::new(16, 12));
        assert_eq!(source_svg(svg, None).unwrap(), svg);
        for size in [(f64::NAN, 1.0), (1.0, f64::INFINITY), (0.0, 1.0), (16384.0, 16384.0)] {
            assert!(source_svg(svg, Some(size)).is_err());
        }
        assert!(source_svg(b"%PDF-1.6", None).is_err());
    }

    #[test]
    fn conversion_requires_successful_open_and_export_replies() {
        let good = r#"{"id":1,"result":{"isError":false}}
{"id":2,"result":{"isError":false}}"#;
        assert!(validate_responses(good).is_ok());
        for bad in [
            "",
            r#"{"id":2,"result":{"isError":false}}"#,
            r#"{"id":1,"result":{"isError":true}}
{"id":2,"result":{"isError":false}}"#,
            r#"{"id":1,"error":{"message":"denied"}}"#,
        ] {
            assert!(validate_responses(bad).is_err());
        }
    }
}

#[cfg(test)]
mod live_vectorcraft_tests {
    use super::*;
    use std::sync::{Arc, mpsc};

    /// Opt-in executable test uses only our own PostScript, never user artwork or Adobe assets.
    #[test]
    #[ignore = "set VECTORCRAFT_TEST_EDITOR to a local VectorCraft executable"]
    fn real_vectorcraft_converts_eps_and_rejects_failed_open_without_blank_replacement() {
        let editor = std::env::var_os("VECTORCRAFT_TEST_EDITOR").expect("VECTORCRAFT_TEST_EDITOR");
        let editor = Path::new(&editor);
        let (_send, stop) = mpsc::channel();
        let eps = b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 8 6\n0 0 1 setrgbcolor 0 0 8 6 rectfill\nshowpage\n%%EOF\n";
        let source = ExternalVectorSource { bytes: Arc::new(eps.to_vec()), extension: "eps".into(), size: Some((16.0, 12.0)) };
        let (owned, svg) = prepare(editor, &source, &stop).unwrap();
        assert!(owned.file.exists());
        let raster = photocraft_io::svg::rasterize_bounded(&svg, 1.0, 24_000_000).unwrap();
        assert_eq!((raster.rect.width(), raster.rect.height()), (16, 12));
        let invalid = ExternalVectorSource { bytes: Arc::new(b"%PDF-1.6\nnot a PDF".to_vec()), extension: "pdf".into(), size: None };
        assert!(prepare(editor, &invalid, &stop).is_err());
        let root = owned.root.clone();
        drop(owned);
        assert!(!root.exists());
    }
}
