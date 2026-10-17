use std::arch::x86_64::_rdtsc;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

#[cfg(feature = "egui")]
mod egui_ui;
#[cfg(feature = "egui")]
pub use egui_ui::{ProfilerUi, ShareOf, scope_color, show_scopes, show_timeline};

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
    paths: RefCell<Paths>,
    active_scopes: RefCell<Vec<ActiveScope>>,
}

/// Every scope path started so far, under a dense id that indexes `entries`.
#[derive(Default)]
struct Paths {
    ids: HashMap<(Option<usize>, &'static str), usize>,
    entries: Vec<PathEntry>,
}

/// One scope path's totals since the start, and its time buckets within the window.
struct PathEntry {
    path: Vec<&'static str>,
    stats: ScopeStats,
    recent: VecDeque<TimeBucket>,
}

impl Paths {
    /// The id of `name` under the path with id `parent`, adding the path if it is new.
    fn id(&mut self, parent: Option<usize>, name: &'static str) -> usize {
        if let Some(&id) = self.ids.get(&(parent, name)) {
            return id;
        }
        let mut path = parent.map_or_else(Vec::new, |parent| self.entries[parent].path.clone());
        path.push(name);
        let id = self.entries.len();
        self.entries.push(PathEntry {
            path,
            stats: ScopeStats::default(),
            recent: VecDeque::new(),
        });
        self.ids.insert((parent, name), id);
        id
    }

