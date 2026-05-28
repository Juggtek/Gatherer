//! Structural mutations over the [`Project`] tree — the edit API the
//! Navigator UI drives. All operations are methods on `Project`,
//! uuid-addressed (the UI holds uuids, not indices), and they preserve
//! the model invariants: canonical section order (Intro < Main < Outro)
//! and non-empty parents (≥1 page/module/section).
//!
//! Clipboard copy/paste and cross-parent drag-moves are deferred to the
//! UI phase (B1) where they're actually invoked.

use super::model::{Asset, Module, Page, Pattern, Project, Region, Section, SectionKind};
use uuid::Uuid;

impl Project {
    // ── normalization ───────────────────────────────────────────────

    /// Stable-sort a module's sections into canonical Intro < Main <
    /// Outro order. Called after every section add/move.
    fn normalize_module_sections(sections: &mut [Section]) {
        sections.sort_by_key(|s| s.kind.order_index());
    }

    /// Re-sort sections in every module (cheap; call after bulk edits).
    pub fn normalize(&mut self) {
        for a in &mut self.assets {
            for p in &mut a.pages {
                for m in &mut p.modules {
                    Self::normalize_module_sections(&mut m.sections);
                }
            }
        }
    }

    /// Display label for a section, computed on the fly: "Main",
    /// "Main 2", … numbering same-kind siblings 1-based (the "1" is
    /// dropped when it's the only one of its kind). Replaces
    /// Navigator's stored `displayType`.
    pub fn section_display_label(&self, section_uuid: Uuid) -> String {
        let Some(path) = self.find_section(section_uuid) else {
            return String::new();
        };
        let Some(module) = self.module(super::queries::ModulePath {
            asset: path.asset,
            page: path.page,
            module: path.module,
        }) else {
            return String::new();
        };
        let kind = module.sections[path.section].kind;
        let same: Vec<usize> = module
            .sections
            .iter()
            .enumerate()
            .filter(|(_, s)| s.kind == kind)
            .map(|(i, _)| i)
            .collect();
        let label = kind.label();
        if same.len() <= 1 {
            label.to_string()
        } else {
            let ordinal = same.iter().position(|&i| i == path.section).unwrap_or(0) + 1;
            format!("{label} {ordinal}")
        }
    }

    // ── add ──────────────────────────────────────────────────────────

    /// New asset seeded with one page / one module / one Main section.
    /// Returns its uuid.
    pub fn add_asset(&mut self) -> Uuid {
        let n = self.assets.len() + 1;
        let mut asset = Asset::new(format!("Asset {n}"));
        let mut page = Page::new("Page 1");
        let mut module = Module::new("Module 1");
        module.sections.push(Section::new(SectionKind::Main));
        page.modules.push(module);
        asset.pages.push(page);
        let uuid = asset.uuid;
        self.assets.push(asset);
        uuid
    }

    /// New page (seeded with one module + one Main section) appended to
    /// the asset. Returns the page uuid.
    pub fn add_page(&mut self, asset_uuid: Uuid) -> Option<Uuid> {
        let p = self.find_asset(asset_uuid)?;
        let asset = self.assets.get_mut(p.asset)?;
        let n = asset.pages.len() + 1;
        let mut page = Page::new(format!("Page {n}"));
        let mut module = Module::new("Module 1");
        module.sections.push(Section::new(SectionKind::Main));
        page.modules.push(module);
        let uuid = page.uuid;
        asset.pages.push(page);
        Some(uuid)
    }

    /// New module (seeded with one Main section) inserted right after
    /// the given module. Returns the new module uuid.
    pub fn add_module_after(&mut self, module_uuid: Uuid) -> Option<Uuid> {
        let loc = self.find_module(module_uuid)?;
        let page = self.page_mut(super::queries::PagePath {
            asset: loc.asset,
            page: loc.page,
        })?;
        let n = page.modules.len() + 1;
        let mut module = Module::new(format!("Module {n}"));
        module.sections.push(Section::new(SectionKind::Main));
        let uuid = module.uuid;
        page.modules.insert(loc.module + 1, module);
        Some(uuid)
    }

