//! The Hemlock configuration language.
//!
//! A JunOS/Nightshade-style curly-brace hierarchical format:
//!
//! ```text
//! system {
//!     hostname hemlock-sw1;
//! }
//! interfaces {
//!     ethernet Ethernet0 {
//!         description "uplink to core";
//!         admin-state disabled;
//!     }
//! }
//! ```
//!
//! Two node shapes: **leaves** (`name value...;`) and **blocks**
//! (`name [key...] { ... }`). Comments: `#`, `//`, and `/* ... */`.
//! Values with whitespace or special characters are double-quoted.
//!
//! Parsing is a hand-rolled lexer + recursive descent (no grammar DSLs),
//! with line/column error reporting. `to_text()` renders the canonical
//! form; `parse(to_text(t)) == t` holds for every tree.

mod lexer;
mod parser;
mod tree;

pub use parser::{parse, ParseError};
pub use tree::{ConfigTree, Item};
