use super::*;
use photocraft_doc::{DocId, Locks, SmartFilter};
use photocraft_geom::Affine;

const RED: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="6"><rect width="8" height="6" fill="red"/></svg>"#;
const BLUE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="6"><rect width="8" height="6" fill="blue"/></svg>"#;
const GREEN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="6"><rect width="8" height="6" fill="lime"/></svg>"#;

fn session(depth: u32) -> (Session, LayerId) {
    let mut s = Session::new();
    s.execute("file.new", json!({"width":32,"height":24,"depth":depth})).unwrap();
    let result = s.execute("edit.pasteVector", json!({"svg":RED,"mode":"smartObject"})).unwrap();
    let id = LayerId(result["layers"][0].as_u64().unwrap());
    (s, id)
}

fn request(s: &mut Session, id: LayerId, svg: &str) -> Value {
    let handoff = s.execute(EXPORT, json!({"layer":id.0})).unwrap();
    json!({"document":handoff["document"],"layer":id.0,"svg":svg,"fingerprint":handoff["fingerprint"],"contentsId":handoff["contentsId"]})
}

fn source(s: &Session, id: LayerId) -> Vec<u8> {
    embedded(&s.active().unwrap().doc, smart(&s.active().unwrap().doc, id).unwrap()).unwrap().1.to_vec()
}

fn fails_without_edit(s: &mut Session, params: Value) {
    let before = s.active().unwrap().doc.clone();
    let revision = s.active().unwrap().revision;
    assert!(s.execute(UPDATE, params).is_err());
    assert_eq!(s.active().unwrap().doc, before);
    assert_eq!(s.active().unwrap().revision, revision);
}

#[test]
fn export_and_unchanged_save_have_no_history_step() {
    let (mut s, id) = session(8);
    let revision = s.active().unwrap().revision;
    let params = request(&mut s, id, RED);
    let result = s.execute(UPDATE, params).unwrap();
    assert_eq!(result["updated"], false);
    assert_eq!(s.active().unwrap().revision, revision);
    assert!(s.undo());
    assert!(s.active().unwrap().doc.layer(id).is_none());
}

#[test]
fn deleting_last_svg_object_saves_transparency_and_undo_restores_source() {
    let (mut s, id) = session(8);
    let before = s.active().unwrap().doc.clone();
    let empty = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="6"/>"#;
    let params = request(&mut s, id, empty);
    s.execute(UPDATE, params).unwrap();
    assert_eq!(source(&s, id), empty.as_bytes());
    assert_eq!(smart(&s.active().unwrap().doc, id).unwrap().cache.as_ref().unwrap().pixel(15, 11), vec![0.0; 4]);
    assert!(s.undo());
    assert_eq!(s.active().unwrap().doc, before);
}

#[test]
fn valid_updates_repeat_at_all_depths_and_roundtrip_original_svg() {
    for depth in [8, 16, 32] {
        let (mut s, id) = session(depth);
        let original = s.active().unwrap().doc.clone();
        let params = request(&mut s, id, BLUE);
        let result = s.execute(UPDATE, params).unwrap();
        assert_eq!(result["updated"], true);
        let sm = smart(&s.active().unwrap().doc, id).unwrap();
        assert_eq!(sm.cache.as_ref().unwrap().format(), s.active().unwrap().doc.pixel_format());
        assert_eq!(sm.cache.as_ref().unwrap().pixel(15, 11), vec![0.0, 0.0, 1.0, 1.0]);
        assert_eq!(source(&s, id), BLUE.as_bytes());
        let after = s.active().unwrap().doc.clone();
        assert!(s.undo());
        assert_eq!(s.active().unwrap().doc, original);
        assert!(s.redo());
        assert_eq!(s.active().unwrap().doc, after);
        let params = request(&mut s, id, GREEN);
        s.execute(UPDATE, params).unwrap();
        assert_eq!(source(&s, id), GREEN.as_bytes());
        let bytes = photocraft_format::save_to_bytes(&s.active().unwrap().doc, &Default::default()).unwrap();
        let loaded = photocraft_format::load_from_bytes(&bytes).unwrap();
        let loaded_source = &smart(&loaded, id).unwrap().source;
        assert!(matches!(loaded_source, SmartSource::Embedded {file_name,bytes} if file_name == "Clipboard.svg" && bytes.as_slice() == GREEN.as_bytes()));
    }
}

