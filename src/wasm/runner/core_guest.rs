//! WASI preview 1 runner for core-module guests.
//!
//! The guest is a `wasm32-wasip1` command: the JSON request arrives on stdin,
//! the JSON response is written to stdout, and capabilities are exposed
//! through the `xfetch.host_call` import. Everything else is denied by the
//! sandbox: no filesystem unless preopened, no environment unless allowlisted,
//! no sockets, no processes except through the host operation.

use super::map_execution_error;
use crate::wasm::engine;
use crate::wasm::host::{self, HostContext};
use crate::wasm::manifest::{FsMode, Manifest};
use crate::wasm::policy::FsMount;
use std::time::Instant;
use wasmtime::{Caller, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

/// Cap for guest stderr forwarded to the terminal. Diagnostics are for humans;
/// large dumps are truncated by the pipe and reported through the exit path.
const STDERR_CAP: usize = 256 * 1024;

/// Store data for core-module guests.
struct CoreState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
    ctx: HostContext,
}

/// Runs a core module and returns its stdout buffer.
pub(super) fn run(
    bytes: &[u8],
    manifest: &Manifest,
    ctx: HostContext,
    request: &[u8],
    _kind: crate::wasm::GuestKind,
) -> Result<Vec<u8>, String> {
    let engine = engine::engine();
    let module = Module::new(engine, bytes)
        .map_err(|err| format!("Failed to compile wasm module '{}': {}", ctx.name, err))?;

    let stdout = MemoryOutputPipe::new(manifest.limits.output_bytes().max(1));
    let stderr = MemoryOutputPipe::new(STDERR_CAP);
    let wasi = build_wasi(&ctx, request, stdout.clone(), stderr.clone())?;

    let timeout = ctx.deadline.saturating_duration_since(Instant::now());
    let deadline = ctx.deadline;
    let name = ctx.name.clone();

    let mut linker: Linker<CoreState> = Linker::new(engine);
    p1::add_to_linker_sync(&mut linker, |state: &mut CoreState| &mut state.wasi)
        .map_err(|err| format!("Failed to configure WASI for '{}': {}", name, err))?;
    linker
        .func_wrap("xfetch", "host_call", host_call)
        .map_err(|err| format!("Failed to configure host calls for '{}': {}", name, err))?;

    let limits = StoreLimitsBuilder::new()
        .memory_size(manifest.limits.memory_bytes().max(1))
        .build();
    let mut store = Store::new(engine, CoreState { wasi, limits, ctx });
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(engine::deadline_ticks(timeout));
    store.epoch_deadline_trap();

    let pre = linker
        .instantiate_pre(&module)
        .map_err(|err| format!("Failed to prepare '{}': {}", name, err))?;
    let instance = pre
        .instantiate(&mut store)
        .map_err(|err| format!("Failed to instantiate '{}': {}", name, err))?;

    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|_| {
            format!(
                "Wasm guest '{}' does not export _start; build it as a WASI command \
                 (wasm32-wasip1)",
                name
            )
        })?;

    let result = start.call(&mut store, ());
    forward_stderr(&stderr);
    if let Err(err) = result {
        match super::exit_code(&err) {
            // `proc_exit(0)` is a clean shutdown (Go guests exit this way).
            Some(0) => {}
            Some(code) => {
                return Err(format!("Wasm guest '{}' exited with code {}", name, code));
            }
            None => return Err(map_execution_error(&name, deadline, timeout, err)),
        }
    }

    let output = stdout.contents().to_vec();
    if output.is_empty() {
        return Err(format!("Wasm guest '{}' produced no output", name));
    }

    Ok(output)
}

/// Builds the preview1 context: stdio pipes, allowlisted environment,
/// filesystem preopens and `argv`.
fn build_wasi(
    ctx: &HostContext,
    request: &[u8],
    stdout: MemoryOutputPipe,
    stderr: MemoryOutputPipe,
) -> Result<WasiP1Ctx, String> {
    let mut builder = WasiCtxBuilder::new();
    builder.stdin(MemoryInputPipe::new(request.to_vec()));
    builder.stdout(stdout);
    builder.stderr(stderr);
    // `argv` stays empty unless the manifest grants the `args` capability.
    if ctx.policy.args {
        builder.args(&[ctx.name.as_str()]);
    }

    for (name, value) in allowed_env(ctx) {
        builder.env(name, value);
    }

    for mount in ctx.policy.fs_mounts() {
        builder
            .preopened_dir(&mount.host, &mount.guest, mount_perms(mount))
            .map_err(|err| {
                format!(
                    "Failed to mount '{}' for '{}': {}",
                    mount.host.display(),
                    ctx.name,
                    err
                )
            })?;
    }

    Ok(builder.build_p1())
}

