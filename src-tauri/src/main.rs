// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Release builds must be warning-free.
#![cfg_attr(not(debug_assertions), deny(warnings))]

fn main() {
    canvaz_lib::run()
}
