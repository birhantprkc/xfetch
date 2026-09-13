//! Effect installer (`xfetch effects install`), mirror of the plugin
//! installer: builds an effect crate and copies its binary to
//! `~/.config/xfetch/effects/`.

use crate::effects::{
    CARGO_CMD, CARGO_TOML, EFFECT_PREFIX, ENV_CARGO_NET_GIT_FETCH_WITH_CLI, GIT_CMD,
    TARGET_RELEASE, default_effect_dir, default_effect_repo, effect_binary_name,
};
use crate::wasm::install::SourceAction;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn has_path_separator(name: &str) -> bool {
    name.contains('/') || name.contains('\\')
}

/// True for http(s) URLs, which install prebuilt wasm artifacts directly.
fn is_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

/// Derives the effect name from a direct wasm file path.
fn effect_name_from_wasm_file(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let name = stem.strip_prefix("xfetch-effect-").unwrap_or(stem);
    (!name.is_empty()).then(|| name.to_string())
}

fn resolve_local_effect_dir(path: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(path);

    if candidate.is_dir() {
        return Ok(candidate);
    }

    if let Ok(cwd) = env::current_dir() {
        let mut search_paths = vec![
            cwd.join(path),
            cwd.join("effects").join(path),
            cwd.join("effects").join("effects").join(path),
        ];

        if let Some(parent) = cwd.parent() {
            search_paths.push(parent.join("effects").join(path));
            search_paths.push(parent.join("effects").join("effects").join(path));
        }

        for search_path in search_paths {
            if search_path.is_dir() {
                return Ok(search_path);
            }
        }
    }

    Err(format!("Effect not found locally: '{}'", path))
}

pub fn install_effect(name_or_path: &str, repo: Option<&str>) -> Result<(), String> {
    // Direct URL: download a prebuilt wasm artifact from a release.
    if is_url(name_or_path) {
        let name = crate::wasm::install::name_from_path(name_or_path)
            .ok_or_else(|| format!("Cannot derive an effect name from URL '{}'", name_or_path))?;
        return crate::wasm::install::download_and_install(
            name_or_path,
            None,
            &default_effect_dir(),
            EFFECT_PREFIX,
            &name,
            "effect",
        );
    }

    // Direct wasm file.
    let direct = PathBuf::from(name_or_path);
    if direct.is_file() && crate::wasm::is_wasm_file(&direct) {
        let name = effect_name_from_wasm_file(&direct)
            .ok_or_else(|| format!("Cannot derive an effect name from '{}'", name_or_path))?;
        return crate::wasm::install::install_file(
            &direct,
            &default_effect_dir(),
            EFFECT_PREFIX,
            &name,
            "effect",
        );
    }

    let effect_dir = resolve_local_effect_dir(name_or_path);

    match effect_dir {
        Ok(dir) => build_and_install_effect(&dir, name_or_path),
        Err(_) if repo.is_some() || has_path_separator(name_or_path) => {
            let default_repo = default_effect_repo();
            let repo_url = repo.unwrap_or(default_repo.as_str());
            install_remote_effect(name_or_path, repo_url)
        }
        Err(_) => {
            let default_repo = default_effect_repo();
            install_remote_effect(name_or_path, default_repo.as_str())
        }
    }
}

