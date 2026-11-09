// Boot OS Pro — main.rs — build 1.0.2 — Monday 9 November 2026

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    bootospro_lib::run();
}
