//! Idiomatic Rust macOS Metal harness binary (`nf-macos-harness`).
//!
//! Usage:
//!   nf-macos-harness <iso-or-carved-dir> [out-dir]

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use nfmacos::engine::{InputState, MacEngine};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data_path = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../007 - Nightfire (USA).iso"));
    let out_dir = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("work/macos_rust_harness"));

    let mut engine = MacEngine::init(&data_path)?;
    println!("[nf-macos-harness] initialized: {}", engine.status_line());

    let cases: &[(&str, &str, u32, InputState, bool)] = &[
        ("fe_main", "07000048.bin", 30, InputState::default(), false),
        ("onfoot_01_exchange", "07000001.bin", 30, InputState { move_y: 0.5, ..Default::default() }, false),
        ("onfoot_05_alpine", "07000005.bin", 30, InputState::default(), false),
        ("onfoot_08_nightshift", "07000008.bin", 30, InputState::default(), false),
        ("mp_23_skyrail", "07000023.bin", 30, InputState::default(), false),
        ("driving_mis01_paris", "09000001.bin", 60, InputState { move_y: 1.0, ..Default::default() }, true),
        ("driving_mis3_alpine", "09000005.bin", 60, InputState { move_y: 1.0, ..Default::default() }, true),
        ("driving_mis4_vanquished", "09000006.bin", 60, InputState { move_y: 1.0, ..Default::default() }, true),
        ("driving_mis11_sub", "09000002.bin", 60, InputState { move_y: 1.0, ..Default::default() }, true),
        ("driving_mis13a_ultralight", "09000003.bin", 60, InputState { move_y: 1.0, ..Default::default() }, true),
        ("driving_mis13b_island", "09000004.bin", 60, InputState { move_y: 1.0, ..Default::default() }, true),
        ("driving_mis13c_boat", "09000007.bin", 60, InputState { move_y: 1.0, ..Default::default() }, true),
        ("driving_race_bonus", "09000008.bin", 60, InputState { move_y: 1.0, ..Default::default() }, true),
    ];

    let mut failed = 0usize;
    for &(name, archive, steps, input, check_motion) in cases {
        let t0 = Instant::now();
        engine.load_level(archive)?;
        let start_pos = engine.telemetry().pos;
        engine.set_input(input);
        for _ in 0..steps {
            engine.step(1.0 / 60.0);
        }
        let end_tel = engine.telemetry();
        let moved = ((end_tel.pos[0] - start_pos[0]).powi(2)
            + (end_tel.pos[2] - start_pos[2]).powi(2))
        .sqrt();
        let png_path = out_dir.join(format!("{name}.png"));
        let stats = engine.render_png(1280, 720, &png_path)?;
        let render_ok = stats.mean_luma > 3.0 && stats.std_luma > 4.0;
        let motion_ok = !check_motion || moved > 0.5;
        let ok = render_ok && motion_ok;
        let tag = if ok { "PASS" } else { "FAIL" };
        if !ok {
            failed += 1;
        }
        println!(
            "[{tag}] {name:26} {}x{} mean={:5.1} std={:5.1} moved={:5.1}m ({:.2}s) | {}",
            stats.width,
            stats.height,
            stats.mean_luma,
            stats.std_luma,
            moved,
            t0.elapsed().as_secs_f64(),
            engine.status_line()
        );
    }

    if failed > 0 {
        anyhow::bail!("{failed} harness case(s) failed pixel sanity checks");
    }
    Ok(())
}
