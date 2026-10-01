// Release builds are GUI-subsystem apps: without this Windows opens a console
// window next to the island. Debug builds keep the console for the log output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Entry point only -- see lib.rs for the actual app. Tauri wants a binary
// crate present even though everything lives in the library crate (so it
// can also be exercised from tests / mobile targets later).
fn main() {
    nadi_lib::run();
}