    /// Entries in path order: each parent before its children, and siblings by name.
    fn sorted(&self) -> Vec<&PathEntry> {
        let mut entries: Vec<&PathEntry> = self.entries.iter().collect();
        entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        entries
    }
}

/// A scope whose guard is still alive, and the time its finished children took so far.
struct ActiveScope {
    id: usize,
    child_ticks: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct ScopeStats {
    hits: u64,
    ticks: u64,
    self_ticks: u64,
}

/// The hits, ticks and self ticks of scopes that ended within one [`BUCKET`] of time.
struct TimeBucket {
    index: u64,
    hits: u64,
    ticks: u64,
    self_ticks: u64,
}

/// Records a scope when dropped. Nested scopes contribute to their parents' time.
/// Uses approximate RDTSC boundaries, with the same clock assumptions as calibration.
/// Guards must be dropped in reverse creation order, as they are in nested blocks.
#[must_use = "bind this guard to a named variable to keep the scope running"]
pub struct ProfileScope<'a> {
    profiler: &'a Profiler,
    id: usize,
    tsc_start: u64,
}

impl Drop for ProfileScope<'_> {
    fn drop(&mut self) {
        let tsc_end = rdtsc();
        let ticks = tsc_end - self.tsc_start;
        let finished = {
            let mut active = self.profiler.active_scopes.borrow_mut();
            let position = active
                .iter()
                .rposition(|scope| scope.id == self.id)
                .unwrap();
            let was_innermost = position + 1 == active.len();
            // Remove this guard before checking the order so remaining guards can unwind.
            let finished = active.remove(position);
            assert!(
                was_innermost,
                "profiling scopes must end in reverse creation order"
            );
            if let Some(parent) = active.last_mut() {
                parent.child_ticks += ticks;
            }
            finished
        };
        let self_ticks = ticks.saturating_sub(finished.child_ticks);
        self.profiler
            .record(finished.id, tsc_end, ticks, self_ticks);
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
            paths: RefCell::new(Paths::default()),
            active_scopes: RefCell::new(Vec::new()),
        }
    }

    /// How far back [`Profiler::recent_snapshot`] looks.
    pub fn window(&self) -> Duration {
        self.window
    }

    fn record(&self, id: usize, tsc_end: u64, ticks: u64, self_ticks: u64) {
        let index = self.bucket_index(tsc_end);
        let oldest = self.oldest_bucket(index);
        let mut paths = self.paths.borrow_mut();
        let entry = &mut paths.entries[id];
        let buckets = &mut entry.recent;
        match buckets.back_mut() {
            Some(last) if last.index == index => {
                last.hits += 1;
                last.ticks += ticks;
                last.self_ticks += self_ticks;
            }
            _ => buckets.push_back(TimeBucket {
                index,
                hits: 1,
                ticks,
                self_ticks,
            }),
        }
        while buckets.front().is_some_and(|bucket| bucket.index < oldest) {
            buckets.pop_front();
        }
        let stats = &mut entry.stats;
        stats.hits += 1;
        stats.ticks += ticks;
        stats.self_ticks += self_ticks;
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
        let id = {
            let mut active = self.active_scopes.borrow_mut();
            let parent = active.last().map(|parent| parent.id);
            let id = self.paths.borrow_mut().id(parent, name);
            active.push(ActiveScope { id, child_ticks: 0 });
            id
        };
        ProfileScope {
            profiler: self,
            id,
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
        let paths = self.paths.borrow();
        for PathEntry { path, stats, .. } in paths.sorted() {
            if stats.hits == 0 {
                continue;
            }
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
        let paths = self.paths.borrow();
        self.tree(paths.sorted().into_iter().filter_map(|entry| {
            let stats = entry.stats;
            (stats.hits > 0).then_some((&entry.path, stats.hits, stats.ticks, stats.self_ticks))
        }))
    }

    /// Like [`Profiler::snapshot`], but counting only scopes that ended in the last
    /// [`Profiler::window`], to a granularity of 100 ms.
    pub fn recent_snapshot(&self) -> Vec<ScopeReport> {
        self.snapshot_at(rdtsc())
    }

    fn snapshot_at(&self, tsc_now: u64) -> Vec<ScopeReport> {
        let oldest = self.oldest_bucket(self.bucket_index(tsc_now));
        let paths = self.paths.borrow();
        self.tree(paths.sorted().into_iter().filter_map(|entry| {
            let in_window = entry.recent.iter().filter(|bucket| bucket.index >= oldest);
            let (hits, ticks, self_ticks) =
                in_window.fold((0, 0, 0), |(hits, ticks, self_ticks), bucket| {
                    (
                        hits + bucket.hits,
                        ticks + bucket.ticks,
                        self_ticks + bucket.self_ticks,
                    )
                });
            (hits > 0).then_some((&entry.path, hits, ticks, self_ticks))
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
        let paths = self.paths.borrow();
        let scopes = paths
            .sorted()
            .into_iter()
            .filter_map(|entry| {
                let mut self_ticks = vec![0; length];
                for bucket in entry.recent.iter().filter(|bucket| bucket.index <= newest) {
                    let age = (newest - bucket.index) as usize;
                    if age < length {
                        self_ticks[length - 1 - age] += bucket.self_ticks;
                    }
                }
                self_ticks
                    .iter()
                    .any(|&ticks| ticks > 0)
                    .then(|| ScopeSeries {
                        path: entry.path.clone(),
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

    /// Nest `(path, hits, ticks, self_ticks)` entries, given in path order, into a tree.
    fn tree<'a>(
        &self,
        entries: impl Iterator<Item = (&'a Vec<&'static str>, u64, u64, u64)>,
    ) -> Vec<ScopeReport> {
        let mut roots = Vec::new();
        // Sorted paths visit each parent before its children, and siblings in name order.
        for (path, hits, ticks, self_ticks) in entries {
            let (name, parents) = path.split_last().unwrap();
            let mut siblings = &mut roots;
            for &parent in parents {
                siblings = ScopeReport::children_named(siblings, parent);
            }
            siblings.push(ScopeReport {
                name,
                hits,
                total: self.duration(ticks),
                self_total: self.duration(self_ticks),
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
    /// Time across all hits outside child scopes, measured as each hit ended.
    self_total: Duration,
    pub children: Vec<ScopeReport>,
}

impl ScopeReport {
    /// Time spent in this scope itself, outside its child scopes.
    pub fn self_time(&self) -> Duration {
        self.self_total
    }

    /// A report whose self time is its total less its children's, for tests to build trees.
    #[cfg(test)]
    pub(crate) fn with_children(
        name: &'static str,
        hits: u64,
        total: Duration,
        children: Vec<ScopeReport>,
    ) -> ScopeReport {
        let children_total: Duration = children.iter().map(|child| child.total).sum();
        ScopeReport {
            name,
            hits,
            total,
            self_total: total.saturating_sub(children_total),
            children,
        }
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
                self_total: Duration::ZERO,
                children: Vec::new(),
            });
        }
        &mut siblings.last_mut().unwrap().children
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Profiler {
        /// The id of `path`, adding it and any missing parents.
        fn id_of(&self, path: &[&'static str]) -> usize {
            let mut paths = self.paths.borrow_mut();
            let mut id = None;
            for &name in path {
                id = Some(paths.id(id, name));
            }
            id.unwrap()
        }

        /// Record one scope of `path` ending at `tsc_end`, as its guard would.
        pub(crate) fn record_path(
            &self,
            path: &[&'static str],
            tsc_end: u64,
            ticks: u64,
            self_ticks: u64,
        ) {
            self.record(self.id_of(path), tsc_end, ticks, self_ticks);
        }

        /// Replace the totals of `path`.
        fn set_stats(&self, path: &[&'static str], hits: u64, ticks: u64, self_ticks: u64) {
            let id = self.id_of(path);
            self.paths.borrow_mut().entries[id].stats = ScopeStats {
                hits,
                ticks,
                self_ticks,
            };
        }

        /// The totals of `path`, or `None` until a scope of it has ended.
        fn stats(&self, path: &[&'static str]) -> Option<ScopeStats> {
            let paths = self.paths.borrow();
            let entry = paths.entries.iter().find(|entry| entry.path == path)?;
            (entry.stats.hits > 0).then_some(entry.stats)
        }

        /// How many paths have had a scope end.
        fn recorded(&self) -> usize {
            let paths = self.paths.borrow();
            paths
                .entries
                .iter()
                .filter(|entry| entry.stats.hits > 0)
                .count()
        }
    }

    #[test]
    fn records_only_when_scope_ends() {
        let profiler = Profiler::new();
        let scope = profiler.scope("work");

        assert_eq!(profiler.recorded(), 0);
        drop(scope);

        assert_eq!(profiler.stats(&["work"]).unwrap().hits, 1);
    }

    #[test]
    fn nested_scopes_finish_independently() {
        let profiler = Profiler::new();
        profile_scope!(profiler, "outer", {
            profile_scope!(profiler, "inner", {});
            assert_eq!(profiler.stats(&["outer", "inner"]).unwrap().hits, 1);
            assert!(profiler.stats(&["outer"]).is_none());
        });

        let outer = profiler.stats(&["outer"]).unwrap();
        let inner = profiler.stats(&["outer", "inner"]).unwrap();
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
        let first_ticks = profiler.stats(&["work"]).unwrap().ticks;

        {
            let _third = profiler.scope("work");
        }

        assert_eq!(profiler.recorded(), 2);
        assert_eq!(profiler.stats(&["work"]).unwrap().hits, 2);
        assert_eq!(profiler.stats(&["work", "work"]).unwrap().hits, 1);
        assert!(profiler.stats(&["work"]).unwrap().ticks >= first_ticks);
    }

    #[test]
    fn macro_guards_live_until_the_callers_block_ends() {
        let profiler = Profiler::new();
        {
            let _profile_scope = "caller variable";
            profile_scope!(profiler, "first");
            profile_scope!(&profiler, "second",);

            assert_eq!(profiler.recorded(), 0);
            assert_eq!(_profile_scope, "caller variable");
        }

        assert_eq!(profiler.recorded(), 2);
        assert_eq!(profiler.stats(&["first"]).unwrap().hits, 1);
        assert_eq!(profiler.stats(&["first", "second"]).unwrap().hits, 1);
    }

    #[test]
    fn block_scope_returns_its_value_and_records_at_the_closing_brace() {
        let profiler = Profiler::new();
        let mut calls = 0;
        let value = profile_scope!(&profiler, "work", {
            assert_eq!(profiler.recorded(), 0);
            calls += 1;
            String::from("finished")
        },);

        assert_eq!(value, "finished");
        assert_eq!(calls, 1);
        assert_eq!(profiler.stats(&["work"]).unwrap().hits, 1);
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
        assert_eq!(profiler.stats(&["work"]).unwrap().hits, 3);
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
        assert_eq!(profiler.stats(&["iteration"]).unwrap().hits, 3);
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

        assert_eq!(profiler.recorded(), 4);
        assert_eq!(profiler.stats(&["first"]).unwrap().hits, 2);
        assert_eq!(profiler.stats(&["first", "work"]).unwrap().hits, 2);
        assert_eq!(profiler.stats(&["second"]).unwrap().hits, 1);
        assert_eq!(profiler.stats(&["second", "work"]).unwrap().hits, 1);
    }

    #[test]
    fn report_indents_children_and_does_not_add_their_time_to_the_parent() {
        let profiler = millisecond_profiler();
        profiler.set_stats(&["frame", "update"], 2, 30, 20);
        profiler.set_stats(&["frame"], 2, 100, 50);
        profiler.set_stats(&["frame", "update", "physics"], 2, 10, 10);
        profiler.set_stats(&["frame", "render"], 2, 20, 20);

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
        ScopeReport::with_children(name, hits, Duration::from_millis(milliseconds), children)
    }

    #[test]
    fn snapshot_nests_children_sorted_by_name() {
        let profiler = millisecond_profiler();
        profiler.set_stats(&["frame", "update"], 2, 30, 20);
        profiler.set_stats(&["frame"], 2, 100, 50);
        profiler.set_stats(&["frame", "update", "physics"], 2, 10, 10);
        profiler.set_stats(&["frame", "render"], 2, 20, 20);
        profiler.set_stats(&["load"], 1, 5, 5);

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
        profiler.record_path(&["frame"], 1_000, 90, 90);
        profiler.record_path(&["frame"], 11_000, 4, 4);
        profiler.record_path(&["frame"], 12_000, 6, 6);

        let recent = profiler.snapshot_at(12_000);

        assert_eq!(recent[0].average(), Some(Duration::from_millis(5)));
    }

    /// A profiler whose clock ticks once per millisecond, for recording at chosen times.
    pub(crate) fn millisecond_profiler() -> Profiler {
        Profiler {
            tsc_freq: 1000.0,
            window: Duration::from_secs(10),
            paths: RefCell::new(Paths::default()),
            active_scopes: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn recent_snapshot_counts_only_scopes_that_ended_within_the_window() {
        let profiler = millisecond_profiler();
        profiler.record_path(&["frame"], 1_000, 5, 5);
        profiler.record_path(&["frame"], 12_000, 7, 7);

        assert_eq!(
            profiler.snapshot_at(12_000),
            vec![report("frame", 1, 7, vec![])]
        );
        assert_eq!(profiler.snapshot(), vec![report("frame", 2, 12, vec![])]);
    }

    #[test]
    fn recent_snapshot_sums_every_hit_within_the_window() {
        let profiler = millisecond_profiler();
        profiler.record_path(&["frame"], 3_000, 4, 4);
        profiler.record_path(&["frame"], 3_050, 6, 6);
        profiler.record_path(&["frame"], 9_000, 10, 10);

        assert_eq!(
            profiler.snapshot_at(10_000),
            vec![report("frame", 3, 20, vec![])]
        );
    }

    #[test]
    fn recent_snapshot_self_time_excludes_a_child_that_ended_before_the_window() {
        let profiler = millisecond_profiler();
        // At 12.0 s the window starts at 2.1 s: the child ends just before, its parent after.
        profiler.record_path(&["frame", "render"], 2_090, 10, 10);
        profiler.record_path(&["frame"], 2_110, 30, 20);

        let recent = profiler.snapshot_at(12_000);

        assert_eq!(recent[0].self_time(), Duration::from_millis(20));
    }

    #[test]
    fn recent_snapshot_omits_scopes_with_no_hits_in_the_window() {
        let profiler = millisecond_profiler();
        profiler.record_path(&["frame", "update"], 1_000, 2, 2);
        profiler.record_path(&["frame"], 1_000, 5, 5);
        profiler.record_path(&["load"], 12_000, 3, 3);

        assert_eq!(
            profiler.snapshot_at(12_000),
            vec![report("load", 1, 3, vec![])]
        );
    }

    #[test]
    fn time_buckets_older_than_the_window_are_dropped() {
        let profiler = millisecond_profiler();
        for millisecond in (0..60_000).step_by(10) {
            profiler.record_path(&["frame"], millisecond, 1, 1);
        }

        let id = profiler.id_of(&["frame"]);
        let buckets = profiler.paths.borrow().entries[id].recent.len();
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
        profiler.record_path(&["load"], 5_050, 7, 7);
        profiler.record_path(&["frame", "render"], 9_940, 10, 10);
        profiler.record_path(&["frame"], 9_950, 30, 20);

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
        profiler.record_path(&["old"], 1_000, 5, 5);
        profiler.record_path(&["frame", "update"], 11_950, 10, 10);
        profiler.record_path(&["frame"], 11_950, 10, 0);

        let recent = profiler.series_at(12_000);

        assert_eq!(
            recent.scopes,
            vec![series(vec!["frame", "update"], &[(98, 10)])]
        );
    }

    #[test]
    fn recent_series_self_time_matches_the_tree_when_a_child_ends_in_an_earlier_bucket() {
        // One tick per bucket, so the child and its parent always end in different buckets.
        let profiler = Profiler {
            tsc_freq: 10.0,
            window: Duration::from_secs(100_000),
            ..millisecond_profiler()
        };
        profile_scope!(profiler, "frame", {
            profile_scope!(profiler, "render", {});
        });

        let tsc_now = rdtsc();
        let tree_self = profiler.snapshot_at(tsc_now)[0].self_time();
        let series = profiler.series_at(tsc_now);
        let frame = series.scopes.iter().find(|scope| scope.path == ["frame"]);
        let series_self: Duration =
            frame.map_or(Duration::ZERO, |frame| frame.self_times.iter().sum());

        let difference = series_self.abs_diff(tree_self);
        assert!(
            difference < Duration::from_millis(1),
            "series {series_self:?}, tree {tree_self:?}"
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
        assert_eq!(profiler.stats(&["outer"]).unwrap().hits, 1);
        assert_eq!(profiler.stats(&["outer", "inner"]).unwrap().hits, 1);
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
        assert_eq!(profiler.stats(&["after"]).unwrap().hits, 1);
    }
}