    /// Add a section of `kind` to a module, inserted in canonical order.
    /// Returns the new section uuid.
    pub fn add_section(&mut self, module_uuid: Uuid, kind: SectionKind) -> Option<Uuid> {
        let loc = self.find_module(module_uuid)?;
        let module = self.module_mut(super::queries::ModulePath {
            asset: loc.asset,
            page: loc.page,
            module: loc.module,
        })?;
        let section = Section::new(kind);
        let uuid = section.uuid;
        module.sections.push(section);
        Self::normalize_module_sections(&mut module.sections);
        Some(uuid)
    }

    /// Append an empty pattern to a section. Returns its uuid.
    pub fn add_pattern(&mut self, section_uuid: Uuid) -> Option<Uuid> {
        let section = self.find_section_mut(section_uuid)?;
        let n = section.patterns.len() + 1;
        let pat = Pattern::new(format!("Pattern {n}"));
        let uuid = pat.uuid;
        section.patterns.push(pat);
        Some(uuid)
    }

    // ── delete (guarded; returns the uuids actually removed) ─────────

    pub fn delete_asset(&mut self, uuid: Uuid) -> Vec<Uuid> {
        if !self.can_delete_asset(uuid) {
            return Vec::new();
        }
        let Some(p) = self.find_asset(uuid) else {
            return Vec::new();
        };
        let removed = collect_asset_uuids(&self.assets[p.asset]);
        self.assets.remove(p.asset);
        removed
    }

    pub fn delete_page(&mut self, uuid: Uuid) -> Vec<Uuid> {
        if !self.can_delete_page(uuid) {
            return Vec::new();
        }
        let Some(p) = self.find_page(uuid) else {
            return Vec::new();
        };
        let asset = &mut self.assets[p.asset];
        let removed = collect_page_uuids(&asset.pages[p.page]);
        asset.pages.remove(p.page);
        removed
    }

    pub fn delete_module(&mut self, uuid: Uuid) -> Vec<Uuid> {
        if !self.can_delete_module(uuid) {
            return Vec::new();
        }
        let Some(p) = self.find_module(uuid) else {
            return Vec::new();
        };
        let page = &mut self.assets[p.asset].pages[p.page];
        let removed = collect_module_uuids(&page.modules[p.module]);
        page.modules.remove(p.module);
        removed
    }

    pub fn delete_section(&mut self, uuid: Uuid) -> Vec<Uuid> {
        if !self.can_delete_section(uuid) {
            return Vec::new();
        }
        let Some(p) = self.find_section(uuid) else {
            return Vec::new();
        };
        let module = &mut self.assets[p.asset].pages[p.page].modules[p.module];
        let removed = collect_section_uuids(&module.sections[p.section]);
        module.sections.remove(p.section);
        removed
    }

    pub fn delete_pattern(&mut self, uuid: Uuid) -> bool {
        let Some(p) = self.find_pattern_path(uuid) else {
            return false;
        };
        self.assets[p.asset].pages[p.page].modules[p.module].sections[p.section]
            .patterns
            .remove(p.pattern);
        true
    }

    // ── duplicate (keep content_uuid) vs clone (regenerate) ─────────

    /// Duplicate a section in place (right after the original). New node
    /// uuids, patterns get fresh uuids. Sections carry no content_uuid,
    /// so duplicate and clone differ only at module/page level — here we
    /// just deep-copy with fresh uuids.
    pub fn duplicate_section_right(&mut self, uuid: Uuid) -> Option<Uuid> {
        let p = self.find_section(uuid)?;
        let module = self.module_mut(super::queries::ModulePath {
            asset: p.asset,
            page: p.page,
            module: p.module,
        })?;
        let mut copy = module.sections[p.section].clone();
        freshen_section(&mut copy);
        let new_uuid = copy.uuid;
        module.sections.insert(p.section + 1, copy);
        Self::normalize_module_sections(&mut module.sections);
        Some(new_uuid)
    }

