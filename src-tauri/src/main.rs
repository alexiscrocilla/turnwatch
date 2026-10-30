// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Some(code) = turnwatch_lib::companion_install::run_from_args() {
        std::process::exit(code);
    }
    turnwatch_lib::run()
}
