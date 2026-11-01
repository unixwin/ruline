//! GNU readline 8.x C ABI adapter built on ruline.
//!
//! The crate intentionally exports only the symbols declared in
//! `include/readline/*.h`.  Unimplemented GNU entry points are absent rather
//! than stubbed, so an audit can distinguish "not implemented yet" from
//! "implemented but silently doing nothing".

#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(clippy::missing_safety_doc)]

mod callback;
mod chars;
mod completion;
mod display;
mod editing;
mod history;
mod interactive;
mod keymap;
mod keymap_defaults;
mod line;
mod macros;
mod misc_text;
mod state;
mod terminal;
mod undo;
mod yankarg;

pub use callback::*;
pub use chars::*;
pub use completion::*;
pub use display::*;
pub use editing::*;
pub use history::*;
pub use interactive::*;
pub use keymap::*;
pub use line::*;
pub use macros::*;
pub use misc_text::*;
pub use state::*;
pub use terminal::*;
pub use undo::*;
pub use yankarg::*;
