# nf-macos

Native macOS Metal engine wrapper (`nfmacos`) and CLI verification harness (`nf-macos-harness`).
It is not part of the default workspace build; the cross-platform game crate (`nf-view`) does not depend on it.

## Build the macOS app bundle

```sh
./macos/build_app.sh
```

This builds `NightfirePort`, `nf-view`, and `nf-macos-harness` in release mode and packages `macos/NightfirePort.app`.
Optional feature flags can be passed via `NF_CARGO_FEATURES` (for example `NF_CARGO_FEATURES="movies gamepad"`).

## Run the Rust Metal CLI harness

```sh
# Full verification suite (FrontEnd, On-Foot, Multiplayer, and all 8 Driving missions)
cargo run --release -p nf-macos --bin nf-macos-harness -- "<path-to-iso-or-carved-dir>"

# Filter cases or inspect a single level
cargo run --release -p nf-macos --bin nf-macos-harness -- "<iso>" --only driving
cargo run --release -p nf-macos --bin nf-macos-harness -- "<iso>" --level 09000001.bin --steps 90 --throttle 1.0 --json

# Discover all on-foot and driving archives in an ISO or carved directory
cargo run --release -p nf-macos --bin nf-macos-harness -- "<iso>" --list-levels

# Interactive persistent Metal control REPL (stdin commands -> JSON lines on stdout)
# Commands: load, input, menu, step, shot, pos, cam, auto_transition, status, levels, quit
cargo run --release -p nf-macos --bin nf-macos-harness -- "<iso>" --repl
```

Game data is read directly from the player's own disc image (`.iso`) or carved asset folder; no game bytes are bundled.
