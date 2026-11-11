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
    warp_agent::run();
}
