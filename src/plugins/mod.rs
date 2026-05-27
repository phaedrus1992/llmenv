//! Plugin + marketplace resolution and marketplace caching.
//!
//! Mirrors the MCP subsystem ([`crate::mcp`]): `resolve` selects plugin
//! collections by tag intersection and flattens them into render-ready entries;
//! `cache` fetches marketplace sources into a shared on-disk cache and reports a
//! content hash so a marketplace update invalidates the materialized scope.

pub mod cache;
pub mod resolve;
