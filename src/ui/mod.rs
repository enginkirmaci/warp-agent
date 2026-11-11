//! ui module: shared helpers + the main window.

pub mod about;
pub mod markdown;
pub mod settings;
pub mod tile;
pub mod window;

// The control shapes, the icon builder and the media helpers are the family's:
// they live in warpui-kit-core now, and these names keep the call sites here
// reading the way they always have.
pub use warpui_kit_core::{BtnKind, button, menu_row, popover_card};
pub use warpui_kit_core::{icon, mime_for_path};
// the overlay tooltip is core's: one mechanism, not a copy per app
pub use warpui_kit_core::tooltip_overlay as tooltip;