#[test]
fn shared_instances_keep_placement_filters_masks_and_one_undo_transaction() {
    let (mut s, id) = session(8);
    let duplicate = {
        let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
        let mut layer = doc.layer(id).unwrap().duplicate();
        let LayerContent::Smart(sm) = &mut layer.content else { panic!("smart object") };
        sm.transform = Affine::translate(2.0, 3.0);
        sm.smart_filters.push(SmartFilter {
            command: "filter.invert".into(),
            params: json!({}),
            blend: photocraft_color::BlendMode::Normal,
            opacity: 0.5,
            visible: true,
        });
        sm.filters_enabled = false;
        sm.filter_mask = Some(photocraft_doc::LayerMask {
            surface: photocraft_raster::Surface::new(photocraft_color::PixelFormat::GRAY8),
            enabled: true,
            linked: true,
            density: 0.5,
            feather: 1.0,
        });
        let duplicate = layer.id;
        doc.layers.push(layer);
        duplicate
    };
    let before = s.active().unwrap().doc.clone();
    let preserved = smart(&before, duplicate).unwrap().clone();
    let revision = s.active().unwrap().revision;
    let params = request(&mut s, id, BLUE);
    s.execute(UPDATE, params).unwrap();
    assert_eq!(s.active().unwrap().revision, revision + 1);
    assert_eq!(source(&s, id), source(&s, duplicate));
    let edited = smart(&s.active().unwrap().doc, duplicate).unwrap();
    assert_eq!(edited.transform, preserved.transform);
    assert_eq!(edited.smart_filters, preserved.smart_filters);
    assert_eq!(edited.filter_mask, preserved.filter_mask);
    assert_eq!(edited.filters_enabled, preserved.filters_enabled);
    assert_eq!(edited.contents_id, preserved.contents_id);
    assert_eq!(edited.cache.as_ref().unwrap().pixel(4, 4), vec![0.0, 0.0, 1.0, 1.0]);
    assert!(s.undo());
    assert_eq!(s.active().unwrap().doc, before);
    assert!(s.redo());
    assert_eq!(source(&s, id), BLUE.as_bytes());
}

#[test]
fn invalid_oversized_and_external_resources_are_bounded_atomic() {
    let (mut s, id) = session(8);
    for svg in ["garbage", "<svg>", "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"99999\" height=\"99999\"><rect width=\"99999\" height=\"99999\"/></svg>"]
    {
        let params = request(&mut s, id, svg);
        fails_without_edit(&mut s, params);
    }
    let huge = " ".repeat(crate::vector_paste_cmds::MAX_VECTOR_CLIPBOARD_BYTES + 1);
    let params = request(&mut s, id, &huge);
    fails_without_edit(&mut s, params);
    // External images cannot trigger ambient filesystem or network access.
    let external = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="6"><image href="file:///secret.png" width="8" height="6"/></svg>"#;
    let params = request(&mut s, id, external);
    let before = s.active().unwrap().doc.clone();
    match s.execute(UPDATE, params) {
        Err(_) => assert_eq!(s.active().unwrap().doc, before),
        Ok(_) => assert_eq!(smart(&s.active().unwrap().doc, id).unwrap().cache.as_ref().unwrap().pixel(15, 11), vec![0.0; 4]),
    }
}

#[test]
fn stale_hash_content_identity_wrong_parent_deleted_and_locked_reject() {
    let (mut s, id) = session(8);
    let params = request(&mut s, id, BLUE);
    for (key, value) in [("fingerprint", json!("0".repeat(64))), ("contentsId", json!(0)), ("document", json!(DocId::fresh().0)), ("layer", json!("wrong"))] {
        let mut bad = params.clone();
        bad[key] = value;
        fails_without_edit(&mut s, bad);
    }
    let mut missing = params.clone();
    missing.as_object_mut().unwrap().remove("document");
    fails_without_edit(&mut s, missing);
    Arc::make_mut(&mut s.active_mut().unwrap().doc).layer_mut(id).unwrap().locks = Locks { all: true, ..Default::default() };
    fails_without_edit(&mut s, params.clone());
    Arc::make_mut(&mut s.active_mut().unwrap().doc).layer_mut(id).unwrap().locks = Default::default();
    s.execute(UPDATE, params.clone()).unwrap();
    fails_without_edit(&mut s, params.clone());
    Arc::make_mut(&mut s.active_mut().unwrap().doc).remove(id).unwrap();
    fails_without_edit(&mut s, params);
}

