//! One chat tile in the grid: transcript, composer and stream state for a
//! single session — the ChatTile view port.

use crate::oc::CommandInfo;
use crate::settings::HarnessKind;
use crate::store::{ChatItem, DEFAULT_MODEL_ENTRY, QuestionEntry, Store, ToolItem, at_token_start};
use crate::theme;
use crate::ui::{BtnKind, fold_body, menu_row, popover_card};
use warpui_kit_picker::{
    DismissFn, FilteredPicker, OverlayAnchor, PanelChrome, PickerBody, PickerData, PickerIcons,
    PickerItem, PickerRow, PickerSpec, PickerTarget, Selection, TriggerScale, model_name,
    model_provider,
};

/// The tile head, one row of the shell's rhythm.
const TILE_HEAD_HEIGHT: f32 = 44.;
/// A message's measure: a tile can be far wider than a line should read.
const MESSAGE_WIDTH: f32 = 760.;
const CARD_WIDTH: f32 = 640.;
const PILL_HEIGHT: f32 = 28.;
const SEND_SIZE: f32 = 30.;

/// How much of the tile's chrome fits its width. The window sets it: tiles are
/// cached views, and a paint-time measure could not re-render them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TileDensity {
    #[default]
    Full,
    /// Under 560: the locked harness keeps its icon, the context its numbers.
    Compact,
    /// Under 440: the variant keeps its gauge, and Running its dot.
    Tight,
}

impl TileDensity {
    pub const COMPACT_WIDTH: f32 = 560.;
    pub const TIGHT_WIDTH: f32 = 440.;

    /// `scale` is the text size against the base ladder: larger text needs the
    /// same room in fewer words.
    pub fn for_width(width: f32, scale: f32) -> Self {
        if width < Self::TIGHT_WIDTH * scale {
            Self::Tight
        } else if width < Self::COMPACT_WIDTH * scale {
            Self::Compact
        } else {
            Self::Full
        }
    }
}

/// A model pick from the composer pill: applies through the store/tile.
type ModelPick = Box<dyn Fn(&str, &mut App)>;
use crate::ui::markdown::render_markdown;
use crate::ui::tooltip::tip;
use gpui::prelude::*;
use gpui::{
    AccessibleAction, Animation, AnimationExt as _, App, Bounds, Context, Entity, Focusable,
    ImageFormat, MouseButton, Pixels, Render, Role, ScrollHandle, SharedString, Toggled, Window,
    actions, canvas, div, img, px, relative,
};
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::input::{
    Enter, Escape, InputEvent, InputState, MoveDown, MoveUp, Position, TextareaState,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use warpui_kit_core::{motion, shell};
use warpui_kit_diff::{render_code, render_diff, sniff_lang};
use warpui_kit_markdown::LiveEditorState;

// composer-only key overrides, bound in the "Input" context after
// gpui_kit::init so the later binding wins over the editor's own up/down
actions!(warp_agent, [ComposerUp, ComposerDown, ComposerCycleAgent]);

// composer-suggestion keys, bound over a "Composer > Input" context (the
// composer frame sets "Composer"): they win the same-depth tie as later
// registrations and never fire for the app's other inputs, which keep the
// stock editor bindings
actions!(
    warp_agent,
    [
        ComposerSlashUp,
        ComposerSlashDown,
        ComposerSlashEnter,
        ComposerSlashEscape
    ]
);

/// How many suggestion rows either menu shows at once; the rest collapse
/// into a "keep typing" hint.
const SLASH_MENU_VISIBLE: usize = 8;

/// Which suggestion menu the composer is showing: server commands for a
/// leading `/`, filename mentions for an active `@token`.
#[derive(Clone, Copy, PartialEq)]
enum SuggestMode {
    Commands,
    Files,
}

#[derive(Clone, Copy)]
enum PickerKind {
    Model,
    Variant,
    Agent,
}

#[derive(Clone)]
enum ContextMenu {
    /// copy text
    Copy(SharedString),
    /// user message → back into the composer
    EditInput(SharedString),
    /// user message → send its text again in this chat
    Reprompt(SharedString),
    /// user message (by item key) → new session seeded with the transcript up to here
    ForkFromHere(usize),
    /// copy image bytes
    CopyImage { png: Arc<Vec<u8>> },
}

/// One cached assistant-message editor: the entity, the text it was last
/// fed, and the content height the tile last saw (the tile re-renders only
/// on real height moves — see `md_editor`).
struct MdEditor {
    fed: SharedString,
    editor: Entity<LiveEditorState>,
    seen_height: Rc<Cell<f32>>,
}

pub struct TileView {
    pub key: String,
    store: Entity<Store>,
    scroll: ScrollHandle,
    pinned: bool,
    last_max: Pixels,
    expanded: HashSet<String>, // fold state by item id
    /// Bumped on every fold toggle: `expanded` is unordered, so the cache-key
    /// fingerprint hashes this counter instead of the set.
    fold_rev: u64,
    /// The cache keys handed to `prune_caches` last time and the content
    /// fingerprint they were built from. A repaint with no content change
    /// (caret, scroll, resize) reuses both instead of building a set of
    /// Strings per frame (audit B7).
    live_keys: HashSet<String>,
    live_fingerprint: Option<u64>,
    picker: Option<PickerKind>,
    /// the composer model dropdown as its own entity: filter typing repaints
    /// only the picker (see FilteredPicker::overlay); created on first open
    model_picker: Option<Entity<FilteredPicker>>,
    menu: Option<TileMenu>,
    /// window-space bounds of the tile root, captured each paint; converts
    /// click positions into tile-local menu coordinates
    origin: std::rc::Rc<std::cell::Cell<(f32, f32, f32, f32)>>,
    /// window-space bounds of the composer pills, captured at prepaint (one
    /// per PickerKind) so a dropdown opens against its own pill instead of
    /// the tile's corner
    pill_bounds: [std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<Pixels>>>>; 3],
    /// where the open variant/agent dropdown sits in the tile: its left edge
    /// and the distance from the tile's bottom to its bottom edge
    picker_placement: Option<(Pixels, Pixels)>,
    composer: Entity<TextareaState>,
    /// the same textarea state wrapped in a leaf view: a keystroke notifies
    /// TextareaState, and any view that read the entity during its render
    /// re-renders — embedding the kit element straight in TileView made every
    /// keypress re-render the whole transcript. TileView keeps the state
    /// handle for event-time reads (send, history, suggest).
    composer_field: Entity<ComposerField>,
    /// focus flag mirrored from InputEvent::Focus/Blur — reading the live
    /// focus handle in render would re-create the per-keystroke dependency
    composer_focused: bool,
    /// field text as of the last Change event or programmatic set_value
    /// (set_value emits no Change) — replaces render-time value reads
    last_field_text: String,
    question_inputs: HashMap<String, Entity<QuestionField>>,
    /// question input keys touched by the current render pass — the prune
    /// after the transcript loop drops inputs of resolved/removed questions
    question_keys_seen: HashSet<String>,
    last_composer_text: String,
    /// composer suggestion menu as its own leaf view: it subscribes to the
    /// composer state directly, so the filter re-evaluates per keystroke
    /// (see SuggestMenu below)
    suggest: Entity<SuggestMenu>,
    flyout_open: bool,
    density: TileDensity,
    last_spring_seen: bool,
    /// item id whose copy button is flashing its "copied" state right now
    copied_flash: Option<String>,
    /// message text as pointer-stable SharedStrings by item id — the kit's
    /// text view recognizes the same allocation across frames and skips
    /// re-parsing; entries are replaced when a message's text changes
    md_cache: HashMap<String, SharedString>,
    /// the read-only live editor per assistant message, with the text it was
    /// last fed — an unchanged message (the common case) skips set_value,
    /// which would reset the document; entries die with their transcript item
    md_editors: HashMap<String, MdEditor>,
    /// decoded images by item id / attachment key — PNG decode is far too
    /// expensive to repeat on every repaint; evicted with the transcript
    img_cache: HashMap<String, Arc<gpui::RenderImage>>,
}

/// Bidi embedding/override controls (U+202A–U+202E, U+2066–U+2069) reorder
/// glyphs without appearing. gpui-pre's line splitter underflows on the
/// resulting unordered line (`LineLayout::split_at`), which crashed the tile
/// on a reply containing U+202E. They carry no visible content, so strip them
/// from display text; real RTL text and directional marks stay.
fn strip_bidi_overrides(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(is_bidi_override) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.chars().filter(|c| !is_bidi_override(*c)).collect())
}

fn is_bidi_override(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

impl TileView {
    /// Start the one-second "copied" flash for `key` (a message item id, or
    /// "{item_id}#code{n}" for a fenced code block inside it). Reverts itself
    /// after the second unless a newer click has taken over the slot.
    pub fn flash_copied(&mut self, key: String, cx: &mut Context<Self>) {
        self.copied_flash = Some(key.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1000))
                .await;
            let _ = this.update(cx, |this, cx| this.clear_copied_flash(&key, cx));
        })
        .detach();
    }

    /// Revert the flash only if `key` still owns it (a newer click keeps its flash)
    pub fn clear_copied_flash(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.copied_flash.as_deref() == Some(key) {
            self.copied_flash = None;
            cx.notify();
        }
    }

    /// Message text as a pointer-stable SharedString for the kit's text view,
    /// which recognizes the same allocation across frames and skips re-parsing
    /// an unchanged document. Replaced whenever the store's copy has moved on
    /// (the streaming tail).
    fn md_text(&mut self, item_id: &str, text: &str) -> SharedString {
        let text = strip_bidi_overrides(text);
        if let Some(cached) = self.md_cache.get(item_id)
            && cached.as_ref() == text.as_ref()
        {
            return cached.clone();
        }
        let fresh = SharedString::from(text.into_owned());
        self.md_cache.insert(item_id.to_string(), fresh.clone());
        fresh
    }

    /// The read-only live editor for one assistant message: created on first
    /// render, then fed new text only when the message actually changed —
    /// set_value rebuilds the document, so an unchanged render must skip it.
    fn md_editor(
        &mut self,
        item_id: &str,
        text: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<LiveEditorState> {
        if let Some(entry) = self.md_editors.get(item_id) {
            let editor = entry.editor.clone();
            if entry.fed == text {
                return editor;
            }
            editor.update(cx, |editor, cx| editor.set_value(text.clone(), window, cx));
            self.md_editors.insert(
                item_id.to_string(),
                MdEditor {
                    fed: text,
                    editor: editor.clone(),
                    seen_height: entry.seen_height.clone(),
                },
            );
            return editor;
        }
        let editor = cx.new(|cx| {
            let mut editor = LiveEditorState::new(window, cx);
            editor.set_read_only(true, cx);
            editor.set_value(text.clone(), window, cx);
            editor
        });
        // the tile is a cached view: it re-renders only when it is notified,
        // and plain reads register no dependency — so the height the message
        // wrapper sizes itself from must be chased explicitly. Only real
        // moves notify: the editor fires on every selection drag too, and a
        // repaint per mouse move would rebuild the transcript for nothing.
        let seen_height = Rc::new(Cell::new(f32::NAN));
        {
            let seen_height = seen_height.clone();
            cx.observe(&editor, move |_, editor, cx| {
                let height = editor.read(cx).content_height();
                if height != seen_height.get() {
                    seen_height.set(height);
                    cx.notify();
                }
            })
            .detach();
        }
        self.md_editors.insert(
            item_id.to_string(),
            MdEditor {
                fed: text,
                editor: editor.clone(),
                seen_height,
            },
        );
        editor
    }

    /// Read-only markdown text for a transcript sub-surface (tool title /
    /// details, question text, notes): pointer-stable text through md_text
    /// plus a stable per-surface element id, so the kit neither re-parses on
    /// every render nor collides element ids between surfaces.
    fn transcript_text(
        &mut self,
        cache_key: &str,
        text: &str,
        id_name: &'static str,
    ) -> gpui_kit::base::text::TextView {
        let stable = self.md_text(cache_key, text);
        gpui_kit::base::text::TextView::markdown(
            gpui::ElementId::NamedInteger(id_name.into(), Self::hash(cache_key)),
            stable,
        )
    }

    /// Decoded image for a transcript item or composer attachment, cached by a
    /// stable key (item id / "att{key}") — both name immutable bytes, so an
    /// entry never goes stale, only unreachable (pruned after each render).
    fn image_for(&mut self, cache_key: &str, png: &[u8]) -> Option<Arc<gpui::RenderImage>> {
        if let Some(cached) = self.img_cache.get(cache_key) {
            return Some(cached.clone());
        }
        let decoded = warpui_kit_core::decode_png_render(png)?;
        self.img_cache
            .insert(cache_key.to_string(), decoded.clone());
        Some(decoded)
    }

    /// Drops cache entries whose transcript item or attachment is gone.
    fn prune_caches(
        &mut self,
        live: &HashSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.md_cache.retain(|k, _| live.contains(k));
        self.md_editors.retain(|k, _| live.contains(k));
        // evicted decoded images must also leave the window's sprite atlas —
        // atlas tiles persist until drop_image, so without this every image
        // ever shown in the session would stay on the GPU forever
        let evicted: Vec<Arc<gpui::RenderImage>> = self
            .img_cache
            .iter()
            .filter(|(k, _)| !live.contains(k.as_str()))
            .map(|(_, img)| Arc::clone(img))
            .collect();
        self.img_cache.retain(|k, _| live.contains(k));
        for img in evicted {
            cx.drop_image(img, Some(window));
        }
    }

    /// Decoded images still held for this tile's transcript/attachments —
    /// dropped from the atlas when the tile closes (see render_main).
    pub fn held_images(&self) -> Vec<Arc<gpui::RenderImage>> {
        self.img_cache.values().cloned().collect()
    }
}

/// A context menu request: the actions to offer plus the tile-local anchor point.
#[derive(Clone)]
struct TileMenu {
    items: Vec<ContextMenu>,
    x: f32,
    y: f32,
}

/// Miniature context-usage gauge: a ring of dots whose arc fills clockwise
/// from 12 o'clock in proportion to the context window consumed. Green below
/// 60%, amber below 85%, salmon at the danger line. Absolutely-positioned dot
/// divs, not canvas — the canvas variant painted with its bounds shifted by
/// half the container, floating the ring high beside the header text.
fn context_ring(frac: f32) -> gpui::AnyElement {
    let frac = frac.clamp(0., 1.);
    let filled_color = if frac >= 0.85 {
        theme::DANGER
    } else if frac >= 0.6 {
        theme::WARN
    } else {
        theme::text_dim()
    };
    let track_color = theme::line_strong();
    let dot = 2.6_f32;
    let ring_size = 16.0_f32;
    let radius = (ring_size - dot) / 2.;
    let segments = 28;
    let mut ring = div().relative().flex_shrink_0().size(px(ring_size));
    for i in 0..segments {
        let phase = i as f32 / segments as f32;
        let theta = phase * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let (sin, cos) = theta.sin_cos();
        let color = if phase < frac {
            filled_color
        } else {
            track_color
        };
        ring = ring.child(
            div()
                .absolute()
                .left(px(ring_size / 2. + sin * radius - dot / 2.))
                .top(px(ring_size / 2. - cos * radius - dot / 2.))
                .size(px(dot))
                .rounded_full()
                .bg(color),
        );
    }
    ring.into_any_element()
}

/// Three dots for a chat that is thinking: the run has started and nothing has
/// arrived yet. Reduced motion leaves them static, because the tile's state
/// word already says what is happening.
fn thinking_dots(id: &str, color: gpui::Hsla) -> gpui::AnyElement {
    let mut row = div()
        .id(SharedString::from(format!("{id}-dots")))
        .flex()
        .items_center()
        .gap(px(5.));
    for dot in 0..3u32 {
        // the svg is the animated element (a box cannot animate in this gpui);
        // each dot runs a third of a cycle behind the one before it
        let glyph = crate::ui::icon("icons/dot.svg", px(5.), color);
        let body = if warpui_kit_core::motion::reduced() {
            glyph.into_any_element()
        } else {
            glyph
                .with_animation(
                    gpui::ElementId::Name(SharedString::from(format!("{id}-dot-{dot}"))),
                    Animation::new(std::time::Duration::from_millis(900)).repeat(),
                    move |glyph, delta| {
                        let phase = (delta + dot as f32 / 3.).fract();
                        let wave = 1. - (phase * 2. - 1.).abs();
                        glyph.opacity(0.3 + 0.7 * wave)
                    },
                )
                .into_any_element()
        };
        row = row.child(body);
    }
    row.into_any_element()
}

/// The word a tile's head shows while its chat waits on a person. The newest
/// open card speaks for the chat.
fn waiting_word<'a>(items: impl DoubleEndedIterator<Item = &'a ChatItem>) -> Option<&'static str> {
    items.rev().find_map(|item| match item {
        ChatItem::Permission(perm) if perm.is_pending() => Some("Needs approval"),
        ChatItem::Question(question) if question.is_pending() => Some("Has a question"),
        _ => None,
    })
}

impl TileView {
    /// The window's measure of this tile. Only a real change repaints it.
    pub fn set_density(&mut self, density: TileDensity, cx: &mut Context<Self>) {
        if self.density != density {
            self.density = density;
            cx.notify();
        }
    }

