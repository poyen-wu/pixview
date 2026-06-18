//! Library facade for pixview.
//!
//! The crate ships both a binary (`src/main.rs`) and this library so that
//! integration tests under `tests/` can exercise the archive module directly
//! without going through the interactive TUI.

pub mod archive;
