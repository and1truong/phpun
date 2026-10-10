//! phpun-core: PHP 8.5 language semantics implemented in Rust.
//!
//! Pipeline today: `lexer` → `parser` → `interp` (tree-walking).
//! The IR boundary (`ast`) is deliberately stable so a bytecode compiler and
//! VM can slot in later without changing the surface syntax layers.

pub mod ast;
pub mod builtins;
pub mod error;
pub mod fmt;
pub mod highlight;
pub mod interp;
pub mod lexer;
pub mod parser;
mod pcre;
pub mod pdo;
pub mod serve;
mod tzdata;
pub mod value;

pub use error::PhpError;
pub use interp::{Interp, RunResult};

/// Dev-only alloc-site dump for PHPUN_ALLOC=1 (phpun binary prints it).
pub fn alloc_sites_dump() -> Vec<(String, usize)> {
    crate::interp::util::ALLOC_SITE_NAMES
        .iter()
        .enumerate()
        .map(|(i, n)| {
            (
                n.to_string(),
                crate::interp::util::ALLOC_SITES[i].load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .collect()
}

/// Arm the alloc_hit counters (PHPUN_ALLOC=1 path in the phpun binary).
pub fn set_alloc_counting() {
    crate::interp::util::ALLOC_ON.store(1, std::sync::atomic::Ordering::Relaxed);
}
