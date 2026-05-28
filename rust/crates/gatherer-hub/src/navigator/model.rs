//! Project tree data model — the structural hierarchy Gatherer authors,
//! mirroring the engine's Navigator model (see `rust/docs/NAVIGATOR_PORT.md`).
//!
//! ```text
//! Project
//! └── assets[]              Asset    (uuid, id, meta)
//!     └── pages[]           Page     (uuid, content_uuid, id)
//!         └── modules[]     Module   (uuid, content_uuid, base_name, id)
//!             └── sections[] Section (uuid, kind, name)   [Navigator "role"]
//!                 └── patterns[] Pattern (loop / regions / clip / strip)
//! ```
//!
//! Everything is owned by value (no uid-keyed pool) — lookups traverse.
//! Identity is a real v4 [`Uuid`]; `content_uuid` on Page/Module carries
//! the "duplicate vs clone" distinction (duplicate shares it, clone
//! regenerates it).
//!
//! ## TOML field-ordering discipline
//!
//! `session.toml` is TOML. serde serialises struct fields in declaration
//! order, and TOML forbids a bare value after a table within the same
//! table. So in every struct here, **all scalar fields come first, then
//! sub-tables, then arrays-of-tables** — do not reorder casually. The
//! `roundtrips_through_toml` test guards this.

use crate::session::TakeState;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Number of parallel layers a music section supports (matches the
/// adaptive mixer's `SLOT_COUNT`).
pub const LAYER_COUNT: usize = 8;

/// Asset bundle type. v1 authors only `Music`; the others are reserved
/// so the serialized schema stays stable when we add them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetType {
    Music,
    #[allow(dead_code)]
    Locations,
    #[allow(dead_code)]
    Globals,
    #[allow(dead_code)]
    Sounds,
}

impl Default for AssetType {
    fn default() -> Self {
        AssetType::Music
    }
}

/// Header metadata that feeds the `.ttasset` `meta` block + the
/// `<CODE> - <Variant>_TT/` folder name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssetMeta {
    #[serde(default)]
    pub production_code: String,
    #[serde(default)]
    pub variant_name: String,
    #[serde(default)]
    pub asset_name: String,
    #[serde(default)]
    pub asset_type: AssetType,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SectionKind {
    Intro,
    Main,
    Outro,
}

impl SectionKind {
    /// Canonical order Intro < Main < Outro (used by `normalize` +
    /// section-insert positioning).
    pub const ORDER: [SectionKind; 3] = [SectionKind::Intro, SectionKind::Main, SectionKind::Outro];

    pub fn slug(self) -> &'static str {
        match self {
            SectionKind::Intro => "intro",
            SectionKind::Main => "main",
            SectionKind::Outro => "outro",
        }
    }

    /// Pattern-id prefix in `.wlamodel` clip names (`i1`, `m1`, `o1`).
    pub fn pattern_letter(self) -> char {
        match self {
            SectionKind::Intro => 'i',
            SectionKind::Main => 'm',
            SectionKind::Outro => 'o',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SectionKind::Intro => "Intro",
            SectionKind::Main => "Main",
            SectionKind::Outro => "Outro",
        }
    }

    /// Index in the canonical order (for sorting).
    pub fn order_index(self) -> usize {
        match self {
            SectionKind::Intro => 0,
            SectionKind::Main => 1,
            SectionKind::Outro => 2,
        }
    }
}

/// One fade window — see the full transition spec on the original
/// `Region` doc (preserved below). Units are **sample frames**.
///
/// Sync point: an in-region's is `sync_frames` (engine-explicit; on a
/// fresh region we initialise it to `end_frames`), an out-region's is
/// likewise `sync_frames` (initialised to `begin_frames`). At the sync
/// instant both clips are at 100%; `fade_pct × (end−begin)` is the
/// engine fade duration anchored at the sync point (`0.0` ⇒ no engine
/// fade, trust the baked audio). `fade_shape ∈ [0,1]` curves the slope.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub begin_frames: u64,
    pub end_frames: u64,
    pub sync_frames: u64,
    pub fade_pct: f32,
    pub fade_shape: f32,
    #[serde(default)]
    pub group: u32,
}

impl Region {
    /// In-region with the sync point at the end (the default — B is
    /// fully faded in by `end_frames`).
    pub fn new_in(begin_frames: u64, end_frames: u64, fade_pct: f32, fade_shape: f32) -> Self {
        Self {
            begin_frames,
            end_frames,
            sync_frames: end_frames,
            fade_pct,
            fade_shape,
            group: 0,
        }
    }