    pub fn new(
        store: Entity<Store>,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // the store bumps this tile's revision entity when its content
        // changed: as a cached view (see render_main) this tile re-renders
        // on that bump and on nothing else — other tiles' streams, sidebar
        // events and global chrome pings keep the recycled subtree
        if let Some(rev) = store.read(cx).tile_by_key(&key).and_then(|t| t.rev.clone()) {
            cx.observe(&rev, |_, _, cx| cx.notify()).detach();
        }
        // kit textarea: submit_on_enter gives Warp-style Enter=send, Shift+Enter=newline;
        // a persisted draft (ui.json) is restored straight into the field
        let initial = store
            .read(cx)
            .tile_by_key(&key)
            .map(|t| t.input.clone())
            .unwrap_or_default();
        let initial_text = initial.clone();
        let harness_label = store
            .read(cx)
            .tile_by_key(&key)
            .map(|t| t.harness.label())
            .unwrap_or("opencode");
        let composer = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .placeholder(SharedString::from(format!("Ask {harness_label}…")))
                .submit_on_enter(true)
                .auto_grow(2, 9);
            if !initial.is_empty() {
                state.set_value(initial.as_str(), window, cx);
            }
            state
        });
        let composer_field = cx.new(|_| ComposerField {
            state: composer.clone(),
            store: store.clone(),
            key: key.clone(),
        });

        let store_handle = store.clone();
        let key_for_change = key.clone();
        cx.subscribe(&composer, move |this, field, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let text = field.read(cx).value().to_string();
                this.last_field_text = text.clone();
                let key = key_for_change.clone();
                store_handle.update(cx, move |s, _| {
                    if let Some(tile) = s.tile_by_key_mut(&key)
                        && tile.input != text
                    {
                        tile.input = text;
                        tile.reset_history();
                    }
                });
            }
        })
        .detach();

        // focus transitions are rare; the mirrored flag keeps render from
        // reading the live focus handle (a TextareaState dependency)
        cx.subscribe(&composer, move |this, _, event: &InputEvent, cx| {
            let focused = match event {
                InputEvent::Focus => true,
                InputEvent::Blur => false,
                _ => return,
            };
            if this.composer_focused != focused {
                this.composer_focused = focused;
                cx.notify();
            }
        })
        .detach();

        let store_handle = store.clone();
        let key_for_send = key.clone();
        // the first arg is this TileView, already mid-update — mutating it
        // directly is required; this.update() here would re-enter the entity
        // and gpui panics ("cannot update TileView while it is already being
        // updated")
        cx.subscribe(&composer, move |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                let key = key_for_send.clone();
                store_handle.update(cx, move |s, cx| s.send_from_tile(&key, cx));
                this.pinned = true;
                this.scroll.scroll_to_bottom();
                cx.notify();
            }
        })
        .detach();

        // the suggestion menu is a leaf view subscribed to the composer
        // state itself: the filter re-evaluates on every keystroke without
        // repainting this tile (see SuggestMenu)
        let tile_handle = cx.entity();
        let suggest = cx.new(|cx| {
            let menu = SuggestMenu {
                composer: composer.clone(),
                store: store.clone(),
                tile: tile_handle.clone(),
                tile_key: key.clone(),
                focused: false,
                open: false,
                mode: SuggestMode::Commands,
                selected: 0,
                dismissed: false,
                at_start: 0,
                at_query: String::new(),
                find_requested: None,
                pending_search: None,
                search_task: None,
            };
            cx.subscribe(
                &composer,
                move |this: &mut SuggestMenu, field, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        let text = field.read(cx).value().to_string();
                        this.recompute(&text, true, cx);
                    }
                    InputEvent::Focus => {
                        this.focused = true;
                        let text = field.read(cx).value().to_string();
                        this.recompute(&text, false, cx);
                    }
                    InputEvent::Blur => {
                        // the menu lives only while the composer holds focus:
                        // clicking elsewhere closes it like Escape would, and a
                        // later refocus may open it again
                        this.focused = false;
                        this.open = false;
                        this.dismissed = false;
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
            menu
        });

        Self {
            key,
            store,
            scroll: ScrollHandle::new(),
            pinned: true,
            last_max: px(0.),
            expanded: HashSet::new(),
            fold_rev: 0,
            live_keys: HashSet::new(),
            live_fingerprint: None,
            picker: None,
            model_picker: None,
            menu: None,
            origin: std::rc::Rc::new(std::cell::Cell::new((0., 0., 0., 0.))),
            pill_bounds: std::array::from_fn(|_| std::rc::Rc::new(std::cell::Cell::new(None))),
            picker_placement: None,
            composer,
            composer_field,
            composer_focused: false,
            last_field_text: initial_text,
            question_inputs: HashMap::new(),
            question_keys_seen: HashSet::new(),
            last_composer_text: String::new(),
            suggest,
            flyout_open: false,
            density: TileDensity::default(),
            last_spring_seen: false,
            copied_flash: None,
            md_cache: HashMap::new(),
            md_editors: HashMap::new(),
            img_cache: HashMap::new(),
        }
    }

    /// The pill's and the tile's window bounds: what a dropdown needs to
    /// open against its own pill. Both come from the last painted frame.
    fn picker_bounds(&self, kind: PickerKind) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        let trigger = self.pill_bounds[kind as usize].get()?;
        let (x, y, w, h) = self.origin.get();
        if w <= 0. || h <= 0. {
            return None;
        }
        Some((
            trigger,
            Bounds {
                origin: gpui::point(px(x), px(y)),
                size: gpui::size(px(w), px(h)),
            },
        ))
    }

    /// Where a dropdown opens for `kind`: left-aligned with its pill, just
    /// above it, kept inside the tile.
    fn picker_anchor(&self, kind: PickerKind) -> Option<(Pixels, Pixels)> {
        self.picker_bounds(kind)
            .map(|(trigger, host)| warpui_kit_picker::composer_placement(trigger, host))
    }

    /// Window click position → tile-local menu anchor, kept inside the tile.
    fn menu_anchor(&self, event: &gpui::MouseDownEvent) -> (f32, f32) {
        let (ox, oy, w, h) = self.origin.get();
        let x = (event.position.x.as_f32() - ox).clamp(8., (w - 185.).max(8.));
        let y = (event.position.y.as_f32() - oy).clamp(8., (h - 64.).max(8.));
        (x, y)
    }

    /// File attach via the platform picker; files over 512 KB are skipped like the C# app.
    fn pick_files(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach files".into()),
        });
        let store = self.store.clone();
        let key = self.key.clone();
        cx.spawn(async move |_this, cx| {
            if let Ok(Ok(Some(paths))) = task.await {
                for path in paths {
                    let Ok(meta) = std::fs::metadata(&path) else {
                        continue;
                    };
                    if meta.len() > 512 * 1024 {
                        let name = path.display().to_string();
                        store.update(cx, |s, cx| {
                            s.announce(&format!("{name} is over 512 KB — skipped."), true, cx)
                        });
                        continue;
                    }
                    if let Ok(data) = std::fs::read(&path) {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string());
                        let mime = crate::ui::mime_for_path(&name);
                        let key = key.clone();
                        store.update(cx, move |s, _| s.attach_file(&key, name, mime.into(), data));
                    }
                }
            }
        })
        .detach();
    }

    // ---- composer suggestions: /commands · @files ----
    // the menu itself lives in SuggestMenu (leaf view) — only the composer
    // write-back and the key-action plumbing stay on the tile

    /// Writes `text` into the composer AND the store copy (set_value emits no
    /// Change, so the store would keep the old draft), caret parked at the
    /// end — the shared tail of menu acceptance. `last_composer_text` moves
    /// too: text starting with `/` must not re-trigger a stale menu open.
    fn insert_composer_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.key.clone();
        self.store.update(cx, |s, _| {
            if let Some(tile) = s.tile_by_key_mut(&key) {
                tile.input = text.clone();
                tile.reset_history();
            }
        });
        // Position is line/col: the caret goes after the last character
        let lines = text.split('\n').collect::<Vec<_>>();
        let end = Position {
            line: (lines.len() - 1) as u32,
            character: lines.last().map(|l| l.chars().count() as u32).unwrap_or(0),
        };
        self.composer.update(cx, |field, cx| {
            field.set_value(text.as_str(), window, cx);
            field.set_cursor_position(end, window, cx);
        });
        self.last_field_text = text.clone();
        self.last_composer_text = text;
        // set_value emits no Change, so the suggest menu's filter would go
        // stale against the inserted text — recompute it explicitly. The
        // menu stays closed: acceptance dismisses it and `allow_reopen` is
        // false (only a real keystroke may reopen a dismissed menu).
        let text = self.last_field_text.clone();
        self.suggest
            .update(cx, |menu, cx| menu.recompute(&text, false, cx));
    }
    /// Instant scroll to `y`, counting distance scrolled down — the handle
    /// stores the offset negative-down (0 at top, -max at bottom).
    fn scroll_to(&mut self, y: Pixels, cx: &mut Context<Self>) {
        self.scroll
            .set_offset(gpui::point(self.scroll.offset().x, -y));
        cx.notify();
    }

    fn render_item(
        &mut self,
        item_id: &str,
        item: &ChatItem,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match item {
            ChatItem::UserMessage {
                key: msg_key,
                text,
                created_ms,
                ..
            } => {
                // cached, pointer-stable: keeps the kit from re-parsing the
                // bubble's TextView on every render (same contract as md_text)
                let text = self.md_text(item_id, text);
                let time = crate::store::message_time_label(*created_ms);
                let copy_text = text.clone();
                let flash_key = item_id.to_string();
                let flashed = self.copied_flash.as_deref() == Some(item_id);
                let (copy_icon_path, copy_icon_color, copy_border) = if flashed {
                    ("icons/check.svg", accent, accent)
                } else {
                    ("icons/copy.svg", theme::text_muted(), theme::hairline())
                };
                div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .group("umsg")
                            .min_w_0()
                            .max_w(px(MESSAGE_WIDTH))
                            .rounded(px(8.))
                            .bg(theme::bubble_user())
                            .p_2()
                            .px_3()
                            .on_mouse_down(MouseButton::Right, {
                                let menu_text = text.clone();
                                let msg_key = *msg_key;
                                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                    let (x, y) = this.menu_anchor(event);
                                    this.menu = Some(TileMenu {
                                        items: vec![
                                            ContextMenu::Copy(menu_text.clone()),
                                            ContextMenu::EditInput(menu_text.clone()),
                                            ContextMenu::Reprompt(menu_text.clone()),
                                            ContextMenu::ForkFromHere(msg_key),
                                        ],
                                        x,
                                        y,
                                    });
                                    cx.notify();
                                })
                            })
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                // markdown TextView (not plain div text) so
                                // the user's own message keeps inline styling
                                div()
                                    .text_size(px(fonts.font_body))
                                    .text_color(theme::text())
                                    .child(gpui_kit::base::text::TextView::markdown(
                                        gpui::ElementId::NamedInteger(
                                            "utext".into(),
                                            TileView::hash(item_id),
                                        ),
                                        text.clone(),
                                    )),
                            )
                            .child(
                                // copy button shares the time line: pinned left,
                                // time pushed to the right (mirrors the assistant
                                // meta line)
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .id(gpui::ElementId::NamedInteger(
                                                "ucopy".into(),
                                                TileView::hash(item_id),
                                            ))
                                            .role(Role::Button)
                                            .aria_label("Copy message")
                                            .cursor_pointer()
                                            .p_0p5()
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(copy_border)
                                            .bg(theme::bubble_user())
                                            .opacity(0.)
                                            .group_hover("umsg", |s| s.opacity(1.))
                                            .hover(|s| s.bg(theme::HOVER))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.write_to_clipboard(
                                                    gpui::ClipboardItem::new_string(
                                                        copy_text.to_string(),
                                                    ),
                                                );
                                                this.flash_copied(flash_key.clone(), cx);
                                            }))
                                            .child(crate::ui::icon(
                                                copy_icon_path,
                                                px(fonts.icon_medium),
                                                copy_icon_color,
                                            )),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_micro))
                                            .text_color(theme::text_muted())
                                            .child(time),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            ChatItem::AssistantText {
                text,
                meta_label,
                created_ms,
                ..
            } => {
                let time = crate::store::message_time_label(*created_ms);
                let meta = meta_label.clone();
                // cached, pointer-stable: the kit skips re-parsing unchanged text,
                // and the copy button's string is a refcount, not a fresh copy
                let text = self.md_text(item_id, text);
                let copy_text = text.clone();
                let flash_key = item_id.to_string();
                let flashed = self.copied_flash.as_deref() == Some(item_id);
                let (copy_icon_path, copy_icon_color, copy_border) = if flashed {
                    ("icons/check.svg", accent, accent)
                } else {
                    ("icons/copy.svg", theme::text_muted(), theme::hairline())
                };
                let markdown = render_markdown(
                    &self.md_editor(item_id, text, window, cx),
                    fonts.font_body,
                    fonts.icon_medium,
                    item_id.to_string(),
                    self.copied_flash.as_deref(),
                    cx.entity().downgrade(),
                    accent,
                    cx,
                );
                div()
                    .w_full()
                    .flex()
                    .justify_start()
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .max_w(px(MESSAGE_WIDTH))
                            .group("amsg")
                            .rounded(px(8.))
                            .bg(theme::bubble_assistant())
                            .p_2()
                            .px_3()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .text_size(px(fonts.font_body))
                            .child(markdown)
                            .child(
                                // copy button shares the meta line: pinned left,
                                // token flow + time pushed to the right
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .id(gpui::ElementId::NamedInteger(
                                                "acopy".into(),
                                                TileView::hash(item_id),
                                            ))
                                            .role(Role::Button)
                                            .aria_label("Copy message")
                                            .cursor_pointer()
                                            .p_0p5()
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(copy_border)
                                            .bg(theme::bubble_assistant())
                                            .opacity(0.)
                                            .group_hover("amsg", |s| s.opacity(1.))
                                            .hover(|s| s.bg(theme::HOVER))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.write_to_clipboard(
                                                    gpui::ClipboardItem::new_string(
                                                        copy_text.to_string(),
                                                    ),
                                                );
                                                this.flash_copied(flash_key.clone(), cx);
                                            }))
                                            .child(crate::ui::icon(
                                                copy_icon_path,
                                                px(fonts.icon_medium),
                                                copy_icon_color,
                                            )),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_micro))
                                            .text_color(theme::text_muted())
                                            .child(meta),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_micro))
                                            .text_color(theme::text_muted())
                                            .child(time),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            ChatItem::Reasoning { text, .. } => {
                let open = self.expanded.contains(item_id);
                let body = self.md_text(item_id, text);
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(self.fold_header(item_id, "thinking", None, open, fonts, cx))
                    .when(open, |d| {
                        d.child(
                            fold_body(gpui::ElementId::NamedInteger(
                                "fold-thinking".into(),
                                TileView::hash(item_id),
                            ))
                            .max_w(px(MESSAGE_WIDTH))
                            .text_size(px(fonts.font_meta))
                            .text_color(theme::text_dim())
                            .child(
                                gpui_kit::base::text::TextView::markdown(
                                    gpui::ElementId::NamedInteger(
                                        "rtext".into(),
                                        TileView::hash(item_id),
                                    ),
                                    body,
                                ),
                            ),
                        )
                    })
                    .into_any_element()
            }
            ChatItem::Tool(tool) => self.render_tool(item_id, tool, fonts, window, cx),
            ChatItem::Image {
                file_name,
                png,
                created_ms,
                ..
            } => {
                let time = crate::store::message_time_label(*created_ms);
                // decoded once per item; PNG decode on every repaint dominated
                // long transcripts with screenshots
                let render = self.image_for(item_id, png);
                let mut image_el = div().child(div());
                if let Some(render) = render {
                    image_el = div().child(
                        img(render)
                            .max_w(px(360.))
                            .max_h(px(240.))
                            .object_fit(gpui::ObjectFit::Contain),
                    );
                }
                let menu_png = png.clone();
                div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .max_w(px(380.))
                            .rounded_lg()
                            .bg(theme::bubble_user())
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .on_mouse_down(MouseButton::Right, {
                                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                    let (x, y) = this.menu_anchor(event);
                                    this.menu = Some(TileMenu {
                                        items: vec![ContextMenu::CopyImage {
                                            png: menu_png.clone(),
                                        }],
                                        x,
                                        y,
                                    });
                                    cx.notify();
                                })
                            })
                            .child(image_el)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_meta))
                                            .text_color(theme::text_dim())
                                            .child(SharedString::from(file_name.clone())),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_micro))
                                            .text_color(theme::text_muted())
                                            .child(time),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            ChatItem::Permission(perm) => {
                self.render_permission(item_id, perm, fonts, accent, window, cx)
            }
            ChatItem::Question(question) => {
                self.render_question(item_id, question, fonts, accent, window, cx)
            }
            ChatItem::SystemNote { text, is_error, .. } => {
                if *is_error {
                    div()
                        .w_full()
                        .flex()
                        .child(
                            div()
                                .min_w_0()
                                .max_w(px(MESSAGE_WIDTH))
                                .rounded(px(8.))
                                .border_1()
                                .border_color(theme::DANGER)
                                .bg(theme::tint(theme::DANGER))
                                .flex()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .py_2()
                                        .px_3()
                                        .child(
                                            div()
                                                .text_size(px(fonts.font_body))
                                                .text_color(theme::DANGER)
                                                .child(
                                                    self.transcript_text(item_id, text, "errnote"),
                                                ),
                                        ),
                                ),
                        )
                        .into_any_element()
                } else {
                    div()
                        .w_full()
                        .text_align(gpui::TextAlign::Center)
                        .text_size(px(fonts.font_meta))
                        .text_color(theme::text_muted())
                        .italic()
                        .py_0p5()
                        .child(self.transcript_text(item_id, text, "sysnote"))
                        .into_any_element()
                }
            }
        }
    }

    fn render_tool(
        &mut self,
        item_id: &str,
        tool: &ToolItem,
        fonts: theme::FontTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let open_diff = self.expanded.contains(&format!("{item_id}:diff"));
        let open_details = self.expanded.contains(&format!("{item_id}:details"));
        let has_diff = tool.has_diff();
        let has_details = tool.has_details();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .flex_shrink_0()
                            .rounded(px(4.))
                            .px_1p5()
                            .bg(theme::raised())
                            .text_size(px(fonts.font_mono_meta))
                            .font_family(theme::code_font())
                            .text_color(theme::text())
                            .child(SharedString::from(tool.tool.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(fonts.font_body))
                            .text_color(theme::text_dim())
                            .truncate()
                            .child(
                                self.transcript_text(
                                    &format!("{item_id}:title"),
                                    &tool.title,
                                    "tooltitle",
                                )
                                .max_lines(1),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(fonts.font_meta))
                            .text_color(if tool.is_error {
                                theme::DANGER
                            } else {
                                theme::text_muted()
                            })
                            .child(SharedString::from(tool.status.clone())),
                    ),
            )
            .when(tool.is_error && !tool.error_preview.is_empty(), |d| {
                d.child(
                    div()
                        .min_w_0()
                        .text_size(px(fonts.font_meta))
                        .text_color(theme::DANGER)
                        .truncate()
                        .child(SharedString::from(tool.error_preview.clone())),
                )
            })
            .when(has_diff, |d| {
                d.child(self.fold_header(
                    &format!("{item_id}:diff"),
                    "Changes",
                    Some((
                        tool.numstat.as_ref().map(|n| n.additions).unwrap_or(0),
                        tool.numstat.as_ref().map(|n| n.deletions).unwrap_or(0),
                    )),
                    open_diff,
                    fonts,
                    cx,
                ))
                .when(open_diff, |d| {
                    d.child(
                        fold_body(gpui::ElementId::NamedInteger(
                            "fold-diff".into(),
                            Self::hash(item_id),
                        ))
                        .child(render_diff(
                            tool.diff.as_deref().unwrap_or(""),
                            Some(&tool.file_path),
                            fonts.font_meta,
                            window,
                            cx,
                        )),
                    )
                })
            })
            .when(has_details, |d| {
                d.child(self.fold_header(
                    &format!("{item_id}:details"),
                    "details",
                    None,
                    open_details,
                    fonts,
                    cx,
                ))
                .when(open_details, |d| {
                    let body = fold_body(gpui::ElementId::NamedInteger(
                        "fold-details".into(),
                        Self::hash(item_id),
                    ))
                    .flex()
                    .flex_col()
                    .gap_2p5();
                    let body = body
                        .when(!tool.details.input.is_empty(), |d| {
                            d.child(self.detail_section(
                                &format!("{item_id}:input"),
                                "input",
                                &tool.details.input,
                                theme::text_dim(),
                                sniff_lang(&tool.details.input),
                                fonts,
                                window,
                                cx,
                            ))
                        })
                        .when(!tool.details.output.is_empty(), |d| {
                            d.child(self.detail_section(
                                &format!("{item_id}:output"),
                                "output",
                                &tool.details.output,
                                theme::text_dim(),
                                sniff_lang(&tool.details.output),
                                fonts,
                                window,
                                cx,
                            ))
                        })
                        .when(!tool.details.error.is_empty(), |d| {
                            d.child(self.detail_section(
                                &format!("{item_id}:error"),
                                "error",
                                &tool.details.error,
                                theme::DANGER,
                                None,
                                fonts,
                                window,
                                cx,
                            ))
                        });
                    d.child(body)
                })
            })
            .into_any_element()
    }

    /// One labeled block inside the details fold: small faint label over the
    /// body. Structured payloads (sniffed JSON) and arbitrary text render as
    /// line-numbered code; error payloads keep the plain red block.
    #[expect(
        clippy::too_many_arguments,
        reason = "GPUI render helper: self + cx are structural, the rest are one render's inputs"
    )]
    fn detail_section(
        &mut self,
        cache_key: &str,
        label: &str,
        text: &str,
        color: gpui::Rgba,
        lang: Option<&'static str>,
        fonts: theme::FontTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(fonts.font_meta))
                    .text_color(theme::text_faint())
                    .child(SharedString::from(label.to_string())),
            )
            .child(match lang {
                Some(lang) => div()
                    .rounded_md()
                    .bg(theme::chip())
                    .p_2()
                    .child(render_code(
                        text,
                        Some(lang),
                        color,
                        fonts.font_meta,
                        window,
                        cx,
                    )),
                None => div()
                    .rounded_md()
                    .bg(theme::chip())
                    .p_2()
                    .text_size(px(fonts.font_meta))
                    .font_family(theme::code_font())
                    .text_color(color)
                    .child(self.transcript_text(cache_key, text, "tooldetails")),
            })
            .into_any_element()
    }

    /// A run of ≥2 consecutive tool calls collapses into one disclosure row so
    /// a queued turn reads as one line instead of a wall of pending chips.
    #[expect(
        clippy::too_many_arguments,
        reason = "GPUI render helper: self + cx are structural, the rest are one render's inputs"
    )]
    fn render_tool_group(
        &mut self,
        run: &[(String, Arc<ChatItem>)],
        group_key: &str,
        open: bool,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut adds = 0i64;
        let mut dels = 0i64;
        let mut running = 0usize;
        let mut pending = 0usize;
        let mut failed = 0usize;
        for (_, item) in run {
            let ChatItem::Tool(tool) = item.as_ref() else {
                continue;
            };
            if tool.is_running() {
                running += 1;
            } else if tool.status == "pending" {
                pending += 1;
            }
            if tool.is_error {
                failed += 1;
            }
            if let Some(n) = &tool.numstat {
                adds += n.additions;
                dels += n.deletions;
            }
        }
        let state_word = if running > 0 {
            "running".to_string()
        } else if pending > 0 {
            format!("{pending} pending")
        } else if failed > 0 {
            format!("{failed} failed")
        } else {
            "done".to_string()
        };
        let label = format!("{} tool calls · {}", run.len(), state_word);
        let numstat = (adds > 0 || dels > 0).then_some((adds, dels));
        let mut group = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(self.fold_header(group_key, &label, numstat, open, fonts, cx));
        if open {
            let mut body = fold_body(gpui::ElementId::NamedInteger(
                "fold-tools".into(),
                Self::hash(group_key),
            ))
            .flex()
            .flex_col()
            .gap_1();
            for (member_id, member) in run {
                body = body.child(self.render_item(
                    member_id,
                    member.as_ref(),
                    fonts,
                    accent,
                    window,
                    cx,
                ));
            }
            group = group.child(body);
        }
        group.into_any_element()
    }

    /// A disclosure row: chevron + label, full row is the click target.
    fn fold_header(
        &self,
        key: &str,
        label: &str,
        numstat: Option<(i64, i64)>,
        open: bool,
        fonts: theme::FontTokens,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = key.to_string();
        let label = label.to_string();
        div()
            .id(gpui::ElementId::NamedInteger(
                "fold".into(),
                Self::hash(key.as_str()),
            ))
            .role(Role::Button)
            .aria_label(SharedString::from(label.clone()))
            .aria_expanded(open)
            .flex()
            .items_center()
            .gap_1p5()
            .px_1()
            .py_0p5()
            .rounded(px(6.))
            .hover(|s| s.bg(theme::raised()))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.expanded.contains(&key) {
                    this.expanded.remove(&key);
                } else {
                    this.expanded.insert(key.clone());
                    // the body grows below the header — stop pinning to the tail
                    // or the expanded content lands above the viewport
                    this.pinned = false;
                }
                this.fold_rev = this.fold_rev.wrapping_add(1);
                cx.notify();
            }))
            .child(crate::ui::icon(
                if open {
                    "icons/chevron-down.svg"
                } else {
                    "icons/chevron-right.svg"
                },
                px(fonts.icon_tiny),
                theme::text_muted(),
            ))
            .child(
                div()
                    .text_size(px(fonts.font_meta))
                    .text_color(theme::text_dim())
                    .child(label),
            )
            .when_some(numstat, |d, (adds, dels)| {
                d.when(adds > 0 || dels > 0, |d| {
                    d.child(
                        div()
                            .font_family(theme::code_font())
                            .text_size(px(fonts.font_mono_meta))
                            .text_color(theme::ADD)
                            .child(format!("+{adds}")),
                    )
                    .child(
                        div()
                            .font_family(theme::code_font())
                            .text_size(px(fonts.font_mono_meta))
                            .text_color(theme::DEL)
                            .child(format!("−{dels}")),
                    )
                })
            })
            .into_any_element()
    }

    fn render_permission(
        &mut self,
        item_id: &str,
        perm: &crate::store::PermissionItem,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let open_diff = self.expanded.contains(&format!("{item_id}:diff"));
        let on_accent = self.store.read(cx).accent.on;
        div()
            .w_full()
            .flex()
            .child(
                div()
                    .debug_selector(|| "permission-card".into())
                    .min_w_0()
                    .max_w(px(CARD_WIDTH))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme::CHANGES)
                    .bg(theme::panel())
                    .overflow_hidden()
                    .flex()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .py_3()
                            .px_3p5()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(crate::ui::icon(
                                        "icons/shield.svg",
                                        px(fonts.icon_small),
                                        theme::CHANGES,
                                    ))
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_body))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme::CHANGES)
                                            .child(SharedString::from(perm.kind.clone())),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(fonts.font_mono_meta))
                                    .font_family(theme::code_font())
                                    .text_color(theme::text_dim())
                                    .child(SharedString::from(perm.detail.clone())),
                            )
                            .when(perm.has_diff(), |d| {
                                d.child(self.fold_header(
                                    &format!("{item_id}:diff"),
                                    "View diff",
                                    None,
                                    open_diff,
                                    fonts,
                                    cx,
                                ))
                                .when(open_diff, |d| {
                                    d.child(
                                        fold_body(gpui::ElementId::NamedInteger(
                                            "perm-diff".into(),
                                            Self::hash(item_id),
                                        ))
                                        .child(
                                            render_diff(
                                                perm.diff.as_deref().unwrap_or(""),
                                                None,
                                                fonts.font_meta,
                                                window,
                                                cx,
                                            ),
                                        ),
                                    )
                                })
                            })
                            .when(perm.is_pending(), |d| {
                                let id = perm.id.clone();
                                let store = self.store.clone();
                                let auto_text = if perm.has_auto_countdown() {
                                    perm.auto_countdown_text()
                                } else {
                                    Default::default()
                                };
                                d.child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap_2()
                                        .pt_1()
                                        .child(crate::ui::button(
                                            gpui::ElementId::NamedInteger(
                                                "pill".into(),
                                                Self::hash("Allow"),
                                            ),
                                            "Allow",
                                            BtnKind::Primary(on_accent),
                                            accent,
                                            fonts.font_control,
                                            true,
                                            {
                                                let store = store.clone();
                                                let id = id.clone();
                                                move |_, _, cx| {
                                                    store.update(cx, |s, cx| {
                                                        s.respond_permission(&id, "once", false, cx)
                                                    });
                                                }
                                            },
                                        ))
                                        .child(crate::ui::button(
                                            gpui::ElementId::NamedInteger(
                                                "pill".into(),
                                                Self::hash("Always"),
                                            ),
                                            "Always",
                                            BtnKind::Chip,
                                            accent,
                                            fonts.font_control,
                                            true,
                                            {
                                                let store = store.clone();
                                                let id = id.clone();
                                                move |_, _, cx| {
                                                    store.update(cx, |s, cx| {
                                                        s.respond_permission(
                                                            &id, "always", false, cx,
                                                        )
                                                    });
                                                }
                                            },
                                        ))
                                        .child(crate::ui::button(
                                            gpui::ElementId::NamedInteger(
                                                "pill".into(),
                                                Self::hash("Deny"),
                                            ),
                                            "Deny",
                                            BtnKind::Chip,
                                            accent,
                                            fonts.font_control,
                                            true,
                                            {
                                                let store = store.clone();
                                                let id = id.clone();
                                                move |_, _, cx| {
                                                    store.update(cx, |s, cx| {
                                                        s.respond_permission(
                                                            &id, "reject", false, cx,
                                                        )
                                                    });
                                                }
                                            },
                                        ))
                                        .when(!auto_text.is_empty(), |d| {
                                            d.child(
                                                div()
                                                    .text_size(px(fonts.font_meta))
                                                    .text_color(theme::text_muted())
                                                    .child(auto_text),
                                            )
                                        }),
                                )
                            })
                            .when(!perm.is_pending(), |d| {
                                d.child(
                                    div()
                                        .italic()
                                        .text_size(px(fonts.font_meta))
                                        .text_color(theme::text_dim())
                                        .child(SharedString::from(
                                            perm.answer.clone().unwrap_or_default(),
                                        )),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_question(
        &mut self,
        item_id: &str,
        question: &crate::store::QuestionItem,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pending = question.is_pending();
        let on_accent = self.store.read(cx).accent.on;
        let mut card = div().flex().flex_col().gap_1p5().flex_1().min_w_0();
        if pending {
            let store = self.store.clone();
            let id = question.id.clone();
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(crate::ui::icon(
                        "icons/chat-outline.svg",
                        px(fonts.icon_small),
                        theme::CHANGES,
                    ))
                    .child(
                        div()
                            .text_size(px(fonts.font_body))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::CHANGES)
                            .child("question"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(fonts.font_meta))
                            .text_color(theme::text_muted())
                            .truncate()
                            .child("pick an answer for each question"),
                    )
                    .child(
                        div()
                            .id(gpui::ElementId::NamedInteger(
                                "dismiss".into(),
                                Self::hash(&id),
                            ))
                            .role(Role::Button)
                            .aria_label("Dismiss question")
                            .flex_shrink_0()
                            .cursor_pointer()
                            .p_1()
                            .rounded(px(4.))
                            .hover(|s| s.bg(theme::raised()))
                            .on_click({
                                let id = id.clone();
                                move |_, _, cx| {
                                    store.update(cx, |s, cx| s.dismiss_question(&id, cx));
                                }
                            })
                            .child(crate::ui::icon(
                                "icons/close.svg",
                                px(fonts.icon_small),
                                theme::text_dim(),
                            )),
                    ),
            );
        }
        for (qi, entry) in question.questions.iter().enumerate() {
            card = card.child(self.render_question_entry(
                self.key.clone(),
                item_id,
                question,
                qi,
                entry,
                fonts,
                accent,
                window,
                cx,
            ));
        }
        if pending {
            let id = question.id.clone();
            let can_submit = question.can_submit();
            let store = self.store.clone();
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .child(crate::ui::button(
                        gpui::ElementId::NamedInteger("answer".into(), Self::hash(&id)),
                        "Answer",
                        BtnKind::Primary(on_accent),
                        accent,
                        fonts.font_control,
                        can_submit,
                        {
                            let store = store.clone();
                            let id = id.clone();
                            move |_, _, cx| {
                                if can_submit {
                                    store.update(cx, |s, cx| s.submit_question(&id, cx));
                                }
                            }
                        },
                    )),
            );
        } else if let Some(resolution) = &question.resolution {
            card = card.child(
                div()
                    .italic()
                    .text_size(px(fonts.font_body))
                    .text_color(theme::text_faint())
                    .child(self.transcript_text(&format!("{item_id}:res"), resolution, "qres")),
            );
        }
        div()
            .w_full()
            .flex()
            .child(
                div()
                    .debug_selector(|| "question-card".into())
                    .min_w_0()
                    .max_w(px(CARD_WIDTH))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(if pending {
                        theme::CHANGES
                    } else {
                        theme::hairline()
                    })
                    .bg(theme::panel())
                    .py_3()
                    .px_3p5()
                    .flex()
                    .child(card),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_question_entry(
        &mut self,
        this_tile_key: String,
        item_id: &str,
        question: &crate::store::QuestionItem,
        qi: usize,
        entry: &QuestionEntry,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let input_key = format!("{}#{qi}", question.id);
        self.question_keys_seen.insert(input_key.clone());
        // custom-answer field, created lazily as its own leaf view: typing
        // must not re-render the transcript (see QuestionField below)
        if let std::collections::hash_map::Entry::Vacant(slot) =
            self.question_inputs.entry(input_key.clone())
        {
            let store = self.store.clone();
            let qid = question.id.clone();
            let this_key = self.key.clone();
            let tile = cx.entity();
            let field = cx.new(|cx| {
                let state = cx.new(|cx| InputState::new(window, cx).placeholder("custom answer…"));
                cx.subscribe(
                    &state,
                    move |this: &mut QuestionField, field, event: &InputEvent, cx| {
                        if !matches!(event, InputEvent::Change) {
                            return;
                        }
                        let text = field.read(cx).value().to_string();
                        let filled = !text.trim().is_empty();
                        let flip = filled != this.filled;
                        this.filled = filled;
                        let store = this.store.clone();
                        let tile_key = this.tile_key.clone();
                        let question_id = this.question_id.clone();
                        let qi = this.qi;
                        store.update(cx, move |s, _| {
                            s.set_question_custom(&tile_key, &question_id, qi, text)
                        });
                        if flip {
                            // the Answer button's can_submit only moves when some
                            // entry's filled state flips; every other keystroke
                            // stays inside this leaf
                            this.tile.update(cx, |_, cx| cx.notify());
                        }
                    },
                )
                .detach();
                QuestionField {
                    state,
                    store,
                    tile,
                    tile_key: this_key,
                    question_id: qid,
                    qi,
                    filled: false,
                }
            });
            slot.insert(field);
        }
        let field = self.question_inputs.get(&input_key).cloned().unwrap();
        let entry_locked = entry.locked;
        let mut column = div().flex().flex_col().gap_1().pb_1();
        column = column
            .when(!entry.header.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(fonts.font_meta))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::text_faint())
                        .child(SharedString::from(entry.header.clone())),
                )
            })
            .child(
                div()
                    .text_size(px(fonts.font_body))
                    .text_color(theme::text())
                    .child(self.transcript_text(
                        &format!("{item_id}#{qi}"),
                        &entry.question,
                        "qtext",
                    )),
            );
        for (oi, option) in entry.options.iter().enumerate() {
            let store = self.store.clone();
            let qid = question.id.clone();
            let this_key2 = this_tile_key.clone();
            let is_multi = entry.multiple;
            let selected = option.is_selected;
            let label = option.label.clone();
            let description = option.description.clone();
            column = column.child(
                div()
                    .id(gpui::ElementId::NamedInteger(
                        "qopt".into(),
                        Self::hash(&format!("{item_id}:{qi}:{oi}")),
                    ))
                    .role(if is_multi {
                        Role::CheckBox
                    } else {
                        Role::RadioButton
                    })
                    .aria_label(SharedString::from(format!(
                        "{} — {}",
                        entry
                            .question
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(60)
                            .collect::<String>(),
                        option.label
                    )))
                    .aria_toggled(if selected {
                        Toggled::True
                    } else {
                        Toggled::False
                    })
                    .w_full()
                    .flex()
                    .items_start()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(theme::line_strong())
                    .when(selected, |d| d.bg(theme::q_opt_selected()))
                    .when(!selected, |d| d.hover(|s| s.bg(theme::raised())))
                    .cursor_pointer()
                    .when(!entry_locked, |d| {
                        d.on_click({
                            let store = store.clone();
                            let qid = qid.clone();
                            move |_, _, cx| {
                                store.update(cx, |s, cx| {
                                    s.toggle_question_option(
                                        &this_key2, &qid, qi, oi, is_multi, cx,
                                    );
                                });
                            }
                        })
                    })
                    .child(
                        // marker: checkbox for multi, radio for single
                        if is_multi {
                            if selected {
                                div()
                                    .size(px(13.))
                                    .mt_1()
                                    .rounded_sm()
                                    .bg(accent)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(crate::ui::icon(
                                        "icons/check.svg",
                                        px(fonts.icon_nano),
                                        theme::surface(),
                                    ))
                                    .into_any_element()
                            } else {
                                div()
                                    .size(px(13.))
                                    .mt_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(theme::text_muted())
                                    .into_any_element()
                            }
                        } else if selected {
                            crate::ui::icon("icons/check.svg", px(fonts.icon_small), accent)
                                .into_any_element()
                        } else {
                            div()
                                .size(px(12.))
                                .mt_0p5()
                                .rounded_full()
                                .border_1()
                                .border_color(theme::text_muted())
                                .into_any_element()
                        },
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(fonts.font_body))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child(SharedString::from(label)),
                            )
                            .when(!description.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_size(px(fonts.font_meta))
                                        .text_color(theme::text_faint())
                                        .child(SharedString::from(description)),
                                )
                            }),
                    ),
            );
        }
        if !entry_locked {
            column = column.child(div().px_1().child(field));
        }
        column.into_any_element()
    }

    pub fn hash(s: &str) -> u64 {
        let mut hash: u64 = 5381;
        for b in s.as_bytes() {
            hash = hash.wrapping_mul(33) ^ *b as u64;
        }
        hash
    }
}

