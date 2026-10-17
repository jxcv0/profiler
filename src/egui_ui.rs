use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::{Profiler, ScopeReport, ScopeSeries, TimeSeries};

/// Bar colour for the time a scope spends in itself.
pub const SELF_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 120, 60);
/// Bar colour for the time a scope spends in its child scopes.
pub const CHILDREN_COLOR: egui::Color32 = egui::Color32::from_rgb(110, 140, 190);

/// Height of the chart drawn by [`show_timeline`], axis labels aside.
const TIMELINE_HEIGHT: f32 = 120.0;
/// Room left of the timeline chart for its percentage labels.
const AXIS_WIDTH: f32 = 36.0;
/// Number columns right of each tree row: heading and width.
const COLUMNS: [(&str, f32); 5] = [
    ("%", 56.0),
    ("self", 72.0),
    ("total", 72.0),
    ("hits", 64.0),
    ("avg", 72.0),
];
/// Space between a row's name, bar and number columns.
const GAP: f32 = 8.0;
/// A sibling only moves above a slower-sorted one once it is this many times slower,
/// so rows hold still while live timings jitter.
const REORDER_MARGIN: f64 = 1.1;

type Path = Vec<&'static str>;

/// What a scope's percentage and bar are a share of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShareOf {
    /// All root scope time together.
    #[default]
    Root,
    /// The scope's parent, or all root time for a root scope.
    Parent,
}

/// A profiler view that keeps its settings between frames: pause, a name filter,
/// a threshold for small scopes, percentages of root or parent, and scopes hidden
/// from the timeline by clicking their legend entries. Keep one alive across frames.
///
/// ```
/// # let ctx = egui::Context::default();
/// let profiler = profiler::Profiler::new();
/// let mut view = profiler::ProfilerUi::default();
/// # let mut output = ctx.run_ui(Default::default(), |ui| {
/// view.show(ui, &profiler);
/// # });
/// # output.textures_delta.clear();
/// ```
#[derive(Default)]
pub struct ProfilerUi {
    /// What percentages and bars in the tree are a share of.
    pub share_of: ShareOf,
    /// Show only scopes whose name contains this, ignoring case, with their ancestors
    /// and descendants. Empty shows every scope.
    pub filter: String,
    /// Leave out tree scopes under this percentage of all root time.
    pub min_percent: f32,
    /// Scope paths left out of the timeline.
    pub hidden: HashSet<Path>,
    paused: Option<(TimeSeries, Vec<ScopeReport>)>,
}