    /// Duplicate a module in place. Keeps `content_uuid` (engine treats
    /// the copy as the same logical content). Returns the new uuid.
    pub fn duplicate_module_right(&mut self, uuid: Uuid) -> Option<Uuid> {
        let p = self.find_module(uuid)?;
        let page = self.page_mut(super::queries::PagePath {
            asset: p.asset,
            page: p.page,
        })?;
        let mut copy = page.modules[p.module].clone();
        freshen_module(&mut copy, false); // keep content_uuid
        let new_uuid = copy.uuid;
        page.modules.insert(p.module + 1, copy);
        Some(new_uuid)
    }

    /// Clone a module in place. Regenerates `content_uuid` (independent
    /// content) and appends " Copy" to the id.
    pub fn clone_module_right(&mut self, uuid: Uuid) -> Option<Uuid> {
        let p = self.find_module(uuid)?;
        let page = self.page_mut(super::queries::PagePath {
            asset: p.asset,
            page: p.page,
        })?;
        let mut copy = page.modules[p.module].clone();
        freshen_module(&mut copy, true); // regenerate content_uuid
        copy.id = format!("{} Copy", copy.id);
        let new_uuid = copy.uuid;
        page.modules.insert(p.module + 1, copy);
        Some(new_uuid)
    }

    /// Duplicate a page in place — keeps every `content_uuid`.
    pub fn duplicate_page_right(&mut self, uuid: Uuid) -> Option<Uuid> {
        let p = self.find_page(uuid)?;
        let asset = self.assets.get_mut(p.asset)?;
        let mut copy = asset.pages[p.page].clone();
        freshen_page(&mut copy, false);
        let new_uuid = copy.uuid;
        asset.pages.insert(p.page + 1, copy);
        Some(new_uuid)
    }

    /// Clone a page in place — regenerates every `content_uuid`,
    /// appends " Copy".
    pub fn clone_page_right(&mut self, uuid: Uuid) -> Option<Uuid> {
        let p = self.find_page(uuid)?;
        let asset = self.assets.get_mut(p.asset)?;
        let mut copy = asset.pages[p.page].clone();
        freshen_page(&mut copy, true);
        copy.id = format!("{} Copy", copy.id);
        let new_uuid = copy.uuid;
        asset.pages.insert(p.page + 1, copy);
        Some(new_uuid)
    }

    /// Clone a single pattern in place (fresh uuid, " Copy" suffix).
    pub fn clone_pattern(&mut self, uuid: Uuid) -> Option<Uuid> {
        let p = self.find_pattern_path(uuid)?;
        let section = self.section_mut(super::queries::SectionPath {
            asset: p.asset,
            page: p.page,
            module: p.module,
            section: p.section,
        })?;
        let mut copy = section.patterns[p.pattern].clone();
        copy.uuid = Uuid::new_v4();
        copy.id = format!("{} Copy", copy.id);
        let new_uuid = copy.uuid;
        section.patterns.insert(p.pattern + 1, copy);
        Some(new_uuid)
    }

    // ── pattern properties ──────────────────────────────────────────

