// Suppress the console window on Windows release builds. Debug builds keep it
// so `eprintln!` diagnostics stay visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    mk_multitool_lib::run();
}
