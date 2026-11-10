mod clip_buffer;
mod cursor;
mod edit_stack;
mod editor;
mod graphemes;
mod line;
mod line_buffer;
mod resolve;
mod rest_policy;
mod word;

#[cfg(test)]
mod a6_probe;

#[cfg(feature = "system_clipboard")]
pub(crate) use clip_buffer::get_system_clipboard;
pub(crate) use clip_buffer::{Clipboard, LocalClipboard};
pub(crate) use cursor::{CaretGeometry, Cursor, Movement, SelectionExtent};
pub(crate) use edit_stack::EditStack;
pub(crate) use editor::EditCommandStatus;
pub use editor::Editor;
pub(crate) use graphemes::{ensure_grapheme_boundary_next, ensure_grapheme_boundary_prev};
pub use line_buffer::LineBuffer;
pub(crate) use resolve::{operator_span, resolve_motion};
pub(crate) use rest_policy::{commit, recohere, RestPolicy};
