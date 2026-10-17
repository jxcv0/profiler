//! Profile some work every frame and draw where the time goes, over time and as a tree.
//!
//! Run with `cargo run --example egui_tree --features egui`.

use profiler::{Profiler, profile_scope, show_scopes, show_timeline};

fn simulate(profiler: &Profiler, values: &mut [u64]) {
    profile_scope!(profiler, "simulate");
    for (index, value) in values.iter_mut().enumerate() {
        *value = value
            .wrapping_mul(6364136223846793005)
            .wrapping_add(index as u64);
    }
}

fn sort(profiler: &Profiler, values: &[u64]) -> Vec<u64> {
    profile_scope!(profiler, "sort", {
        let mut sorted = profile_scope!(profiler, "copy", { values.to_vec() });
        sorted.sort_unstable();
        sorted
    })
}

fn main() -> eframe::Result {
    let profiler = Profiler::new();
    let mut values: Vec<u64> = (0..100_000).collect();

    eframe::run_ui_native(
        "Profiler",
        eframe::NativeOptions::default(),
        move |ui, _frame| {
            profile_scope!(profiler, "frame", {
                simulate(&profiler, &mut values);
                sort(&profiler, &values);
            });

            egui::CentralPanel::default().show(ui, |ui| {
                let window = profiler.window().as_secs();
                ui.heading(format!("Scope timings, last {window} s"));
                ui.label("Self time per 100 ms; a full column is a fully busy 100 ms.");
                show_timeline(ui, &profiler.recent_series());
                ui.separator();
                ui.label("Bars: orange is time in the scope itself, blue is time in its children.");
                show_scopes(ui, &profiler.recent_snapshot());
            });
            ui.ctx().request_repaint();
        },
    )
}
