//! zerch — a smarter terminal log viewer.
//!
//! The crate is split into an `engine` (indexing + search over a log file) and,
//! in later phases, a pure `view` model and a `tui` layer. Pure logic lives in
//! the library so it is unit-tested directly; the binary (`src/main.rs`) is a
//! thin shell over it.

pub mod engine;
pub mod levels;
pub mod tui;
pub mod view;

/// Choose a rayon worker-thread count sized to the file. The parallel work
/// (indexing, searching) scales with the file, so tiny files don't need a pool at
/// all while huge files get every core. Heuristic: ~1 thread per 32 MiB, clamped
/// to `[1, cores]`. File size is a byte proxy for line count — approximate, but
/// cheap and known before indexing starts.
pub fn plan_threads(file_len: u64, cores: usize) -> usize {
    const PER_THREAD_BYTES: u64 = 32 * 1024 * 1024;
    let cores = cores.max(1);
    let by_size = (file_len / PER_THREAD_BYTES + 1) as usize;
    by_size.clamp(1, cores)
}

#[cfg(test)]
mod tests {
    use super::plan_threads;

    #[test]
    fn scales_with_size_up_to_cores() {
        // Tiny/empty files use a single thread regardless of core count.
        assert_eq!(plan_threads(0, 8), 1);
        assert_eq!(plan_threads(8 * 1024 * 1024, 8), 1);
        // Grows ~1 thread per 32 MiB.
        assert_eq!(plan_threads(32 * 1024 * 1024, 8), 2);
        assert_eq!(plan_threads(96 * 1024 * 1024, 8), 4);
        // Extremely large files are capped at the core count.
        assert_eq!(plan_threads(4 * 1024 * 1024 * 1024, 8), 8);
        // Never exceeds cores, never below 1.
        assert_eq!(plan_threads(u64::MAX, 4), 4);
        assert_eq!(plan_threads(u64::MAX, 0), 1);
    }
}