#[test]
fn linked_sources_and_internal_editor_conflicts_reject_without_reads() {
    let (mut s, id) = session(8);
    let params = request(&mut s, id, BLUE);
    let parent = s.active().unwrap().doc.id;
    let contents_id = smart(&s.active().unwrap().doc, id).unwrap().contents_id;
    s.smart_links.push(crate::smart_cmds::SmartLink { child: DocId::fresh(), parent, layer: id, contents_id });
    fails_without_edit(&mut s, params.clone());
    assert!(s.execute(EXPORT, json!({"layer":id.0})).is_err());
    s.smart_links.clear();
    let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
    let LayerContent::Smart(sm) = &mut doc.layer_mut(id).unwrap().content else { panic!("smart object") };
    sm.source = SmartSource::Linked { path: "C:/not-authorized/source.svg".into() };
    fails_without_edit(&mut s, params);
    assert!(s.execute(EXPORT, json!({"layer":id.0})).is_err());
}

#[test]
fn inherited_shared_instance_lock_and_unbounded_placement_reject() {
    let (mut s, id) = session(8);
    let params = request(&mut s, id, BLUE);
    {
        let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
        let mut duplicate = doc.layer(id).unwrap().duplicate();
        duplicate.locks.pixels = true;
        doc.layers.push(duplicate);
    }
    fails_without_edit(&mut s, params.clone());
    {
        let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
        doc.layers.pop();
        let LayerContent::Smart(sm) = &mut doc.layer_mut(id).unwrap().content else { panic!("smart object") };
        sm.transform = Affine { m: [10000.0, 0.0, 0.0, 10000.0, 0.0, 0.0] };
    }
    fails_without_edit(&mut s, params);
}

#[test]
fn diverged_shared_source_is_never_overwritten() {
    let (mut s, id) = session(8);
    let params = request(&mut s, id, BLUE);
    let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
    let mut duplicate = doc.layer(id).unwrap().duplicate();
    let LayerContent::Smart(sm) = &mut duplicate.content else { panic!("smart object") };
    sm.source = SmartSource::Embedded { file_name: "Other.svg".into(), bytes: Arc::new(GREEN.as_bytes().to_vec()) };
    doc.layers.push(duplicate);
    fails_without_edit(&mut s, params);
}

#[test]
fn failed_second_instance_render_does_not_commit_first_instance() {
    let (mut s, id) = session(8);
    {
        let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
        let mut duplicate = doc.layer(id).unwrap().duplicate();
        let LayerContent::Smart(sm) = &mut duplicate.content else { panic!("smart object") };
        sm.transform = Affine { m: [100.0, 0.0, 0.0, 0.001, 0.0, 0.0] };
        doc.layers.push(duplicate);
        let LayerContent::Smart(sm) = &mut doc.layer_mut(id).unwrap().content else { panic!("smart object") };
        sm.transform = Affine { m: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0] };
    }
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="2000" height="2000"><rect width="2000" height="2000" fill="blue"/></svg>"#;
    let params = request(&mut s, id, svg);
    fails_without_edit(&mut s, params);
}

#[test]
fn inherited_parent_lock_refuses_content_edits() {
    let (mut s, id) = session(8);
    let params = request(&mut s, id, BLUE);
    let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
    let layer = doc.remove(id).unwrap();
    let mut group = Layer::new("Locked parent", LayerContent::Group(photocraft_doc::Group { children: vec![layer], expanded: true, artboard: None }));
    group.locks.pixels = true;
    doc.layers.push(group);
    fails_without_edit(&mut s, params);
}

#[test]
fn perspective_and_warp_geometry_survive_an_update() {
    for warp in [false, true] {
        let (mut s, id) = session(8);
        let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
        let LayerContent::Smart(sm) = &mut doc.layer_mut(id).unwrap().content else { panic!("smart object") };
        sm.perspective = Some([1.0, 0.0, 12.0, 0.0, 1.0, 9.0, 0.01, 0.0, 1.0]);
        if warp {
            sm.warp = Some(photocraft_geom::warp::Warp::preset(photocraft_geom::warp::WarpStyle::Arc, 10.0, [0.0, 0.0, 8.0, 6.0]));
        }
        let before = sm.clone();
        let params = request(&mut s, id, BLUE);
        s.execute(UPDATE, params).unwrap();
        let after = smart(&s.active().unwrap().doc, id).unwrap();
        assert_eq!(after.warp, before.warp);
        assert_eq!(after.perspective, before.perspective);
        assert_eq!(after.transform, before.transform);
        assert_eq!(source(&s, id), BLUE.as_bytes());
    }
}

