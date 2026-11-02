mod painter;

// The test-only resize-query stub lives in the painter; engine tests
// install it from outside this module (niubash#822 backport: the resize
// resolution re-queries the tty).
#[cfg(test)]
pub(crate) use painter::{clear_resize_query_stub, stub_resize_query};
mod prompt_lines;
mod styled_text;
#[cfg(test)]
mod test_screen;
mod utils;

pub use painter::{Painter, PainterSuspendedState, RenderSnapshot, W};
pub(crate) use prompt_lines::PromptLines;
pub use styled_text::StyledText;
pub(crate) use utils::{
    escape_control, escape_control_keep_sgr, estimate_single_line_wraps, sgr_len,
};
