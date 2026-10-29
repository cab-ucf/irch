#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Wayland: WebKitGTK's DMA-BUF renderer asks for buffers some drivers
    // (NVIDIA's above all) cannot give, and the compositor drops irch:
    // "Error 71 (Protocol error)". Off unless set; a chat needs no more.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let nvidia = std::path::Path::new("/proc/driver/nvidia").exists();
        let off = [
            ("WEBKIT_DISABLE_DMABUF_RENDERER", true),
            ("__NV_DISABLE_EXPLICIT_SYNC", nvidia),
        ];
        for (k, on) in off {
            if on && std::env::var_os(k).is_none() {
                // Before any thread starts.
                unsafe { std::env::set_var(k, "1") };
            }
        }
    }
    irch_app::run()
}
