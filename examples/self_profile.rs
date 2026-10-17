//! Profile the profiler: a second profiler times batches of work on a target one, and
//! serves those timings on 127.0.0.1:5600 for the MCP server in `mcp_server/`.
//! A batch's total divided by its hits and by `SCOPES` is the cost of one target scope.
//!
//! Run with `cargo run --profile profiling --example self_profile --features server`,
//! adding the `egui` feature to also draw the timings in a window.

use profiler::{ProfileServer, Profiler, profile_scope};

/// Target scopes opened and closed in each scope batch.
const SCOPES: usize = 100_000;
/// How deep the nested batch goes.
const DEPTH: usize = 8;
/// Distinct sibling names the wide batch cycles through.
const NAMES: [&str; 64] = [
    "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "b0", "b1", "b2", "b3", "b4", "b5", "b6", "b7",
    "c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7",
    "e0", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7",
    "g0", "g1", "g2", "g3", "g4", "g5", "g6", "g7", "h0", "h1", "h2", "h3", "h4", "h5", "h6", "h7",
];

fn nest(target: &Profiler, depth: usize) {
    if depth > 0 {
        profile_scope!(target, NAMES[depth]);
        nest(target, depth - 1);
    }
}

/// Time one round of every batch on `target` with `meta`.
fn frame(meta: &Profiler, target: &Profiler) {
    profile_scope!(meta, "frame", {
        profile_scope!(meta, "flat", {
            for _ in 0..SCOPES {
                profile_scope!(target, "flat");
            }
        });
        profile_scope!(meta, "nested depth 8", {
            for _ in 0..SCOPES / DEPTH {
                nest(target, DEPTH);
            }
        });
        profile_scope!(meta, "64 siblings", {
            for index in 0..SCOPES {
                profile_scope!(target, NAMES[index % NAMES.len()]);
            }
        });
        profile_scope!(meta, "snapshot", {
            std::hint::black_box(target.snapshot());
        });
        profile_scope!(meta, "recent_snapshot", {
            std::hint::black_box(target.recent_snapshot());
        });
        profile_scope!(meta, "recent_series", {
            std::hint::black_box(target.recent_series());
        });
    });
}

#[cfg(not(feature = "egui"))]
fn main() -> std::io::Result<()> {
    let meta = Profiler::new();
    let target = Profiler::new();
    let mut server = ProfileServer::bind("127.0.0.1:5600")?;

    loop {
        frame(&meta, &target);
        server.poll(&meta);
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

#[cfg(feature = "egui")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use profiler::{show_scopes, show_timeline};

    let meta = Profiler::new();
    let target = Profiler::new();
    let mut server = ProfileServer::bind("127.0.0.1:5600")?;

    eframe::run_ui_native(
        "Profiler self-profile",
        eframe::NativeOptions::default(),
        move |ui, _frame| {
            frame(&meta, &target);
            server.poll(&meta);

            egui::CentralPanel::default().show(ui, |ui| {
                let window = meta.window().as_secs();
                ui.heading(format!("Profiler batch timings, last {window} s"));
                show_timeline(ui, &meta.recent_series());
                ui.separator();
                show_scopes(ui, &meta.recent_snapshot());
            });
            ui.ctx().request_repaint();
        },
    )?;
    Ok(())
}