/// Everything one render pass needs from the store, snapshotted up front so
/// the store borrow is released before the element tree is built.
struct TileSnap {
    fonts: theme::FontTokens,
    accent: gpui::Rgba,
    accent_text: gpui::Rgba,
    input: String,
    is_busy: bool,
    is_connected: bool,
    is_reconnecting: bool,
    is_focused: bool,
    title: String,
    ctx: String,
    ctx_frac: f32,
    has_ctx: bool,
    items: Vec<(String, Arc<ChatItem>)>,
    queue_keys: Vec<usize>,
    queue: Vec<crate::store::QueueEntry>,
    attachments: Vec<crate::store::AttachmentItem>,
    has_queue: bool,
    has_messages: bool,
    has_variants: bool,
    selected_variant: String,
    selected_model_short: String,
    selected_agent: String,
    has_agents: bool,
    /// Backend of this chat: label for the harness pill, disabled once the
    /// session exists (a chat is bound to the server it was created on).
    harness_label: &'static str,
    harness_locked: bool,
    has_tasks_or_agents: bool,
    has_tasks: bool,
    tasks_progress: String,
    tasks: Vec<crate::store::TaskItem>,
    has_tile_agents: bool,
    agents_summary: String,
    tile_agents: Vec<crate::store::RunningAgentItem>,
    multiple_workspaces: bool,
    /// What this chat waits on, if a person has to answer it.
    waiting: Option<&'static str>,
    density: TileDensity,
    allow_on: bool,
    /// MCP toggle: any servers configured for the tile's workspace, and
    /// whether the toggle currently keeps them enabled
    has_mcp: bool,
    mcp_on: bool,
    spring: bool,
}

