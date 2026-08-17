// Windows: hide the console window for GUI (release) builds. Dev builds keep
// the console so `println!` / panics / tracing surface during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    poseidon_app::run();
}