/// Maps a manifest mount mode to WASI permissions.
fn mount_perms(mount: &FsMount) -> FsPerms {
    match mount.mode {
        FsMode::Ro => FsPerms::ReadOnly,
        FsMode::Rw => FsPerms::ReadWrite,
    }
}

/// Environment variables forwarded from the parent process.
fn allowed_env(ctx: &HostContext) -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(name, _)| ctx.policy.env_allowed(name))
        .collect()
}

/// The `xfetch.host_call` host function.
///
/// Reads the operation name and JSON arguments from guest memory, dispatches
/// through the shared host layer, allocates the response with the guest's
/// `xfetch_alloc` export and returns the packed `(len << 32) | ptr`.
fn host_call(
    mut caller: Caller<'_, CoreState>,
    op_ptr: i32,
    op_len: i32,
    args_ptr: i32,
    args_len: i32,
) -> i64 {
    let Some(memory) = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
    else {
        return 0;
    };
    let Some(op) = read_string(&memory, &caller, op_ptr, op_len) else {
        return 0;
    };
    let args = read_bytes(&memory, &caller, args_ptr, args_len).unwrap_or_default();

    let response = {
        let data = caller.data();
        host::dispatch(&op, &args, &data.ctx)
    };

    let Some(alloc) = caller
        .get_export("xfetch_alloc")
        .and_then(|export| export.into_func())
    else {
        eprintln!(
            "[{}] host_call: guest does not export xfetch_alloc; host calls are unavailable",
            caller.data().ctx.name
        );
        return 0;
    };
    let Ok(alloc) = alloc.typed::<i32, i32>(&caller) else {
        return 0;
    };
    let Ok(ptr) = alloc.call(&mut caller, response.len() as i32) else {
        return 0;
    };
    if ptr <= 0 {
        return 0;
    }

    let Some(memory) = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
    else {
        return 0;
    };
    if memory.write(&mut caller, ptr as usize, &response).is_err() {
        return 0;
    }

    pack_pointer(ptr as u32, response.len() as u32)
}

/// Packs a guest pointer and length into the `host_call` return value.
fn pack_pointer(ptr: u32, len: u32) -> i64 {
    ((u64::from(len)) << 32 | u64::from(ptr)) as i64
}

/// Copies a UTF-8 range out of guest memory.
fn read_string(
    memory: &Memory,
    caller: &Caller<'_, CoreState>,
    ptr: i32,
    len: i32,
) -> Option<String> {
    let bytes = read_bytes(memory, caller, ptr, len)?;
    String::from_utf8(bytes).ok()
}

/// Copies an arbitrary range out of guest memory with bounds checks.
fn read_bytes(
    memory: &Memory,
    caller: &Caller<'_, CoreState>,
    ptr: i32,
    len: i32,
) -> Option<Vec<u8>> {
    if ptr < 0 || len < 0 {
        return None;
    }
    let start = ptr as usize;
    let end = start.checked_add(len as usize)?;
    memory.data(caller).get(start..end).map(<[u8]>::to_vec)
}

/// Forwards captured guest stderr to the terminal.
fn forward_stderr(stderr: &MemoryOutputPipe) {
    let bytes = stderr.contents();
    if !bytes.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pack_pointer_layout_is_stable() {
        let packed = pack_pointer(0x1234, 0x56);
        assert_eq!(packed as u64 & 0xffff_ffff, 0x1234);
        assert_eq!((packed as u64 >> 32) as u32, 0x56);
    }

    #[test]
    fn timeout_helper_handles_zero_duration() {
        assert_eq!(engine::deadline_ticks(Duration::from_millis(0)), 2);
    }
}
