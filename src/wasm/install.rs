//! Installation helpers for wasm artifacts.
//!
//! Shared by the plugin, effect and extension installers. A wasm source
//! directory is recognized by any of:
//!
//! - a source manifest (`xfetch-<label>.json`, `<label>.json` or
//!   `<name>.json`) that declares a wasm runtime, `artifact`, `artifact_url`
//!   or `build`;
//! - a prebuilt artifact following the conventional locations
//!   (`artifact` field, `dist/<name>.wasm`, `<name>.wasm`,
//!   `xfetch-<label>-<name>.wasm`, or the cargo wasm targets).
//!
//! Installation copies the artifact as `<prefix><name>.wasm` and the manifest
//! as `<prefix><name>.json` (the sidecar convention the runtime resolves), so
//! installed wasm guests look exactly like native ones to the loaders.
//!
//! Remote sources can ship prebuilt artifacts through `artifact_url`, which
//! lets users install wasm guests without any language toolchain installed.

use crate::wasm::detect::is_wasm_file;
use crate::wasm::manifest::Manifest;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Upper bound for a downloaded artifact (release wasm files are small; this
/// only guards against runaway responses).
const DOWNLOAD_LIMIT: u64 = 64 * 1024 * 1024;

/// What the installer should do with a source directory.
#[derive(Debug)]
pub enum SourceAction {
    /// A wasm artifact is ready to install.
    Install(InstallPlan),
    /// Run a build command, then plan again.
    BuildThenInstall { command: String },
    /// Fetch a prebuilt artifact from a URL.
    Download {
        url: String,
        manifest: Option<Manifest>,
    },
    /// Not a wasm source: the caller falls back to the native cargo flow.
    NotWasm,
}

/// A resolved artifact plus its optional manifest.
#[derive(Debug)]
pub struct InstallPlan {
    pub artifact: PathBuf,
    pub manifest_path: Option<PathBuf>,
    pub manifest: Option<Manifest>,
}

/// Inspects a source directory and decides how to install it.
///
/// `label` is `plugin`, `effect` or `extension`; `name` is the guest name.
pub fn plan(dir: &Path, label: &str, name: &str) -> Result<SourceAction, String> {
    let manifest_path = find_source_manifest(dir, label);
    let manifest = match &manifest_path {
        Some(path) => Some(parse_manifest(path)?),
        None => None,
    };

    if let Some(artifact) = find_artifact(dir, label, name, manifest.as_ref()) {
        return Ok(SourceAction::Install(InstallPlan {
            artifact,
            manifest_path,
            manifest,
        }));
    }

    if let Some(manifest) = &manifest
        && let Some(url) = manifest.artifact_url.clone()
    {
        return Ok(SourceAction::Download {
            url,
            manifest: Some(manifest.clone()),
        });
    }

    if let Some(command) = manifest.as_ref().and_then(|m| m.build.clone()) {
        return Ok(SourceAction::BuildThenInstall { command });
    }

    // No manifest and no artifact: only treat the directory as a wasm source
    // when an artifact actually exists, so plain native crates are untouched.
    if manifest.is_some() && declares_wasm(manifest.as_ref()) {
        return Err(format!(
            "Wasm source '{}' declares a wasm runtime but no artifact was found; \
             add an artifact, an artifact_url or a build command",
            dir.display()
        ));
    }

    Ok(SourceAction::NotWasm)
}

/// Runs the manifest build command inside the source directory.
pub fn run_build(dir: &Path, command: &str) -> Result<(), String> {
    println!("Building wasm guest in '{}'...", dir.display());

    #[cfg(unix)]
    let mut builder = {
        let mut builder = Command::new("sh");
        builder.arg("-c").arg(command);
        builder
    };
    #[cfg(windows)]
    let mut builder = {
        let mut builder = Command::new("cmd");
        builder.arg("/C").arg(command);
        builder
    };

    let status = builder
        .current_dir(dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| format!("Failed to run build command '{}': {}", command, err))?;

    if !status.success() {
        return Err(format!("Build command failed: {}", command));
    }

    Ok(())
}

/// Copies an installed plan into the destination directory.
pub fn install(
    plan: InstallPlan,
    dest_dir: &Path,
    prefix: &str,
    name: &str,
    label: &str,
) -> Result<(), String> {
    if !is_wasm_file(&plan.artifact) {
        return Err(format!(
            "Artifact '{}' is not a WebAssembly binary",
            plan.artifact.display()
        ));
    }

    fs::create_dir_all(dest_dir)
        .map_err(|err| format!("Failed to create {} directory: {}", label, err))?;

    let wasm_dest = dest_dir.join(format!("{}{}.wasm", prefix, name));
    fs::copy(&plan.artifact, &wasm_dest).map_err(|err| {
        format!(
            "Failed to copy wasm artifact '{}': {}",
            plan.artifact.display(),
            err
        )
    })?;

    if let (Some(manifest_path), Some(manifest)) = (&plan.manifest_path, &plan.manifest) {
        let sidecar = dest_dir.join(format!("{}{}.json", prefix, name));
        fs::write(
            &sidecar,
            serde_json::to_vec_pretty(manifest)
                .map_err(|err| format!("Failed to serialize manifest: {}", err))?,
        )
        .map_err(|err| format!("Failed to write manifest '{}': {}", sidecar.display(), err))?;
        println!("Installed manifest '{}'", manifest_path.display());
    }

    println!(
        "Installed wasm {} '{}' to {}",
        label,
        name,
        wasm_dest.display()
    );
    Ok(())
}

/// Downloads a prebuilt artifact from `url` and installs it, optionally with
/// the manifest shipped by the source repository.
pub fn download_and_install(
    url: &str,
    manifest: Option<Manifest>,
    dest_dir: &Path,
    prefix: &str,
    name: &str,
    label: &str,
) -> Result<(), String> {
    println!("Downloading wasm {} '{}' from {}...", label, name, url);

    let response = ureq::get(url)
        .timeout(Duration::from_secs(120))
        .call()
        .map_err(|err| format!("Failed to download '{}': {}", url, err))?;

    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(DOWNLOAD_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("Failed to read '{}': {}", url, err))?;

    if bytes.len() as u64 > DOWNLOAD_LIMIT {
        return Err(format!(
            "Downloaded artifact exceeds the {} MiB limit",
            DOWNLOAD_LIMIT / (1024 * 1024)
        ));
    }

    fs::create_dir_all(dest_dir)
        .map_err(|err| format!("Failed to create {} directory: {}", label, err))?;

    let wasm_dest = dest_dir.join(format!("{}{}.wasm", prefix, name));
    fs::write(&wasm_dest, &bytes)
        .map_err(|err| format!("Failed to write '{}': {}", wasm_dest.display(), err))?;

    if !is_wasm_file(&wasm_dest) {
        let _ = fs::remove_file(&wasm_dest);
        return Err(format!(
            "Downloaded file from '{}' is not a WebAssembly binary",
            url
        ));
    }

    if let Some(manifest) = manifest {
        let sidecar = dest_dir.join(format!("{}{}.json", prefix, name));
        fs::write(
            &sidecar,
            serde_json::to_vec_pretty(&manifest)
                .map_err(|err| format!("Failed to serialize manifest: {}", err))?,
        )
        .map_err(|err| format!("Failed to write manifest '{}': {}", sidecar.display(), err))?;
    }

    println!(
        "Installed wasm {} '{}' to {}",
        label,
        name,
        wasm_dest.display()
    );
    Ok(())
}

/// Installs a single `.wasm` file chosen by the user.
pub fn install_file(
    artifact: &Path,
    dest_dir: &Path,
    prefix: &str,
    name: &str,
    label: &str,
) -> Result<(), String> {
    let manifest_path = crate::wasm::manifest::sidecar_path(artifact).filter(|path| path.is_file());
    let manifest = match &manifest_path {
        Some(path) => Some(parse_manifest(path)?),
        None => None,
    };

    install(
        InstallPlan {
            artifact: artifact.to_path_buf(),
            manifest_path,
            manifest,
        },
        dest_dir,
        prefix,
        name,
        label,
    )
}

/// Derives a guest name from a URL or file path (`foo.wasm` -> `foo`).
pub fn name_from_path(path: &str) -> Option<String> {
    let trimmed = path.split(['?', '#']).next().unwrap_or(path);
    let file = trimmed.rsplit('/').next()?;
    let stem = file.strip_suffix(".wasm").unwrap_or(file);
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

/// Source manifest names accepted in a repository directory.
///
/// Only the explicit label names are recognized so an unrelated `<name>.json`
/// file in a native plugin directory can never be mistaken for a manifest.
fn find_source_manifest(dir: &Path, label: &str) -> Option<PathBuf> {
    let candidates = [format!("xfetch-{}.json", label), format!("{}.json", label)];
    candidates
        .iter()
        .map(|candidate| dir.join(candidate))
        .find(|path| path.is_file())
}

/// Parses a source manifest with a helpful error message.
fn parse_manifest(path: &Path) -> Result<Manifest, String> {
    let bytes = fs::read(path)
        .map_err(|err| format!("Failed to read manifest '{}': {}", path.display(), err))?;
    let mut manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|err| format!("Invalid manifest '{}': {}", path.display(), err))?;
    if manifest.manifest_version == 0 {
        manifest.manifest_version = crate::wasm::manifest::MANIFEST_VERSION;
    }
    Ok(manifest)
}

/// Resolves the artifact path using the manifest and conventional locations.
fn find_artifact(
    dir: &Path,
    label: &str,
    name: &str,
    manifest: Option<&Manifest>,
) -> Option<PathBuf> {
    if let Some(relative) = manifest.and_then(|m| m.artifact.as_deref()) {
        let candidate = dir.join(relative);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let candidates = [
        dir.join(format!("dist/{}.wasm", name)),
        dir.join(format!("{}.wasm", name)),
        dir.join(format!("xfetch-plugin-{}.wasm", name)),
        dir.join(format!("xfetch-effect-{}.wasm", name)),
        dir.join(format!("xfetch-extension-{}.wasm", name)),
        dir.join(format!("target/wasm32-wasip1/release/{}.wasm", name)),
        dir.join(format!(
            "target/wasm32-wasip1/release/xfetch-{}-{}.wasm",
            label, name
        )),
        dir.join(format!("target/wasm32-wasi/release/{}.wasm", name)),
    ];

    candidates.into_iter().find(|path| path.is_file())
}

/// True when a manifest asks for a wasm runtime.
fn declares_wasm(manifest: Option<&Manifest>) -> bool {
    let Some(manifest) = manifest else {
        return false;
    };
    if manifest.artifact.is_some() || manifest.artifact_url.is_some() || manifest.build.is_some() {
        return true;
    }
    matches!(
        manifest.runtime.as_deref(),
        Some("wasm") | Some("core") | Some("component")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xfetch-wasm-install-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn name_from_url_strips_extension_and_query() {
        assert_eq!(
            name_from_path("https://example.com/releases/foo.wasm?download=1"),
            Some("foo".to_string())
        );
        assert_eq!(
            name_from_path("/tmp/xfetch-plugin-bar.wasm"),
            Some("xfetch-plugin-bar".to_string())
        );
    }

    #[test]
    fn plan_detects_artifact_url() {
        let dir = temp_dir("url");
        fs::write(
            dir.join("xfetch-plugin.json"),
            r#"{ "artifact_url": "https://example.com/foo.wasm" }"#,
        )
        .expect("write manifest");

        match plan(&dir, "plugin", "foo").expect("plan") {
            SourceAction::Download { url, .. } => {
                assert_eq!(url, "https://example.com/foo.wasm");
            }
            other => panic!("expected download, got {:?}", other),
        }
    }

    #[test]
    fn plan_falls_back_to_native_without_hints() {
        let dir = temp_dir("native");
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"foo\"").expect("write cargo");
        assert!(matches!(
            plan(&dir, "plugin", "foo").expect("plan"),
            SourceAction::NotWasm
        ));
    }

    #[test]
    fn plan_requires_artifact_when_manifest_declares_wasm() {
        let dir = temp_dir("missing");
        fs::write(dir.join("xfetch-plugin.json"), r#"{ "runtime": "core" }"#)
            .expect("write manifest");
        assert!(plan(&dir, "plugin", "foo").is_err());
    }

    #[test]
    fn plan_ignores_unrelated_json_sidecar() {
        let dir = temp_dir("sidecar");
        fs::write(dir.join("foo.json"), r#"{ "unrelated": true }"#).expect("write json");
        // A JSON file named after the guest parses as a manifest but has no
        // wasm hints; the source is not wasm-only, so the native path wins.
        assert!(matches!(
            plan(&dir, "plugin", "foo").expect("plan"),
            SourceAction::NotWasm
        ));
    }
}
