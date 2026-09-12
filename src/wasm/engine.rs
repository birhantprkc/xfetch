//! Shared wasmtime engine and epoch ticker.
//!
//! Engine construction is expensive (compiler setup, cache connection) so it
//! happens once per process behind a `OnceLock`. The same engine serves core
//! modules and components, and a single detached ticker thread advances the
//! engine epoch every [`TICK_MS`] milliseconds: each store converts its
//! deadline into a tick count and traps when the epoch catches up, which is
//! how wall-clock timeouts interrupt runaway guests deterministically.
//!
//! Native code is cached by wasmtime under `~/.cache/xfetch/wasmtime`
//! (platform cache dir), keyed by the wasmtime version and the module hash, so
//! repeated xfetch runs pay compilation only once.

use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use wasmtime::{Cache, CacheConfig, Config, Engine};

/// Epoch tick period. Timeouts are rounded up to this resolution.
pub const TICK_MS: u64 = 10;

/// Process-wide engine.
static ENGINE: OnceLock<Engine> = OnceLock::new();
/// Guards the ticker thread so it starts exactly once.
static TICKER: OnceLock<()> = OnceLock::new();

/// Returns the shared engine, initializing it on first use.
pub fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
        let engine = build_engine();
        start_ticker(engine.clone());
        engine
    })
}

/// Converts a timeout into an epoch deadline relative to "now".
pub fn deadline_ticks(timeout: Duration) -> u64 {
    let millis = timeout.as_millis().max(1) as u64;
    millis.div_ceil(TICK_MS).saturating_add(1)
}

/// Builds the engine with epoch interruption and the on-disk compilation
/// cache. Falls back to a clean configuration when the cache cannot be set up
/// (read-only home, invalid config, ...).
fn build_engine() -> Engine {
    let mut config = Config::new();
    config.epoch_interruption(true);

    if let Some(cache) = build_cache() {
        config.cache(Some(cache));
    }

    Engine::new(&config).unwrap_or_else(|_| {
        Engine::new(&Config::new()).expect("wasmtime engine initialization must succeed")
    })
}

/// Opens the wasmtime cache under the xfetch cache directory.
fn build_cache() -> Option<Cache> {
    let directory = dirs::cache_dir()?.join("xfetch").join("wasmtime");
    let mut config = CacheConfig::new();
    config.with_directory(directory);
    Cache::new(config).ok()
}

/// Starts the detached epoch ticker. The thread lives until process exit; a
/// CLI fetch is short-lived, so no shutdown plumbing is required.
fn start_ticker(engine: Engine) {
    TICKER.get_or_init(|| {
        let _ = thread::Builder::new()
            .name("xfetch-wasm-epoch".to_string())
            .spawn(move || {
                let period = Duration::from_millis(TICK_MS);
                loop {
                    thread::sleep(period);
                    engine.increment_epoch();
                }
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_rounds_up_to_ticks() {
        assert_eq!(deadline_ticks(Duration::from_millis(1)), 2);
        assert_eq!(deadline_ticks(Duration::from_millis(10)), 2);
        assert_eq!(deadline_ticks(Duration::from_millis(11)), 3);
        assert_eq!(deadline_ticks(Duration::from_secs(30)), 3001);
    }

    #[test]
    fn engine_is_shared() {
        let first = engine();
        let second = engine();
        assert!(std::ptr::eq(first, second));
    }
}
