//! GNU readline 8.x C ABI adapter built on ruline.
//!
//! The crate intentionally exports only the symbols declared in
//! `include/readline/*.h`.  Unimplemented GNU entry points are absent rather
//! than stubbed, so an audit can distinguish "not implemented yet" from
//! "implemented but silently doing nothing".

#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(clippy::missing_safety_doc)]

mod history;
mod interactive;
mod keymap;
mod keymap_defaults;
mod line;
mod state;

pub use history::*;
pub use interactive::*;
pub use line::*;
