use std::hash::{Hash, Hasher};
use std::time::Duration;

use crate::{ScopeReport, ScopeSeries, TimeSeries};

/// Bar colour for the time a scope spends in itself.
pub const SELF_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 120, 60);
/// Bar colour for the time a scope spends in its child scopes.
pub const CHILDREN_COLOR: egui::Color32 = egui::Color32::from_rgb(110, 140, 190);

/// Width of the bar drawn for a scope that takes all of the profiled time.
const BAR_WIDTH: f32 = 160.0;

/// Draw a [`crate::Profiler::snapshot`] as a tree, slowest sibling first.
/// Each scope gets a bar sized by its share of all root time: self time, then child time.
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
    let whole = scopes
        .iter()
        .map(|scope| scope.total)
        .sum::<Duration>()
        .as_secs_f64();
    show_level(ui, scopes, whole);
}

fn show_level(ui: &mut egui::Ui, scopes: &[ScopeReport], whole: f64) {
    let mut slowest_first: Vec<&ScopeReport> = scopes.iter().collect();
    slowest_first.sort_by_key(|scope| std::cmp::Reverse(scope.total));
    for scope in slowest_first {
        if scope.children.is_empty() {
            ui.horizontal(|ui| show_row(ui, scope, whole));
        } else {
            let id = ui.make_persistent_id(scope.name);
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
                .show_header(ui, |ui| show_row(ui, scope, whole))
                .body(|ui| show_level(ui, &scope.children, whole));
        }
    }
}

