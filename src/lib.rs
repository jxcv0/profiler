use std::arch::x86_64::_rdtsc;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

#[cfg(feature = "egui")]
mod egui_ui;
#[cfg(feature = "egui")]
pub use egui_ui::{scope_color, show_scopes, show_timeline};

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
pub use server::ProfileServer;

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
    window: Duration,
    scopes: RefCell<BTreeMap<Vec<&'static str>, ScopeStats>>,
    recent: RefCell<BTreeMap<Vec<&'static str>, VecDeque<TimeBucket>>>,
    active_scopes: RefCell<Vec<Vec<&'static str>>>,
}

#[derive(Default)]
struct ScopeStats {
    hits: u64,
    ticks: u64,
}

/// The hits and ticks of scopes that ended within one [`BUCKET`] of time.
struct TimeBucket {
    index: u64,
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
        let tsc_end = rdtsc();
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
        self.profiler
            .record(path, tsc_end, tsc_end - self.tsc_start);
    }
}

impl Default for Profiler {
    fn default() -> Self {
        Profiler::new()
    }
}

/// How far back [`Profiler::new`] looks for [`Profiler::recent_snapshot`].
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(10);

/// Granularity of the recent window: a scope counts as recent by the bucket it ended in.
const BUCKET: Duration = Duration::from_millis(100);

impl Profiler {
    pub fn new() -> Self {
        Self::with_window(DEFAULT_WINDOW)
    }

    /// Create a profiler whose [`Profiler::recent_snapshot`] covers the last `window`.
    pub fn with_window(window: Duration) -> Self {
        let tsc_freq = estimate_tsc_frequency();
        Self {
            tsc_freq,
            window,
            scopes: RefCell::new(BTreeMap::new()),
            recent: RefCell::new(BTreeMap::new()),
            active_scopes: RefCell::new(Vec::new()),
        }
    }

    /// How far back [`Profiler::recent_snapshot`] looks.
    pub fn window(&self) -> Duration {
        self.window
    }

    fn record(&self, path: Vec<&'static str>, tsc_end: u64, ticks: u64) {
        let index = self.bucket_index(tsc_end);
        let oldest = self.oldest_bucket(index);
        let mut recent = self.recent.borrow_mut();
        let buckets = recent.entry(path.clone()).or_default();
        match buckets.back_mut() {
            Some(last) if last.index == index => {
                last.hits += 1;
                last.ticks += ticks;
            }
            _ => buckets.push_back(TimeBucket {
                index,
                hits: 1,
                ticks,
            }),
        }
        while buckets.front().is_some_and(|bucket| bucket.index < oldest) {
            buckets.pop_front();
        }
        let mut scopes = self.scopes.borrow_mut();
        let stats = scopes.entry(path).or_default();
        stats.hits += 1;
        stats.ticks += ticks;
    }

    fn bucket_index(&self, tsc: u64) -> u64 {
        (tsc as f64 / (self.tsc_freq * BUCKET.as_secs_f64())) as u64
    }

    /// The oldest bucket still inside the window when `newest` is the current bucket.
    fn oldest_bucket(&self, newest: u64) -> u64 {
        (newest + 1).saturating_sub(self.window_buckets() as u64)
    }

