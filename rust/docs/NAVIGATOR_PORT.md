# Navigator → Gatherer Hub: Port Specification

Reference C++ source: [`CollectorII/Navigator/`](../../../../CollectorII/Navigator/)
- Model: `NavigatorModel.{h,cpp}` (~46 KB)
- UI: `NavigatorComponent.{h,cpp}` (~138 KB)
- Docs: `Documentation/{ARCHITECTURE,REQUIREMENTS_AND_USE_CASES,README,KNOWN_ISSUES_AND_FIXES,TODOS,WISHLIST}.md`
- Engine API map: `Resources/Asset Hierarchy_Data Model.txt`

## What the Navigator is

A **JUCE plugin** whose job is to **author the structural tree of a
project** of assets. It is one part of the engine team's three-tool
stack:

1. **Navigator** — interactive editor; outputs a JSON snapshot of the
   project (`exportState` / `importState`).
2. **WLA Page Bundler 1.4.0.0** — separate CLI that consumes the
   snapshot + WAVs and produces the engine's `<CODE>.ttasset` +
   `.wlamodel` + `.wlabank` bundle (the format Gatherer already reads
   and writes).
3. **Engine** — the runtime that loads bundles and plays them
   (`WlaMainAudioProcessor`, `WlaAssetPlayerProcessor`,
   `WlaTransportState`, `WlaAdaptiveMixerState`).

The Navigator has **zero audio code** — no DSP, no playback, no mixer.
Pure data + structural-edit UI.

## Why Gatherer absorbs Navigator's role

The user has chosen to make Gatherer Hub **self-contained**: it
records the stems, drives the adaptive mixer, edits the structural
tree, and writes the engine bundle directly (skipping the Page
Bundler step). Gatherer therefore needs Navigator's *model* and
enough of its *interaction model* to author one music asset
end-to-end.

## Data model (verbatim from `NavigatorModel.h`)

### Hierarchy

```
NavigatorProject
└── assets[]                              NavigatorAsset
    └── pages[]                           NavigatorPage   ── contentUuid (dedup)
        └── modules[]                     NavigatorModule ── contentUuid (dedup)
            └── roles[]                   NavigatorRole   (a.k.a. Section)
                └── patterns[]            NavigatorPattern
                    ├── clipSource        NavigatorClipSource
                    ├── inRegions[]       NavigatorRegion
                    └── outRegions[]      NavigatorRegion
```

All containers are inline `std::vector<T>` (children stored by value,
not by uid reference). Lookups traverse. **There is no pattern pool in
the model** — the docs mention one as a wishlist item; the production
code keeps patterns inline.

### Field reference (with Rust target types)

| C++ struct          | Field            | C++ type           | Rust target                                |
|---------------------|------------------|--------------------|--------------------------------------------|
| `NavigatorRegion`   | `start`, `end`   | `double` (frames)  | `u64`                                      |
|                     | `sync`           | `double` (frames)  | `u64` (explicit, not derived)              |
|                     | `shape`          | `int`              | `u32` enum (linear/equal-power/…)          |
|                     | `group`          | `int`              | `u32`                                      |
| `NavigatorClipSource` | `path`         | `juce::String`     | `PathBuf` (resolved against session root)  |
| `NavigatorPattern`  | `uuid`, `id`     | `juce::String`     | `Uuid`, `String`                           |
|                     | `enabled`        | `bool`             | `bool`                                     |
|                     | `refEdit`        | `bool`             | `bool` (per-pattern "ref edit" toggle)     |
|                     | `loopStart/End`  | `double` (frames)  | `u64`                                      |
|                     | `xfade`          | `double` (frames)  | `u64`                                      |
|                     | `xoffset`        | `double` (frames)  | `i64`                                      |
|                     | `looping`        | `bool`             | `bool`                                     |
|                     | `clipSource`     | struct             | `Option<NavigatorClipSource>`              |
|                     | `inRegions`, `outRegions` | `vec`     | `Vec<NavigatorRegion>`                     |
| `NavigatorRole`     | `uuid`           | `juce::String`     | `Uuid`                                     |
|                     | `type`           | `juce::String`     | `enum { Intro, Main, Outro }`              |
|                     | `displayType`    | `juce::String`     | derived ("Main 1", "Main 2" …) — don't store |
|                     | `name`           | `juce::String`     | `String` (optional, often empty)           |
|                     | `patterns`       | `vec`              | `Vec<NavigatorPattern>`                    |
| `NavigatorModule`   | `uuid`, `id`     | `juce::String`     | `Uuid`, `String`                           |
|                     | `contentUuid`    | `juce::String`     | `Uuid` (duplicate-vs-clone identity)       |
|                     | `baseName`       | `juce::String`     | `String`                                   |
|                     | `roles`          | `vec`              | `Vec<NavigatorRole>`                       |
| `NavigatorPage`     | `uuid`, `id`     | `juce::String`     | `Uuid`, `String`                           |
|                     | `contentUuid`    | `juce::String`     | `Uuid`                                     |
|                     | `modules`        | `vec`              | `Vec<NavigatorModule>`                     |
| `NavigatorAsset`    | `uuid`, `id`     | `juce::String`     | `Uuid`, `String`                           |
|                     | `pages`          | `vec`              | `Vec<NavigatorPage>`                       |
| `NavigatorProject`  | `assets`         | `vec`              | `Vec<NavigatorAsset>`                      |

