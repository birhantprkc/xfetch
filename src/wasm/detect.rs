//! WebAssembly artifact detection.
//!
//! xfetch treats a plugin/effect/extension as WebAssembly when the resolved
//! file starts with the wasm magic bytes, regardless of its name. Detection is
//! content-based (never extension-based) so a stale `.wasm` file with garbage
//! content is rejected before spawning a runtime.
//!
//! Two artifact shapes are recognized:
//!
//! - Core modules (`wasm32-wasip1` commands): header version `1`, run through
//!   WASI preview 1 with the JSON stdin/stdout protocol.
//! - Components: header version `13`, layer `1`, run through the component
//!   model with the WIT world in `wit/xfetch-runtime.wit`.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// The kind of WebAssembly artifact found on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmKind {
    /// A core module, typically targeting `wasm32-wasip1`.
    CoreModule,
    /// A component-model binary implementing one of the xfetch worlds.
    Component,
}

impl WasmKind {
    /// Stable label used in diagnostics and `xfetch wasm inspect`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CoreModule => "core-module",
            Self::Component => "component",
        }
    }
}

/// Every wasm binary (core or component) starts with the same magic.
const MAGIC: [u8; 4] = *b"\0asm";
/// Core module header version.
const CORE_VERSION: [u8; 4] = [0x01, 0x00, 0x00, 0x00];
/// Component header: version 13 (`0x0d`) plus layer 1 (bytes 6-7).
const COMPONENT_VERSION_LAYER: [u8; 4] = [0x0d, 0x00, 0x01, 0x00];
/// Size of the wasm preamble.
const HEADER_LEN: usize = 8;

/// Classifies an 8-byte wasm preamble.
pub fn kind_from_header(header: &[u8]) -> Option<WasmKind> {
    if header.len() < HEADER_LEN || header[..4] != MAGIC {
        return None;
    }

    let version = &header[4..8];
    if version == CORE_VERSION.as_slice() {
        Some(WasmKind::CoreModule)
    } else if version == COMPONENT_VERSION_LAYER.as_slice() {
        Some(WasmKind::Component)
    } else {
        None
    }
}

/// Reads the first bytes of `path` and classifies the artifact.
///
/// Returns `None` for missing, unreadable, too-short or non-wasm files; callers
/// fall back to the native subprocess path in that case.
pub fn kind_of_file(path: &Path) -> Option<WasmKind> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header).ok()?;
    kind_from_header(&header)
}

/// True when `path` is a WebAssembly artifact xfetch can execute.
pub fn is_wasm_file(path: &Path) -> bool {
    kind_of_file(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(version_layer: [u8; 4]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&version_layer);
        bytes
    }

    #[test]
    fn detects_core_module() {
        assert_eq!(
            kind_from_header(&header(CORE_VERSION)),
            Some(WasmKind::CoreModule)
        );
    }

    #[test]
    fn detects_component() {
        assert_eq!(
            kind_from_header(&header(COMPONENT_VERSION_LAYER)),
            Some(WasmKind::Component)
        );
    }

    #[test]
    fn rejects_unknown_version() {
        assert_eq!(kind_from_header(&header([0x02, 0x00, 0x00, 0x00])), None);
    }

    #[test]
    fn rejects_short_and_non_wasm_input() {
        assert_eq!(kind_from_header(b"\0asm"), None);
        assert_eq!(kind_from_header(b"#!/bin/sh\n"), None);
    }
}
