//! End-to-end tests for the wasm runtime through the real CLI.
//!
//! Fixtures are compiled from WAT at test time, so the suite needs no wasm
//! toolchain and no checked-in binaries. Each test drives
//! `CARGO_BIN_EXE_xfetch wasm run` in a scratch directory, exercising header
//! detection, the WASI stdio protocol, host calls, capability denial and the
//! epoch timeout.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Scratch directory unique to one test process.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xfetch-wasm-e2e-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Compiles WAT text and writes it as `<dir>/<name>.wasm`.
fn write_fixture(dir: &Path, name: &str, wat_source: &str) -> PathBuf {
    let bytes = wat::parse_str(wat_source).expect("compile fixture");
    let path = dir.join(format!("{}.wasm", name));
    fs::write(&path, bytes).expect("write fixture");
    path
}

/// Runs `xfetch wasm run` and returns (success, stdout, stderr, elapsed).
fn run_guest(path: &Path, request: &str, timeout_secs: u64) -> (bool, String, String, Duration) {
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_xfetch"))
        .args([
            "wasm",
            "run",
            path.to_str().expect("utf-8 path"),
            "--request",
            request,
            "--timeout",
            &timeout_secs.to_string(),
        ])
        .output()
        .expect("run xfetch");
    let elapsed = started.elapsed();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        elapsed,
    )
}

/// Escapes a Rust string for a WAT text-format string literal.
fn wat_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A WASI command that ignores stdin and writes a fixed JSON response.
fn writer_wat(response: &str) -> String {
    format!(
        r#"(module
            (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 100) "{response}")
            (func (export "_start")
                (i32.store (i32.const 0) (i32.const 100))
                (i32.store (i32.const 4) (i32.const {len}))
                (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))"#,
        response = wat_string(response),
        len = response.len()
    )
}

#[test]
fn core_module_writes_json_response() {
    let dir = scratch("writer");
    let path = write_fixture(&dir, "writer", &writer_wat(r#"{"lines":["from-wasm"]}"#));

    let (success, stdout, stderr, _) = run_guest(&path, "{}", 10);

    assert!(success, "guest failed: {}", stderr);
    assert_eq!(stdout.trim(), r#"{"lines":["from-wasm"]}"#);
}

#[test]
fn core_module_respects_the_manifest_limits() {
    let dir = scratch("manifest");
    let path = write_fixture(&dir, "writer", &writer_wat(r#"{"lines":["ok"]}"#));
    fs::write(
        dir.join("writer.json"),
        r#"{
            "manifest_version": 1,
            "name": "writer",
            "kind": "info_provider",
            "limits": { "timeout_ms": 5000, "memory_mb": 32 }
        }"#,
    )
    .expect("write manifest");

    let output = Command::new(env!("CARGO_BIN_EXE_xfetch"))
        .args([
            "wasm",
            "inspect",
            path.to_str().expect("utf-8 path"),
            "--json",
        ])
        .output()
        .expect("inspect");
    assert!(output.status.success());

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("inspect JSON");
    assert_eq!(report["kind"], "core-module");
    assert_eq!(report["manifest_source"], "sidecar");
    assert_eq!(report["manifest"]["limits"]["memory_mb"], 32);
    assert_eq!(report["manifest"]["kind"], "info_provider");
}

#[test]
fn infinite_loop_is_interrupted_by_the_timeout() {
    let dir = scratch("timeout");
    let path = write_fixture(
        &dir,
        "spin",
        r#"(module
            (memory (export "memory") 1)
            (func (export "_start")
                (loop $forever (br $forever))))"#,
    );

    let (success, _, stderr, elapsed) = run_guest(&path, "{}", 1);

    assert!(!success, "runaway guest must be killed");
    assert!(
        stderr.contains("timeout"),
        "expected a timeout error, got: {}",
        stderr
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "timeout took too long: {:?}",
        elapsed
    );
}

#[test]
fn host_call_denial_is_returned_to_the_guest() {
    let dir = scratch("hostcall");
    let args = r#"{"url":"https://denied.example/"}"#;
    let wat_source = format!(
        r#"(module
            (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
            (import "xfetch" "host_call"
                (func $host_call (param i32 i32 i32 i32) (result i64)))
            (memory (export "memory") 1)
            (global $bump (mut i32) (i32.const 4096))
            (data (i32.const 100) "http")
            (data (i32.const 200) "{args}")
            (func (export "xfetch_alloc") (param $size i32) (result i32)
                (local $ptr i32)
                (local.set $ptr (global.get $bump))
                (global.set $bump (i32.add (global.get $bump) (local.get $size)))
                (local.get $ptr))
            (func (export "_start")
                (local $packed i64) (local $ptr i32) (local $len i32)
                (local.set $packed
                    (call $host_call
                        (i32.const 100) (i32.const 4)
                        (i32.const 200) (i32.const {args_len})))
                (local.set $ptr (i32.wrap_i64 (local.get $packed)))
                (local.set $len (i32.wrap_i64 (i64.shr_u (local.get $packed) (i64.const 32))))
                (i32.store (i32.const 0) (local.get $ptr))
                (i32.store (i32.const 4) (local.get $len))
                (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))"#,
        args = wat_string(args),
        args_len = args.len()
    );

    let path = write_fixture(&dir, "hostcall", &wat_source);
    let (success, stdout, stderr, _) = run_guest(&path, "{}", 10);

    assert!(success, "guest failed: {}", stderr);
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON response");
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["kind"], "denied");
}

#[test]
fn missing_start_export_reports_a_clear_error() {
    let dir = scratch("nostart");
    let path = write_fixture(
        &dir,
        "lib",
        r#"(module
            (memory (export "memory") 1)
            (func (export "not_start")))"#,
    );

    let (success, _, stderr, _) = run_guest(&path, "{}", 5);

    assert!(!success);
    assert!(
        stderr.contains("_start"),
        "expected a _start diagnostic, got: {}",
        stderr
    );
}

#[test]
fn inspect_rejects_non_wasm_files() {
    let dir = scratch("badfile");
    let path = dir.join("not-wasm.bin");
    fs::write(&path, b"#!/bin/sh\necho nope\n").expect("write file");

    let output = Command::new(env!("CARGO_BIN_EXE_xfetch"))
        .args(["wasm", "inspect", path.to_str().expect("utf-8 path")])
        .output()
        .expect("inspect");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not a WebAssembly binary"),
        "unexpected error: {}",
        stderr
    );
}