    /// Out-region with the sync point at the start (the default — A
    /// begins fading out at `begin_frames`).
    pub fn new_out(begin_frames: u64, end_frames: u64, fade_pct: f32, fade_shape: f32) -> Self {
        Self {
            begin_frames,
            end_frames,
            sync_frames: begin_frames,
            fade_pct,
            fade_shape,
            group: 0,
        }
    }

    /// Base region length (frames).
    fn span(&self) -> f32 {
        self.end_frames.saturating_sub(self.begin_frames) as f32
    }

    /// Pixel/frame span of the **drawn fade ramp** when used as an
    /// in-region (fade-in): `[begin, begin + fade_pct·span]`. The ramp
    /// extends past `end` (the sync point) when `fade_pct > 1`.
    pub fn fade_span_in(&self) -> (u64, u64) {
        let len = (self.fade_pct * self.span()).max(1.0) as u64;
        (self.begin_frames, self.begin_frames + len)
    }

    /// Frame span of the fade ramp when used as an out-region
    /// (fade-out): `[end − fade_pct·span, end]`. Extends before `begin`
    /// (the sync point) when `fade_pct > 1`.
    pub fn fade_span_out(&self) -> (u64, u64) {
        let len = (self.fade_pct * self.span()).max(1.0) as u64;
        (self.end_frames.saturating_sub(len), self.end_frames)
    }

    /// Gain (0..1) at local `frame` when this region is an **in-region**.
    /// Sync point is `end`; the fade rises from `begin` over
    /// `fade_pct·span` frames (so for `fade_pct = 1` it reaches 1.0 at
    /// the sync). `fade_pct = 0` ⇒ full gain throughout (pre-faded audio).
    pub fn gain_as_in(&self, frame: u64) -> f32 {
        if self.fade_pct <= 0.0 {
            return 1.0;
        }
        let (s, e) = self.fade_span_in();
        if frame <= s {
            0.0
        } else if frame >= e {
            1.0
        } else {
            fade_shape_gain((frame - s) as f32 / (e - s) as f32, self.fade_shape)
        }
    }

    /// Gain (0..1) at local `frame` when this region is an **out-region**.
    /// Sync point is `begin`; the fade falls to 0 at `end`, starting
    /// `fade_pct·span` frames before it. `fade_pct = 0` ⇒ full gain
    /// throughout (pre-faded audio).
    pub fn gain_as_out(&self, frame: u64) -> f32 {
        if self.fade_pct <= 0.0 {
            return 1.0;
        }
        let (s, e) = self.fade_span_out();
        if frame <= s {
            1.0
        } else if frame >= e {
            0.0
        } else {
            1.0 - fade_shape_gain((frame - s) as f32 / (e - s) as f32, self.fade_shape)
        }
    }
}

/// Shape a normalised 0..1 fade position. `fade_shape ≈ 0` →
/// exponential (convex, slow start), `0.5` → linear, `≈ 1` →
/// logarithmic (concave, fast start). Implemented as `x^k` with
/// `k = 2^((0.5 − shape)·4)` (shape 0 → k=4, 0.5 → k=1, 1 → k=0.25).
pub fn fade_shape_gain(x: f32, fade_shape: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    let k = 2f32.powf((0.5 - fade_shape) * 4.0);
    x.powf(k)
}

/// Stereo channel-strip values (matches Atlas `.wlamodel`
/// `channelStrip`). Pan 0.5 = centre, width 1.0 = full.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ChannelStrip {
    pub volume: f32,
    pub pan: f32,
    pub width: f32,
}

impl Default for ChannelStrip {
    fn default() -> Self {
        Self {
            volume: 1.0,
            pan: 0.5,
            width: 1.0,
        }
    }
}

/// Where a pattern's audio comes from. `path` is resolved against the
/// session/asset root; `take` carries the recording metadata when the
/// clip was captured by Gatherer's recorder.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClipSource {
    #[serde(default)]
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<TakeState>,
}

/// One pattern inside a section — the unit that carries a clip + loop +
/// transition regions. Scalars first, then sub-tables, then
/// arrays-of-tables (TOML ordering discipline).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub uuid: Uuid,
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub ref_edit: bool,
    #[serde(default)]
    pub loop_start: u64,
    #[serde(default)]
    pub loop_end: u64,
    #[serde(default = "default_true")]
    pub looping: bool,
    #[serde(default)]
    pub xfade: u64,
    #[serde(default)]
    pub xoffset: i64,
    #[serde(default)]
    pub channel_strip: ChannelStrip,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_source: Option<ClipSource>,
    #[serde(default)]
    pub in_regions: Vec<Region>,
    #[serde(default)]
    pub out_regions: Vec<Region>,
}

fn default_true() -> bool {
    true
}

