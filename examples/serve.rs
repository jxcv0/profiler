//! Profile some work in a loop and answer profile requests on 127.0.0.1:5600,
//! where the MCP server in `mcp_server/profiler_mcp.py` reads them.
//!
//! Run with `cargo run --example serve --features server`.

use profiler::{ProfileServer, Profiler, profile_scope};

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

fn main() -> std::io::Result<()> {
    let profiler = Profiler::new();
    let mut server = ProfileServer::bind("127.0.0.1:5600")?;
    let mut values: Vec<u64> = (0..100_000).collect();

    loop {
        profile_scope!(profiler, "frame", {
            simulate(&profiler, &mut values);
            sort(&profiler, &values);
        });
        server.poll(&profiler);
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}