fn build_and_install_effect(effect_dir: &Path, name: &str) -> Result<(), String> {
    let effect_name = effect_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "Invalid effect directory name".to_string())?;

    // Wasm sources (prebuilt artifact, artifact_url or build command) are
    // installed through the shared wasm installer; native crates keep the
    // cargo flow below.
    match crate::wasm::install::plan(effect_dir, "effect", effect_name)? {
        SourceAction::Install(plan) => {
            return crate::wasm::install::install(
                plan,
                &default_effect_dir(),
                EFFECT_PREFIX,
                effect_name,
                "effect",
            );
        }
        SourceAction::Download { url, manifest } => {
            return crate::wasm::install::download_and_install(
                &url,
                manifest,
                &default_effect_dir(),
                EFFECT_PREFIX,
                effect_name,
                "effect",
            );
        }
        SourceAction::BuildThenInstall { command } => {
            crate::wasm::install::run_build(effect_dir, &command)?;
            return match crate::wasm::install::plan(effect_dir, "effect", effect_name)? {
                SourceAction::Install(plan) => crate::wasm::install::install(
                    plan,
                    &default_effect_dir(),
                    EFFECT_PREFIX,
                    effect_name,
                    "effect",
                ),
                _ => Err(format!(
                    "Build command in '{}' did not produce a wasm artifact",
                    effect_dir.display()
                )),
            };
        }
        SourceAction::NotWasm => {}
    }

    if !effect_dir.join(CARGO_TOML).exists() {
        let display = effect_dir.display();
        if name.contains('/') || name.contains('\\') {
            return Err(format!("No Cargo.toml found in '{}'", display));
        }
        return Err(format!(
            "Effect '{}' not found locally and could not be fetched remotely.\n\
             No Cargo.toml found in '{}'.\n\
             Try specifying a different path or check the effect name.",
            name, display
        ));
    }

    println!("Building effect '{}'...", effect_name);
    let status = Command::new(CARGO_CMD)
        .args(["build", "--release"])
        .env(ENV_CARGO_NET_GIT_FETCH_WITH_CLI, "true")
        .current_dir(effect_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| format!("Failed to run cargo: {}", err))?;

    if !status.success() {
        return Err("Cargo build failed".to_string());
    }

    let binary_name = effect_binary_name(effect_name);
    let mut built_binary_candidates = vec![effect_dir.join(TARGET_RELEASE).join(&binary_name)];

    let mut current = effect_dir.parent();
    while let Some(dir) = current {
        built_binary_candidates.push(dir.join(TARGET_RELEASE).join(&binary_name));
        current = dir.parent();
    }

    let built_binary = built_binary_candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            format!(
                "Built binary not found at '{}' or in any workspace target ancestor",
                effect_dir.join(TARGET_RELEASE).join(&binary_name).display(),
            )
        })?;

    let dest_dir = default_effect_dir();
    fs::create_dir_all(&dest_dir)
        .map_err(|err| format!("Failed to create effect directory: {}", err))?;

    let dest_path = dest_dir.join(&binary_name);
    crate::atomic_fs::copy_atomic(&built_binary, &dest_path)
        .map_err(|err| format!("Failed to copy effect binary: {}", err))?;

    println!(
        "Installed effect '{}' to {}",
        effect_name,
        dest_path.display()
    );
    Ok(())
}

fn install_remote_effect(name: &str, repo_url: &str) -> Result<(), String> {
    let temp_dir = env::temp_dir().join(format!("{}{}", EFFECT_PREFIX, name));

    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)
            .map_err(|err| format!("Failed to clean temp directory: {}", err))?;
    }

    let repo_display = repo_url.trim_end_matches(".git");
    println!("Fetching effect '{}' from {}...", name, repo_display);

    let status = Command::new(GIT_CMD)
        .args(["clone", "--depth", "1", repo_url])
        .arg(&temp_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| format!("Failed to run git: {}. Is git installed?", err))?;

    if !status.success() {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err("Failed to clone repository".to_string());
    }

    // The official repository nests effect crates under `effects/effects/`;
    // forks may keep them at the root or one level down.
    let effect_path = [
        temp_dir.join(name),
        temp_dir.join("effects").join(name),
        temp_dir.join("effects").join("effects").join(name),
    ]
    .into_iter()
    .find(|path| path.is_dir());

    let Some(effect_path) = effect_path else {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(format!(
            "Effect '{}' not found in repository '{}'.\n\
             Available effects can be found in the repository root or under {}/tree/main/effects",
            name, repo_display, repo_display
        ));
    };

    let result = build_and_install_effect(&effect_path, name);

    let _ = fs::remove_dir_all(&temp_dir);
    result
}
