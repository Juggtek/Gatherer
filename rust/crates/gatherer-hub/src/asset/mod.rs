//! Asset bundle I/O — `.ttasset`, `.wlamodel`, `.wlabank` writers and readers.
//! See `rust/docs/NAVIGATOR_PORT.md` for the bundle format spec.
#![allow(dead_code)]

pub mod ogg;
pub mod ttasset;
pub mod wlabank;
pub mod wlamodel;

/// Phase F acceptance tests: import the Atlas bundle, re-export, and
/// verify round-trip fidelity. These hit the Atlas drive directly and
/// skip gracefully when it isn't mounted. (Kept as unit tests because
/// the crate is binary-only — no lib target for `tests/`.)
#[cfg(test)]
mod acceptance {
    use crate::adaptive::AdaptiveMixer;
    use crate::asset::{ttasset, wlabank, wlamodel};
    use crate::navigator::model::AssetMeta;
    use crate::navigator::SectionKind;
    use std::path::{Path, PathBuf};

    const ATLAS_DIR: &str =
        "/Volumes/Plottn/GREENLOBSTER/COLLECTION/Integrated Assets/Music/ANDD02 - Atlas_TT";

    fn atlas_path(ext: &str) -> PathBuf {
        Path::new(ATLAS_DIR).join(format!("ANDD02.{ext}"))
    }
    fn atlas_available() -> bool {
        atlas_path("ttasset").exists()
    }

    #[test]
    fn ttasset_parse_emit_semantically_equal() {
        if !atlas_available() {
            return;
        }
        let mut mixer = AdaptiveMixer::new();
        let mut meta = AssetMeta::default();
        let (music_id, page_id) =
            ttasset::read(&atlas_path("ttasset"), &mut mixer, &mut meta).unwrap();

        let tmp =
            std::env::temp_dir().join(format!("ANDD02-rt-{}.ttasset", std::process::id()));
        let json = ttasset::to_json(&mixer, &meta, &music_id, &page_id);
        std::fs::write(&tmp, &json).unwrap();

        let mut mixer2 = AdaptiveMixer::new();
        let mut meta2 = AssetMeta::default();
        let (id2, pid2) = ttasset::read(&tmp, &mut mixer2, &mut meta2).unwrap();
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(id2, music_id, "music uuid preserved");
        assert_eq!(pid2, page_id, "page uuid preserved");
        assert_eq!(meta2.production_code, "ANDD02");
        for s in 0..8 {
            let (a, b) = (mixer.slot_params[s], mixer2.slot_params[s]);
            assert_eq!(a.formula, b.formula, "layer {s} formula");
            assert!((a.steepness - b.steepness).abs() < 1e-4);
            assert!((a.minimum - b.minimum).abs() < 1e-4);
            assert!((a.maximum - b.maximum).abs() < 1e-4);
            for m in 0..3 {
                assert!((mixer.mood_weight[m][s] - mixer2.mood_weight[m][s]).abs() < 1e-4);
                assert!((mixer.balancer_mask[m][s] - mixer2.balancer_mask[m][s]).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn wlamodel_parse_emit_section_regions_preserved() {
        if !atlas_available() {
            return;
        }
        let (_m, _p, sections) = wlamodel::read(&atlas_path("wlamodel")).unwrap();
        let main = sections.iter().find(|s| s.kind == SectionKind::Main).unwrap();
        assert_eq!(main.out_regions.len(), 38, "Atlas main: 38 outRegions");

        let dir = std::env::temp_dir().join(format!("wlamodel-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let specs: Vec<wlamodel::SectionSpec<'_>> = sections
            .iter()
            .map(|s| wlamodel::SectionSpec {
                kind: s.kind,
                pattern_id: &s.pattern_id,
                len_frames: s.len_frames,
                bank_name: "ANDD02.wlabank",
                in_regions: &s.in_regions,
                out_regions: &s.out_regions,
                layer_ids: s.layer_ids.clone(),
            })
            .collect();
        wlamodel::write(&dir, "ANDD02", "mod-uuid", "page-uuid", &specs, 48_000).unwrap();
        let (_, _, sections2) = wlamodel::read(&dir.join("ANDD02.wlamodel")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let main2 = sections2.iter().find(|s| s.kind == SectionKind::Main).unwrap();
        assert_eq!(main2.out_regions.len(), 38);
        assert_eq!(main.out_regions[0].begin_frames, main2.out_regions[0].begin_frames);
        assert_eq!(main.out_regions[0].end_frames, main2.out_regions[0].end_frames);
    }

    #[test]
    fn wlabank_payload_byte_identical_after_import_export() {
        if !atlas_available() {
            return;
        }
        let original = wlabank::read(&atlas_path("wlabank")).unwrap();
        let clips: Vec<wlabank::BankClip> = original
            .iter()
            .map(|c| wlabank::BankClip {
                clip_name: c.clip_name.clone(),
                channels: c.channels,
                sample_rate: c.sample_rate,
                frame_count: c.frame_count,
                ogg: c.ogg.clone().unwrap_or_default(),
            })
            .collect();
        let repacked = wlabank::encode(&clips);
        let reparsed = wlabank::parse(&repacked).unwrap();
        assert_eq!(reparsed.len(), original.len(), "clip count preserved");
        for (i, (orig, re)) in original.iter().zip(&reparsed).enumerate() {
            assert_eq!(re.clip_name, orig.clip_name, "clip {i} name");
            assert_eq!(re.frame_count, orig.frame_count, "clip {i} frames");
            assert_eq!(
                re.ogg.as_deref(),
                orig.ogg.as_deref(),
                "clip {i} ({}) Ogg payload must be byte-identical",
                orig.clip_name
            );
        }
    }
}
