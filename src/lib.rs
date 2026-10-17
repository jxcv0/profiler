use std::arch::x86_64::_rdtsc;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Profile a supplied block and return its value.
/// The profiler is borrowed, and repeated names under the same parent share results.
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

/// A single-threaded profiler that accumulates completed scopes by their nesting path.
pub struct Profiler {
    tsc_freq: f64,
    scopes: RefCell<BTreeMap<Vec<&'static str>, ScopeStats>>,
    active_scopes: RefCell<Vec<Vec<&'static str>>>,
}

#[derive(Default)]
struct ScopeStats {
    hits: u64,
    ticks: u64,
}

/// Records a scope when dropped. Nested scopes contribute to their parents' time.
/// Uses approximate RDTSC boundaries, with the same clock assumptions as calibration.
/// Guards must be dropped in reverse creation order, as they are in nested blocks.
#[must_use = "bind this guard to a named variable to keep the scope running"]
pub struct ProfileScope<'a> {
    profiler: &'a Profiler,
    path: Vec<&'static str>,
    tsc_start: u64,
}

impl Drop for ProfileScope<'_> {
    fn drop(&mut self) {
        let ticks = rdtsc() - self.tsc_start;
        let path = {
            let mut active = self.profiler.active_scopes.borrow_mut();
            let position = active.iter().rposition(|path| path == &self.path).unwrap();
            let was_innermost = position + 1 == active.len();
            // Remove this guard before checking the order so remaining guards can unwind.
            let path = active.remove(position);
            assert!(
                was_innermost,
                "profiling scopes must end in reverse creation order"
            );
            path
        };
        let mut scopes = self.profiler.scopes.borrow_mut();
        let stats = scopes.entry(path).or_default();
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
            scopes: RefCell::new(BTreeMap::new()),
            active_scopes: RefCell::new(Vec::new()),
        }
    }

    /// Start a named scope. Bind the guard to a local such as `_scope`, not `_`.
    /// Repeated names under the same parent share totals. Child time is included in parents.
    /// Finish guards in reverse creation order; the block macro handles this automatically.
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
        let path = {
            let mut active = self.active_scopes.borrow_mut();
            let mut path = active.last().cloned().unwrap_or_default();
            path.push(name);
            active.push(path.clone());
            path
        };
        ProfileScope {
            profiler: self,
            path,
            tsc_start: rdtsc(),
        }
    }

    /// Print completed scopes as a tree, with siblings sorted by name.
    /// Each total includes child time. Do not add parent and child totals together.
    /// Active scopes are recorded only when their guards are dropped.
    /// Report after all active scopes finish for consistent parent and child totals.
    pub fn report(&self) {
        print!("{}", self.format_report());
    }

    fn format_report(&self) -> String {
        let mut report = String::from("Scope timings (totals include children):\n");
        let scopes = self.scopes.borrow();
        for (path, stats) in scopes.iter() {
            let indent = "  ".repeat(path.len() - 1);
            let name = path.last().unwrap();
            let milliseconds = stats.ticks as f64 / self.tsc_freq * 1000.0;
            report.push_str(&format!(
                "{indent}{name}: {} hits, {milliseconds:.3} ms total\n",
                stats.hits
            ));
        }
        report
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
        assert_eq!(scopes[["work"].as_slice()].hits, 1);
    }

    #[test]
    fn nested_scopes_finish_independently() {
        let profiler = Profiler::new();
        profile_scope!(profiler, "outer", {
            profile_scope!(profiler, "inner", {});
            let scopes = profiler.scopes.borrow();
            assert_eq!(scopes[["outer", "inner"].as_slice()].hits, 1);
            assert!(!scopes.contains_key(["outer"].as_slice()));
        });

        let scopes = profiler.scopes.borrow();
        let outer = &scopes[["outer"].as_slice()];
        let inner = &scopes[["outer", "inner"].as_slice()];
        assert_eq!(outer.hits, 1);
        assert_eq!(inner.hits, 1);
        assert!(outer.ticks >= inner.ticks);
    }

    #[test]
    fn repeated_paths_share_totals_and_recursive_names_stay_separate() {
        let profiler = Profiler::new();
        {
            let _first = profiler.scope("work");
            let _nested = profiler.scope("work");
        }
        let first_ticks = profiler.scopes.borrow()[["work"].as_slice()].ticks;

        {
            let _third = profiler.scope("work");
        }

        let scopes = profiler.scopes.borrow();
        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes[["work"].as_slice()].hits, 2);
        assert_eq!(scopes[["work", "work"].as_slice()].hits, 1);
        assert!(scopes[["work"].as_slice()].ticks >= first_ticks);
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
        assert_eq!(scopes[["first"].as_slice()].hits, 1);
        assert_eq!(scopes[["first", "second"].as_slice()].hits, 1);
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
        assert_eq!(profiler.scopes.borrow()[["work"].as_slice()].hits, 1);
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
        assert_eq!(profiler.scopes.borrow()[["work"].as_slice()].hits, 3);
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
        assert_eq!(profiler.scopes.borrow()[["iteration"].as_slice()].hits, 3);
    }

    #[test]
    fn the_same_label_under_different_parents_has_separate_totals() {
        let profiler = Profiler::new();
        for _ in 0..2 {
            profile_scope!(profiler, "first", {
                profile_scope!(profiler, "work", {});
            });
        }
        profile_scope!(profiler, "second", {
            profile_scope!(profiler, "work", {});
        });

        let scopes = profiler.scopes.borrow();
        assert_eq!(scopes.len(), 4);
        assert_eq!(scopes[["first"].as_slice()].hits, 2);
        assert_eq!(scopes[["first", "work"].as_slice()].hits, 2);
        assert_eq!(scopes[["second"].as_slice()].hits, 1);
        assert_eq!(scopes[["second", "work"].as_slice()].hits, 1);
    }

    #[test]
    fn report_indents_children_and_does_not_add_their_time_to_the_parent() {
        let profiler = Profiler {
            tsc_freq: 1000.0,
            scopes: RefCell::new(BTreeMap::from([
                (vec!["frame", "update"], ScopeStats { hits: 2, ticks: 30 }),
                (
                    vec!["frame"],
                    ScopeStats {
                        hits: 2,
                        ticks: 100,
                    },
                ),
                (
                    vec!["frame", "update", "physics"],
                    ScopeStats { hits: 2, ticks: 10 },
                ),
                (vec!["frame", "render"], ScopeStats { hits: 2, ticks: 20 }),
            ])),
            active_scopes: RefCell::new(Vec::new()),
        };

        assert_eq!(
            profiler.format_report(),
            concat!(
                "Scope timings (totals include children):\n",
                "frame: 2 hits, 100.000 ms total\n",
                "  render: 2 hits, 20.000 ms total\n",
                "  update: 2 hits, 30.000 ms total\n",
                "    physics: 2 hits, 10.000 ms total\n",
            )
        );
    }

    #[test]
    fn unwinding_restores_the_active_scope_stack() {
        let profiler = Profiler::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            profile_scope!(profiler, "outer", {
                profile_scope!(profiler, "inner", {
                    panic!("test panic");
                });
            });
        }));

        assert!(result.is_err());
        assert!(profiler.active_scopes.borrow().is_empty());
        let scopes = profiler.scopes.borrow();
        assert_eq!(scopes[["outer"].as_slice()].hits, 1);
        assert_eq!(scopes[["outer", "inner"].as_slice()].hits, 1);
    }

    #[test]
    fn dropping_a_parent_first_reports_the_error_without_a_second_panic() {
        let profiler = Profiler::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let outer = profiler.scope("outer");
            let _inner = profiler.scope("inner");
            drop(outer);
        }));

        assert!(result.is_err());
        assert!(profiler.active_scopes.borrow().is_empty());
        profile_scope!(profiler, "after", {});
        assert_eq!(profiler.scopes.borrow()[["after"].as_slice()].hits, 1);
    }
}
