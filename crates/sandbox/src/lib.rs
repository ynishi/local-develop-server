//! Sandboxed execution primitives for lds.
//!
//! Provides application-level sandboxing without Docker dependency:
//! - [`fs::SandboxFs`] — file operations with automatic snapshot and rollback
//! - (future) `python` — Python execution with preamble security guard

pub mod fs;
