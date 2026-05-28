//! Read-only traversal over the [`Project`] tree: locate nodes by uuid,
//! fetch them by index path, collect patterns, and answer the
//! "can I delete this?" guards. Mutations live in `ops.rs` and build on
//! these. Index-path structs are the lingua franca — a uuid lookup
//! returns a path, and the path resolves to `&`/`&mut` refs.

use super::model::{Asset, Module, Page, Pattern, Project, Section};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetPath {
    pub asset: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagePath {
    pub asset: usize,
    pub page: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModulePath {
    pub asset: usize,
    pub page: usize,
    pub module: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionPath {
    pub asset: usize,
    pub page: usize,
    pub module: usize,
    pub section: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternPath {
    pub asset: usize,
    pub page: usize,
    pub module: usize,
    pub section: usize,
    pub pattern: usize,
}

impl Project {
    // ── uuid → index path ──────────────────────────────────────────

    pub fn find_asset(&self, uuid: Uuid) -> Option<AssetPath> {
        self.assets
            .iter()
            .position(|a| a.uuid == uuid)
            .map(|asset| AssetPath { asset })
    }

    pub fn find_page(&self, uuid: Uuid) -> Option<PagePath> {
        for (asset, a) in self.assets.iter().enumerate() {
            if let Some(page) = a.pages.iter().position(|p| p.uuid == uuid) {
                return Some(PagePath { asset, page });
            }
        }
        None
    }

    pub fn find_module(&self, uuid: Uuid) -> Option<ModulePath> {
        for (asset, a) in self.assets.iter().enumerate() {
            for (page, p) in a.pages.iter().enumerate() {
                if let Some(module) = p.modules.iter().position(|m| m.uuid == uuid) {
                    return Some(ModulePath {
                        asset,
                        page,
                        module,
                    });
                }
            }
        }
        None
    }

    pub fn find_section(&self, uuid: Uuid) -> Option<SectionPath> {
        for (asset, a) in self.assets.iter().enumerate() {
            for (page, p) in a.pages.iter().enumerate() {
                for (module, m) in p.modules.iter().enumerate() {
                    if let Some(section) = m.sections.iter().position(|s| s.uuid == uuid) {
                        return Some(SectionPath {
                            asset,
                            page,
                            module,
                            section,
                        });
                    }
                }
            }
        }
        None
    }

    pub fn find_pattern_path(&self, uuid: Uuid) -> Option<PatternPath> {
        for (asset, a) in self.assets.iter().enumerate() {
            for (page, p) in a.pages.iter().enumerate() {
                for (module, m) in p.modules.iter().enumerate() {
                    for (section, s) in m.sections.iter().enumerate() {
                        if let Some(pattern) = s.patterns.iter().position(|pat| pat.uuid == uuid) {
                            return Some(PatternPath {
                                asset,
                                page,
                                module,
                                section,
                                pattern,
                            });
                        }
                    }
                }
            }
        }
        None
    }

    // ── index path → refs (None if out of bounds) ──────────────────

    pub fn asset(&self, p: AssetPath) -> Option<&Asset> {
        self.assets.get(p.asset)
    }
    pub fn asset_mut(&mut self, p: AssetPath) -> Option<&mut Asset> {
        self.assets.get_mut(p.asset)
    }

    pub fn page(&self, p: PagePath) -> Option<&Page> {
        self.assets.get(p.asset)?.pages.get(p.page)
    }
    pub fn page_mut(&mut self, p: PagePath) -> Option<&mut Page> {
        self.assets.get_mut(p.asset)?.pages.get_mut(p.page)
    }

    pub fn module(&self, p: ModulePath) -> Option<&Module> {
        self.page(PagePath {
            asset: p.asset,
            page: p.page,
        })?
        .modules
        .get(p.module)
    }
    pub fn module_mut(&mut self, p: ModulePath) -> Option<&mut Module> {
        self.page_mut(PagePath {
            asset: p.asset,
            page: p.page,
        })?
        .modules
        .get_mut(p.module)
    }

    pub fn section(&self, p: SectionPath) -> Option<&Section> {
        self.module(ModulePath {
            asset: p.asset,
            page: p.page,
            module: p.module,
        })?
        .sections
        .get(p.section)
    }
    pub fn section_mut(&mut self, p: SectionPath) -> Option<&mut Section> {
        self.module_mut(ModulePath {
            asset: p.asset,
            page: p.page,
            module: p.module,
        })?
        .sections
        .get_mut(p.section)
    }

    pub fn pattern(&self, p: PatternPath) -> Option<&Pattern> {
        self.section(SectionPath {
            asset: p.asset,
            page: p.page,
            module: p.module,
            section: p.section,
        })?
        .patterns
        .get(p.pattern)
    }
    pub fn pattern_mut(&mut self, p: PatternPath) -> Option<&mut Pattern> {
        self.section_mut(SectionPath {
            asset: p.asset,
            page: p.page,
            module: p.module,
            section: p.section,
        })?
        .patterns
        .get_mut(p.pattern)
    }

    // ── uuid → ref convenience ──────────────────────────────────────

    pub fn find_pattern(&self, uuid: Uuid) -> Option<&Pattern> {
        self.find_pattern_path(uuid).and_then(|p| self.pattern(p))
    }
    pub fn find_pattern_mut(&mut self, uuid: Uuid) -> Option<&mut Pattern> {
        let path = self.find_pattern_path(uuid)?;
        self.pattern_mut(path)
    }
    pub fn find_section_mut(&mut self, uuid: Uuid) -> Option<&mut Section> {
        let path = self.find_section(uuid)?;
        self.section_mut(path)
    }

    // ── collections ─────────────────────────────────────────────────

    pub fn collect_all_patterns(&self) -> Vec<&Pattern> {
        let mut out = Vec::new();
        for a in &self.assets {
            for p in &a.pages {
                for m in &p.modules {
                    for s in &m.sections {
                        out.extend(s.patterns.iter());
                    }
                }
            }
        }
        out
    }

    pub fn collect_patterns_in_asset(&self, asset_uuid: Uuid) -> Vec<&Pattern> {
        let mut out = Vec::new();
        if let Some(a) = self.assets.iter().find(|a| a.uuid == asset_uuid) {
            for p in &a.pages {
                for m in &p.modules {
                    for s in &m.sections {
                        out.extend(s.patterns.iter());
                    }
                }
            }
        }
        out
    }

    /// How many Page+Module nodes share this `content_uuid` (the
    /// "reference count" the pool view shows). A freshly-created node
    /// references only itself → count 1; a duplicate bumps it.
    pub fn content_ref_count(&self, content_uuid: Uuid) -> usize {
        let mut n = 0;
        for a in &self.assets {
            for p in &a.pages {
                if p.content_uuid == content_uuid {
                    n += 1;
                }
                for m in &p.modules {
                    if m.content_uuid == content_uuid {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    // ── delete guards (never let a parent become empty) ─────────────

    pub fn can_delete_section(&self, uuid: Uuid) -> bool {
        self.find_section(uuid)
            .and_then(|p| {
                self.module(ModulePath {
                    asset: p.asset,
                    page: p.page,
                    module: p.module,
                })
            })
            .map(|m| m.sections.len() > 1)
            .unwrap_or(false)
    }

    pub fn can_delete_module(&self, uuid: Uuid) -> bool {
        self.find_module(uuid)
            .and_then(|p| {
                self.page(PagePath {
                    asset: p.asset,
                    page: p.page,
                })
            })
            .map(|pg| pg.modules.len() > 1)
            .unwrap_or(false)
    }

    pub fn can_delete_page(&self, uuid: Uuid) -> bool {
        self.find_page(uuid)
            .and_then(|p| self.assets.get(p.asset))
            .map(|a| a.pages.len() > 1)
            .unwrap_or(false)
    }

    pub fn can_delete_asset(&self, uuid: Uuid) -> bool {
        self.find_asset(uuid).is_some() && self.assets.len() > 1
    }
}