fn show_row(ui: &mut egui::Ui, scope: &ScopeReport, whole: f64) {
    let share = |time: Duration| match whole {
        0.0 => 0.0,
        whole => time.as_secs_f64() / whole,
    };
    let self_share = share(scope.self_time()) as f32;
    let total_share = share(scope.total) as f32;

    let height = ui.text_style_height(&egui::TextStyle::Body);
    let (bar, _) = ui.allocate_exact_size(egui::vec2(BAR_WIDTH, height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(bar, 0.0, ui.visuals().faint_bg_color);
    let self_end = bar.left() + BAR_WIDTH * self_share;
    let total_end = bar.left() + BAR_WIDTH * total_share;
    if self_end > bar.left() {
        let self_bar = egui::Rect::from_x_y_ranges(bar.left()..=self_end, bar.y_range());
        painter.rect_filled(self_bar, 0.0, SELF_COLOR);
    }
    if total_end > self_end {
        let children_bar = egui::Rect::from_x_y_ranges(self_end..=total_end, bar.y_range());
        painter.rect_filled(children_bar, 0.0, CHILDREN_COLOR);
    }

    let milliseconds = |time: Duration| time.as_secs_f64() * 1000.0;
    let mut label = format!(
        "{}: {:.1}% · self {:.3} ms · {} hits",
        scope.name,
        total_share * 100.0,
        milliseconds(scope.self_time()),
        scope.hits,
    );
    if let Some(average) = scope.average() {
        label.push_str(&format!(" · avg {:.3} ms", milliseconds(average)));
    }
    ui.label(label);
}

/// The colour a scope path is drawn in by [`show_timeline`], the same every frame.
pub fn scope_color(path: &[&str]) -> egui::Color32 {
    let mut hasher = std::hash::DefaultHasher::new();
    path.hash(&mut hasher);
    let hue = (hasher.finish() % 360) as f32 / 360.0;
    egui::ecolor::Hsva::new(hue, 0.6, 0.85, 1.0).into()
}

/// Height of the chart drawn by [`show_timeline`].
const TIMELINE_HEIGHT: f32 = 120.0;

/// Draw a [`crate::Profiler::recent_series`] as stacked columns, oldest bucket on the left,
/// with a legend below. A full-height column means the whole bucket was spent in scopes.
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
    let size = egui::vec2(ui.available_width(), TIMELINE_HEIGHT);
    let (chart, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(chart, 0.0, ui.visuals().faint_bg_color);

    let buckets = series
        .scopes
        .first()
        .map_or(0, |scope| scope.self_times.len());
    let column_width = chart.width() / buckets.max(1) as f32;
    let bucket = series.bucket.as_secs_f64();
    for position in 0..buckets {
        let left = chart.left() + column_width * position as f32;
        let mut bottom = chart.bottom();
        for scope in &series.scopes {
            let time = scope.self_times[position];
            if time.is_zero() {
                continue;
            }
            let height = (time.as_secs_f64() / bucket) as f32 * chart.height();
            let top = (bottom - height).max(chart.top());
            let column = egui::Rect::from_x_y_ranges(left..=left + column_width, top..=bottom);
            painter.rect_filled(column, 0.0, scope_color(&scope.path));
            bottom = top;
        }
    }

    ui.horizontal_wrapped(|ui| {
        for scope in &series.scopes {
            show_legend_entry(ui, scope);
        }
    });
}

fn show_legend_entry(ui: &mut egui::Ui, scope: &ScopeSeries) {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let (swatch, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let color = scope_color(&scope.path);
    ui.painter()
        .circle_filled(swatch.center(), size / 3.0, color);
    ui.label(scope.path.join("/"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(shape: &egui::Shape, found: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => found.push(text.galley.text().to_owned()),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| texts(shape, found)),
            _ => {}
        }
    }

    fn drawn_text(scopes: &[ScopeReport]) -> Vec<String> {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| show_scopes(ui, scopes));
        output.textures_delta.clear();
        let mut found = Vec::new();
        for clipped in &output.shapes {
            texts(&clipped.shape, &mut found);
        }
        found
    }

    fn rects(shape: &egui::Shape, fill: egui::Color32, found: &mut Vec<egui::Rect>) {
        match shape {
            egui::Shape::Rect(rect) if rect.fill == fill => found.push(rect.rect),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| rects(shape, fill, found)),
            _ => {}
        }
    }

    fn drawn_rects(draw: impl FnMut(&mut egui::Ui), fill: egui::Color32) -> Vec<egui::Rect> {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), draw);
        output.textures_delta.clear();
        let mut found = Vec::new();
        for clipped in &output.shapes {
            rects(&clipped.shape, fill, &mut found);
        }
        found
    }

    fn drawn_bars(scopes: &[ScopeReport], fill: egui::Color32) -> Vec<f32> {
        let bars = drawn_rects(|ui| show_scopes(ui, scopes), fill);
        bars.iter().map(|bar| bar.width()).collect()
    }

    #[test]
    fn timeline_stacks_each_scopes_self_time_in_its_bucket_column() {
        let in_third_bucket = |milliseconds| {
            let mut self_times = vec![Duration::ZERO; 4];
            self_times[2] = Duration::from_millis(milliseconds);
            self_times
        };
        let series = TimeSeries {
            bucket: Duration::from_millis(100),
            scopes: vec![
                ScopeSeries {
                    path: vec!["frame"],
                    self_times: in_third_bucket(50),
                },
                ScopeSeries {
                    path: vec!["frame", "render"],
                    self_times: in_third_bucket(25),
                },
            ],
        };
        let frame_color = scope_color(&["frame"]);
        let render_color = scope_color(&["frame", "render"]);

        let frame = drawn_rects(|ui| show_timeline(ui, &series), frame_color);
        let render = drawn_rects(|ui| show_timeline(ui, &series), render_color);

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
    fn labels_each_scope_with_its_share_self_time_and_average() {
        let scopes = [report(
            "frame",
            2,
            100,
            vec![report("render", 2, 20, vec![])],
        )];

        let drawn = drawn_text(&scopes);

        let frame = "frame: 100.0% · self 80.000 ms · 2 hits · avg 50.000 ms";
        let render = "render: 20.0% · self 20.000 ms · 2 hits · avg 10.000 ms";
        assert!(drawn.contains(&frame.to_owned()), "{drawn:?}");
        assert!(drawn.contains(&render.to_owned()), "{drawn:?}");
    }

    #[test]
    fn an_unfinished_scope_has_no_average() {
        let scopes = [report("frame", 0, 0, vec![report("update", 1, 30, vec![])])];

        let drawn = drawn_text(&scopes);

        assert!(
            drawn.contains(&"frame: 0.0% · self 0.000 ms · 0 hits".to_owned()),
            "{drawn:?}"
        );
    }

    #[test]
    fn siblings_are_drawn_slowest_first() {
        let scopes = [report("fast", 1, 25, vec![]), report("slow", 1, 75, vec![])];

        let drawn = drawn_text(&scopes);

        let position = |name: &str| {
            drawn
                .iter()
                .position(|text| text.starts_with(name))
                .unwrap()
        };
        assert!(position("slow") < position("fast"), "{drawn:?}");
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
}