impl ProfilerUi {
    /// Draw controls, then the profiler's recent timeline and scope tree, scrolling the tree.
    pub fn show(&mut self, ui: &mut egui::Ui, profiler: &Profiler) {
        let mut paused = self.is_paused();
        let (series, scopes) = self
            .paused
            .take()
            .unwrap_or_else(|| (profiler.recent_series(), profiler.recent_snapshot()));
        self.show_controls(ui, &mut paused);
        self.show_timeline(ui, &series);
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| self.show_scopes(ui, &scopes));
        if paused {
            self.paused = Some((series, scopes));
        }
    }

    /// Whether [`ProfilerUi::show`] is drawing timings frozen by [`ProfilerUi::pause`].
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    /// Freeze the profiler's current timings, so they can be read while work goes on.
    pub fn pause(&mut self, profiler: &Profiler) {
        self.paused = Some((profiler.recent_series(), profiler.recent_snapshot()));
    }

    /// Go back to drawing live timings.
    pub fn resume(&mut self) {
        self.paused = None;
    }

    fn show_controls(&mut self, ui: &mut egui::Ui, paused: &mut bool) {
        ui.horizontal_wrapped(|ui| {
            let label = if *paused { "▶ Resume" } else { "⏸ Pause" };
            if ui.button(label).clicked() {
                *paused = !*paused;
            }
            ui.separator();
            ui.label("Filter");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("scope name")
                    .desired_width(120.0),
            );
            ui.label("Hide under");
            ui.add(
                egui::DragValue::new(&mut self.min_percent)
                    .range(0.0..=100.0)
                    .speed(0.1)
                    .suffix("%"),
            );
            ui.separator();
            ui.label("% of");
            ui.selectable_value(&mut self.share_of, ShareOf::Root, "all")
                .on_hover_text("Percentages and bars are shares of all root time");
            ui.selectable_value(&mut self.share_of, ShareOf::Parent, "parent")
                .on_hover_text("Percentages and bars are shares of the parent scope");
        });
    }

    /// Draw a scope tree such as [`Profiler::snapshot`], slowest sibling first, with
    /// these settings. See [`show_scopes`].
    pub fn show_scopes(&self, ui: &mut egui::Ui, scopes: &[ScopeReport]) {
        let root_total = scopes
            .iter()
            .map(|scope| scope.total)
            .sum::<Duration>()
            .as_secs_f64();
        let tree = Tree {
            view: self,
            root_total,
            bar_width: (ui.available_width() * 0.2).clamp(60.0, 240.0),
            filter: self.filter.to_lowercase(),
            hovered: hovered_path(ui.ctx()),
        };
        show_key(ui);
        tree.header(ui);
        tree.show_level(ui, scopes, &mut Vec::new(), root_total, false);
    }

    /// Draw a series such as [`Profiler::recent_series`] as stacked columns with these
    /// settings. Clicking a legend entry hides or shows that scope. See [`show_timeline`].
    pub fn show_timeline(&mut self, ui: &mut egui::Ui, series: &TimeSeries) {
        self.timeline(ui, series, true);
    }

    /// Draw the timeline; `toggles` lets legend clicks hide scopes, which only lasts
    /// in a kept view.
    fn timeline(&mut self, ui: &mut egui::Ui, series: &TimeSeries, toggles: bool) {
        ui.weak(format!(
            "Self time per {} bucket; a full column is a fully busy bucket.",
            format_duration(series.bucket)
        ));
        let hovered = hovered_path(ui.ctx());
        let small = egui::TextStyle::Small.resolve(ui.style());
        let text_height = ui.text_style_height(&egui::TextStyle::Small);
        let size = egui::vec2(
            ui.available_width(),
            TIMELINE_HEIGHT + text_height * 1.5 + 2.0,
        );
        let (outer, response) = ui.allocate_exact_size(size, egui::Sense::hover());
        let chart = egui::Rect::from_min_max(
            egui::pos2(outer.left() + AXIS_WIDTH, outer.top() + text_height / 2.0),
            egui::pos2(outer.right(), outer.bottom() - text_height - 2.0),
        );
        let painter = ui.painter();
        painter.rect_filled(chart, 0.0, ui.visuals().faint_bg_color);

        let shown: Vec<&ScopeSeries> = series
            .scopes
            .iter()
            .filter(|scope| !self.hidden.contains(&scope.path))
            .collect();
        let buckets = series
            .scopes
            .first()
            .map_or(0, |scope| scope.self_times.len());
        let column_width = chart.width() / buckets.max(1) as f32;
        let bucket = series.bucket.as_secs_f64();
        let colors: Vec<egui::Color32> = shown
            .iter()
            .map(|scope| {
                let color = scope_color(ui.ctx(), &scope.path);
                if hovered.as_ref().is_some_and(|path| *path != scope.path) {
                    color.gamma_multiply(0.3)
                } else {
                    color
                }
            })
            .collect();
        let pointer = response.hover_pos();
        let mut pointed_at = None;
        for position in 0..buckets {
            let left = chart.left() + column_width * position as f32;
            let x_range = left..=left + column_width;
            let mut bottom = chart.bottom();
            let mut busy = Duration::ZERO;
            for (scope, &color) in shown.iter().zip(&colors) {
                let time = scope.self_times[position];
                if time.is_zero() {
                    continue;
                }
                busy += time;
                let height = (time.as_secs_f64() / bucket) as f32 * chart.height();
                let top = (bottom - height).max(chart.top());
                let column = egui::Rect::from_x_y_ranges(x_range.clone(), top..=bottom);
                painter.rect_filled(column, 0.0, color);
                if pointer.is_some_and(|pointer| column.contains(pointer)) {
                    pointed_at = Some(&scope.path);
                }
                bottom = top;
            }
            if busy > series.bucket {
                let marker = egui::Rect::from_x_y_ranges(x_range, chart.top()..=chart.top() + 3.0);
                painter.rect_filled(marker, 0.0, ui.visuals().error_fg_color);
            }
        }

        let grid = ui.visuals().widgets.noninteractive.bg_stroke;
        let weak = ui.visuals().weak_text_color();
        for (fraction, label) in [(1.0, "100%"), (0.5, "50%"), (0.0, "0%")] {
            let y = chart.bottom() - chart.height() * fraction;
            if fraction > 0.0 {
                painter.line_segment(
                    [egui::pos2(chart.left(), y), egui::pos2(chart.right(), y)],
                    grid,
                );
            }
            let at = egui::pos2(chart.left() - 4.0, y);
            painter.text(at, egui::Align2::RIGHT_CENTER, label, small.clone(), weak);
        }
        let window = series.bucket * buckets as u32;
        let y = chart.bottom() + 2.0;
        let ticks = [
            (
                chart.left(),
                egui::Align2::LEFT_TOP,
                format!("-{}", format_duration(window)),
            ),
            (
                chart.center().x,
                egui::Align2::CENTER_TOP,
                format!("-{}", format_duration(window / 2)),
            ),
            (chart.right(), egui::Align2::RIGHT_TOP, "now".to_owned()),
        ];
        for (x, anchor, label) in ticks {
            painter.text(egui::pos2(x, y), anchor, label, small.clone(), weak);
        }

        if let Some(path) = pointed_at {
            set_hovered(ui.ctx(), path);
        }
        if let Some(pointer) = pointer.filter(|pointer| chart.contains(*pointer) && buckets > 0) {
            let position = (((pointer.x - chart.left()) / column_width) as usize).min(buckets - 1);
            response.on_hover_ui_at_pointer(|ui| {
                show_bucket_tooltip(ui, series, &shown, buckets, position);
            });
        }

        ui.horizontal_wrapped(|ui| {
            for scope in &series.scopes {
                self.show_legend_entry(ui, scope, hovered.as_ref(), toggles);
            }
        });
    }

    fn show_legend_entry(
        &mut self,
        ui: &mut egui::Ui,
        scope: &ScopeSeries,
        hovered: Option<&Path>,
        toggles: bool,
    ) {
        let hidden = self.hidden.contains(&scope.path);
        show_swatch(ui, scope_color(ui.ctx(), &scope.path), !hidden);
        let mut text = egui::RichText::new(scope.path.join("/"));
        if hidden {
            text = text.weak().strikethrough();
        } else if hovered == Some(&scope.path) {
            text = text.strong();
        }
        let label = ui.add(egui::Label::new(text).sense(if toggles {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        }));
        let label = if toggles {
            label.on_hover_text(if hidden {
                "Click to show in the chart"
            } else {
                "Click to hide from the chart"
            })
        } else {
            label
        };
        if label.hovered() {
            set_hovered(ui.ctx(), &scope.path);
        }
        if label.clicked() && !self.hidden.remove(&scope.path) {
            self.hidden.insert(scope.path.clone());
        }
    }
}