### Identity model

- Every node has a `uuid` (random v4) generated by `generateUuid()` at
  creation. UUIDs are persistent across save/load.
- Pages and modules additionally carry a `contentUuid`:
  - **Duplicate** (right-click → Duplicate Right): the new node gets a
    fresh `uuid` but the `contentUuid` is left unchanged → engine
    treats both nodes as the same logical content.
  - **Clone** (right-click → Clone Right): both `uuid` AND
    `contentUuid` are regenerated → engine sees an independent copy.
  - Patterns are always cloned (all uuids regenerated) on paste,
    because pattern audio is not refcounted in the snapshot.
- The duplicate-vs-clone split lets a project reuse one Module across
  many Pages while a clone gives the user an editable copy.

### Snapshot JSON

`exportState()` writes a single document under the key `project`:

```json
{
  "project": {
    "assets": [
      { "uuid": "...", "id": "Atlas",
        "pages": [
          { "uuid": "...", "contentUuid": "...", "id": "Page 1",
            "modules": [
              { "uuid": "...", "contentUuid": "...",
                "baseName": "Module", "id": "Module 1",
                "roles": [
                  { "uuid": "...", "type": "Main", "name": "",
                    "patterns": [
                      { "uuid": "...", "id": "Pattern 1",
                        "enabled": true, "refEdit": false,
                        "loopStart": 0, "loopEnd": 0, "looping": true,
                        "xfade": 0, "xoffset": 0,
                        "clipPath": "...", "inRegions": [...],
                        "outRegions": [
                          { "start": 0, "end": 0,
                            "sync": 0, "shape": 0, "group": 0 }
                        ]
                      } ] } ] } ] } ] }
```

Note `"type"` is the section role ("Intro" | "Main" | "Outro"), not
the asset type. Asset type (music / locations / globals / sounds) is
NOT stored here — it lives on the engine bundle's `.ttasset` `meta`
block. The Navigator only authors music in production.

## Mutation API (the operations Gatherer must expose)

Grouped, with the canonical method names from `NavigatorModel`. I'll
mirror these names in Rust (`Project::add_page`, `Project::move_module`,
…). The `uuid` arguments below are all string-shaped uuids.

### Asset / Page / Module / Section CRUD

- `add_asset() -> AssetUuid` — creates Asset + Page + Module with a
  Main role pre-seeded.
- `add_page(asset_uuid) -> PageUuid`
- `add_module_after(module_uuid) -> ModuleUuid`
- `add_section(module_uuid, kind)` — inserts the new role at the
  position dictated by `getSectionInsertIndex` (canonical order:
  Intro < Main < Outro).
- `delete_{asset|page|module|section}(uuid)` — guards: never let a
  parent become empty (≥1 page per asset, ≥1 module per page, ≥1
  section per module). Returns the removed uuids.
- `move_{page|module|section}(uuid, new_parent, index)` — same-parent
  reorder vs cross-parent move; uses canonical-order snapping for
  sections.
- `duplicate_{section|module|page}_right(uuid) -> uuid` — duplicate
  keeps `contentUuid`.
- `clone_{section|module|page}_right(uuid) -> uuid` — clone
  regenerates everything; appends " Copy" to the display id.