impl TileView {
    /// Header row: close, busy pulse, title, then the right cluster —
    /// reconnecting pill, context ring readout, tasks/agents toggle.
    fn render_header(&mut self, snap: &TileSnap, cx: &mut Context<Self>) -> gpui::AnyElement {
        let fonts = snap.fonts;
        let close_store = self.store.clone();
        let close_key = self.key.clone();

        let mut header_right = div().flex_shrink_0().flex().items_center().gap_1p5();
        // stream-status pill: amber while this tile's workspace pump retries
        // its event stream
        if snap.is_reconnecting {
            header_right = header_right.child(
                tip(
                    div()
                        .id("reconnect-pill")
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .px_2()
                        .py_1()
                        .rounded_lg()
                        .bg(theme::tint(theme::WARN)),
                    "Connection lost — retrying…",
                )
                .child(
                    div()
                        .size(px(5.))
                        .rounded_full()
                        .bg(theme::WARN)
                        .flex_shrink_0(),
                )
                .child(
                    div()
                        .text_size(px(fonts.font_meta))
                        .text_color(theme::WARN)
                        .child("reconnecting"),
                ),
            );
        }
        // the context readout: the ring and its numbers, and the way into the
        // flyout when the chat has tasks
        let readout = if snap.density == TileDensity::Full {
            snap.ctx.as_str()
        } else {
            snap.ctx
                .split_once(" · ")
                .map_or(snap.ctx.as_str(), |(_, numbers)| numbers)
        };
        let readout = div()
            .flex()
            .items_center()
            .gap_1p5()
            .when(snap.has_ctx, |d| {
                d.child(context_ring(snap.ctx_frac)).child(
                    div()
                        .max_w(px(180.))
                        .text_size(px(fonts.font_meta))
                        .text_color(theme::text_dim())
                        .truncate()
                        .child(SharedString::from(readout.to_string())),
                )
            });
        if snap.has_tasks_or_agents {
            header_right = header_right.child(
                tip(
                    div()
                        .id("tasks-btn")
                        .debug_selector(|| "tile-flyout-toggle".into())
                        .role(Role::Button)
                        .aria_label("Tasks & agents")
                        .aria_expanded(self.flyout_open)
                        .px_1p5()
                        .py_0p5()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::raised()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.flyout_open = !this.flyout_open;
                            cx.notify();
                        })),
                    "Tasks & agents",
                )
                .child(if snap.has_ctx {
                    readout.into_any_element()
                } else {
                    crate::ui::icon("icons/expand.svg", px(fonts.icon_small), theme::text_dim())
                        .into_any_element()
                }),
            );
        } else if snap.has_ctx {
            header_right = header_right.child(
                tip(
                    div()
                        .id("ctx-readout")
                        .role(Role::Label)
                        .aria_label(SharedString::from(format!("Context used: {}", snap.ctx)))
                        .px_1p5(),
                    format!("Context used: {}", snap.ctx),
                )
                .child(readout),
            );
        }

        let state = snap
            .waiting
            .map(|word| (word, true))
            .or((snap.is_busy && snap.density != TileDensity::Tight).then_some(("Running", false)));

        div()
            .debug_selector(|| "tile-head".into())
            .flex_shrink_0()
            .h(px(shell::scaled(TILE_HEAD_HEIGHT)))
            .flex()
            .items_center()
            .gap_1p5()
            .pl_2()
            .pr_1p5()
            .border_b_1()
            // the head's hairline is the quiet one (line-soft)
            .border_color(theme::border_light())
            .child(
                tip(
                    div()
                        .id("close-tile")
                        .role(Role::Button)
                        .aria_label(SharedString::from(format!("Close chat {}", snap.title)))
                        .flex_shrink_0()
                        .cursor_pointer()
                        .p_1()
                        .rounded(px(4.))
                        .hover(|s| s.bg(theme::raised()))
                        .on_click(move |_, _, cx| {
                            close_store.update(cx, |s, cx| s.close_tile(&close_key, cx));
                        }),
                    "Close",
                )
                .child(crate::ui::icon(
                    "icons/close.svg",
                    px(fonts.icon_small),
                    theme::text_dim(),
                )),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(px(fonts.font_name))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .truncate()
                    .child(SharedString::from(snap.title.clone())),
            )
            .when_some(state, |d, (word, needs_you)| {
                let ink = if needs_you {
                    theme::CHANGES
                } else {
                    theme::BEHIND
                };
                let mut stat = div()
                    .debug_selector(|| "tile-state".into())
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(fonts.font_meta))
                    .text_color(ink);
                if needs_you {
                    stat = stat
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(crate::ui::icon(
                            if word == "Needs approval" {
                                "icons/shield.svg"
                            } else {
                                "icons/chat-outline.svg"
                            },
                            px(fonts.icon_tiny),
                            theme::CHANGES,
                        ));
                } else {
                    // Running: the same spinner the tree uses, in `behind`
                    stat = stat.child(motion::spinner(
                        gpui::ElementId::Name(SharedString::from(format!(
                            "tile-state-spin-{}",
                            snap.title
                        ))),
                        "icons/spinner.svg",
                        px(fonts.icon_tiny),
                        theme::BEHIND,
                    ));
                }
                d.child(stat.child(word))
            })
            .when(
                snap.is_busy && snap.waiting.is_none() && snap.density == TileDensity::Tight,
                |d| {
                    d.child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(theme::WORKING)
                            .flex_shrink_0(),
                    )
                },
            )
            .child(div().flex_1())
            .child(header_right)
            .into_any_element()
    }

    /// Queued messages: Warp-style full-width rows with Steer / Edit / Remove.
    fn render_queue_strip(&mut self, snap: &TileSnap) -> gpui::AnyElement {
        let fonts = snap.fonts;
        let mut queue_rows_el = div().flex().flex_col().gap_1();
        for qkey in &snap.queue_keys {
            if let Some(entry) = snap.queue.iter().find(|e| e.key == *qkey) {
                let text = entry.text.clone();
                let count = entry.attachments.len();
                let store_handle = self.store.clone();
                let steer_key = entry.key;
                let edit_key = entry.key;
                let remove_key = entry.key;
                queue_rows_el = queue_rows_el.child(
                    div()
                        .debug_selector(|| "queue-row".into())
                        .w_full()
                        .min_h(px(36.))
                        .rounded(px(6.))
                        .bg(theme::panel())
                        .pl_3()
                        .pr_1p5()
                        .py_1()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(crate::ui::icon(
                            "icons/clock.svg",
                            px(fonts.icon_tiny),
                            theme::text_muted(),
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(fonts.font_body))
                                        .text_color(theme::text())
                                        .truncate()
                                        .child(SharedString::from(text)),
                                )
                                .when(count > 0, |d| {
                                    d.child(
                                        div()
                                            .text_size(px(fonts.font_meta))
                                            .text_color(theme::text_muted())
                                            .child(format!("{count} attachment(s)")),
                                    )
                                }),
                        )
                        .child(
                            tip(
                                div()
                                    .id(gpui::ElementId::NamedInteger(
                                        "steer".into(),
                                        steer_key as u64,
                                    ))
                                    .role(Role::Button)
                                    .aria_label(SharedString::from(format!(
                                        "Steer — {}",
                                        entry.text.chars().take(40).collect::<String>()
                                    )))
                                    .flex_shrink_0()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(px(6.))
                                    .hover(|s| s.bg(theme::raised()))
                                    .cursor_pointer()
                                    .text_size(px(fonts.font_control))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(snap.accent_text),
                                "Abort the running turn and send this next",
                            )
                            .child("Steer")
                            .on_click({
                                let store = store_handle.clone();
                                move |_, _, cx| {
                                    store.update(cx, |s, cx| s.steer_queued(steer_key, cx));
                                }
                            }),
                        )
                        .child(
                            tip(
                                div()
                                    .id(gpui::ElementId::NamedInteger(
                                        "qedit".into(),
                                        edit_key as u64,
                                    ))
                                    .role(Role::Button)
                                    .aria_label(SharedString::from(format!(
                                        "Edit queued — {}",
                                        entry.text.chars().take(40).collect::<String>()
                                    )))
                                    .flex_shrink_0()
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded(px(4.))
                                    .hover(|s| s.bg(theme::raised())),
                                "Edit in the composer",
                            )
                            .on_click({
                                let store = store_handle.clone();
                                move |_, _, cx| {
                                    store.update(cx, |s, cx| s.edit_queued(edit_key, cx));
                                }
                            })
                            .child(crate::ui::icon(
                                "icons/edit.svg",
                                px(fonts.icon_small),
                                theme::text_muted(),
                            )),
                        )
                        .child(
                            tip(
                                div()
                                    .id(gpui::ElementId::NamedInteger(
                                        "qrm".into(),
                                        remove_key as u64,
                                    ))
                                    .role(Role::Button)
                                    .aria_label(SharedString::from(format!(
                                        "Remove queued — {}",
                                        entry.text.chars().take(40).collect::<String>()
                                    )))
                                    .flex_shrink_0()
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded(px(4.))
                                    .hover(|s| s.bg(theme::raised())),
                                "Remove from the queue",
                            )
                            .on_click({
                                let store = store_handle.clone();
                                move |_, _, cx| {
                                    store.update(cx, |s, cx| s.remove_queued(remove_key, cx));
                                }
                            })
                            .child(crate::ui::icon(
                                "icons/close.svg",
                                px(fonts.icon_small),
                                theme::text_muted(),
                            )),
                        ),
                );
            }
        }
        queue_rows_el.into_any_element()
    }

    /// Composer attachment chips: thumbnail or doc glyph, with a remove X.
    fn render_attachments(&mut self, snap: &TileSnap) -> gpui::AnyElement {
        let fonts = snap.fonts;
        let mut attachments_el = div();
        if !snap.attachments.is_empty() {
            let mut strip = div().flex().gap_1p5();
            for att in &snap.attachments {
                let store_handle = self.store.clone();
                let akey = att.key;
                let key = self.key.clone();
                let mut chip = div()
                    .rounded_lg()
                    .bg(theme::chip())
                    .p_1()
                    .flex()
                    .flex_col()
                    .items_center();
                if att.is_image() {
                    if let Some(render) = self.image_for(&format!("att{akey}"), &att.data) {
                        chip = chip.child(
                            img(render)
                                .size(px(56.))
                                .object_fit(gpui::ObjectFit::Cover)
                                .rounded_sm(),
                        );
                    }
                } else {
                    chip = chip.child(crate::ui::icon(
                        "icons/doc.svg",
                        px(64.),
                        theme::text_muted(),
                    ));
                }
                chip = chip.child(
                    div()
                        .id(gpui::ElementId::NamedInteger("att-rm".into(), akey as u64))
                        .role(Role::Button)
                        .aria_label(SharedString::from(format!(
                            "Remove attachment {}",
                            att.file_name
                        )))
                        .cursor_pointer()
                        .on_click({
                            let store_handle = store_handle.clone();
                            let key = key.clone();
                            move |_, _, cx| {
                                let key = key.clone();
                                store_handle
                                    .update(cx, move |s, cx| s.remove_attachment(&key, akey, cx));
                            }
                        })
                        .child(crate::ui::icon(
                            "icons/close.svg",
                            px(fonts.icon_small),
                            theme::text_muted(),
                        )),
                );
                strip = strip.child(chip);
            }
            attachments_el = strip;
        }
        attachments_el.into_any_element()
    }

    /// The chrome-less selector pills: agent, model and variant.
    fn render_pills(&mut self, snap: &TileSnap, cx: &mut Context<Self>) -> gpui::AnyElement {
        let fonts = snap.fonts;
        let accent_text = snap.accent_text;
        let locked_small = snap.harness_locked && snap.density != TileDensity::Full;
        // each pill reports its own window bounds, so a dropdown opens
        // against the pill that owns it instead of the tile's corner
        let agent_bounds = self.pill_bounds[PickerKind::Agent as usize].clone();
        let model_bounds = self.pill_bounds[PickerKind::Model as usize].clone();
        let variant_bounds = self.pill_bounds[PickerKind::Variant as usize].clone();
        // the one that gives way is the model: the rest keep their words
        let mut pills = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap_0p5()
            .overflow_hidden();
        // harness pill: click toggles opencode ↔ dsh on a draft; a chat that
        // already has a session is bound to the server it was created on
        let next_harness = if snap.harness_label == "dsh" {
            "opencode"
        } else {
            "dsh"
        };
        pills = pills.child(
            tip(
                div()
                    .id("pill-harness")
                    .debug_selector(|| "pill-harness".into())
                    .flex_shrink_0()
                    .role(Role::Button)
                    .aria_label(SharedString::from(format!(
                        "Harness: {} — click to use {}",
                        snap.harness_label, next_harness
                    )))
                    .flex()
                    .items_center()
                    .gap_1()
                    .h(px(PILL_HEIGHT))
                    .px_1p5()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(theme::text_muted())
                    .when(!snap.harness_locked, |d| {
                        d.text_color(theme::text())
                            .hover(|s| s.bg(theme::raised()).text_color(accent_text))
                    })
                    .when(snap.harness_locked, |d| d.cursor_default()),
                if snap.harness_locked {
                    format!("runs on {} — new chats can switch", snap.harness_label)
                } else {
                    format!("Harness — click to use {}", next_harness)
                },
            )
            .on_click(cx.listener(|this, _, window, cx| this.toggle_tile_harness(window, cx)))
            .on_a11y_action(
                AccessibleAction::Click,
                TileView::a11y_activator(cx, TileView::toggle_tile_harness),
            )
            .when(snap.harness_locked, |d| {
                d.child(crate::ui::icon(
                    "icons/spark.svg",
                    px(fonts.icon_tiny),
                    theme::text_muted(),
                ))
            })
            .when(!locked_small, |d| {
                d.child(
                    div()
                        .debug_selector(|| "pill-harness-word".into())
                        .text_size(px(fonts.font_meta))
                        .child(SharedString::from(snap.harness_label)),
                )
            })
            .when(!snap.harness_locked, |d| {
                d.child(crate::ui::icon(
                    "icons/chevron-down.svg",
                    px(fonts.icon_tiny),
                    theme::text_muted(),
                ))
            }),
        );
        if snap.has_agents {
            pills = pills.child(
                tip(
                    div()
                        .id("pill-agent")
                        .flex_shrink_0()
                        .role(Role::ComboBox)
                        .aria_label("Agent")
                        .aria_value(SharedString::from(if snap.selected_agent.is_empty() {
                            crate::store::DEFAULT_AGENT_ENTRY
                        } else {
                            snap.selected_agent.as_str()
                        }))
                        .aria_expanded(matches!(self.picker, Some(PickerKind::Agent)))
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(PILL_HEIGHT))
                        .px_1p5()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(theme::text())
                        .hover(|s| s.bg(theme::raised()).text_color(accent_text)),
                    "Agent",
                )
                .on_prepaint(move |bounds, _, _| agent_bounds.set(Some(bounds)))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_agent_picker(window, cx)))
                .on_a11y_action(
                    AccessibleAction::Click,
                    TileView::a11y_activator(cx, TileView::toggle_agent_picker),
                )
                .child(
                    div()
                        .text_size(px(fonts.font_meta))
                        .child(SharedString::from(if snap.selected_agent.is_empty() {
                            crate::store::DEFAULT_AGENT_ENTRY.to_string()
                        } else {
                            snap.selected_agent.clone()
                        })),
                )
                .child(crate::ui::icon(
                    "icons/chevron-down.svg",
                    px(fonts.icon_tiny),
                    theme::text_muted(),
                )),
            );
        }
        pills = pills
            .child(
                tip(
                    div()
                        .id("pill-model")
                        .debug_selector(|| "pill-model".into())
                        .min_w(px(56.))
                        .role(Role::ComboBox)
                        .aria_label("Model")
                        .aria_value(SharedString::from(
                            if snap.selected_model_short.is_empty() {
                                crate::store::DEFAULT_MODEL_ENTRY
                            } else {
                                snap.selected_model_short.as_str()
                            },
                        ))
                        .aria_expanded(matches!(self.picker, Some(PickerKind::Model)))
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(PILL_HEIGHT))
                        .px_1p5()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(theme::text())
                        .hover(|s| s.bg(theme::raised()).text_color(accent_text)),
                    "Model",
                )
                .on_prepaint(move |bounds, _, _| model_bounds.set(Some(bounds)))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_model_picker(window, cx)))
                .on_a11y_action(
                    AccessibleAction::Click,
                    TileView::a11y_activator(cx, TileView::toggle_model_picker),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(fonts.font_meta))
                        .child(SharedString::from(
                            if snap.selected_model_short.is_empty() {
                                crate::store::DEFAULT_MODEL_ENTRY.to_string()
                            } else {
                                snap.selected_model_short.clone()
                            },
                        )),
                )
                .child(crate::ui::icon(
                    "icons/chevron-down.svg",
                    px(fonts.icon_tiny),
                    theme::text_muted(),
                )),
            )
            .child(
                tip(
                    div()
                        .id("pill-variant")
                        .flex_shrink_0()
                        .role(Role::ComboBox)
                        .aria_label("Response variant")
                        .aria_value(SharedString::from(snap.selected_variant.clone()))
                        .aria_expanded(matches!(self.picker, Some(PickerKind::Variant)))
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(PILL_HEIGHT))
                        .px_1p5()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(snap.has_variants, |d| {
                            d.text_color(theme::text())
                                .hover(|s| s.bg(theme::raised()).text_color(accent_text))
                        })
                        .when(!snap.has_variants, |d| d.opacity(0.4)),
                    "Response variant",
                )
                .on_prepaint(move |bounds, _, _| variant_bounds.set(Some(bounds)))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_variant_picker(window, cx)))
                .on_a11y_action(
                    AccessibleAction::Click,
                    TileView::a11y_activator(cx, TileView::toggle_variant_picker),
                )
                .child(crate::ui::icon(
                    "icons/gauge.svg",
                    px(fonts.icon_small),
                    theme::text_muted(),
                ))
                .when(snap.density != TileDensity::Tight, |d| {
                    d.child(
                        div()
                            .debug_selector(|| "pill-variant-word".into())
                            .text_size(px(fonts.font_meta))
                            .child(SharedString::from(snap.selected_variant.clone())),
                    )
                })
                .child(crate::ui::icon(
                    "icons/chevron-down.svg",
                    px(fonts.icon_tiny),
                    theme::text_muted(),
                )),
            );

        pills.into_any_element()
    }

    /// AT activation for the pill row: gpui's fallback for the Click action
    /// synthesizes a mouse event at the node's reported bounds, and those
    /// land off the painted control for these pills (the composer row hugs
    /// the card's clipped bottom edge), so the fallback click never reaches
    /// them. The direct listener runs the same handler the mouse uses.
    fn a11y_activator(
        cx: &Context<Self>,
        activate: fn(&mut Self, &mut Window, &mut Context<Self>),
    ) -> impl FnMut(Option<&gpui::accesskit::ActionData>, &mut Window, &mut App) + 'static {
        let listener = cx.listener(move |this, _: &(), window, cx| activate(this, window, cx));
        move |_, window, cx| listener(&(), window, cx)
    }

    /// Harness pill: toggles opencode ↔ dsh on a draft; a chat that already
    /// has a session is bound to the server it was created on.
    fn toggle_tile_harness(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.key.clone();
        let store = self.store.clone();
        let locked = store
            .read(cx)
            .tile_by_key(&key)
            .map(|t| t.session_id.is_some())
            .unwrap_or(true);
        if locked {
            return;
        }
        let is_dsh = store
            .read(cx)
            .tile_by_key(&key)
            .map(|t| t.harness == HarnessKind::Dsh)
            .unwrap_or(false);
        let next = HarnessKind::from_label(if is_dsh { "opencode" } else { "dsh" });
        store.update(cx, |s, cx| s.set_tile_harness(&key, next, cx));
        // the kit placeholder was baked in at construction; the flip rewrites
        // it (the leaf's aria_placeholder follows via notify)
        let label = next.label();
        self.composer.update(cx, |state, cx| {
            state.set_placeholder(format!("Ask {label}…"), window, cx);
        });
        self.composer_field.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    fn toggle_agent_picker(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.picker = if matches!(self.picker, Some(PickerKind::Agent)) {
            None
        } else {
            Some(PickerKind::Agent)
        };
        self.picker_placement = self.picker.and_then(|kind| self.picker_anchor(kind));
        cx.notify();
    }

    fn toggle_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.picker, Some(PickerKind::Model)) {
            if let Some(picker) = &self.model_picker {
                picker.update(cx, |p, cx| p.set_open(false, window, cx));
            }
            self.picker = None;
        } else {
            self.picker = Some(PickerKind::Model);
            let bounds = self.picker_bounds(PickerKind::Model);
            let picker = self.ensure_model_picker(window, cx);
            picker.update(cx, |p, cx| {
                if let Some((trigger, host)) = bounds {
                    p.set_composer_anchor(trigger, host, cx);
                }
                // the host mounts the picker only while the pill is open, so
                // the entity's own flag is raised again on every open: a
                // dismissal or a pick leaves it false, and a stale false made
                // the catcher's click-elsewhere a no-op
                p.set_open(true, window, cx);
                p.reset_filter(window, cx);
            });
        }
        cx.notify();
    }

    fn toggle_variant_picker(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self
            .store
            .read(cx)
            .tile_by_key(&self.key)
            .map(|t| t.has_variants())
            .unwrap_or(false)
        {
            self.picker = if matches!(self.picker, Some(PickerKind::Variant)) {
                None
            } else {
                Some(PickerKind::Variant)
            };
            self.picker_placement = self.picker.and_then(|kind| self.picker_anchor(kind));
            cx.notify();
        }
    }

    /// Stop while a turn runs, beside Send: text sent then lands in the queue.
    /// Send mutes out while the workspace is disconnected.
    fn render_send_button(&self, snap: &TileSnap) -> gpui::AnyElement {
        let fonts = snap.fonts;
        let accent_text = snap.accent_text;
        let square = |id: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .role(Role::Button)
                .flex_shrink_0()
                .size(px(SEND_SIZE))
                .rounded(px(6.))
                .border_1()
                .border_color(theme::line_strong())
                .bg(theme::panel())
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
        };
        let mut row = div().flex_shrink_0().flex().items_center().gap_1().ml_1();
        if snap.is_busy {
            let store = self.store.clone();
            let key = self.key.clone();
            let a11y_store = store.clone();
            let a11y_key = key.clone();
            row = row.child(
                tip(
                    square("stop")
                        .aria_label("Stop")
                        .hover(|s| s.bg(theme::raised()))
                        .on_click(move |_, _, cx| {
                            store.update(cx, |s, cx| s.abort_tile(&key, cx));
                        })
                        .on_a11y_action(AccessibleAction::Click, move |_, _, cx| {
                            a11y_store.update(cx, |s, cx| s.abort_tile(&a11y_key, cx));
                        }),
                    "Stop",
                )
                .child(crate::ui::icon(
                    "icons/stop.svg",
                    px(fonts.icon_small),
                    theme::text_dim(),
                )),
            );
        }
        let store = self.store.clone();
        let key = self.key.clone();
        let a11y_store = store.clone();
        let a11y_key = key.clone();
        row.child(
            tip(
                square("send")
                    .aria_label("Send")
                    .when(snap.is_connected, |d| {
                        d.hover(|s| s.bg(theme::raised()).text_color(accent_text))
                    })
                    .when(!snap.is_connected, |d| d.opacity(0.4))
                    .on_click(move |_, _, cx| {
                        store.update(cx, |s, cx| s.send_from_tile(&key, cx));
                    })
                    .on_a11y_action(AccessibleAction::Click, move |_, _, cx| {
                        a11y_store.update(cx, |s, cx| s.send_from_tile(&a11y_key, cx));
                    }),
                "Send",
            )
            .child(crate::ui::icon(
                "icons/arrow-up.svg",
                px(fonts.icon_small),
                theme::text(),
            )),
        )
        .into_any_element()
    }

    /// The tasks/agents flyout, anchored under the header toggle.
    fn render_flyout(&self, snap: &TileSnap) -> gpui::AnyElement {
        let fonts = snap.fonts;
        let accent = snap.accent;
        let mut flyout = div();
        if self.flyout_open && snap.has_tasks_or_agents {
            let mut content = div().flex().flex_col().gap_2().w(px(300.));
            if snap.has_tasks {
                content = content
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(fonts.font_brand))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child("Progress"),
                            )
                            .child(
                                div()
                                    .text_size(px(fonts.font_meta))
                                    .text_color(theme::text_muted())
                                    .child(SharedString::from(snap.tasks_progress.clone())),
                            ),
                    )
                    .child({
                        let mut list = div().flex().flex_col();
                        for task in &snap.tasks {
                            // all-gray rows; status reads from brightness:
                            // active row brightest, finished dimmest
                            let (icon, text) = if task.completed() {
                                (
                                    crate::ui::icon(
                                        "icons/circle-check.svg",
                                        px(fonts.icon_small),
                                        theme::SUCCESS_BRIGHT,
                                    )
                                    .into_any_element(),
                                    theme::text_dim(),
                                )
                            } else if task.in_progress() {
                                (
                                    motion::spinner(
                                        "task-spinner",
                                        "icons/spinner.svg",
                                        px(fonts.icon_small),
                                        theme::BEHIND,
                                    ),
                                    theme::text(),
                                )
                            } else if task.cancelled() {
                                (
                                    crate::ui::icon(
                                        "icons/dash.svg",
                                        px(fonts.icon_small),
                                        theme::text_muted(),
                                    )
                                    .into_any_element(),
                                    theme::text_muted(),
                                )
                            } else {
                                (
                                    crate::ui::icon(
                                        "icons/clock.svg",
                                        px(fonts.icon_small),
                                        theme::text_muted(),
                                    )
                                    .into_any_element(),
                                    theme::text_dim(),
                                )
                            };
                            list = list.child(
                                div().flex().items_start().gap_2().py_1().child(icon).child(
                                    div()
                                        .min_w_0()
                                        .text_size(px(fonts.font_body))
                                        .text_color(text)
                                        .child(SharedString::from(task.content.clone())),
                                ),
                            );
                        }
                        list
                    });
            }
            if snap.has_tasks && snap.has_tile_agents {
                content = content.child(div().h(px(1.)).bg(theme::hairline()));
            }
            if snap.has_tile_agents {
                content = content
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(fonts.font_brand))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child("Agents"),
                            )
                            .child(
                                div()
                                    .text_size(px(fonts.font_meta))
                                    .text_color(theme::text_muted())
                                    .child(SharedString::from(snap.agents_summary.clone())),
                            ),
                    )
                    .child({
                        let mut list = div().flex().flex_col();
                        for agent in &snap.tile_agents {
                            let store_handle = self.store.clone();
                            let sid_open = agent.session_id.clone();
                            let sid_stop = agent.session_id.clone();
                            let is_subagent = agent.is_subagent;
                            list = list.child(
                                div()
                                    // the whole row opens the agent's chat; the stop
                                    // button below keeps itself click-exclusive
                                    .id(gpui::ElementId::NamedInteger(
                                        "open-agent".into(),
                                        TileView::hash(&sid_open),
                                    ))
                                    .role(Role::Button)
                                    .aria_label(SharedString::from(format!(
                                        "Open agent {}",
                                        agent.title
                                    )))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::HOVER))
                                    .on_click({
                                        let store = store_handle.clone();
                                        move |_, _, cx| {
                                            store.update(cx, |s, cx| {
                                                if is_subagent {
                                                    s.open_subagent_session(&sid_open, cx);
                                                } else {
                                                    s.open_running(&sid_open, cx);
                                                }
                                            });
                                        }
                                    })
                                    .child(div().size(px(5.)).rounded_full().bg(accent))
                                    .when(agent.is_subagent, |d| {
                                        d.child(crate::ui::icon(
                                            "icons/play.svg",
                                            px(fonts.icon_nano),
                                            accent,
                                        ))
                                    })
                                    .when(agent.needs_attention, |d| {
                                        d.child(crate::ui::icon(
                                            "icons/warn.svg",
                                            px(fonts.icon_medium),
                                            theme::WARN,
                                        ))
                                    })
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .max_w(px(220.))
                                                    .text_size(px(fonts.font_body))
                                                    .text_color(theme::text())
                                                    .truncate()
                                                    .child(SharedString::from(agent.title.clone())),
                                            )
                                            .when(snap.multiple_workspaces, |d| {
                                                d.child(
                                                    div()
                                                        .text_size(px(fonts.font_meta))
                                                        .text_color(theme::text_muted())
                                                        .child(SharedString::from(
                                                            agent.workspace.clone(),
                                                        )),
                                                )
                                            }),
                                    )
                                    .child(div().flex_1())
                                    .child(crate::ui::icon(
                                        "icons/chevron-right.svg",
                                        px(fonts.icon_tiny),
                                        theme::text_muted(),
                                    ))
                                    .child(
                                        div()
                                            .id(gpui::ElementId::NamedInteger(
                                                "stop-agent".into(),
                                                TileView::hash(&sid_stop),
                                            ))
                                            .role(Role::Button)
                                            .aria_label(SharedString::from(format!(
                                                "Stop agent {}",
                                                agent.title
                                            )))
                                            .cursor_pointer()
                                            .p_1()
                                            .rounded_sm()
                                            .hover(|s| s.bg(theme::HOVER))
                                            .on_click({
                                                let store = store_handle.clone();
                                                move |_, _, cx| {
                                                    cx.stop_propagation(); // the row under this button must not open
                                                    store.update(cx, |s, cx| {
                                                        s.stop_running(&sid_stop, cx)
                                                    });
                                                }
                                            })
                                            .child(crate::ui::icon(
                                                "icons/stop.svg",
                                                px(fonts.icon_small),
                                                theme::text_muted(),
                                            )),
                                    ),
                            );
                        }
                        list
                    });
            }
            flyout = div().absolute().top(px(40.)).right_2().child(
                div()
                    .debug_selector(|| "tile-flyout".into())
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme::line_strong())
                    .bg(theme::overlay())
                    .p_3()
                    .shadow_lg()
                    .id("flyout-body")
                    .max_h(px(460.))
                    .overflow_y_scroll()
                    .child(content),
            );
        }
        flyout.into_any_element()
    }

    /// Create (once) the filtered model dropdown for the composer pill. It
    /// applies picks and closes itself through these callbacks; the tile is
    /// held weakly so picker ↔ tile can't keep each other alive.
    fn ensure_model_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<FilteredPicker> {
        if let Some(picker) = &self.model_picker {
            return picker.clone();
        }
        let store = self.store.clone();
        let key = self.key.clone();
        let tile = cx.entity().downgrade();
        let selected = {
            let store = store.clone();
            let key = key.clone();
            Box::new(move |cx: &App| {
                store
                    .read(cx)
                    .tile_by_key(&key)
                    .and_then(|t| t.selected_model.clone())
                    .filter(|m| m != DEFAULT_MODEL_ENTRY)
                    .unwrap_or_default()
            }) as Box<dyn Fn(&App) -> String>
        };
        let pick = {
            let store = store.clone();
            let key = key.clone();
            let tile = tile.clone();
            Box::new(move |value: &str, cx: &mut App| {
                let model = if value.is_empty() {
                    Some(DEFAULT_MODEL_ENTRY.into())
                } else {
                    Some(value.to_string())
                };
                store.update(cx, |s, _| s.set_tile_model(&key, model));
                if let Some(tile) = tile.upgrade() {
                    tile.update(cx, |tv, cx| {
                        tv.picker = None;
                        cx.notify();
                    });
                }
            }) as Box<dyn Fn(&str, &mut App)>
        };
        let close = {
            let tile = tile.clone();
            Box::new(move |cx: &mut App| {
                if let Some(tile) = tile.upgrade() {
                    tile.update(cx, |tv, cx| {
                        tv.picker = None;
                        cx.notify();
                    });
                }
            }) as Box<dyn Fn(&mut App)>
        };
        let picker = composer_model_picker(&store, selected, pick, close, window, cx);
        self.model_picker = Some(picker.clone());
        picker
    }

    /// Model/variant/agent dropdown, anchored above the composer's right side.
    /// Model is the filtered picker entity (filter typing repaints only it);
    /// variant and agent stay flat server lists.
    fn render_picker(
        &mut self,
        snap: &TileSnap,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let fonts = snap.fonts;
        let mut picker_el: gpui::AnyElement = div().into_any_element();
        if let Some(kind) = &self.picker {
            match kind {
                PickerKind::Model => {
                    picker_el = self.ensure_model_picker(window, cx).into_any_element();
                }
                PickerKind::Variant | PickerKind::Agent => {
                    let entries: Vec<String> = match kind {
                        PickerKind::Variant => {
                            let store = self.store.read(cx);
                            store
                                .tile_by_key(&self.key)
                                .map(|t| t.variants.clone())
                                .unwrap_or_default()
                        }
                        PickerKind::Agent => {
                            let store = self.store.read(cx);
                            store.agents.clone()
                        }
                        PickerKind::Model => unreachable!(),
                    };
                    let kind_tag = match kind {
                        PickerKind::Model => 0u64,
                        PickerKind::Variant => 1,
                        PickerKind::Agent => 2,
                    };
                    let mut menu = div()
                        .id("picker-list")
                        .flex()
                        .flex_col()
                        .min_w(px(220.))
                        .max_h(px(320.))
                        .overflow_y_scroll();
                    for entry in &entries {
                        let value = entry.clone();
                        let is_sentinel = value.starts_with('(');
                        let key = format!("pk{kind_tag}{value}");
                        menu = menu.child(
                            div()
                                .id(gpui::ElementId::NamedInteger(
                                    "pkrow".into(),
                                    TileView::hash(&key),
                                ))
                                .role(Role::ListBoxOption)
                                .aria_label(SharedString::from(value.clone()))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_size(px(fonts.font_body))
                                .text_color(theme::text())
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::HOVER))
                                .on_click({
                                    let value = value.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        let kind = this.picker.take();
                                        let key = this.key.clone();
                                        let store = this.store.clone();
                                        let value = value.clone();
                                        store.update(cx, move |s, _| match kind {
                                            Some(PickerKind::Model) => {
                                                s.set_tile_model(
                                                    &key,
                                                    if is_sentinel {
                                                        Some("(server default)".into())
                                                    } else {
                                                        Some(value)
                                                    },
                                                );
                                            }
                                            Some(PickerKind::Variant) => {
                                                s.set_tile_variant(
                                                    &key,
                                                    if is_sentinel {
                                                        Some("(default)".into())
                                                    } else {
                                                        Some(value)
                                                    },
                                                );
                                            }
                                            Some(PickerKind::Agent) => {
                                                s.set_tile_agent(
                                                    &key,
                                                    if is_sentinel {
                                                        Some("(default build)".into())
                                                    } else {
                                                        Some(value)
                                                    },
                                                );
                                            }
                                            None => {}
                                        });
                                        cx.notify();
                                    })
                                })
                                .child(SharedString::from(value)),
                        );
                    }
                    picker_el = div()
                        .id("picker-catcher")
                        .absolute()
                        .inset_0()
                        // hitbox hover is geometric in gpui: without occlusion the
                        // composer/transcript under the dropdown keeps receiving clicks
                        .occlude()
                        // click-elsewhere dismiss; menu rows are children, so they
                        // stay above the catcher and keep working
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.picker = None;
                            this.picker_placement = None;
                            cx.notify();
                        }))
                        .child({
                            // the card sits against its own pill (see
                            // picker_anchor); the corner is the fallback
                            // until the pill has reported its bounds
                            let card = div().absolute();
                            let card = match self.picker_placement {
                                Some((left, bottom)) => card.left(left).bottom(bottom),
                                None => card.bottom_10().right_8(),
                            };
                            card.child(popover_card().child(menu))
                        })
                        .into_any_element();
                }
            }
        }
        picker_el
    }

    /// The right-click context menu over transcript items.
    fn render_menu(&mut self, snap: &TileSnap, cx: &mut Context<Self>) -> gpui::AnyElement {
        let fonts = snap.fonts;
        let tile_entity = cx.entity();
        let menu_owner_key = self.key.clone();
        let mut menu_el: gpui::AnyElement = div().into_any_element();
        if let Some(menu) = self.menu.clone() {
            let store_handle = self.store.clone();
            let composer_handle = self.composer.clone();
            // click-elsewhere catcher closes the menu. Must close on full click,
            // not mouse-down: a down-handler unmounts the menu rows before
            // their on_click (mouse-up) can run, so actions never fire.
            let close_entity = tile_entity.clone();
            let mut rows = div().flex().flex_col();
            for (ix, item) in menu.items.iter().enumerate() {
                let label = match item {
                    ContextMenu::Copy(_) => "Copy",
                    ContextMenu::EditInput(_) => "Edit in input",
                    ContextMenu::Reprompt(_) => "Re-prompt",
                    ContextMenu::ForkFromHere(_) => "Fork from here",
                    ContextMenu::CopyImage { .. } => "Copy image",
                };
                let store_handle = store_handle.clone();
                let tile_entity = tile_entity.clone();
                let menu_owner_key = menu_owner_key.clone();
                let composer_handle = composer_handle.clone();
                let item = item.clone();
                rows = rows.child(menu_row(
                    gpui::ElementId::NamedInteger("menu-row".into(), ix as u64),
                    label,
                    false,
                    fonts.font_body,
                    move |_, window, cx| {
                        match &item {
                            ContextMenu::Copy(text) => {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    text.to_string(),
                                ));
                            }
                            ContextMenu::EditInput(text) => {
                                let text = text.clone();
                                // the tile that showed the menu, not wherever focus happens to be
                                let key = menu_owner_key.clone();
                                let composer = composer_handle.clone();
                                store_handle.update(cx, {
                                    let text = text.clone();
                                    move |s, _| {
                                        if let Some(tile) = s.tile_by_key_mut(&key) {
                                            tile.input = text.to_string();
                                            tile.reset_history();
                                        }
                                    }
                                });
                                // set_value emits no Change, so the store copy above stands
                                composer.update(cx, |field, cx| {
                                    field.set_value(text.as_ref(), window, cx)
                                });
                                // keep the tile's field-text mirror in step
                                tile_entity
                                    .update(cx, |tv, _| tv.last_field_text = text.to_string());
                            }
                            ContextMenu::Reprompt(text) => {
                                let key = menu_owner_key.clone();
                                let text = text.to_string();
                                store_handle.update(cx, move |s, cx| s.reprompt(&key, &text, cx));
                            }
                            ContextMenu::ForkFromHere(msg_key) => {
                                let key = menu_owner_key.clone();
                                let msg_key = *msg_key;
                                store_handle.update(cx, move |s, cx| {
                                    s.fork_from_message(&key, msg_key, cx)
                                });
                            }
                            ContextMenu::CopyImage { png } => {
                                let image = gpui::Image {
                                    format: ImageFormat::Png,
                                    bytes: png.as_ref().clone(),
                                    id: 0,
                                };
                                cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image));
                            }
                        }
                        tile_entity.update(cx, |tv, cx| {
                            tv.menu = None;
                            cx.notify();
                        });
                    },
                ));
            }
            menu_el = div()
                .id("menu-catcher")
                .absolute()
                .inset_0()
                // hitbox hover is geometric in gpui: without occlusion the
                // message/composer under a menu item receives the same click
                .occlude()
                .on_click(move |_, _, cx| {
                    close_entity.update(cx, |tv, cx| {
                        tv.menu = None;
                        cx.notify();
                    });
                })
                .child(
                    div()
                        .absolute()
                        .left(px(menu.x))
                        .top(px(menu.y))
                        .child(popover_card().min_w(px(180.)).child(rows)),
                )
                .into_any_element();
        }
        menu_el
    }

    /// The composer frame: attachment strip, textarea, bottom row, and the
    /// slash/fill popovers anchored above it.
    fn render_composer_frame(
        &mut self,
        snap: &TileSnap,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let fonts = snap.fonts;
        let accent = snap.accent;
        let accent_text = snap.accent_text;
        let attachments_el = self.render_attachments(snap);
        let pills = self.render_pills(snap, cx);
        let composer_focus_is = self.composer_focused;

        let bottom_row = div()
            .debug_selector(|| "composer-row".into())
            .flex()
            .items_center()
            .min_w_0()
            .gap_1p5()
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .child(
                        tip(
                            div()
                                .id("attach")
                                .role(Role::Button)
                                .aria_label("Attach files")
                                .cursor_pointer()
                                .p_1()
                                .rounded(px(4.))
                                .text_color(theme::text_muted())
                                .hover(|s| s.bg(theme::raised()).text_color(accent_text))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.pick_files(window, cx);
                                })),
                            "Attach files",
                        )
                        .child(crate::ui::icon(
                            "icons/plus.svg",
                            px(fonts.icon_small),
                            theme::text_muted(),
                        )),
                    )
                    .child(
                        tip(
                            div()
                                .id("allow-toggle")
                                .role(Role::Button)
                                .aria_label("Auto-allow tool permissions")
                                .aria_toggled(if snap.allow_on {
                                    Toggled::True
                                } else {
                                    Toggled::False
                                })
                                .cursor_pointer()
                                .p_1()
                                .rounded(px(4.))
                                .hover(|s| s.bg(theme::raised()))
                                .on_click({
                                    let store = self.store.clone();
                                    move |_, _, cx| {
                                        store.update(cx, |s, cx| {
                                            s.auto_allow = !s.auto_allow;
                                            s.ui.auto_allow_always = Some(s.auto_allow);
                                            s.save_state();
                                            cx.notify();
                                        });
                                    }
                                }),
                            "Auto-allow tool permissions",
                        )
                        .child(if snap.allow_on {
                            crate::ui::icon(
                                "icons/shield-filled.svg",
                                px(fonts.icon_small),
                                theme::WARN,
                            )
                        } else {
                            crate::ui::icon(
                                "icons/shield.svg",
                                px(fonts.icon_small),
                                theme::text_muted(),
                            )
                        }),
                    )
                    .child(
                        tip(
                            div()
                                .id("mcp-toggle")
                                .role(Role::Button)
                                .aria_label("Toggle MCP servers")
                                .aria_toggled(if snap.mcp_on {
                                    Toggled::True
                                } else {
                                    Toggled::False
                                })
                                .when(!snap.has_mcp, |d| d.opacity(0.4))
                                .cursor_pointer()
                                .p_1()
                                .rounded(px(4.))
                                .hover(|s| s.bg(theme::raised()))
                                .on_click({
                                    let store = self.store.clone();
                                    let key = self.key.clone();
                                    move |_, _, cx| {
                                        store.update(cx, |s, cx| s.toggle_mcp(&key, cx));
                                    }
                                }),
                            "Toggle MCP servers",
                        )
                        // the lit plug gets its own hue like the shield's
                        // WARN: teal, unused elsewhere in this app, so the
                        // three composer toggles read apart at a glance
                        .child(if snap.mcp_on {
                            crate::ui::icon("icons/plug.svg", px(fonts.icon_small), theme::BRANCH)
                        } else {
                            crate::ui::icon(
                                "icons/plug.svg",
                                px(fonts.icon_small),
                                theme::text_muted(),
                            )
                        }),
                    ),
            )
            .child(pills)
            .child(self.render_send_button(snap));

        // slash-menu / fill-in popovers hover above the composer frame; the
        // menu is its own leaf — keystrokes re-filter it without touching
        // this tile (see SuggestMenu)
        let slash_el = self.suggest.clone();

        // the composer is the tile's foot, not a box inside it: a quiet
        // hairline, the well field, then the pill row on the tile itself
        div()
            // anchors the popovers and scopes the slash-menu key
            // bindings ("Composer > Input") to this frame only
            .relative()
            .key_context("Composer")
            .flex_shrink_0()
            .px_3()
            .pt_2()
            .pb_2p5()
            .border_t_1()
            .border_color(theme::border_light())
            .flex()
            .flex_col()
            .gap_2()
            .child(attachments_el)
            .child(
                div()
                    .rounded(px(6.))
                    .border_1()
                    // the design's `textarea.area`: the default `line`, the
                    // accent ink while it holds focus
                    .when(composer_focus_is, |d| d.border_color(accent_text))
                    .when(!composer_focus_is, |d| d.border_color(theme::hairline()))
                    .bg(theme::well())
                    .pt(px(warpui_kit_core::shell::scaled(9.)))
                    .pb(px(warpui_kit_core::shell::scaled(9.)))
                    .px(px(warpui_kit_core::shell::scaled(10.)))
                    .flex()
                    // files dragged from the file manager land here as attachments
                    .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| {
                        style.border_color(accent)
                    })
                    .on_drop({
                        let store = self.store.clone();
                        let key = self.key.clone();
                        move |paths: &gpui::ExternalPaths, _, cx| {
                            let paths: Vec<std::path::PathBuf> = paths.paths().to_vec();
                            let key = key.clone();
                            store.update(cx, move |s, cx| s.attach_paths(&key, &paths, cx));
                        }
                    })
                    .on_mouse_down(MouseButton::Left, {
                        let composer = self.composer.clone();
                        move |_, window, cx| {
                            let handle = composer.read(cx).focus_handle(cx).clone();
                            window.focus(&handle, cx);
                        }
                    })
                    // the leaf view owns the kit element: keystrokes re-render
                    // it alone, never this tile (see ComposerField)
                    .child(div().flex_1().min_h_0().child(self.composer_field.clone())),
            )
            .child(bottom_row)
            .child(slash_el)
            .into_any_element()
    }

    fn action_composer_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.composer.clone();
        if !state.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        let before = state.read(cx).cursor();
        let focus = state.read(cx).focus_handle(cx).clone();
        focus.dispatch_action(&MoveUp, window, cx);
        let moved = state.read(cx).cursor() != before;
        if !moved {
            let key = self.key.clone();
            self.store
                .update(cx, |s, _| s.cycle_prompt_history(&key, true));
        }
    }

    fn action_composer_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.composer.clone();
        if !state.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        let before = state.read(cx).cursor();
        let focus = state.read(cx).focus_handle(cx).clone();
        focus.dispatch_action(&MoveDown, window, cx);
        let moved = state.read(cx).cursor() != before;
        if !moved {
            let key = self.key.clone();
            self.store
                .update(cx, |s, _| s.cycle_prompt_history(&key, false));
        }
    }

    fn action_cycle_agent(&mut self, cx: &mut Context<Self>) {
        let key = self.key.clone();
        self.store.update(cx, |s, _| s.cycle_agent(&key));
    }

    fn action_slash_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // the menu leaf owns the selection; moving it notifies only the leaf
        if self.suggest.read(cx).open {
            self.suggest
                .update(cx, |menu, cx| menu.move_selection(-1, cx));
            return;
        }
        // stock composer up-key: caret move, else per-tile history
        self.action_composer_up(window, cx);
    }

    fn action_slash_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.suggest.read(cx).open {
            self.suggest
                .update(cx, |menu, cx| menu.move_selection(1, cx));
            return;
        }
        self.action_composer_down(window, cx);
    }

    fn action_slash_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.suggest.read(cx).open {
            // split acceptance: compute + close in the leaf, composer
            // write-back here — re-entering TileView from inside its own
            // action context would panic (entity already being updated).
            // Nothing to accept leaves the menu open, like a no-op row.
            if let Some(text) = self.suggest.read(cx).compute_acceptance(cx) {
                self.suggest
                    .update(cx, |menu, cx| menu.close_after_accept(cx));
                self.insert_composer_text(text, window, cx);
            }
            return;
        }
        let state = self.composer.clone();
        if state.read(cx).focus_handle(cx).is_focused(window) {
            let focus = state.read(cx).focus_handle(cx).clone();
            focus.dispatch_action(
                &Enter {
                    secondary: false,
                    shift: false,
                },
                window,
                cx,
            );
        }
    }

    fn action_slash_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.suggest.read(cx).open {
            // close and leave the typed text as-is; the menu stays closed
            // until the text changes, and the inserted-body case re-arms via
            // acceptance the same way
            self.suggest.update(cx, |menu, cx| {
                menu.open = false;
                menu.dismissed = true;
                cx.notify();
            });
            return;
        }
        let state = self.composer.clone();
        if state.read(cx).focus_handle(cx).is_focused(window) {
            let focus = state.read(cx).focus_handle(cx).clone();
            focus.dispatch_action(&Escape, window, cx);
        }
    }
    /// A cheap content fingerprint for the cache-prune key set: item ids,
    /// the tool/question keys derived from them, attachments and the fold
    /// revision. Text changes under the same ids do not move it (the prune
    /// set is about which keys exist, not their contents).
    fn live_fingerprint(
        items: &[(String, std::sync::Arc<ChatItem>)],
        visible_tools: &HashSet<String>,
        attachments: &[crate::store::AttachmentItem],
        fold_rev: u64,
    ) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (id, item) in items {
            id.hash(&mut hasher);
            match item.as_ref() {
                ChatItem::Tool(tool) => {
                    true.hash(&mut hasher);
                    visible_tools.contains(id).hash(&mut hasher);
                    tool.has_details().hash(&mut hasher);
                }
                ChatItem::Question(question) => {
                    false.hash(&mut hasher);
                    question.questions.len().hash(&mut hasher);
                    question.resolution.is_some().hash(&mut hasher);
                }
                _ => 0u8.hash(&mut hasher),
            }
        }
        attachments.len().hash(&mut hasher);
        for attachment in attachments {
            attachment.key.hash(&mut hasher);
        }
        fold_rev.hash(&mut hasher);
        hasher.finish()
    }
}