    pub fn set_pattern_enabled(&mut self, uuid: Uuid, enabled: bool) {
        if let Some(p) = self.find_pattern_mut(uuid) {
            p.enabled = enabled;
        }
    }
    pub fn toggle_pattern_enabled(&mut self, uuid: Uuid) {
        if let Some(p) = self.find_pattern_mut(uuid) {
            p.enabled = !p.enabled;
        }
    }
    pub fn rename_pattern(&mut self, uuid: Uuid, name: impl Into<String>) {
        if let Some(p) = self.find_pattern_mut(uuid) {
            let name = name.into();
            p.id = if name.trim().is_empty() {
                "Pattern".to_string()
            } else {
                name
            };
        }
    }
    pub fn set_loop(
        &mut self,
        uuid: Uuid,
        start: u64,
        end: u64,
        looping: bool,
        xfade: u64,
        xoffset: i64,
    ) {
        if let Some(p) = self.find_pattern_mut(uuid) {
            p.loop_start = start;
            p.loop_end = end;
            p.looping = looping;
            p.xfade = xfade;
            p.xoffset = xoffset;
        }
    }

    // ── regions ──────────────────────────────────────────────────────

    pub fn add_in_region(&mut self, pattern_uuid: Uuid, region: Region) {
        if let Some(p) = self.find_pattern_mut(pattern_uuid) {
            p.in_regions.push(region);
        }
    }
    pub fn add_out_region(&mut self, pattern_uuid: Uuid, region: Region) {
        if let Some(p) = self.find_pattern_mut(pattern_uuid) {
            p.out_regions.push(region);
        }
    }
    pub fn remove_in_region(&mut self, pattern_uuid: Uuid, index: usize) {
        if let Some(p) = self.find_pattern_mut(pattern_uuid) {
            if index < p.in_regions.len() {
                p.in_regions.remove(index);
            }
        }
    }
    pub fn remove_out_region(&mut self, pattern_uuid: Uuid, index: usize) {
        if let Some(p) = self.find_pattern_mut(pattern_uuid) {
            if index < p.out_regions.len() {
                p.out_regions.remove(index);
            }
        }
    }

    // ── ref-edit toggle (per-pattern) ───────────────────────────────

    pub fn set_ref_edit(&mut self, pattern_uuid: Uuid, on: bool) {
        if let Some(p) = self.find_pattern_mut(pattern_uuid) {
            p.ref_edit = on;
        }
    }
    pub fn toggle_ref_edit(&mut self, pattern_uuid: Uuid) {
        if let Some(p) = self.find_pattern_mut(pattern_uuid) {
            p.ref_edit = !p.ref_edit;
        }
    }
}

// ── uuid regeneration helpers ────────────────────────────────────────

fn freshen_pattern(p: &mut Pattern) {
    p.uuid = Uuid::new_v4();
}

fn freshen_section(s: &mut Section) {
    s.uuid = Uuid::new_v4();
    for p in &mut s.patterns {
        freshen_pattern(p);
    }
}

/// `regen_content = false` ⇒ duplicate (keep content_uuid);
/// `true` ⇒ clone (new content_uuid).
fn freshen_module(m: &mut Module, regen_content: bool) {
    m.uuid = Uuid::new_v4();
    if regen_content {
        m.content_uuid = m.uuid;
    }
    for s in &mut m.sections {
        freshen_section(s);
    }
}

fn freshen_page(pg: &mut Page, regen_content: bool) {
    pg.uuid = Uuid::new_v4();
    if regen_content {
        pg.content_uuid = pg.uuid;
    }
    for m in &mut pg.modules {
        freshen_module(m, regen_content);
    }
}

// ── subtree uuid collectors (for delete reporting) ───────────────────

