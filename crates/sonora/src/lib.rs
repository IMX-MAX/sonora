#[cfg(target_os = "android")]
mod actions;
#[cfg(target_os = "android")]
mod app;
#[cfg(target_os = "android")]
mod assets;
#[cfg(target_os = "android")]
mod dock;
#[cfg(target_os = "android")]
mod http;
#[cfg(target_os = "android")]
mod logging;
#[cfg(target_os = "android")]
mod memory;
#[cfg(target_os = "android")]
mod tray {
    pub fn install(
        _show: impl Fn(&mut gpui::App) + 'static,
        _toggle: impl Fn(&mut gpui::App) + 'static,
        _cx: &mut gpui::App,
    ) -> bool {
        false
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "Rust" fn android_main(app: gpui_platform::AndroidApp) {
    crate::app::android_main(app);
}