impl Render for TileView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let mut snap = {
            let store = self.store.read(cx);
            let Some(tile) = store.tile_by_key(&self.key) else {
                return div().into_any_element();
            };
            let (has_mcp, mcp_on) = store.mcp_state(&self.key);
            TileSnap {
                fonts: store.fonts,
                accent: store.accent.accent,
                accent_text: store.accent.text,
                input: tile.input.clone(),
                is_busy: tile.is_busy(store),
                is_connected: tile
                    .session_id
                    .as_ref()
                    .map(|id| store.has_runtime(id))
                    .unwrap_or(store.is_server_running),
                is_reconnecting: store.stream_reconnecting(tile.session_id.as_deref()),
                is_focused: tile.is_focused,
                title: tile.title.clone(),
                ctx: tile.context_text.clone().unwrap_or_default(),
                ctx_frac: tile.context_frac,
                has_ctx: tile.has_context(),
                // Arc'd items: this snapshot is pointer copies, the payloads stay put
                items: tile.items.iter().map(|i| (i.id(), Arc::clone(i))).collect(),
                queue_keys: tile.queue_rows.clone(),
                queue: store.queue.clone(),
                attachments: tile.attachments.clone(),
                has_queue: tile.has_queue_rows(),
                has_messages: tile.has_messages(),
                has_variants: tile.has_variants(),
                selected_variant: tile.selected_variant.clone().unwrap_or_default(),
                selected_model_short: tile.selected_model_short(),
                selected_agent: tile.selected_agent.clone().unwrap_or_default(),
                has_agents: store.has_agents(),
                harness_label: tile.harness.label(),
                harness_locked: tile.session_id.is_some(),
                has_tasks_or_agents: tile.has_tasks_or_agents(),
                has_tasks: tile.has_tasks(),
                tasks_progress: tile.tasks_progress(),
                tasks: tile.tasks.clone(),
                has_tile_agents: tile.has_tile_agents(),
                agents_summary: tile.tile_agents_summary(),
                tile_agents: tile.tile_agents.clone(),
                multiple_workspaces: store.multiple_workspaces(),
                waiting: waiting_word(tile.items.iter().map(Arc::as_ref)),
                density: self.density,
                allow_on: store.auto_allow,
                spring: store.todos_spring.as_deref() == Some(self.key.as_str()),
                has_mcp,
                mcp_on,
            }
        };
        let fonts = snap.fonts;
        let accent = snap.accent;

        // external input changes (history, edit-queued) sync into the field;
        // the field's own text arrives via Change events (set_value emits
        // none), so the mirrored copy replaces a render-time entity read
        let composer_text_changed = snap.input != self.last_composer_text;
        if composer_text_changed && self.last_field_text != snap.input {
            let text = snap.input.clone();
            self.composer
                .update(cx, |field, cx| field.set_value(text, window, cx));
            self.last_field_text = snap.input.clone();
            // set_value emits no Change: nudge the suggest menu's filter to
            // the externally inserted text (the recompute is dismiss-safe)
            let text = snap.input.clone();
            self.suggest
                .update(cx, |menu, cx| menu.recompute(&text, true, cx));
        }
        self.last_composer_text = snap.input.clone();

        // a live todo batch springs the flyout open
        if snap.spring {
            if !self.last_spring_seen {
                self.flyout_open = true;
                self.last_spring_seen = true;
            }
        } else {
            self.last_spring_seen = false;
        }

        // transcript auto-scroll: keep pinned while content grows
        let max_now = self.scroll.max_offset().y;
        if self.pinned && max_now > self.last_max + px(0.5) && max_now > px(0.) {
            self.scroll.scroll_to_bottom();
        }
        if max_now != self.last_max {
            self.last_max = max_now;
        }

        // moved out rather than cloned — snap's other fields stay usable
        let items = std::mem::take(&mut snap.items);
        self.question_keys_seen.clear();
        let mut transcript = div()
            .id("transcript")
            .role(Role::Log)
            .aria_label("Transcript")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .track_scroll(&self.scroll.clone())
            .on_scroll(cx.listener(|this, event: &gpui::ScrollEvent, window, cx| {
                // the wheel no longer scrolls natively (overflow_hidden keeps
                // the handle-driven offset) — each delta applies straight to
                // the handle offset, no glide
                let delta = event.delta.pixel_delta(window.line_height()).y;
                if delta == px(0.) {
                    return;
                }
                let max = this.scroll.max_offset();
                // wheel deltas are positive-up; the offset counts down
                let before = -this.scroll.offset().y;
                let next = (before - delta).clamp(px(0.), max.y);
                let pinned = next >= max.y - px(1.);
                let turned = pinned != this.pinned;
                this.pinned = pinned;
                // a tick at either end changes nothing: repainting the
                // whole transcript for it is what made scrolling feel
                // heavy
                if next != before {
                    this.scroll_to(next, cx);
                } else if turned {
                    cx.notify();
                }
                // the transcript consumed the tick: a scrollable inside a
                // message (or a scrollbar hover over one) must not see the
                // same tick scroll it too
                cx.stop_propagation();
            }))
            // the transcript fills the tile: bubbles keep their 760 measure at
            // the edges (the reply left, the prompt right), so a wide tile is
            // used instead of reading as a narrow centred column
            .w_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2p5();
        // consecutive tool calls collapse into one disclosure row; the run is
        // split by anything non-tool (permission cards, notes) in between.
        // visible_tools feeds the cache-prune set below: members hidden inside
        // a collapsed group drop their text cache slots
        let mut visible_tools: HashSet<String> = HashSet::new();
        let mut idx = 0;
        while idx < items.len() {
            let (item_id, item) = &items[idx];
            if matches!(item.as_ref(), ChatItem::Tool(_)) {
                let mut end = idx + 1;
                while end < items.len() && matches!(items[end].1.as_ref(), ChatItem::Tool(_)) {
                    end += 1;
                }
                let run = &items[idx..end];
                if run.len() >= 2 {
                    let group_key = format!("toolgroup:{item_id}");
                    let open = self.expanded.contains(&group_key);
                    if open {
                        visible_tools.extend(run.iter().map(|(id, _)| id.clone()));
                    }
                    transcript = transcript.child(
                        self.render_tool_group(run, &group_key, open, fonts, accent, window, cx),
                    );
                    idx = end;
                    continue;
                }
                visible_tools.insert(item_id.clone());
            }
            transcript = transcript.child(self.render_item(
                item_id,
                item.as_ref(),
                fonts,
                accent,
                window,
                cx,
            ));
            idx += 1;
        }
        let transcript_empty = items.is_empty();
        // Cache entries only live as long as the transcript/composer entries
        // they serve; question inputs die with their question. Building the
        // key set costs a String per item plus a format! per sub-surface, so
        // it is rebuilt only when the content fingerprint moves — caret
        // blinks, scrolling and resizes reuse it.
        let fingerprint =
            Self::live_fingerprint(&items, &visible_tools, &snap.attachments, self.fold_rev);
        if self.live_fingerprint != Some(fingerprint) {
            let mut live: HashSet<String> = items.iter().map(|(id, _)| id.clone()).collect();
            for (id, item) in &items {
                match item.as_ref() {
                    ChatItem::Tool(tool) => {
                        if !visible_tools.contains(id) {
                            continue;
                        }
                        live.insert(format!("{id}:title"));
                        if tool.has_details() {
                            live.insert(format!("{id}:details"));
                        }
                    }
                    ChatItem::Question(q) => {
                        for qi in 0..q.questions.len() {
                            live.insert(format!("{id}#{qi}"));
                        }
                        if q.resolution.is_some() {
                            live.insert(format!("{id}:res"));
                        }
                    }
                    _ => {}
                }
            }
            live.extend(snap.attachments.iter().map(|a| format!("att{}", a.key)));
            self.live_keys = live;
            self.live_fingerprint = Some(fingerprint);
            // prune only when the set can have changed; `live_keys` is taken
            // out so the &mut self call does not see it borrowed
            let live = std::mem::take(&mut self.live_keys);
            self.prune_caches(&live, window, cx);
            self.live_keys = live;
        }
        self.question_inputs
            .retain(|k, _| self.question_keys_seen.contains(k));

        let jump_visible = !self.pinned && snap.has_messages;

        let header = self.render_header(&snap, cx);
        let queue_el = self.render_queue_strip(&snap);
        let flyout = self.render_flyout(&snap);
        let picker_el = self.render_picker(&snap, window, cx);
        let menu_el = self.render_menu(&snap, cx);
        let composer_frame = self.render_composer_frame(&snap, window, cx);

        // an opened chat with no messages yet still explains itself; the hint
        // is a plain overlay (no handlers), so wheel events reach the
        // scroll-tracked transcript underneath
        // a chat with nothing in it yet either explains itself or, while its
        // first turn runs, thinks out loud with three dots
        let empty_overlay = div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .when(snap.is_busy, |d| {
                d.child(thinking_dots(&self.key, theme::text_faint().into()))
            })
            .when(!snap.is_busy, |d| {
                d.child(crate::ui::icon(
                    "icons/spark.svg",
                    px(32.),
                    theme::text_faint(),
                ))
                .child(
                    div()
                        .text_size(px(fonts.font_body))
                        .text_color(theme::text_faint())
                        .child("Nothing here yet — say hello, or / for commands · @ for files"),
                )
            });
        let transcript_area = div()
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(transcript)
            .when(transcript_empty, |d| d.child(empty_overlay));

        let root =
            div()
                .id(gpui::ElementId::NamedInteger(
                    "tile".into(),
                    TileView::hash(&self.key),
                ))
                .role(Role::Group)
                .aria_label(SharedString::from(format!("Chat: {}", snap.title)))
                // the a11y Click action (AT "press") lands here: same effect as
                // the mouse-down focus path below, and it makes the tile
                // discoverable/clickable by name in accessibility tooling
                .on_click(cx.listener({
                    let store = self.store.clone();
                    let key = self.key.clone();
                    move |_, _, _, cx| {
                        store.update(cx, |s, cx| s.focus_tile(&key, cx));
                    }
                }))
                .relative()
                .size_full()
                // let the grid track shrink below long-word min-content
                .min_w_0()
                .flex()
                .flex_col()
                .rounded(px(8.))
                .border_1()
                // the focus edge wins: the state word still says what it waits on
                .border_color(if snap.is_focused {
                    accent
                } else if snap.waiting.is_some() {
                    theme::CHANGES
                } else {
                    theme::hairline()
                })
                // the edge answers the pointer; the focused and waiting tiles
                // already say something stronger in colour
                .when(!snap.is_focused && snap.waiting.is_none(), |d| {
                    d.hover(|s| s.border_color(theme::line_strong()))
                })
                .bg(theme::surface())
                .overflow_hidden()
                // composer-only key overrides (bound over the kit's "Input" context)
                .on_action(cx.listener(|this, _: &ComposerUp, window, cx| {
                    this.action_composer_up(window, cx)
                }))
                .on_action(cx.listener(|this, _: &ComposerDown, window, cx| {
                    this.action_composer_down(window, cx)
                }))
                .on_action(cx.listener(|this, _: &ComposerCycleAgent, _window, cx| {
                    this.action_cycle_agent(cx)
                }))
                // slash-menu keys (bound over "Composer > Input", see main.rs): while
                // the menu is open they own up/down/enter/escape; closed they fall
                // through to the exact stock behavior
                .on_action(cx.listener(|this, _: &ComposerSlashUp, window, cx| {
                    this.action_slash_up(window, cx)
                }))
                .on_action(cx.listener(|this, _: &ComposerSlashDown, window, cx| {
                    this.action_slash_down(window, cx)
                }))
                .on_action(cx.listener(|this, _: &ComposerSlashEnter, window, cx| {
                    this.action_slash_enter(window, cx)
                }))
                .on_action(cx.listener(|this, _: &ComposerSlashEscape, window, cx| {
                    this.action_slash_escape(window, cx)
                }))
                .on_mouse_down(MouseButton::Left, {
                    let store = self.store.clone();
                    let key = self.key.clone();
                    move |_, _, cx| {
                        store.update(cx, |s, cx| s.focus_tile(&key, cx));
                    }
                })
                // paint-phase capture of the tile's window bounds, so right-click
                // positions can be turned into tile-local context-menu anchors
                // (same paint-listener pattern as the sidebar splitter)
                .child({
                    let origin = self.origin.clone();
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, _, _| {
                            origin.set((
                                bounds.origin.x.as_f32(),
                                bounds.origin.y.as_f32(),
                                bounds.size.width.as_f32(),
                                bounds.size.height.as_f32(),
                            ));
                        },
                    )
                    .absolute()
                    .size_full()
                })
                .child(header)
                .child(transcript_area)
                .when(jump_visible, |d| {
                    d.child(
                        tip(
                            div()
                                .absolute()
                                .bottom(px(150.))
                                .right_2()
                                .id("jump")
                                .role(Role::Button)
                                .aria_label("Jump to latest")
                                .rounded_full()
                                .border_1()
                                .border_color(theme::hairline())
                                .bg(theme::chip())
                                .p_2()
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pinned = true;
                                    let target = this.scroll.max_offset().y;
                                    this.scroll_to(target, cx);
                                })),
                            "Jump to latest",
                        )
                        .child(crate::ui::icon(
                            "icons/arrow-down.svg",
                            px(fonts.icon_medium),
                            theme::text(),
                        )),
                    )
                })
                .when(snap.has_queue, |d| {
                    d.child(
                        div()
                            .id("queue-strip")
                            .flex_shrink_0()
                            .max_h(px(150.))
                            .mx_2()
                            .mt_1p5()
                            .overflow_y_scroll()
                            .child(queue_el),
                    )
                })
                .child(composer_frame)
                .child(flyout)
                .child(picker_el)
                .child(menu_el);

        root.into_any_element()
    }
}

