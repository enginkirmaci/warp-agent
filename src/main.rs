//! WarpAgent's thin launcher.
//!
//! The console attribute belongs to the binary, not the library. Startup,
//! the window and the views live in `warp_agent::run` so probes can open the
//! real window.

// no console window behind the packaged Windows exe; debug builds keep it for logs
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

fn main() {
    // The support contract, before any window or logging: `--version` answers
    // with the same line the About screen copies.
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
    {
        let build = warpui_kit_core::build_info!();
        println!(
            "{}",
            build.version_line(warpui_kit_core::about::app(
                warpui_kit_core::about::AppId::Agent
            ))
        );
        return;
    }
    warp_agent::run();
}