### Pattern CRUD (per section)

- `add_pattern(section_uuid) -> PatternUuid` — appends; no fixed slot
  count is enforced by the model.
- `remove_pattern(section_uuid, index)`
- `delete_pattern(uuid) -> Vec<PatternUuid>` (batch counterpart:
  `delete_patterns(uuids)`).
- `clone_pattern(uuid) -> PatternUuid` — copies all properties, fresh
  uuid, " Copy" suffix.
- `move_or_swap_pattern(pattern_uuid, dest_section_uuid, index)` —
  unified move / reorder / swap.
- `set_pattern_enabled(uuid, bool)` / `toggle_pattern_enabled(uuid)`
- `rename_pattern(uuid, name)`
- `set_clip_source(uuid, path)`
- `set_loop(uuid, start, end, looping, xfade, xoffset)` (5 individual
  setters in the C++; collapse to one in Rust unless undo granularity
  demands the split).
- `add_in_region` / `update_in_region(idx, …)` / `remove_in_region(idx)`,
  mirror for `out_region`.

### Per-slot edit-toggle ("refEdit")

- `toggle_ref_edit(section_uuid, index)`
- `set_ref_edit(section_uuid, index, on)`
- `get_ref_edit(section_uuid, index) -> bool`

`refEdit` is a per-slot bool that the UI uses to mark "I'm editing
this pattern" while another tool (the Engine's Pattern Editor) is
open. In Gatherer we may not need it at all v1, but the field rides
along in the snapshot so we keep it.

### Clipboard

Three opaque clipboard structs — `ClipboardSection`, `ClipboardModule`,
`ClipboardPage` — each holds a deep copy. `copy_*(uuid) -> Clipboard*`
returns it; `paste_*_onto(target_uuid, clipboard)` regenerates uuids
on the pasted children.

### Bulk

- `remove_all_patterns_in_{page|module|section}(uuid)`
- `renumber()` — rebuilds `displayType` ("Main 1", "Main 2" …) and
  re-normalises role order. Called after every structural mutation.
- `reset_to_default()` — fresh project with 1 Asset / 1 Page / 1
  Module / 1 Main role.

### Queries (read-only)

- `find_pattern(uuid) -> Option<&Pattern>`
- `find_pattern_location(uuid) -> Option<PatternLocation>` (path:
  asset, page, module, section, index).
- `find_{section|module|page}_location(uuid)` — analogous.
- `collect_all_patterns() -> Vec<&Pattern>` — used for reference-count
  computation.
- `collect_patterns_in_asset(asset_uuid)`.
- `can_delete_{section|module|page}(uuid) -> bool` — drives the
  enabled-state of delete menu items.

### Invariants enforced by the model

1. **Section role ordering**: Intro, then Main, then Outro. The
   `normalize_roles` pass enforces this after every mutation.
2. **Non-empty parents**: every Asset has ≥1 Page, every Page ≥1
   Module, every Module ≥1 Section. Delete operations refuse to
   remove the last child; the canonical bootstrap (`reset_to_default`)
   primes every level.
3. **Patterns are inline, owned**: no orphan patterns; no shared
   patterns between sections (paste always regenerates uuids).
4. **Tree, not graph**: no back-references or cross-asset links. The
   "cross-piece outro→intro" hand-off is engine-side and computed
   from regions at playback time, not stored in the model.
5. **Pattern slot count is NOT enforced** at the model level. The
   docs and old UI assumed 10 slots; the production code lets
   `addPatternToSection` append freely. We follow the production code.

## UI surface (for an iced port)

### Layout (from `NavigatorComponent.cpp`)

```
┌───────────────────────────────────────────────────────────────┐
│ Header strip: Undo · Redo · Reset · Dump      30 px           │
├───────────┬───────────────────────────────────────────────────┤
│  Pool     │ Asset tree (horizontal scroll)                    │
│ (toggle,  │   ┌───────────────── Asset ──────────────────┐    │
│  160 px,  │   │ Page                                     │    │
│  shows    │   │   Module                                 │    │
│  pattern/ │   │     ┌Intro┐ ┌Main┐ ┌Outro┐ … 125 px each │    │
│  page/    │   │     │slot │ │slot│ │slot │  18 px rows   │    │
│  module   │   │     │slot │ │slot│ │slot │  ≤ 10 visible │    │
│  rows     │   │     │  +  │ │ +  │ │  +  │  add-button   │    │
│  with     │   │     └─────┘ └────┘ └─────┘               │    │
│  refcount)│   │   Module 2 …                             │    │
│           │   │   …                                      │    │
│           │   └──────────────────────────────────────────┘    │
│           │   Asset 2 …                                       │
└───────────┴───────────────────────────────────────────────────┘
```