/// The slash/@ suggestion menu as its own leaf view, subscribed to the
/// composer's InputState directly: the filter re-evaluates on every
/// keystroke and repaints only this popup. The render-time state machine
/// it replaced could only run when some unrelated event repainted the
/// tile — since the composer leaf fix removed per-keystroke tile
/// repaints, the slash menu had gone stale mid-typing and the @ menu
/// lagged a network round-trip behind. The @-file search is debounced so
/// typing also stops firing one GET /find/file per character.
struct SuggestMenu {
    composer: Entity<TextareaState>,
    store: Entity<Store>,
    tile: Entity<TileView>,
    tile_key: String,
    /// focus mirror: reading the live focus handle in render would re-create
    /// the per-keystroke dependency
    focused: bool,
    /// open while the text is a bare `/command` name (server commands) or
    /// holds an active `@token` (file mentions); `selected` indexes the rows
    open: bool,
    mode: SuggestMode,
    selected: usize,
    /// Escape-closed menu: stays closed until the text changes (or the field
    /// blurs), so it doesn't spring back on its own
    dismissed: bool,
    /// byte offset of the `@` the file menu hangs from, and the query typed
    /// after it (the key file results are matched against)
    at_start: usize,
    at_query: String,
    /// the query a /find/file request last went out for, so repeats of the
    /// same token don't re-ask the server
    find_requested: Option<String>,
    /// query waiting out the debounce; replaced (and the timer restarted) by
    /// every keystroke
    pending_search: Option<String>,
    /// the in-flight debounce timer; dropping it cancels the wait
    search_task: Option<gpui::Task<()>>,
}

