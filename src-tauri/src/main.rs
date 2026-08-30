// Hide the extra console window for Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if rivals_toolkit_lib::run_session_watchdog_from_args() {
        return;
    }
    rivals_toolkit_lib::run()
}
