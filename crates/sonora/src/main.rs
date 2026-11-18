#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
#[cfg(windows)]
mod app_icon;
mod assets;
mod console;
mod dock;
#[cfg(target_os = "macos")]
mod gestures;
mod http;
mod logging;
mod memory;
mod single;
#[cfg(windows)]
mod thumbbar;
mod tray;
#[cfg(windows)]
mod window_title;
#[cfg(target_os = "linux")]
mod x11_icon;

fn main() -> std::process::ExitCode {
    app::main()
}