    fn window_buckets(&self) -> usize {
        (self.window.as_secs_f64() / BUCKET.as_secs_f64()).ceil() as usize
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

    /// Return completed scopes as a tree, for a UI such as egui to draw each frame.
    /// Siblings are sorted by name, and each total includes child time.
    /// A parent that is still active appears with zero hits so its children have a place.
    pub fn snapshot(&self) -> Vec<ScopeReport> {
        let scopes = self.scopes.borrow();
        self.tree(
            scopes
                .iter()
                .map(|(path, stats)| (path, stats.hits, stats.ticks)),
        )
    }

    /// Like [`Profiler::snapshot`], but counting only scopes that ended in the last
    /// [`Profiler::window`], to a granularity of 100 ms.
    pub fn recent_snapshot(&self) -> Vec<ScopeReport> {
        self.snapshot_at(rdtsc())
    }

    fn snapshot_at(&self, tsc_now: u64) -> Vec<ScopeReport> {
        let oldest = self.oldest_bucket(self.bucket_index(tsc_now));
        let recent = self.recent.borrow();
        self.tree(recent.iter().filter_map(|(path, buckets)| {
            let in_window = buckets.iter().filter(|bucket| bucket.index >= oldest);
            let (hits, ticks) = in_window.fold((0, 0), |(hits, ticks), bucket| {
                (hits + bucket.hits, ticks + bucket.ticks)
            });
            (hits > 0).then_some((path, hits, ticks))
        }))
    }

    /// Self time of each scope in every 100 ms bucket of the last [`Profiler::window`],
    /// for drawing as a stacked time series. A scope counts in the bucket it ended in.
    pub fn recent_series(&self) -> TimeSeries {
        self.series_at(rdtsc())
    }

    fn series_at(&self, tsc_now: u64) -> TimeSeries {
        let newest = self.bucket_index(tsc_now);
        let length = self.window_buckets();
        let recent = self.recent.borrow();
        let totals: BTreeMap<&[&'static str], Vec<u64>> = recent
            .iter()
            .map(|(path, buckets)| {
                let mut ticks = vec![0; length];
                for bucket in buckets.iter().filter(|bucket| bucket.index <= newest) {
                    let age = (newest - bucket.index) as usize;
                    if age < length {
                        ticks[length - 1 - age] += bucket.ticks;
                    }
                }
                (path.as_slice(), ticks)
            })
            .collect();

        let mut children_ticks: BTreeMap<&[&'static str], Vec<u64>> = BTreeMap::new();
        for (path, ticks) in &totals {
            let parent = &path[..path.len() - 1];
            if !parent.is_empty() {
                let sums = children_ticks
                    .entry(parent)
                    .or_insert_with(|| vec![0; length]);
                sums.iter_mut()
                    .zip(ticks)
                    .for_each(|(sum, ticks)| *sum += ticks);
            }
        }

        let scopes = totals
            .iter()
            .filter_map(|(path, ticks)| {
                let children = children_ticks.get(path);
                let self_ticks = ticks.iter().enumerate().map(|(position, &ticks)| {
                    ticks.saturating_sub(children.map_or(0, |children| children[position]))
                });
                let self_ticks: Vec<u64> = self_ticks.collect();
                self_ticks
                    .iter()
                    .any(|&ticks| ticks > 0)
                    .then(|| ScopeSeries {
                        path: path.to_vec(),
                        self_times: self_ticks
                            .iter()
                            .map(|&ticks| self.duration(ticks))
                            .collect(),
                    })
            })
            .collect();
        TimeSeries {
            bucket: BUCKET,
            scopes,
        }
    }

    fn duration(&self, ticks: u64) -> Duration {
        Duration::from_secs_f64(ticks as f64 / self.tsc_freq)
    }

    /// Nest `(path, hits, ticks)` entries, given in path order, into a tree of reports.
    fn tree<'a>(
        &self,
        entries: impl Iterator<Item = (&'a Vec<&'static str>, u64, u64)>,
    ) -> Vec<ScopeReport> {
        let mut roots = Vec::new();
        // Sorted paths visit each parent before its children, and siblings in name order.
        for (path, hits, ticks) in entries {
            let (name, parents) = path.split_last().unwrap();
            let mut siblings = &mut roots;
            for &parent in parents {
                siblings = ScopeReport::children_named(siblings, parent);
            }
            siblings.push(ScopeReport {
                name,
                hits,
                total: Duration::from_secs_f64(ticks as f64 / self.tsc_freq),
                children: Vec::new(),
            });
        }
        roots
    }
}

/// [`Profiler::recent_series`]: per-bucket self time for each scope, oldest bucket first.
#[derive(Clone, Debug, PartialEq)]
pub struct TimeSeries {
    /// How much time each bucket covers.
    pub bucket: Duration,
    /// Scopes with some self time in the window, in path order.
    pub scopes: Vec<ScopeSeries>,
}

/// One scope's self time in each bucket of a [`TimeSeries`].
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeSeries {
    pub path: Vec<&'static str>,
    pub self_times: Vec<Duration>,
}

/// One node of [`Profiler::snapshot`]: a scope path's totals and its child scopes.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeReport {
    pub name: &'static str,
    pub hits: u64,
    /// Total time across all hits, including children.
    pub total: Duration,
    pub children: Vec<ScopeReport>,
}

impl ScopeReport {
    /// Time spent in this scope itself, outside its child scopes.
    pub fn self_time(&self) -> Duration {
        let children: Duration = self.children.iter().map(|child| child.total).sum();
        self.total.saturating_sub(children)
    }

