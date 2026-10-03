//! phpun-core: PHP 8.5 language semantics implemented in Rust.
//!
//! Pipeline today: `lexer` → `parser` → `interp` (tree-walking).
//! The IR boundary (`ast`) is deliberately stable so a bytecode compiler and
//! VM can slot in later without changing the surface syntax layers.

pub mod ast;
pub mod builtins;
pub mod error;
pub mod highlight;
pub mod interp;
pub mod lexer;
pub mod parser;
mod pcre;
pub mod serve;
pub mod value;

pub use error::PhpError;
pub use interp::{Interp, RunResult};