/// Draw a [`crate::Profiler::snapshot`] as a tree, slowest sibling first.
/// Each scope gets a bar sized by its share of all root time: self time, then child time,
/// and right-aligned columns of its share, self and total time, hits and average.
/// Hover a row for details. Use a kept [`ProfilerUi`] to filter or change settings.
///
/// ```
/// # let ctx = egui::Context::default();
/// # let mut output = ctx.run_ui(Default::default(), |ui| {
/// let profiler = profiler::Profiler::new();
/// profiler::show_scopes(ui, &profiler.snapshot());
/// # });
/// # output.textures_delta.clear();
/// ```
pub fn show_scopes(ui: &mut egui::Ui, scopes: &[ScopeReport]) {
    ProfilerUi::default().show_scopes(ui, scopes);
}

/// Draw a [`crate::Profiler::recent_series`] as stacked columns, oldest bucket on the left,
/// with axes and a legend below. A full-height column means the whole bucket was spent in
/// scopes. Hover a column for its timings. Use a kept [`ProfilerUi`] for clicking legend
/// entries to hide scopes.
///
/// ```
/// # let ctx = egui::Context::default();
/// # let mut output = ctx.run_ui(Default::default(), |ui| {
/// let profiler = profiler::Profiler::new();
/// profiler::show_timeline(ui, &profiler.recent_series());
/// # });
/// # output.textures_delta.clear();
/// ```
pub fn show_timeline(ui: &mut egui::Ui, series: &TimeSeries) {
    ProfilerUi::default().timeline(ui, series, false);
}

