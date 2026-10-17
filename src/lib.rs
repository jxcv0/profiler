use std::arch::x86_64::_rdtsc;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Profile a supplied block and return its value.
/// The profiler is borrowed, and repeated names share accumulated results.
/// The block keeps normal `return`, `?`, `break`, and `continue` behavior.
///
/// ```
/// use profiler::{Profiler, profile_scope};
///
/// let profiler = Profiler::new();
/// let values = profile_scope!(profiler, "sort", {
///     let mut values = vec![3, 1, 2];
///     values.sort_unstable();
///     values
/// });
/// assert_eq!(values, vec![1, 2, 3]);
/// profiler.report();
/// ```
///
/// With two arguments, `profile_scope!(profiler, "work");` instead profiles from that
/// statement until its enclosing block ends.
#[macro_export]
macro_rules! profile_scope {
    ($profiler:expr, $name:expr, $body:block $(,)?) => {{
        let _profile_scope = ($profiler).scope($name);
        $body
    }};
    ($profiler:expr, $name:expr $(,)?) => {
        let _profile_scope = ($profiler).scope($name);
    };
}

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

/// A single-threaded profiler that accumulates completed scopes by name.
pub struct Profiler {
    tsc_freq: f64,
    scopes: RefCell<HashMap<&'static str, ScopeStats>>,
}

#[derive(Default)]
struct ScopeStats {
    hits: u64,
    ticks: u64,
}

/// Records a scope when dropped. Nested scopes contribute to their parents' time.
/// Uses approximate RDTSC boundaries, with the same clock assumptions as calibration.
#[must_use = "bind this guard to a named variable to keep the scope running"]
pub struct ProfileScope<'a> {
    profiler: &'a Profiler,
    name: &'static str,
    tsc_start: u64,
}

impl Drop for ProfileScope<'_> {
    fn drop(&mut self) {
        let ticks = rdtsc() - self.tsc_start;
        let mut scopes = self.profiler.scopes.borrow_mut();
        let stats = scopes.entry(self.name).or_default();
        stats.hits += 1;
        stats.ticks += ticks;
    }
}

impl Default for Profiler {
    fn default() -> Self {
        Profiler::new()
    }
}

impl Profiler {
    pub fn new() -> Self {
        let tsc_freq = estimate_tsc_frequency();
        Self {
            tsc_freq,
            scopes: RefCell::new(HashMap::new()),
        }
    }

    /// Start a named scope. Bind the guard to a local such as `_scope`, not `_`.
    /// Repeated names share totals. Nested timings are inclusive, even for the same name.
    /// Use [`profile_scope!`] to create and bind the guard with a single statement.
    ///
    /// ```
    /// use profiler::Profiler;
    ///
    /// let profiler = Profiler::new();
    /// {
    ///     let _scope = profiler.scope("work");
    ///     std::thread::sleep(std::time::Duration::from_millis(10));
    /// }
    /// profiler.report();
    /// ```
    pub fn scope(&self, name: &'static str) -> ProfileScope<'_> {
        ProfileScope {
            profiler: self,
            name,
            tsc_start: rdtsc(),
        }
    }

    /// Print completed hit counts and total elapsed milliseconds in unspecified order.
    /// Active scopes are recorded only when their guards are dropped.
    /// Inclusive totals overlap, so adding all rows does not give application runtime.
    pub fn report(&self) {
        let scopes = self.scopes.borrow();
        for (name, stats) in scopes.iter() {
            let milliseconds = stats.ticks as f64 / self.tsc_freq * 1000.0;
            println!("{name}: {} hits, {milliseconds:.3} ms total", stats.hits);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_when_scope_ends() {
        let profiler = Profiler::new();
        let scope = profiler.scope("work");

        assert!(profiler.scopes.borrow().is_empty());
        drop(scope);

        let scopes = profiler.scopes.borrow();
        assert_eq!(scopes["work"].hits, 1);
    }

    #[test]
    fn nested_scopes_finish_independently() {
        let profiler = Profiler::new();
        profile_scope!(profiler, "outer", {
            profile_scope!(profiler, "inner", {});
            let scopes = profiler.scopes.borrow();
            assert_eq!(scopes["inner"].hits, 1);
            assert!(!scopes.contains_key("outer"));
        });

        let scopes = profiler.scopes.borrow();
        assert_eq!(scopes["outer"].hits, 1);
        assert_eq!(scopes["inner"].hits, 1);
    }

    #[test]
    fn repeated_and_nested_names_share_totals() {
        let profiler = Profiler::new();
        {
            let _first = profiler.scope("work");
            let _nested = profiler.scope("work");
        }
        let first_ticks = profiler.scopes.borrow()["work"].ticks;

        {
            let _third = profiler.scope("work");
        }

        let scopes = profiler.scopes.borrow();
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes["work"].hits, 3);
        assert!(scopes["work"].ticks >= first_ticks);
    }

    #[test]
    fn macro_guards_live_until_the_callers_block_ends() {
        let profiler = Profiler::new();
        {
            let _profile_scope = "caller variable";
            profile_scope!(profiler, "first");
            profile_scope!(&profiler, "second",);

            assert!(profiler.scopes.borrow().is_empty());
            assert_eq!(_profile_scope, "caller variable");
        }

        let scopes = profiler.scopes.borrow();
        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes["first"].hits, 1);
        assert_eq!(scopes["second"].hits, 1);
    }

    #[test]
    fn block_scope_returns_its_value_and_records_at_the_closing_brace() {
        let profiler = Profiler::new();
        let mut calls = 0;
        let value = profile_scope!(&profiler, "work", {
            assert!(profiler.scopes.borrow().is_empty());
            calls += 1;
            String::from("finished")
        },);

        assert_eq!(value, "finished");
        assert_eq!(calls, 1);
        assert_eq!(profiler.scopes.borrow()["work"].hits, 1);
    }

    #[test]
    fn block_scope_preserves_return_and_question_mark() {
        fn work(profiler: &Profiler, input: Result<u32, ()>) -> Result<u32, ()> {
            profile_scope!(profiler, "work", {
                let value = input?;
                if value == 0 {
                    return Ok(10);
                }
            });
            Ok(20)
        }

        let profiler = Profiler::new();
        assert_eq!(work(&profiler, Err(())), Err(()));
        assert_eq!(work(&profiler, Ok(0)), Ok(10));
        assert_eq!(work(&profiler, Ok(1)), Ok(20));
        assert_eq!(profiler.scopes.borrow()["work"].hits, 3);
    }

    #[test]
    fn block_scope_preserves_loop_control() {
        let profiler = Profiler::new();
        let mut completed = 0;
        for value in 0..3 {
            profile_scope!(profiler, "iteration", {
                if value == 0 {
                    continue;
                }
                if value == 2 {
                    break;
                }
                completed += 1;
            });
        }

        assert_eq!(completed, 1);
        assert_eq!(profiler.scopes.borrow()["iteration"].hits, 3);
    }
}