Constants worth keeping: `pagePad 10`, `modulePad 8`, `sectionW 125`,
`sectionH 90` (forced taller when slot count > 4), row height `18`,
pool column width `160`.

### Pattern slot widget

A 117×18 rectangle with:
- 12×12 checkbox on the left → toggles `refEdit`
- label text in the middle → pattern `id`
- `…` button on the right → opens the pattern context menu
- empty slots render a `+` button → opens the "add" menu
- visual states (border color × line width):
  - strong selection: white 200α, 2 px
  - weak selection: white 120-180α, 1 px (derived from pool selection)
  - normal: no border
  - disabled: label dimmed
  - refEdit on: checkbox filled

### Context menus

Eight `MenuKind` cases. Item lists copied verbatim:
- **pattern**: Edit · Rename · Copy · Cut · Paste · Remove Reference · Remove from Pool · Delete · Select in Pool
- **multiPattern**: Edit · Delete
- **slot** (empty): Paste · Add · Clear · Import
- **section**: Rename · Copy · Paste · Duplicate · Clone · Delete · Delete References · Delete from Pool
- **module**: Copy · Paste · Duplicate · Clone · Delete · Select in Pool
- **page**: Copy · Paste · Duplicate · Clone · Delete · Select in Pool
- **refMulti**: Edit · Copy · Cut · Paste · Remove References · Select in Pool
- **assetPool**: Select References · Remove from Pool · Remove all References · Clone · Remove unused content

### Drag operations

- pattern (from pool) → slot
- slot → slot: move or swap within or across sections
- slot brush: hold and drag across empty slots → auto-create patterns
- page reorder (within asset)
- module reorder (within page)
- section reorder (within module, snaps to Intro/Main/Outro order)

### Keyboard

- ←/→ across sections; into/out of the pool at asset boundary
- ↑/↓ across slots in a section
- Shift+arrow extends selection from the anchor
- E toggles `refEdit` on the selected slot
- Cmd/Ctrl+C/X/V — copy / cut / paste (pattern, section, module, page;
  copy kind is inferred from the current selection)
- Delete/Backspace — delete modules → pages → patterns (in that order
  of preference)
- Escape — clear selection

### Selection model

Five independent selection sets, each clearable independently:
`ref_keys`, `pool_keys`, `pattern_uids`, `page_uids`, `module_uids`.
Strong = user-clicked; Weak = derived (e.g. selecting a row in the
pool weakly highlights every slot that references that pattern). In
iced this is most naturally a `HashSet<Uuid>` per kind plus a
`SelectionKind` discriminator on the focus / anchor.

### JUCE-specific things to NOT port

- `PopupMenu::showMenuAsync` → roll our own iced overlay menu (we
  already have a popover pattern in `target_curve_popover_view`).
- JUCE drag-image preview → a `dragging: bool` flag in `State` + a
  faint blue overlay drawn by the canvas; no per-frame drag image.
- `Component` lifetimes / inline `TextEditor` overlay → iced
  `text_input` shown conditionally on a `rename: Option<Uuid>` field.
- Hit-test arrays (`patternCheckboxes`, `slotOptionsButtons` …) →
  iced widgets handle hit testing natively.
- `juce::UndoManager` → existing `undo` plan from FIELD's `command.rs`
  or a snapshot-based MVP (Navigator itself is snapshot-based today).

## Mapping back to Gatherer's current state

What Gatherer Hub already has that maps cleanly:
- `SessionState.asset_meta` ↔ NavigatorAsset's `id` + the engine's
  `.ttasset` `meta` block.
- `SessionState.sections: Vec<Section>` ↔ flattened
  `roles: Vec<NavigatorRole>` of the (single) module.