/// The colour the widgets draw a scope path in. Paths get evenly spread hues in the order
/// `ctx` first sees them, and keep them while it lives.
pub fn scope_color(ctx: &egui::Context, path: &[&'static str]) -> egui::Color32 {
    let id = egui::Id::unique("profiler scope colors");
    let index = ctx.data_mut(|data| {
        let indices = data.get_temp_mut_or_default::<HashMap<Path, usize>>(id);
        if let Some(&index) = indices.get(path) {
            return index;
        }
        let next = indices.len();
        indices.insert(path.to_vec(), next);
        next
    });
    // Golden-ratio steps keep each new hue far from all earlier ones.
    let hue = (0.08 + index as f32 * 0.618_034).fract();
    egui::ecolor::Hsva::new(hue, 0.6, 0.85, 1.0).into()
}

/// A duration in the unit that fits it, to three or four significant figures.
fn format_duration(time: Duration) -> String {
    let seconds = time.as_secs_f64();
    let (value, unit) = match seconds {
        0.0 => return "0".to_owned(),
        s if s >= 1.0 => (s, "s"),
        s if s >= 1e-3 => (s * 1e3, "ms"),
        s if s >= 1e-6 => (s * 1e6, "µs"),
        s => (s * 1e9, "ns"),
    };
    let decimals = match value {
        v if v >= 100.0 => 0,
        v if v >= 10.0 => 1,
        _ => 2,
    };
    format!("{value:.decimals$} {unit}")
}

fn format_percent(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}

/// `time` as a share of `whole` seconds, or zero when there is no whole.
fn share(time: Duration, whole: f64) -> f64 {
    if whole > 0.0 {
        time.as_secs_f64() / whole
    } else {
        0.0
    }
}

/// The path hovered in either view this pass or the last, to highlight in both.
#[derive(Clone)]
struct Hovered {
    pass: u64,
    path: Path,
}

fn hovered_id() -> egui::Id {
    egui::Id::unique("profiler hovered scope")
}

fn hovered_path(ctx: &egui::Context) -> Option<Path> {
    let pass = ctx.cumulative_pass_nr();
    let hovered = ctx.data(|data| data.get_temp::<Hovered>(hovered_id()))?;
    if hovered.pass + 1 == pass {
        // Draw once more so the highlight goes when the pointer leaves.
        ctx.request_repaint();
    }
    (hovered.pass + 1 >= pass).then_some(hovered.path)
}

fn set_hovered(ctx: &egui::Context, path: &[&'static str]) {
    let hovered = Hovered {
        pass: ctx.cumulative_pass_nr(),
        path: path.to_vec(),
    };
    ctx.data_mut(|data| data.insert_temp(hovered_id(), hovered));
}

/// `scopes` slowest first, but keeping last frame's order (stored under `id`) until a
/// scope is [`REORDER_MARGIN`] times slower than the one above it.
fn stable_order<'a>(
    ui: &egui::Ui,
    id: egui::Id,
    scopes: &'a [ScopeReport],
) -> Vec<&'a ScopeReport> {
    let previous: Option<Vec<&'static str>> = ui.data(|data| data.get_temp(id));
    let order = match previous {
        None => {
            let mut order: Vec<&ScopeReport> = scopes.iter().collect();
            order.sort_by_key(|scope| std::cmp::Reverse(scope.total));
            order
        }
        Some(previous) => {
            let rank: HashMap<&str, usize> = previous
                .iter()
                .enumerate()
                .map(|(rank, &name)| (name, rank))
                .collect();
            let mut order: Vec<&ScopeReport> = scopes.iter().collect();
            order.sort_by_key(|scope| rank.get(scope.name).copied().unwrap_or(usize::MAX));
            for index in 1..order.len() {
                let mut at = index;
                while at > 0
                    && order[at].total.as_secs_f64()
                        > order[at - 1].total.as_secs_f64() * REORDER_MARGIN
                {
                    order.swap(at, at - 1);
                    at -= 1;
                }
            }
            order
        }
    };
    let names: Vec<&'static str> = order.iter().map(|scope| scope.name).collect();
    ui.data_mut(|data| data.insert_temp(id, names));
    order
}

/// A text-height dot of `color`, filled or as an outline.
fn show_swatch(ui: &mut egui::Ui, color: egui::Color32, filled: bool) {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let (swatch, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if filled {
        ui.painter()
            .circle_filled(swatch.center(), size / 3.0, color);
    } else {
        let stroke = egui::Stroke::new(1.0, color);
        ui.painter()
            .circle_stroke(swatch.center(), size / 3.0, stroke);
    }
}

/// A swatch-and-text key for the tree's bar colours.
fn show_key(ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        for (color, text) in [
            (SELF_COLOR, "self time"),
            (CHILDREN_COLOR, "time in children"),
        ] {
            show_swatch(ui, color, true);
            ui.weak(text);
        }
        ui.weak("· hover a row for details");
    });
}

fn show_bucket_tooltip(
    ui: &mut egui::Ui,
    series: &TimeSeries,
    shown: &[&ScopeSeries],
    buckets: usize,
    position: usize,
) {
    let age = series.bucket * (buckets - 1 - position) as u32;
    if age.is_zero() {
        ui.strong("Latest bucket");
    } else {
        ui.strong(format!("{} ago", format_duration(age)));
    }
    let mut in_bucket: Vec<(&ScopeSeries, Duration)> = shown
        .iter()
        .map(|scope| (*scope, scope.self_times[position]))
        .filter(|(_, time)| !time.is_zero())
        .collect();
    in_bucket.sort_by_key(|(_, time)| std::cmp::Reverse(*time));
    let bucket = series.bucket.as_secs_f64();
    for (scope, time) in &in_bucket {
        ui.horizontal(|ui| {
            show_swatch(ui, scope_color(ui.ctx(), &scope.path), true);
            ui.label(format!(
                "{}: {} ({})",
                scope.path.join("/"),
                format_duration(*time),
                format_percent(share(*time, bucket)),
            ));
        });
    }
    let busy: Duration = in_bucket.iter().map(|(_, time)| *time).sum();
    ui.label(format!(
        "busy {} of {}",
        format_percent(share(busy, bucket)),
        format_duration(series.bucket)
    ));
    if busy > series.bucket {
        ui.colored_label(
            ui.visuals().error_fg_color,
            "More self time than the bucket lasts, so the column is cut off",
        );
    }
}

/// One frame's settings for drawing a scope tree.
struct Tree<'a> {
    view: &'a ProfilerUi,
    root_total: f64,
    bar_width: f32,
    /// The filter in lower case.
    filter: String,
    hovered: Option<Path>,
}