fn collect_section_uuids(s: &Section) -> Vec<Uuid> {
    let mut out = vec![s.uuid];
    out.extend(s.patterns.iter().map(|p| p.uuid));
    out
}
fn collect_module_uuids(m: &Module) -> Vec<Uuid> {
    let mut out = vec![m.uuid];
    for s in &m.sections {
        out.extend(collect_section_uuids(s));
    }
    out
}
fn collect_page_uuids(p: &Page) -> Vec<Uuid> {
    let mut out = vec![p.uuid];
    for m in &p.modules {
        out.extend(collect_module_uuids(m));
    }
    out
}
fn collect_asset_uuids(a: &Asset) -> Vec<Uuid> {
    let mut out = vec![a.uuid];
    for p in &a.pages {
        out.extend(collect_page_uuids(p));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_module_project() -> (Project, Uuid) {
        let p = Project::reset_to_default();
        let module_uuid = p.assets[0].pages[0].modules[0].uuid;
        (p, module_uuid)
    }

    #[test]
    fn add_section_keeps_canonical_order() {
        let (mut proj, module) = one_module_project();
        // default has a Main; add Outro then Intro — should reorder to
        // Intro, Main, Outro.
        proj.add_section(module, SectionKind::Outro);
        proj.add_section(module, SectionKind::Intro);
        let kinds: Vec<_> = proj.assets[0].pages[0].modules[0]
            .sections
            .iter()
            .map(|s| s.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![SectionKind::Intro, SectionKind::Main, SectionKind::Outro]
        );
    }

    #[test]
    fn cannot_delete_last_child() {
        let (mut proj, module) = one_module_project();
        let only_section = proj.assets[0].pages[0].modules[0].sections[0].uuid;
        assert!(proj.delete_section(only_section).is_empty());
        assert_eq!(proj.assets[0].pages[0].modules[0].sections.len(), 1);

        let only_module = module;
        assert!(proj.delete_module(only_module).is_empty());
        let only_page = proj.assets[0].pages[0].uuid;
        assert!(proj.delete_page(only_page).is_empty());
        let only_asset = proj.assets[0].uuid;
        assert!(proj.delete_asset(only_asset).is_empty());
    }

    #[test]
    fn delete_section_after_adding_a_second_works() {
        let (mut proj, module) = one_module_project();
        let intro = proj.add_section(module, SectionKind::Intro).unwrap();
        let removed = proj.delete_section(intro);
        assert_eq!(removed, vec![intro]);
        assert_eq!(proj.assets[0].pages[0].modules[0].sections.len(), 1);
    }

    #[test]
    fn duplicate_module_keeps_content_uuid_clone_regenerates() {
        let (mut proj, module) = one_module_project();
        let orig_content = proj.assets[0].pages[0].modules[0].content_uuid;

        let dup = proj.duplicate_module_right(module).unwrap();
        let dup_content = proj.find_module(dup).unwrap();
        let dup_content_uuid = proj.assets[0].pages[0].modules
            [dup_content.module]
            .content_uuid;
        assert_eq!(
            dup_content_uuid, orig_content,
            "duplicate shares content_uuid"
        );
        assert_ne!(dup, module, "duplicate gets a fresh node uuid");

        let cln = proj.clone_module_right(module).unwrap();
        let cln_path = proj.find_module(cln).unwrap();
        let cln_content_uuid =
            proj.assets[0].pages[0].modules[cln_path.module].content_uuid;
        assert_ne!(
            cln_content_uuid, orig_content,
            "clone regenerates content_uuid"
        );
    }

    #[test]
    fn clone_pattern_gives_fresh_uuid() {
        let (mut proj, _module) = one_module_project();
        let section = proj.assets[0].pages[0].modules[0].sections[0].uuid;
        let pat = proj.add_pattern(section).unwrap();
        let cloned = proj.clone_pattern(pat).unwrap();
        assert_ne!(cloned, pat);
        assert_eq!(
            proj.assets[0].pages[0].modules[0].sections[0].patterns.len(),
            2
        );
    }

    #[test]
    fn section_display_label_numbers_duplicates() {
        let (mut proj, module) = one_module_project();
        // one Main → "Main"
        let main = proj.assets[0].pages[0].modules[0].sections[0].uuid;
        assert_eq!(proj.section_display_label(main), "Main");
        // add a second Main → "Main 1" / "Main 2"
        let main2 = proj.add_section(module, SectionKind::Main).unwrap();
        assert_eq!(proj.section_display_label(main), "Main 1");
        assert_eq!(proj.section_display_label(main2), "Main 2");
    }
}
