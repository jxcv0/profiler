use crate::ScopeReport;

/// Draw a [`crate::Profiler::snapshot`] as a tree, with scopes that have children collapsible.
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
    for scope in scopes {
        let milliseconds = scope.total.as_secs_f64() * 1000.0;
        let label = format!("{}: {} hits, {milliseconds:.3} ms", scope.name, scope.hits);
        if scope.children.is_empty() {
            ui.label(label);
        } else {
            egui::CollapsingHeader::new(label)
                .id_salt(scope.name)
                .default_open(true)
                .show(ui, |ui| show_scopes(ui, &scope.children));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    #[test]
    fn draws_each_scope_with_its_hits_and_total() {
        let scopes = [ScopeReport {
            name: "frame",
            hits: 2,
            total: Duration::from_millis(100),
            children: vec![ScopeReport {
                name: "render",
                hits: 2,
                total: Duration::from_millis(20),
                children: Vec::new(),
            }],
        }];

        let drawn = drawn_text(&scopes);

        assert!(drawn.contains(&"frame: 2 hits, 100.000 ms".to_owned()), "{drawn:?}");
        assert!(drawn.contains(&"render: 2 hits, 20.000 ms".to_owned()), "{drawn:?}");
    }
}