impl Tree<'_> {
    fn name_matches(&self, scope: &ScopeReport) -> bool {
        self.filter.is_empty() || scope.name.to_lowercase().contains(&self.filter)
    }

    /// Whether to draw `scope`: big enough, and matching the filter itself, through an
    /// ancestor, or through a descendant.
    fn shows(&self, scope: &ScopeReport, ancestor_matched: bool) -> bool {
        let percent = share(scope.total, self.root_total) * 100.0;
        if self.root_total > 0.0 && percent < f64::from(self.view.min_percent) {
            return false;
        }
        ancestor_matched
            || self.name_matches(scope)
            || scope.children.iter().any(|child| self.shows(child, false))
    }

    fn show_level(
        &self,
        ui: &mut egui::Ui,
        scopes: &[ScopeReport],
        path: &mut Path,
        parent_total: f64,
        ancestor_matched: bool,
    ) {
        for scope in stable_order(ui, ui.scope_id().with("profiler order"), scopes) {
            if !self.shows(scope, ancestor_matched) {
                continue;
            }
            let matched = ancestor_matched || (!self.filter.is_empty() && self.name_matches(scope));
            path.push(scope.name);
            let has_children = scope
                .children
                .iter()
                .any(|child| self.shows(child, matched));
            if has_children {
                let id = ui.make_persistent_id(scope.name);
                egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    id,
                    true,
                )
                .show_header(ui, |ui| self.row(ui, scope, path, parent_total))
                .body(|ui| {
                    let total = scope.total.as_secs_f64();
                    self.show_level(ui, &scope.children, path, total, matched)
                });
            } else {
                ui.horizontal(|ui| {
                    // Stand in for the collapse button, so leaves line up with their siblings.
                    let spacing = ui.spacing().item_spacing;
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(ui.spacing().indent);
                    ui.spacing_mut().item_spacing = spacing;
                    self.row(ui, scope, path, parent_total);
                });
            }
            path.pop();
        }
    }

    /// The percentage's whole: all root time, or the parent's total.
    fn whole(&self, parent_total: f64) -> f64 {
        match self.view.share_of {
            ShareOf::Root => self.root_total,
            ShareOf::Parent => parent_total,
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().indent);
            let height = ui.text_style_height(&egui::TextStyle::Body);
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), height),
                egui::Sense::hover(),
            );
            let cells = COLUMNS.map(|(heading, _)| heading.to_owned());
            let bar = match self.view.share_of {
                ShareOf::Root => "share of all",
                ShareOf::Parent => "share of parent",
            };
            self.paint_cells(
                ui,
                rect,
                "scope",
                Some(bar),
                &cells,
                ui.visuals().weak_text_color(),
            );
        });
    }

    fn row(&self, ui: &mut egui::Ui, scope: &ScopeReport, path: &Path, parent_total: f64) {
        let height = ui.text_style_height(&egui::TextStyle::Body);
        let size = egui::vec2(ui.available_width(), height);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
        let whole = self.whole(parent_total);
        let highlighted = response.hovered() || self.hovered.as_ref() == Some(path);
        if highlighted {
            let fill = ui.visuals().selection.bg_fill.gamma_multiply(0.4);
            ui.painter().rect_filled(rect.expand(1.0), 2.0, fill);
        }

        let swatch = egui::pos2(rect.left() + height / 2.0, rect.center().y);
        ui.painter()
            .circle_filled(swatch, height / 3.0, scope_color(ui.ctx(), path));
        let cells = [
            format_percent(share(scope.total, whole)),
            format_duration(scope.self_time()),
            format_duration(scope.total),
            scope.hits.to_string(),
            scope.average().map_or("–".to_owned(), format_duration),
        ];
        let text_color = if highlighted {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().text_color()
        };
        let bar = self.paint_cells(ui, rect, scope.name, None, &cells, text_color);

        let painter = ui.painter();
        painter.rect_filled(bar, 0.0, ui.visuals().faint_bg_color);
        let self_end = bar.left() + bar.width() * (share(scope.self_time(), whole) as f32).min(1.0);
        let total_end = bar.left() + bar.width() * (share(scope.total, whole) as f32).min(1.0);
        if self_end > bar.left() {
            let self_bar = egui::Rect::from_x_y_ranges(bar.left()..=self_end, bar.y_range());
            painter.rect_filled(self_bar, 0.0, SELF_COLOR);
        }
        if total_end > self_end {
            let children_bar = egui::Rect::from_x_y_ranges(self_end..=total_end, bar.y_range());
            painter.rect_filled(children_bar, 0.0, CHILDREN_COLOR);
        }

        if response.hovered() {
            set_hovered(ui.ctx(), path);
        }
        response.on_hover_ui(|ui| {
            ui.strong(path.join(" / "));
            let children = scope.total.saturating_sub(scope.self_time());
            let rows = [
                ("total", format_duration(scope.total)),
                ("self", format_duration(scope.self_time())),
                ("in children", format_duration(children)),
                (
                    "% of all",
                    format_percent(share(scope.total, self.root_total)),
                ),
                (
                    "% of parent",
                    format_percent(share(scope.total, parent_total)),
                ),
                ("hits", scope.hits.to_string()),
                (
                    "average per hit",
                    scope
                        .average()
                        .map_or("no finished hits".to_owned(), format_duration),
                ),
            ];
            egui::Grid::new("profiler row tooltip")
                .num_columns(2)
                .show(ui, |ui| {
                    for (name, value) in rows {
                        ui.weak(name);
                        ui.label(value);
                        ui.end_row();
                    }
                });
        });
    }

    /// Paint a row's name and right-aligned number columns, and return where its bar goes.
    /// A heading row passes `bar_heading` to label the bar column instead.
    fn paint_cells(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        name: &str,
        bar_heading: Option<&str>,
        cells: &[String; 5],
        color: egui::Color32,
    ) -> egui::Rect {
        let font = egui::TextStyle::Body.resolve(ui.style());
        let columns_width: f32 = COLUMNS.iter().map(|(_, width)| width).sum();
        let bar_right = rect.right() - columns_width - GAP;
        let bar = egui::Rect::from_x_y_ranges(
            bar_right - self.bar_width..=bar_right,
            rect.shrink2(egui::vec2(0.0, 2.0)).y_range(),
        );
        let name_left = rect.left() + rect.height() + 2.0;
        let name_rect = egui::Rect::from_x_y_ranges(name_left..=bar.left() - GAP, rect.y_range());
        ui.painter_at(name_rect).text(
            egui::pos2(name_left, rect.center().y),
            egui::Align2::LEFT_CENTER,
            name,
            font.clone(),
            color,
        );
        if let Some(heading) = bar_heading {
            let at = egui::pos2(bar.left(), rect.center().y);
            ui.painter()
                .text(at, egui::Align2::LEFT_CENTER, heading, font.clone(), color);
        }
        let mut right = bar_right + GAP;
        for (text, (_, width)) in cells.iter().zip(COLUMNS) {
            right += width;
            let at = egui::pos2(right, rect.center().y);
            ui.painter()
                .text(at, egui::Align2::RIGHT_CENTER, text, font.clone(), color);
        }
        bar
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shapes(ctx: &egui::Context, draw: impl FnMut(&mut egui::Ui)) -> Vec<egui::Shape> {
        let mut output = ctx.run_ui(egui::RawInput::default(), draw);
        output.textures_delta.clear();
        output
            .shapes
            .into_iter()
            .map(|clipped| clipped.shape)
            .collect()
    }

    fn flatten(shapes: &[egui::Shape], found: &mut Vec<egui::Shape>) {
        for shape in shapes {
            match shape {
                egui::Shape::Vec(inner) => flatten(inner, found),
                shape => found.push(shape.clone()),
            }
        }
    }

    fn flat(shapes: &[egui::Shape]) -> Vec<egui::Shape> {
        let mut found = Vec::new();
        flatten(shapes, &mut found);
        found
    }

    fn texts(shapes: &[egui::Shape]) -> Vec<String> {
        flat(shapes)
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect()
    }

    fn rects(shapes: &[egui::Shape], fill: egui::Color32) -> Vec<egui::Rect> {
        flat(shapes)
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Rect(rect) if rect.fill == fill => Some(rect.rect),
                _ => None,
            })
            .collect()
    }

    fn circles(shapes: &[egui::Shape], fill: egui::Color32) -> Vec<egui::Pos2> {
        flat(shapes)
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Circle(circle) if circle.fill == fill => Some(circle.center),
                _ => None,
            })
            .collect()
    }

    fn drawn_text(scopes: &[ScopeReport]) -> Vec<String> {
        texts(&shapes(&egui::Context::default(), |ui| {
            show_scopes(ui, scopes)
        }))
    }

    fn drawn_bars(scopes: &[ScopeReport], fill: egui::Color32) -> Vec<f32> {
        let drawn = shapes(&egui::Context::default(), |ui| show_scopes(ui, scopes));
        rects(&drawn, fill).iter().map(|bar| bar.width()).collect()
    }

    /// The name and number cells of the row named `name`.
    fn row<'a>(drawn: &'a [String], name: &str) -> &'a [String] {
        let at = drawn.iter().position(|text| text == name).unwrap();
        &drawn[at..at + 1 + COLUMNS.len()]
    }

    fn report(
        name: &'static str,
        hits: u64,
        milliseconds: u64,
        children: Vec<ScopeReport>,
    ) -> ScopeReport {
        ScopeReport::with_children(name, hits, Duration::from_millis(milliseconds), children)
    }

    fn in_third_bucket(milliseconds: u64) -> Vec<Duration> {
        let mut self_times = vec![Duration::ZERO; 4];
        self_times[2] = Duration::from_millis(milliseconds);
        self_times
    }

    fn frame_and_render(frame: u64, render: u64) -> TimeSeries {
        TimeSeries {
            bucket: Duration::from_millis(100),
            scopes: vec![
                ScopeSeries {
                    path: vec!["frame"],
                    self_times: in_third_bucket(frame),
                },
                ScopeSeries {
                    path: vec!["frame", "render"],
                    self_times: in_third_bucket(render),
                },
            ],
        }
    }

    #[test]
    fn timeline_stacks_each_scopes_self_time_in_its_bucket_column() {
        let series = frame_and_render(50, 25);
        let ctx = egui::Context::default();

        let drawn = shapes(&ctx, |ui| show_timeline(ui, &series));

        let frame_color = scope_color(&ctx, &["frame"]);
        let render_color = scope_color(&ctx, &["frame", "render"]);
        let (frame, render) = (rects(&drawn, frame_color), rects(&drawn, render_color));
        assert_ne!(frame_color, render_color);
        assert_eq!((frame.len(), render.len()), (1, 1), "{frame:?} {render:?}");
        let (frame, render) = (frame[0], render[0]);
        assert!(
            (frame.height() / render.height() - 2.0).abs() < 0.01,
            "{frame:?} {render:?}"
        );
        assert!(
            (frame.top() - render.bottom()).abs() < 0.01,
            "{frame:?} {render:?}"
        );
        assert_eq!(frame.x_range(), render.x_range());
    }

    #[test]
    fn timeline_labels_its_axes() {
        let drawn = texts(&shapes(&egui::Context::default(), |ui| {
            show_timeline(ui, &frame_and_render(50, 25))
        }));

        for label in ["100%", "50%", "0%", "-400 ms", "-200 ms", "now"] {
            assert!(drawn.contains(&label.to_owned()), "{label} {drawn:?}");
        }
    }

    #[test]
    fn timeline_leaves_out_hidden_scopes() {
        let mut view = ProfilerUi::default();
        view.hidden.insert(vec!["frame", "render"]);
        let ctx = egui::Context::default();

        let drawn = shapes(&ctx, |ui| view.show_timeline(ui, &frame_and_render(50, 25)));

        assert_eq!(rects(&drawn, scope_color(&ctx, &["frame"])).len(), 1);
        assert!(rects(&drawn, scope_color(&ctx, &["frame", "render"])).is_empty());
    }

    #[test]
    fn timeline_marks_a_bucket_with_more_self_time_than_it_lasts() {
        let ctx = egui::Context::default();
        let error = ctx.global_style().visuals.error_fg_color;

        let busy = shapes(&ctx, |ui| show_timeline(ui, &frame_and_render(80, 40)));
        let fine = shapes(&ctx, |ui| show_timeline(ui, &frame_and_render(50, 25)));

        assert_eq!(rects(&busy, error).len(), 1);
        assert!(rects(&fine, error).is_empty());
    }

    #[test]
    fn timeline_dims_scopes_other_than_the_hovered_one() {
        let ctx = egui::Context::default();
        let series = frame_and_render(50, 25);

        let drawn = shapes(&ctx, |ui| {
            set_hovered(ui.ctx(), &["frame", "render"]);
            show_timeline(ui, &series);
        });

        let frame_color = scope_color(&ctx, &["frame"]);
        let render_color = scope_color(&ctx, &["frame", "render"]);
        assert!(rects(&drawn, frame_color).is_empty());
        assert_eq!(rects(&drawn, frame_color.gamma_multiply(0.3)).len(), 1);
        assert_eq!(rects(&drawn, render_color).len(), 1);
    }

    #[test]
    fn scopes_keep_their_colour_and_get_distinct_ones() {
        let ctx = egui::Context::default();

        let first = scope_color(&ctx, &["a"]);
        let second = scope_color(&ctx, &["b"]);

        assert_ne!(first, second);
        assert_eq!(scope_color(&ctx, &["a"]), first);
    }

    #[test]
    fn labels_each_scope_with_its_share_self_and_total_time_hits_and_average() {
        let scopes = [report(
            "frame",
            2,
            100,
            vec![report("render", 2, 20, vec![])],
        )];

        let drawn = drawn_text(&scopes);

        assert_eq!(
            row(&drawn, "frame"),
            ["frame", "100.0%", "80.0 ms", "100 ms", "2", "50.0 ms"]
        );
        assert_eq!(
            row(&drawn, "render"),
            ["render", "20.0%", "20.0 ms", "20.0 ms", "2", "10.0 ms"]
        );
    }

    #[test]
    fn an_unfinished_scope_has_no_average() {
        let scopes = [report("frame", 0, 0, vec![report("update", 1, 30, vec![])])];

        let drawn = drawn_text(&scopes);

        assert_eq!(row(&drawn, "frame"), ["frame", "0.0%", "0", "0", "0", "–"]);
    }

    #[test]
    fn durations_are_shown_in_a_unit_that_fits() {
        assert_eq!(format_duration(Duration::from_nanos(500)), "500 ns");
        assert_eq!(format_duration(Duration::from_nanos(12_340)), "12.3 µs");
        assert_eq!(format_duration(Duration::from_micros(1_234)), "1.23 ms");
        assert_eq!(format_duration(Duration::from_millis(12_500)), "12.5 s");
    }

    #[test]
    fn siblings_are_drawn_slowest_first() {
        let scopes = [report("fast", 1, 25, vec![]), report("slow", 1, 75, vec![])];

        let drawn = drawn_text(&scopes);

        let position = |name: &str| drawn.iter().position(|text| text == name).unwrap();
        assert!(position("slow") < position("fast"), "{drawn:?}");
    }

    #[test]
    fn siblings_only_swap_once_one_is_clearly_slower() {
        let ctx = egui::Context::default();
        let order = |a: u64, b: u64| {
            let scopes = [report("a", 1, a, vec![]), report("b", 1, b, vec![])];
            let drawn = texts(&shapes(&ctx, |ui| show_scopes(ui, &scopes)));
            let position = |name: &str| drawn.iter().position(|text| text == name).unwrap();
            position("a") < position("b")
        };

        assert!(order(75, 25), "a starts slowest");
        assert!(order(75, 80), "b is not yet clearly slower");
        assert!(!order(75, 100), "b is clearly slower");
    }

    #[test]
    fn bars_split_each_scope_into_self_and_children_time() {
        let scopes = [report(
            "frame",
            1,
            100,
            vec![report("render", 1, 20, vec![])],
        )];

        let self_bars = drawn_bars(&scopes, SELF_COLOR);
        let children_bars = drawn_bars(&scopes, CHILDREN_COLOR);

        assert_eq!(self_bars.len(), 2, "{self_bars:?}");
        assert_eq!(children_bars.len(), 1, "{children_bars:?}");
        let (frame_self, render_self) = (self_bars[0], self_bars[1]);
        assert!(frame_self > 0.0);
        assert!(
            (frame_self / render_self - 4.0).abs() < 0.01,
            "{self_bars:?}"
        );
        assert!(
            (children_bars[0] - render_self).abs() < 0.01,
            "{children_bars:?} {self_bars:?}"
        );
    }

    #[test]
    fn a_leaf_lines_up_with_a_sibling_that_has_children() {
        let scopes = [
            report("parent", 1, 60, vec![report("child", 1, 10, vec![])]),
            report("leaf", 1, 40, vec![]),
        ];
        let ctx = egui::Context::default();

        let drawn = shapes(&ctx, |ui| show_scopes(ui, &scopes));

        let parent = circles(&drawn, scope_color(&ctx, &["parent"]));
        let leaf = circles(&drawn, scope_color(&ctx, &["leaf"]));
        assert_eq!((parent.len(), leaf.len()), (1, 1));
        assert!((parent[0].x - leaf[0].x).abs() < 0.5, "{parent:?} {leaf:?}");
    }

    #[test]
    fn shares_can_be_of_the_parent() {
        let scopes = [report(
            "frame",
            1,
            100,
            vec![report("render", 1, 20, vec![report("draw", 1, 10, vec![])])],
        )];
        let view = ProfilerUi {
            share_of: ShareOf::Parent,
            ..Default::default()
        };

        let of_root = drawn_text(&scopes);
        let of_parent = texts(&shapes(&egui::Context::default(), |ui| {
            view.show_scopes(ui, &scopes)
        }));

        assert_eq!(row(&of_root, "draw")[1], "10.0%");
        assert_eq!(row(&of_parent, "draw")[1], "50.0%");
    }

    #[test]
    fn a_filter_keeps_matches_with_their_ancestors_and_descendants() {
        let scopes = [report(
            "frame",
            1,
            100,
            vec![
                report("render", 1, 50, vec![report("draw", 1, 10, vec![])]),
                report("update", 1, 30, vec![]),
            ],
        )];
        let view = ProfilerUi {
            filter: "REND".to_owned(),
            ..Default::default()
        };

        let drawn = texts(&shapes(&egui::Context::default(), |ui| {
            view.show_scopes(ui, &scopes)
        }));

        for shown in ["frame", "render", "draw"] {
            assert!(drawn.contains(&shown.to_owned()), "{shown} {drawn:?}");
        }
        assert!(!drawn.contains(&"update".to_owned()), "{drawn:?}");
    }

    #[test]
    fn scopes_under_the_threshold_are_left_out() {
        let scopes = [report("big", 1, 95, vec![]), report("small", 1, 5, vec![])];
        let view = ProfilerUi {
            min_percent: 10.0,
            ..Default::default()
        };

        let drawn = texts(&shapes(&egui::Context::default(), |ui| {
            view.show_scopes(ui, &scopes)
        }));

        assert!(drawn.contains(&"big".to_owned()), "{drawn:?}");
        assert!(!drawn.contains(&"small".to_owned()), "{drawn:?}");
    }

    #[test]
    fn a_paused_view_keeps_drawing_the_timings_it_froze() {
        let profiler = Profiler::new();
        crate::profile_scope!(profiler, "before", {});
        let mut view = ProfilerUi::default();
        view.pause(&profiler);
        crate::profile_scope!(profiler, "after", {});

        let drawn = texts(&shapes(&egui::Context::default(), |ui| {
            view.show(ui, &profiler)
        }));

        assert!(view.is_paused());
        assert!(drawn.contains(&"before".to_owned()), "{drawn:?}");
        assert!(!drawn.contains(&"after".to_owned()), "{drawn:?}");
    }
}