impl Pattern {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            uuid: Uuid::new_v4(),
            id: id.into(),
            enabled: true,
            ref_edit: false,
            loop_start: 0,
            loop_end: 0,
            looping: true,
            xfade: 0,
            xoffset: 0,
            channel_strip: ChannelStrip::default(),
            clip_source: None,
            in_regions: Vec::new(),
            out_regions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub uuid: Uuid,
    pub kind: SectionKind,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub patterns: Vec<Pattern>,
}

impl Section {
    pub fn new(kind: SectionKind) -> Self {
        Self {
            uuid: Uuid::new_v4(),
            kind,
            name: String::new(),
            patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Module {
    pub uuid: Uuid,
    pub content_uuid: Uuid,
    #[serde(default)]
    pub base_name: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub sections: Vec<Section>,
}

impl Module {
    pub fn new(id: impl Into<String>) -> Self {
        let uuid = Uuid::new_v4();
        Self {
            uuid,
            content_uuid: uuid,
            base_name: "Module".to_string(),
            id: id.into(),
            sections: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub uuid: Uuid,
    pub content_uuid: Uuid,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub modules: Vec<Module>,
}

impl Page {
    pub fn new(id: impl Into<String>) -> Self {
        let uuid = Uuid::new_v4();
        Self {
            uuid,
            content_uuid: uuid,
            id: id.into(),
            modules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub uuid: Uuid,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub meta: AssetMeta,
    #[serde(default)]
    pub pages: Vec<Page>,
}

impl Asset {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            uuid: Uuid::new_v4(),
            id: id.into(),
            meta: AssetMeta::default(),
            pages: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Project {
    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    /// A fresh project: one Asset → one Page → one Module → one Main
    /// section. Mirrors Navigator's `resetToDefault`.
    pub fn reset_to_default() -> Self {
        let mut p = Project::default();
        let mut asset = Asset::new("Asset 1");
        let mut page = Page::new("Page 1");
        let mut module = Module::new("Module 1");
        module.sections.push(Section::new(SectionKind::Main));
        page.modules.push(module);
        asset.pages.push(page);
        p.assets.push(asset);
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_project_shape() {
        let p = Project::reset_to_default();
        assert_eq!(p.assets.len(), 1);
        assert_eq!(p.assets[0].pages.len(), 1);
        assert_eq!(p.assets[0].pages[0].modules.len(), 1);
        let sections = &p.assets[0].pages[0].modules[0].sections;
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].kind, SectionKind::Main);
    }

    #[test]
    fn module_new_self_references_content_uuid() {
        // Duplicate-vs-clone hinges on this: a fresh module's
        // content_uuid equals its own uuid until a duplicate shares it.
        let m = Module::new("M");
        assert_eq!(m.uuid, m.content_uuid);
    }

    #[test]
    fn roundtrips_through_toml() {
        // Guards the TOML field-ordering discipline: build a fully
        // populated tree (every struct has its table + array fields
        // exercised) and prove it serialises + parses back.
        let mut proj = Project::reset_to_default();
        let asset = &mut proj.assets[0];
        asset.meta.production_code = "ANDD02".into();
        asset.meta.variant_name = "Atlas".into();
        let module = &mut asset.pages[0].modules[0];
        // Add intro + outro around the default main, out of order to
        // exercise normalisation later (here just structural).
        module.sections.insert(0, Section::new(SectionKind::Intro));
        module.sections.push(Section::new(SectionKind::Outro));
        // Put a fully-populated pattern in the Main section.
        let mut pat = Pattern::new("Pattern 1");
        pat.loop_start = 812_571;
        pat.loop_end = 7_123_347;
        pat.clip_source = Some(ClipSource {
            path: PathBuf::from("assets/ANDD02/recording/main/source-01.wav"),
            take: None,
        });
        pat.in_regions.push(Region::new_in(6_274_286, 6_315_428, 1.0, 0.433));
        pat.out_regions
            .push(Region::new_out(829_487, 863_971, 1.0, 0.756));
        module.sections[1].patterns.push(pat);

        let toml_str = toml::to_string_pretty(&proj)
            .expect("project must serialise to TOML (field-ordering discipline)");
        let back: Project = toml::from_str(&toml_str).expect("must parse back");
        assert_eq!(back.assets[0].meta.production_code, "ANDD02");
        let main = &back.assets[0].pages[0].modules[0].sections[1];
        assert_eq!(main.kind, SectionKind::Main);
        assert_eq!(main.patterns.len(), 1);
        assert_eq!(main.patterns[0].loop_end, 7_123_347);
        assert_eq!(main.patterns[0].in_regions[0].sync_frames, 6_315_428);
        assert_eq!(main.patterns[0].out_regions[0].sync_frames, 829_487);
    }
}