impl SuggestMenu {
    /// Server commands filtered by the text after the leading `/`
    /// (case-insensitive substring on the name; empty prefix lists all).
    fn command_matches(&self, text: &str, cx: &App) -> Vec<CommandInfo> {
        let prefix = text.get(1..).unwrap_or("");
        // the typed part is `/name` — cut at the first space so args don't
        // narrow the list away
        let prefix = prefix
            .split(char::is_whitespace)
            .next()
            .unwrap_or("")
            .to_lowercase();
        self.store
            .read(cx)
            .tile_commands(&self.tile_key)
            .iter()
            .filter(|c| prefix.is_empty() || c.name_lower.contains(&prefix))
            .cloned()
            .collect()
    }

    /// Visible row count of the live menu: commands match the typed prefix,
    /// files are the stored answer for the active @query (0 while in flight).
    fn visible(&self, text: &str, cx: &App) -> usize {
        match self.mode {
            SuggestMode::Commands => self.command_matches(text, cx).len().min(SLASH_MENU_VISIBLE),
            SuggestMode::Files => self
                .store
                .read(cx)
                .file_results_for(&self.tile_key, &self.at_query)
                .map(|p| p.len().min(SLASH_MENU_VISIBLE))
                .unwrap_or(0),
        }
    }

    /// Menu state machine: a leading `/` (still naming the command — once a
    /// space follows, args are being typed and the picker is done) opens the
    /// command menu, an active `@token` anywhere opens the file menu. No
    /// trigger, an escaped menu (until the text changes: `allow_reopen`), or
    /// an unfocused composer keeps both closed. A fresh open refreshes the
    /// command list; a changed @query re-arms the debounced file search.
    fn recompute(&mut self, text: &str, allow_reopen: bool, cx: &mut Context<Self>) {
        let at = at_token_start(text);
        let naming_command = text.starts_with('/') && !text[1..].contains(char::is_whitespace);
        let want = if naming_command {
            Some(SuggestMode::Commands)
        } else if at.is_some() {
            Some(SuggestMode::Files)
        } else {
            None
        };
        let menu_wanted = match want {
            Some(mode) if self.focused && (!self.dismissed || allow_reopen) => Some(mode),
            _ => None,
        };
        match (menu_wanted, want) {
            (Some(mode), _) => {
                if !self.open || self.mode != mode {
                    self.open = true;
                    self.mode = mode;
                    self.selected = 0;
                    self.find_requested = None;
                    if mode == SuggestMode::Commands {
                        let key = self.tile_key.clone();
                        self.store.update(cx, |s, _| s.refresh_commands(&key));
                    }
                }
                self.dismissed = false;
                if mode == SuggestMode::Files {
                    let (start, query) = at.unwrap();
                    self.at_start = start;
                    if self.at_query != query {
                        self.at_query = query.clone();
                        self.selected = 0;
                        self.arm_search(query, cx);
                    }
                }
            }
            _ => {
                self.open = false;
                self.find_requested = None;
                self.pending_search = None;
            }
        }
        cx.notify();
    }