#[test]
fn perspective_crossing_zero_is_rejected_before_rendering() {
    let (mut s, id) = session(8);
    let params = request(&mut s, id, BLUE);
    let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
    let LayerContent::Smart(sm) = &mut doc.layer_mut(id).unwrap().content else { panic!("smart object") };
    sm.perspective = Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, -0.5, 0.0, 1.0]);
    fails_without_edit(&mut s, params);
}

#[test]
fn bad_params_are_errors_for_both_commands() {
    let (mut s, _) = session(8);
    for p in [Value::Null, json!([]), json!({"layer":-1}), json!({"layer":"invalid"})] {
        assert!(s.execute(EXPORT, p.clone()).is_err());
        fails_without_edit(&mut s, p);
    }
    fails_without_edit(&mut s, json!({}));
}

/// Embedded PSD link-table vectors are exported in memory, then atomically replaced by SVG.
/// The original payload and shared identities survive undo and native-format save/reload.
#[test]
fn embedded_pdf_ai_and_eps_update_shared_instances_without_reading_linked_paths() {
    for depth in [8, 16, 32] {
        for (name, bytes) in [("Logo.ai", b"%PDF-1.6\nowned synthetic payload".as_slice()), ("Logo.eps", b"%!PS-Adobe-3.0 EPSF-3.0\n".as_slice())] {
            let (mut s, id) = session(depth);
            let duplicate;
            {
                let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
                let payload = photocraft_io::linked::encode_linked_file(&photocraft_io::linked::LinkedFile {
                    uuid: "owned-vector".into(),
                    file_name: name.into(),
                    bytes: bytes.to_vec(),
                });
                doc.metadata.psd_global_blocks.push((*b"8BIM", *b"lnk2", Arc::new(payload)));
                let LayerContent::Smart(sm) = &mut doc.layer_mut(id).unwrap().content else { panic!("smart") };
                sm.source = SmartSource::Linked { path: "owned-vector".into() };
                let layer = doc.layer(id).unwrap().duplicate();
                duplicate = layer.id;
                doc.layers.push(layer);
            }
            let before = s.active().unwrap().doc.clone();
            let exported = s.execute(EXPORT, json!({"layer":id.0})).unwrap();
            assert_eq!(base64::engine::general_purpose::STANDARD.decode(exported["sourceBase64"].as_str().unwrap()).unwrap(), bytes);
            assert_eq!(exported["extension"], if name.ends_with("ai") { "ai" } else { "eps" });
            assert_eq!(s.active().unwrap().doc, before);
            let params =
                json!({"document":exported["document"],"layer":id.0,"svg":BLUE,"fingerprint":exported["fingerprint"],"contentsId":exported["contentsId"]});
            assert_eq!(s.execute(UPDATE, params.clone()).unwrap()["updated"], true);
            for id in [id, duplicate] {
                assert_eq!(source(&s, id), BLUE.as_bytes());
            }
            let after = s.active().unwrap().doc.clone();
            assert!(s.undo());
            assert_eq!(s.active().unwrap().doc, before);
            assert!(s.redo());
            assert_eq!(s.active().unwrap().doc, after);
            fails_without_edit(&mut s, params); // stale first-save fingerprint
            let serialized = photocraft_format::save_to_bytes(&after, &Default::default()).unwrap();
            assert_eq!(photocraft_format::load_from_bytes(&serialized).unwrap(), *after);
        }
    }
}

#[test]
fn vector_handoff_never_reads_a_linked_disk_path_or_accepts_disguised_raster() {
    let (mut s, id) = session(8);
    for source in
        [SmartSource::Linked { path: "/dev/zero".into() }, SmartSource::Embedded { file_name: "Logo.ai".into(), bytes: Arc::new(b"not a vector".to_vec()) }]
    {
        let doc = Arc::make_mut(&mut s.active_mut().unwrap().doc);
        let LayerContent::Smart(sm) = &mut doc.layer_mut(id).unwrap().content else { panic!("smart") };
        sm.source = source;
        let before = s.active().unwrap().doc.clone();
        assert!(s.execute(EXPORT, json!({"layer":id.0})).is_err());
        assert_eq!(s.active().unwrap().doc, before);
    }
}
