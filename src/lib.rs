use std::arch::x86_64::_rdtsc;
use std::time::{Duration, Instant};

fn rdtsc() -> u64 {
    // SAFETY: RDTSC is supported on this x86-64 target.
    unsafe { _rdtsc() }
}

/// Estimate timestamp-counter ticks per second using a 100 ms busy wait.
/// Assumes a constant-rate TSC synchronized across the CPUs we run on.
/// The clock reads are approximate boundaries; RDTSC does not serialize execution.
pub fn estimate_tsc_frequency() -> f64 {
    let calibration_time = Duration::from_millis(100);

    let tsc_start = rdtsc();
    let clock_start = Instant::now();
    let mut elapsed = Duration::ZERO;

    while elapsed < calibration_time {
        elapsed = clock_start.elapsed();
    }

    let tsc_end = rdtsc();
    let ticks = tsc_end - tsc_start;

    ticks as f64 / elapsed.as_secs_f64()
}