    /// Mean time per hit, including children, or `None` for a scope with no hits.
    pub fn average(&self) -> Option<Duration> {
        let hits = u32::try_from(self.hits).unwrap_or(u32::MAX);
        (hits > 0).then(|| self.total / hits)
    }

    /// Find the children of the last sibling called `name`, adding an unfinished one if absent.
    fn children_named<'a>(
        siblings: &'a mut Vec<ScopeReport>,
        name: &'static str,
    ) -> &'a mut Vec<ScopeReport> {
        if siblings.last().is_none_or(|last| last.name != name) {
            siblings.push(ScopeReport {
                name,
                hits: 0,
                total: Duration::ZERO,
                children: Vec::new(),
            });
        }
        &mut siblings.last_mut().unwrap().children
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
            window: DEFAULT_WINDOW,
            recent: RefCell::new(BTreeMap::new()),
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

    fn report(
        name: &'static str,
        hits: u64,
        milliseconds: u64,
        children: Vec<ScopeReport>,
    ) -> ScopeReport {
        ScopeReport {
            name,
            hits,
            total: Duration::from_millis(milliseconds),
            children,
        }
    }

    #[test]
    fn snapshot_nests_children_sorted_by_name() {
        let profiler = Profiler {
            tsc_freq: 1000.0,
            window: DEFAULT_WINDOW,
            recent: RefCell::new(BTreeMap::new()),
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
                (vec!["load"], ScopeStats { hits: 1, ticks: 5 }),
            ])),
            active_scopes: RefCell::new(Vec::new()),
        };

        assert_eq!(
            profiler.snapshot(),
            vec![
                report(
                    "frame",
                    2,
                    100,
                    vec![
                        report("render", 2, 20, vec![]),
                        report("update", 2, 30, vec![report("physics", 2, 10, vec![])]),
                    ],
                ),
                report("load", 1, 5, vec![]),
            ]
        );
    }

    #[test]
    fn self_time_excludes_child_scopes() {
        let frame = report(
            "frame",
            2,
            100,
            vec![
                report("render", 2, 20, vec![]),
                report("update", 2, 30, vec![]),
            ],
        );

        assert_eq!(frame.self_time(), Duration::from_millis(50));
    }

    #[test]
    fn self_time_of_an_unfinished_parent_is_zero() {
        let frame = report("frame", 0, 0, vec![report("update", 1, 30, vec![])]);

        assert_eq!(frame.self_time(), Duration::ZERO);
    }

    #[test]
    fn average_divides_the_total_by_the_hits() {
        let frame = report("frame", 4, 100, vec![report("render", 4, 20, vec![])]);

        assert_eq!(frame.average(), Some(Duration::from_millis(25)));
    }

    #[test]
    fn a_scope_with_no_hits_has_no_average() {
        let frame = report("frame", 0, 0, vec![report("update", 1, 30, vec![])]);

        assert_eq!(frame.average(), None);
    }

    #[test]
    fn recent_snapshot_averages_only_hits_within_the_window() {
        let profiler = millisecond_profiler();
        profiler.record(vec!["frame"], 1_000, 90);
        profiler.record(vec!["frame"], 11_000, 4);
        profiler.record(vec!["frame"], 12_000, 6);

        let recent = profiler.snapshot_at(12_000);

        assert_eq!(recent[0].average(), Some(Duration::from_millis(5)));
    }

    /// A profiler whose clock ticks once per millisecond, for recording at chosen times.
    pub(crate) fn millisecond_profiler() -> Profiler {
        Profiler {
            tsc_freq: 1000.0,
            window: Duration::from_secs(10),
            scopes: RefCell::new(BTreeMap::new()),
            recent: RefCell::new(BTreeMap::new()),
            active_scopes: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn recent_snapshot_counts_only_scopes_that_ended_within_the_window() {
        let profiler = millisecond_profiler();
        profiler.record(vec!["frame"], 1_000, 5);
        profiler.record(vec!["frame"], 12_000, 7);

        assert_eq!(
            profiler.snapshot_at(12_000),
            vec![report("frame", 1, 7, vec![])]
        );
        assert_eq!(profiler.snapshot(), vec![report("frame", 2, 12, vec![])]);
    }

    #[test]
    fn recent_snapshot_sums_every_hit_within_the_window() {
        let profiler = millisecond_profiler();
        profiler.record(vec!["frame"], 3_000, 4);
        profiler.record(vec!["frame"], 3_050, 6);
        profiler.record(vec!["frame"], 9_000, 10);

        assert_eq!(
            profiler.snapshot_at(10_000),
            vec![report("frame", 3, 20, vec![])]
        );
    }

    #[test]
    fn recent_snapshot_omits_scopes_with_no_hits_in_the_window() {
        let profiler = millisecond_profiler();
        profiler.record(vec!["frame", "update"], 1_000, 2);
        profiler.record(vec!["frame"], 1_000, 5);
        profiler.record(vec!["load"], 12_000, 3);

        assert_eq!(
            profiler.snapshot_at(12_000),
            vec![report("load", 1, 3, vec![])]
        );
    }

    #[test]
    fn time_buckets_older_than_the_window_are_dropped() {
        let profiler = millisecond_profiler();
        for millisecond in (0..60_000).step_by(10) {
            profiler.record(vec!["frame"], millisecond, 1);
        }

        let buckets = profiler.recent.borrow()[&vec!["frame"]].len();
        assert!(buckets <= 100, "{buckets} buckets kept for a 10 s window");
    }

    /// A series of 100 buckets, zero except for `(position, milliseconds)` pairs.
    fn series(path: Vec<&'static str>, values: &[(usize, u64)]) -> ScopeSeries {
        let mut self_times = vec![Duration::ZERO; 100];
        for &(position, milliseconds) in values {
            self_times[position] = Duration::from_millis(milliseconds);
        }
        ScopeSeries { path, self_times }
    }

    #[test]
    fn recent_series_puts_self_time_in_the_bucket_each_scope_ended_in() {
        let profiler = millisecond_profiler();
        profiler.record(vec!["load"], 5_050, 7);
        profiler.record(vec!["frame", "render"], 9_940, 10);
        profiler.record(vec!["frame"], 9_950, 30);

        // At 10.0 s the window holds buckets 0.1 s to 10.0 s, so 9.95 s is position 98.
        assert_eq!(
            profiler.series_at(10_000),
            TimeSeries {
                bucket: Duration::from_millis(100),
                scopes: vec![
                    series(vec!["frame"], &[(98, 20)]),
                    series(vec!["frame", "render"], &[(98, 10)]),
                    series(vec!["load"], &[(49, 7)]),
                ],
            }
        );
    }

    #[test]
    fn recent_series_leaves_out_scopes_with_no_self_time_in_the_window() {
        let profiler = millisecond_profiler();
        profiler.record(vec!["old"], 1_000, 5);
        profiler.record(vec!["frame", "update"], 11_950, 10);
        profiler.record(vec!["frame"], 11_950, 10);

        let recent = profiler.series_at(12_000);

        assert_eq!(
            recent.scopes,
            vec![series(vec!["frame", "update"], &[(98, 10)])]
        );
    }

    #[test]
    fn snapshot_shows_an_unfinished_parent_with_zero_hits() {
        let profiler = Profiler::new();
        profile_scope!(profiler, "frame", {
            profile_scope!(profiler, "update", {});
            let snapshot = profiler.snapshot();

            assert_eq!(snapshot.len(), 1);
            assert_eq!(snapshot[0].name, "frame");
            assert_eq!(snapshot[0].hits, 0);
            assert_eq!(snapshot[0].total, Duration::ZERO);
            assert_eq!(snapshot[0].children.len(), 1);
            assert_eq!(snapshot[0].children[0].name, "update");
            assert_eq!(snapshot[0].children[0].hits, 1);
        });
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
