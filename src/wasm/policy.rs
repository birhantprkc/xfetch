#![cfg_attr(not(feature = "wasm"), allow(dead_code))]
//! Capability policy derived from a manifest.
//!
//! The policy is the single authority consulted by every host call. It is
//! intentionally small and pure: patterns in, boolean decisions out, with unit
//! tests covering the edge cases. The runtime never inspects the environment
//! directly; everything goes through this module.

use crate::wasm::manifest::{Capabilities, FsEntry, FsMode, Manifest};
use std::path::PathBuf;

/// One resolved filesystem preopen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsMount {
    pub host: PathBuf,
    pub guest: String,
    pub mode: FsMode,
}

/// Resolved capability decisions for one guest invocation.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    http_allow: Vec<String>,
    exec_allow: Vec<String>,
    exec_env: Vec<String>,
    env: Vec<String>,
    fs: Vec<FsMount>,
    /// Whether `wasi:cli` arguments are exposed beyond `argv[0]`.
    pub args: bool,
    /// Cap for a single host-call response, in bytes.
    pub host_call_bytes: usize,
}

impl Policy {
    /// Builds a policy from a manifest, expanding `~` in filesystem hosts.
    pub fn from_manifest(manifest: &Manifest) -> Self {
        let Capabilities {
            http,
            exec,
            fs,
            env,
            args,
        } = &manifest.capabilities;

        let fs = fs.iter().filter_map(resolve_mount).collect();

        Self {
            http_allow: http.allow.clone(),
            exec_allow: exec.allow.clone(),
            exec_env: exec.env.clone(),
            env: env.clone(),
            fs,
            args: *args,
            host_call_bytes: manifest.limits.host_call_bytes(),
        }
    }

    /// True when the URL matches an `http.allow` pattern.
    ///
    /// Patterns glob-match the full URL, so `https://wttr.in/*` allows query
    /// strings and `https://*.example.com/api/*` restricts by host and path.
    pub fn http_allowed(&self, url: &str) -> bool {
        self.http_allow
            .iter()
            .any(|pattern| glob_match(pattern, url))
    }

    /// True when the program name matches an `exec.allow` pattern.
    ///
    /// Matching is done against the file name so `curl` allows `/usr/bin/curl`
    /// while `../evil` still has to match explicitly.
    pub fn exec_allowed(&self, program: &str) -> bool {
        let name = PathBuf::from(program)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.to_string());
        self.exec_allow
            .iter()
            .any(|pattern| glob_match(pattern, &name))
    }

    /// True when a child-process environment variable may be forwarded.
    pub fn exec_env_allowed(&self, name: &str) -> bool {
        self.exec_env
            .iter()
            .any(|pattern| pattern == "*" || pattern == name)
    }

    /// True when a WASI environment variable may be exposed to the guest.
    pub fn env_allowed(&self, name: &str) -> bool {
        self.env
            .iter()
            .any(|pattern| pattern == "*" || pattern == name)
    }

    /// Resolved filesystem preopens.
    pub fn fs_mounts(&self) -> &[FsMount] {
        &self.fs
    }
}

/// Expands an `FsEntry` into an `FsMount`. Missing homes and empty paths are
/// dropped rather than producing a broken preopen.
fn resolve_mount(entry: &FsEntry) -> Option<FsMount> {
    match entry {
        FsEntry::Path(path) => {
            let host = expand_tilde(path)?;
            let guest = host.to_string_lossy().into_owned();
            Some(FsMount {
                host,
                guest,
                mode: FsMode::Ro,
            })
        }
        FsEntry::Mount { host, guest, mode } => {
            if guest.trim().is_empty() {
                return None;
            }
            Some(FsMount {
                host: expand_tilde(host)?,
                guest: guest.clone(),
                mode: *mode,
            })
        }
    }
}

/// Expands a leading `~` using the user's home directory.
pub fn expand_tilde(path: &str) -> Option<PathBuf> {
    if path == "~" {
        return dirs::home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return Some(home.join(rest));
    }
    Some(PathBuf::from(path))
}

/// Minimal glob matcher supporting `*` (any sequence) and `?` (one character).
///
/// Implemented with the classic linear backtracking algorithm so patterns
/// cannot blow up exponentially.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut retry) = (None, 0usize);

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            retry = t;
            p += 1;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            retry += 1;
            t = retry;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }

    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::manifest::Manifest;

    #[test]
    fn glob_matches_star_patterns() {
        assert!(glob_match("https://wttr.in/*", "https://wttr.in/?format=3"));
        assert!(glob_match(
            "https://*.github.com/*",
            "https://api.github.com/users/x"
        ));
        assert!(!glob_match("https://wttr.in/*", "https://example.com/"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("https://wttr.in/*", "http://wttr.in/"));
    }

    #[test]
    fn glob_handles_question_mark_and_literals() {
        assert!(glob_match("file-?.txt", "file-a.txt"));
        assert!(!glob_match("file-?.txt", "file-ab.txt"));
        assert!(glob_match("curl", "curl"));
        assert!(!glob_match("curl", "curlx"));
    }

    #[test]
    fn empty_policy_denies_everything() {
        let policy = Policy::from_manifest(&Manifest::default());
        assert!(!policy.http_allowed("https://example.com"));
        assert!(!policy.exec_allowed("curl"));
        assert!(!policy.env_allowed("HOME"));
        assert!(!policy.exec_env_allowed("PATH"));
        assert!(policy.fs_mounts().is_empty());
    }

    #[test]
    fn exec_matches_file_name_only() {
        let json = r#"{ "capabilities": { "exec": { "allow": ["curl"] } } }"#;
        let manifest: Manifest = serde_json::from_str(json).expect("parse");
        let policy = Policy::from_manifest(&manifest);
        assert!(policy.exec_allowed("curl"));
        assert!(policy.exec_allowed("/usr/bin/curl"));
        assert!(!policy.exec_allowed("docker"));
    }

    #[test]
    fn string_fs_entry_mounts_read_only_at_host_path() {
        let json = r#"{ "capabilities": { "fs": ["/tmp"] } }"#;
        let manifest: Manifest = serde_json::from_str(json).expect("parse");
        let policy = Policy::from_manifest(&manifest);
        assert_eq!(policy.fs_mounts().len(), 1);
        assert_eq!(policy.fs_mounts()[0].guest, "/tmp");
        assert_eq!(policy.fs_mounts()[0].mode, FsMode::Ro);
    }
}