    /// Restart the debounce for a changed @query: ~200 ms of quiet before
    /// the request leaves, so typing a path fires one search instead of one
    /// per character.
    fn arm_search(&mut self, query: String, cx: &mut Context<Self>) {
        self.pending_search = Some(query);
        self.search_task = Some(cx.spawn(async move |menu, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(200))
                .await;
            let _ = menu.update(cx, |menu, cx| menu.fire_search(cx));
        }));
    }

    /// Debounce elapsed: send the pending query unless typing moved on (the
    /// task was replaced, so a stale timer never reaches here).
    fn fire_search(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self.pending_search.take() else {
            return;
        };
        if self.open
            && self.mode == SuggestMode::Files
            && self.find_requested.as_deref() != Some(query.as_str())
        {
            self.find_requested = Some(query.clone());
            let key = self.tile_key.clone();
            self.store.update(cx, |s, _| s.search_files(&key, &query));
        }
    }

    /// Arrow keys: move the row selection, wrapping on the ends like the
    /// history cycle does.
    fn move_selection(&mut self, dir: i32, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().to_string();
        let count = self.visible(&text, cx);
        if count > 0 {
            self.selected = if dir < 0 {
                if self.selected == 0 {
                    count - 1
                } else {
                    self.selected - 1
                }
            } else {
                (self.selected + 1) % count
            };
            cx.notify();
        }
    }

    /// Pure half of acceptance: the text that WOULD land in the composer.
    /// Commands keep their short `/name args` form — the server expands the
    /// template at send time; file mentions replace the `@token` with
    /// `@path `.
    fn compute_acceptance(&self, cx: &App) -> Option<String> {
        let text = self.composer.read(cx).value().to_string();
        match self.mode {
            SuggestMode::Commands => {
                let matches = self.command_matches(&text, cx);
                let cmd = matches.get(self.selected).cloned()?;
                // whatever follows the typed `/name` rides in as the arguments
                let args = text
                    .get(1..)
                    .and_then(|rest| rest.split_once(char::is_whitespace))
                    .map(|(_, args)| args.trim())
                    .unwrap_or("");
                Some(if args.is_empty() {
                    format!("/{} ", cmd.name)
                } else {
                    format!("/{} {}", cmd.name, args)
                })
            }
            SuggestMode::Files => {
                let paths = self
                    .store
                    .read(cx)
                    .file_results_for(&self.tile_key, &self.at_query)?;
                let path = paths.get(self.selected).cloned()?;
                let start = self.at_start.min(text.len());
                if !text.is_char_boundary(start) {
                    return None;
                }
                Some(format!("{}@{} ", &text[..start], path))
            }
        }
    }

    /// Close after an acceptance; the composer write-back is the caller's
    /// job (the tile owns the mirrors, and re-entering the tile entity from
    /// inside its own action context would panic).
    fn close_after_accept(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.dismissed = true;
        cx.notify();
    }

    /// Row click: same acceptance as Enter — a click does not run inside the
    /// tile's update, so driving the tile entity from here is safe.
    fn accept_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.compute_acceptance(cx) else {
            return;
        };
        self.close_after_accept(cx);
        self.tile
            .update(cx, |tile, cx| tile.insert_composer_text(text, window, cx));
    }
}

impl Render for SuggestMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let (fonts, accent_text) = {
            let store = self.store.read(cx);
            (store.fonts, store.accent.text)
        };
        // reading the composer value here is the point: the leaf re-renders
        // on every keystroke and the filter follows
        let input = self.composer.read(cx).value().to_string();
        let muted_row = |label: String| {
            div()
                .px_2()
                .py_1()
                .text_size(px(fonts.font_meta))
                .italic()
                .text_color(theme::text_faint())
                .child(label)
        };
        let mut rows = div().flex().flex_col();
        let mut count = 0usize;
        match self.mode {
            SuggestMode::Commands => {
                let matches = self.command_matches(&input, cx);
                count = matches.len();
                if count == 0 {
                    rows = rows.child(muted_row(
                        if self.store.read(cx).tile_commands(&self.tile_key).is_empty() {
                            "no server commands".into()
                        } else {
                            "no matching command".into()
                        },
                    ));
                }
                for (ix, cmd) in matches.iter().enumerate() {
                    if ix == SLASH_MENU_VISIBLE {
                        rows = rows.child(muted_row(format!(
                            "+{} more — keep typing",
                            count - SLASH_MENU_VISIBLE
                        )));
                        break;
                    }
                    let selected = ix == self.selected;
                    rows = rows.child(
                        div()
                            .id(gpui::ElementId::NamedInteger(
                                "suggest-row".into(),
                                ix as u64,
                            ))
                            .role(Role::ListBoxOption)
                            .aria_label(SharedString::from(format!(
                                "/{} — {}",
                                cmd.name,
                                cmd.description.lines().next().unwrap_or("")
                            )))
                            .aria_selected(selected)
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(theme::HOVER))
                            .hover(|s| s.bg(theme::HOVER))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.selected = ix;
                                this.accept_row(window, cx);
                            }))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(fonts.font_body))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(accent_text)
                                    .child(SharedString::from(format!("/{}", cmd.name))),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(fonts.font_meta))
                                    .text_color(theme::text_muted())
                                    .truncate()
                                    .child({
                                        let desc = cmd
                                            .description
                                            .lines()
                                            .next()
                                            .unwrap_or("")
                                            .to_string();
                                        SharedString::from(desc)
                                    }),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(fonts.font_control))
                                    .text_color(theme::text_faint())
                                    .child(if cmd.source == "skill" {
                                        "skill"
                                    } else {
                                        "cmd"
                                    }),
                            ),
                    );
                }
            }
            SuggestMode::Files => {
                let results = self
                    .store
                    .read(cx)
                    .file_results_for(&self.tile_key, &self.at_query)
                    .cloned();
                match results {
                    None => rows = rows.child(muted_row("searching files…".into())),
                    Some(paths) if paths.is_empty() => {
                        rows = rows.child(muted_row("no matching file".into()))
                    }
                    Some(paths) => {
                        count = paths.len();
                        for (ix, path) in paths.iter().enumerate() {
                            if ix == SLASH_MENU_VISIBLE {
                                rows = rows.child(muted_row(format!(
                                    "+{} more — keep typing",
                                    count - SLASH_MENU_VISIBLE
                                )));
                                break;
                            }
                            let selected = ix == self.selected;
                            let path = path.clone();
                            rows = rows.child(
                                div()
                                    .id(gpui::ElementId::NamedInteger(
                                        "suggest-row".into(),
                                        ix as u64,
                                    ))
                                    .role(Role::ListBoxOption)
                                    .aria_label(SharedString::from(path.clone()))
                                    .aria_selected(selected)
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .when(selected, |d| d.bg(theme::HOVER))
                                    .hover(|s| s.bg(theme::HOVER))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.selected = ix;
                                        this.accept_row(window, cx);
                                    }))
                                    .child(crate::ui::icon(
                                        "icons/doc.svg",
                                        px(fonts.icon_small),
                                        theme::text_muted(),
                                    ))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(px(fonts.font_meta))
                                            .font_family(theme::code_font())
                                            .text_color(theme::text_dim())
                                            .truncate()
                                            .child(SharedString::from(path)),
                                    ),
                            );
                        }
                    }
                }
            }
        }
        // keyboard selection follows whichever list is live; wrap on the ends
        // like the history cycle does
        let visible = count.min(SLASH_MENU_VISIBLE);
        if visible > 0 && self.selected >= visible {
            self.selected = 0;
        }
        div()
            .absolute()
            .left_2()
            .right_2()
            .bottom(relative(1.))
            .pb_1()
            .child(popover_card().child(rows))
            .into_any_element()
    }
}

/// Leaf view for one question's custom-answer input (same shape as
/// ComposerField): every keystroke notifies the InputState, and GPUI
/// re-renders every view that read that entity during its own render —
/// embedding the Input in TileView made each keypress re-layout the whole
/// transcript. The typed text write-throughs into the store without
/// notifying anyone; the tile is nudged only when the answer's filled
/// state flips, because that is the only keystroke consequence outside
/// this field (the Answer button's can_submit).
struct QuestionField {
    state: Entity<InputState>,
    store: Entity<Store>,
    tile: Entity<TileView>,
    tile_key: String,
    question_id: String,
    qi: usize,
    filled: bool,
}

impl Render for QuestionField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // the kit Input has no a11y of its own — the wrapper carries the role;
        // an AT click lands at the wrapper's center, the input itself
        // the field box owns the focus/a11y node; this wrapper only names it
        // (a second track_focus here made two nodes claim a11y focus in one
        // frame)
        div()
            .id(gpui::ElementId::NamedInteger(
                "qfield".into(),
                TileView::hash(&format!("{}#{}", self.question_id, self.qi)),
            ))
            .role(Role::TextInput)
            .aria_label("Custom answer")
            .aria_placeholder("custom answer…")
            // the prototype's "Custom answer" is a `.field text`, so a value,
            // not prose
            .child(warpui_kit_core::field::mono(
                "field-state",
                &self.state,
                window,
                cx,
            ))
    }
}

/// Leaf view around the kit Textarea. Every keystroke notifies
/// `TextareaState`, and GPUI re-renders every view that read that entity
/// during its own render — embedding the element straight in TileView made
/// each keypress re-render and re-layout the whole transcript. This view is
/// the state's only renderer, so typing repaints one input field.
struct ComposerField {
    state: Entity<TextareaState>,
    store: Entity<Store>,
    key: String,
}

impl Render for ComposerField {
    fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.clone();
        let key = self.key.clone();
        // chrome-less inside: the composer frame (border/bg) is the tile's,
        // the textarea only edits. The kit field owns the focus/a11y node;
        // the wrapper stays a plain layout box — tracking the same focus
        // handle here made both nodes claim a11y focus in one frame, which
        // panicked ("set_focus called more than once") on the first click.
        // image paste → attachment instead of text insertion
        div()
            .id("composer-field")
            .self_start()
            .w_full()
            .child(
                // the same chrome-less area the kit's own box seats, so the
                // composer's text sits on the design's 10/9 padding
                warpui_kit_core::field::bare_area(&self.state)
                    .aria_label("Message composer")
                    .on_paste(move |item: &gpui::ClipboardItem, _, cx| {
                        let image = item.entries().iter().find_map(|entry| match entry {
                            gpui::ClipboardEntry::Image(image) => Some(image),
                            _ => None,
                        });
                        if let Some(image) = image {
                            let png = image.bytes.clone();
                            let key = key.clone();
                            store.update(cx, move |s, _| s.attach_image(&key, png));
                            true
                        } else {
                            false
                        }
                    }),
            )
            .into_any_element()
    }
}

// ---- the composer model picker ---------------------------------------------

/// The app's icon paths for the shared picker shell.
const PICKER_ICONS: PickerIcons = PickerIcons {
    check: "icons/check.svg",
    chevron_down: "icons/chevron-down.svg",
    trash: "icons/trash.svg",
    refresh: "icons/refresh.svg",
};

/// The composer's model dropdown: a shared FilteredPicker mounted by the tile
/// only while the model pill is open. Picks and dismissal land through these
/// callbacks; the catalog comes from the store.
fn composer_model_picker(
    store: &Entity<Store>,
    selected: Box<dyn Fn(&App) -> String>,
    pick: ModelPick,
    close: DismissFn,
    window: &mut Window,
    cx: &mut Context<TileView>,
) -> Entity<FilteredPicker> {
    let spec = PickerSpec {
        key: "composer",
        trigger_label: "Model",
        filter_placeholder: "Filter models",
        icons: PICKER_ICONS,
        trigger_icon: None,
        provider_badge: false,
        refresh: false,
        refresh_label: "",
        stop_trigger_mousedown: false,
        visible_rows: 7,
        selected_accent: false,
        overlay: true,
        anchor: OverlayAnchor::Composer,
        trigger_scale: TriggerScale::Regular,
        trigger_rounded_lg: true,
        trigger_min_h: None,
        panel_chrome: PanelChrome::Cards,
        trigger_id: None,
        panel_id: None,
        filter_selector: Some("model-dd-filter"),
        list_id: Some("model-dd-list"),
    };
    cx.new(|cx| {
        let data = {
            let store = store.clone();
            // the selected readout is not clonable through the box — wrap it
            let selected: std::rc::Rc<dyn Fn(&App) -> String> = std::rc::Rc::from(selected);
            Box::new(move |filter: &str, _window: &Window, cx: &App| {
                composer_model_data(&store, &selected, filter, cx)
            }) as Box<dyn Fn(&str, &Window, &App) -> PickerData>
        };
        let mut picker = FilteredPicker::new(
            spec,
            PickerTarget::Callback(Box::new(move |value, _window, cx| {
                pick(value, cx);
            })),
            data,
            window,
            cx,
        );
        picker.set_on_dismiss(close);
        picker.observe_entity(store, cx);
        picker
    })
}

/// One render's worth of the model catalog: the server-default row, provider
/// groups and the filtered, deduped models.
fn composer_model_data(
    store: &Entity<Store>,
    selected: &std::rc::Rc<dyn Fn(&App) -> String>,
    filter: &str,
    cx: &App,
) -> PickerData {
    let fonts = store.read(cx).fonts;
    let accent = store.read(cx).accent.accent;
    let selected_value = selected(cx);
    let filter = filter.to_lowercase();
    let mut models: Vec<String> = store
        .read(cx)
        .models
        .iter()
        .filter(|model| {
            let model = model.trim();
            !model.is_empty()
                && !model.starts_with('(')
                && (filter.is_empty() || model.to_lowercase().contains(&filter))
        })
        .cloned()
        .collect();
    models.sort_by(|a, b| {
        model_provider(a)
            .to_lowercase()
            .cmp(&model_provider(b).to_lowercase())
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    models.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

    let mut rows = Vec::with_capacity(models.len() + 4);
    rows.push(PickerRow::Item(PickerItem {
        id: "model-default".into(),
        selector: "model-default".into(),
        value: String::new(),
        label: "Server default".to_string(),
        selected: selected_value.is_empty(),
        muted: false,
        trailing_delete: None,
    }));
    if models.is_empty() {
        rows.push(PickerRow::Message {
            id: "model-message".into(),
            label: "No models yet. Start a workspace to load models.".to_string(),
        });
    } else {
        let mut last_provider = String::new();
        for model in models {
            let provider = model_provider(&model).to_string();
            if !provider.is_empty() && provider != last_provider {
                rows.push(PickerRow::Group {
                    id: format!("model-provider-{provider}"),
                    label: provider.to_uppercase(),
                    selector: None,
                });
                last_provider = provider;
            }
            rows.push(PickerRow::Item(PickerItem {
                id: format!("model-{model}"),
                selector: format!("model-{model}"),
                value: model.clone(),
                label: model_name(&model).to_string(),
                selected: model.eq_ignore_ascii_case(&selected_value),
                muted: false,
                trailing_delete: None,
            }));
        }
    }

    PickerData {
        body: PickerBody::Rows(rows),
        selected: Selection {
            badge: None,
            label: if selected_value.is_empty() {
                "Server default".to_string()
            } else {
                model_name(&selected_value).to_string()
            },
            value: selected_value,
        },
        footer: None,
        fonts,
        accent,
        show_tooltips: false,
    }
}

#[cfg(test)]
mod bidi_tests {
    use super::strip_bidi_overrides;

    #[test]
    fn strips_bidi_overrides_and_keeps_rtl_text() {
        let raw = "a\u{202e}b\u{202d}c\u{2067}d";
        assert_eq!(strip_bidi_overrides(raw).as_ref(), "abcd");
        let rtl = "مرحبا بالعالم";
        assert_eq!(strip_bidi_overrides(rtl).as_ref(), rtl);
        let marks = "a\u{200b}\u{200e}b";
        assert_eq!(strip_bidi_overrides(marks).as_ref(), marks);
    }
}