- `Section.kind: SectionKind` ↔ `NavigatorRole.type`.
- `Region {begin_frames, end_frames, fade_pct, fade_shape, group}`
  ↔ `NavigatorRegion {start, end, sync, shape, group}` — almost the
  same shape with two small deltas:
  - their `sync` is an *explicit* frame offset; ours derives it from
    `begin` (out) / `end` (in). Either model works; if we stay
    derived, the wlamodel writer just computes `sync = begin` or
    `sync = end` on the way out. Switching to explicit `sync` is
    safer when sync ≠ region edge in some future template.
  - their `shape: int` is the engine's curve enum; ours stores
    `fade_shape: f32 ∈ [0,1]` (the per-region float we observed in
    Atlas). Likely Navigator's `int shape` is an index into a small
    curve table; the float is the engine-bundle representation. Keep
    Gatherer on the float and translate at the bundle boundary.

What's missing in Gatherer that Navigator has:
- The **Pattern** level: today `Section.take` is a single take. We
  need `Section.patterns: Vec<Pattern>` where `Pattern` carries
  loop/xfade/xoffset/clip + regions. The current `take` collapses to
  `pattern[0].clip + loop_range`.
- The **Page** and **Module** levels above Section. For one-asset-
  at-a-time authoring we can keep these implicit (one page, one
  module) but they need uuids in the snapshot.
- **Duplicate vs clone** (`contentUuid`) — adds a `content_uuid` to
  Page and Module.
- **Reference counts / pool view** — useful when one Module is reused
  across Pages. v1 can skip if every page has one unique module.

## Proposed Rust crate structure

A new `navigator/` module under `gatherer-hub/src/`:

```
src/navigator/
├── mod.rs       — re-exports + integration glue
├── model.rs     — Project / Asset / Page / Module / Section /
│                  Pattern / Region / ClipSource structs + impls
├── ops.rs       — mutation API (the table above). Pure functions
│                  taking &mut Project.
├── snapshot.rs  — serde JSON snapshot reader + writer; mirrors
│                  exportState/importState keys exactly so we can
│                  round-trip Navigator-authored projects.
└── selection.rs — five selection sets + anchor/cursor model.
```

Tests in `tests/navigator_snapshot.rs`:
- import a hand-crafted Navigator snapshot fixture, mutate, re-export,
  diff = 0.
- if we obtain a real `.nav` snapshot from the user, round-trip it
  through Rust and compare.

## Phasing recommendation

The current plan has six phases (A–F). Two scope options:

**Option 1 — Absorb Navigator's model only; keep Gatherer's UI simple.**
- Replace Phase A's flat `SessionState.sections` with the full
  Project tree (Asset/Page/Module/Section/Pattern). Most sessions
  will still have one asset / one page / one module — but the
  schema is there for when it's not.
- Per-pattern loop/xfade/xoffset/regions wire through to the
  `.wlamodel` writer in Phase D as direct copies.
- UI keeps the simple "section switcher" planned for Phase B; no
  pool panel, no drag-brush, no duplicate/clone affordances v1.
- Cheaper. Doesn't replace Navigator's authoring UX, but lets
  Gatherer write structurally-correct bundles.

**Option 2 — Full Navigator port (model + UI) inside Gatherer.**
- Pool panel + slot grid + drag-brush + context menus + keyboard
  nav, on top of Option 1's model.
- 4–8× more UI code than Option 1.
- Replaces the Navigator plugin entirely; one tool instead of three.

I recommend Option 1 to start. The model is the load-bearing piece
(without it the bundle writer can't represent the project shape the
engine expects); the UX polish is independently addable later. If you
already enjoy authoring in the existing Navigator, Option 1 is also
what lets Gatherer *import a Navigator snapshot* and pick up where
the Navigator left off.

## Open questions for the user

1. Does Gatherer need to **author multiple assets** in one session, or
   is "one session = one asset" forever? (Affects whether Project +
   Asset levels are real or stubbed-out.)
2. Should Gatherer **read Navigator-authored snapshots** as an import
   path? If yes, snapshot.rs lands earlier.
3. Are **Pages** ever > 1 in your music workflow, or is it always one
   asset → one page → one module? (Same for Modules.)
4. Does the `int shape` field in `NavigatorRegion` map to a small
   enum we should mirror, or is it an index into a hardcoded table?
   (Need this to keep `fade_shape` round-trippable.)
