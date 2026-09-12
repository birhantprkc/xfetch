//! `xfetch wasm inspect` and `xfetch wasm wit`.
//!
//! Inspection is intentionally runtime-free: it reads the binary header, the
//! resolved manifest and reports capabilities and limits without ever
//! executing the guest. This is the tool authors use to debug a manifest
//! before wiring a guest into their config.

use crate::wasm::{WIT, detect, manifest};
use serde::Serialize;
use std::fs;
use std::path::Path;

/// Serializable inspection report (`--json`).
#[derive(Serialize)]
struct Report {
    path: String,
    size_bytes: u64,
    kind: String,
    manifest_source: String,
    manifest: manifest::Manifest,
}

/// Prints a human-readable or JSON report for one artifact.
pub fn inspect(path: &Path, json: bool) -> Result<(), String> {
    let bytes =
        fs::read(path).map_err(|err| format!("Failed to read '{}': {}", path.display(), err))?;
    let kind = detect::kind_from_header(bytes.get(..8).unwrap_or_default()).ok_or_else(|| {
        format!(
            "'{}' is not a WebAssembly binary (bad header)",
            path.display()
        )
    })?;
    let loaded = manifest::load(path)?;

    let report = Report {
        path: path.display().to_string(),
        size_bytes: bytes.len() as u64,
        kind: kind.as_str().to_string(),
        manifest_source: loaded.source.as_str().to_string(),
        manifest: loaded.manifest,
    };

    if json {
        let encoded = serde_json::to_string_pretty(&report)
            .map_err(|err| format!("Failed to serialize report: {}", err))?;
        println!("{}", encoded);
        return Ok(());
    }

    print_human(&report);
    Ok(())
}

/// Prints the embedded WIT contract.
pub fn print_wit() {
    print!("{}", WIT);
}

/// Human-readable rendering of the report.
fn print_human(report: &Report) {
    let manifest = &report.manifest;
    println!("Artifact:        {}", report.path);
    println!("Size:            {} bytes", report.size_bytes);
    println!("Kind:            {}", report.kind);
    println!("Manifest:        {}", report.manifest_source);

    if let Some(name) = &manifest.name {
        println!("Name:            {}", name);
    }
    if let Some(version) = &manifest.version {
        println!("Version:         {}", version);
    }
    if let Some(kind) = &manifest.kind {
        println!("Contract:        {}", kind);
    }
    if let Some(runtime) = &manifest.runtime {
        println!("Runtime hint:    {}", runtime);
    }

    println!(
        "Limits:          timeout {} ms, memory {} MiB, output {} KiB, host calls {} KiB",
        manifest.limits.timeout_ms,
        manifest.limits.memory_mb,
        manifest.limits.output_kb,
        manifest.limits.host_call_kb
    );

    print_list("HTTP allow", &manifest.capabilities.http.allow);
    print_list("Exec allow", &manifest.capabilities.exec.allow);
    print_list("Exec env", &manifest.capabilities.exec.env);
    print_list("Env", &manifest.capabilities.env);
    println!(
        "Args:            {}",
        if manifest.capabilities.args {
            "allowed"
        } else {
            "denied"
        }
    );

    if manifest.capabilities.fs.is_empty() {
        println!("Filesystem:      none");
    } else {
        println!("Filesystem:");
        for entry in &manifest.capabilities.fs {
            match entry {
                manifest::FsEntry::Path(path) => {
                    println!("  {} -> same path (read-only)", path);
                }
                manifest::FsEntry::Mount { host, guest, mode } => {
                    println!("  {} -> {} ({:?})", host, guest, mode);
                }
            }
        }
    }

    if let Some(artifact) = &manifest.artifact {
        println!("Artifact field:  {}", artifact);
    }
    if let Some(url) = &manifest.artifact_url {
        println!("Artifact URL:    {}", url);
    }
    if let Some(build) = &manifest.build {
        println!("Build command:   {}", build);
    }
}

/// Prints a labeled comma-separated list, or `none`.
fn print_list(label: &str, items: &[String]) {
    if items.is_empty() {
        println!("{:<16} none", format!("{}:", label));
    } else {
        println!("{:<16} {}", format!("{}:", label), items.join(", "));
    }
}
