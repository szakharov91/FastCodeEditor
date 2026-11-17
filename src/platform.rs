#[cfg(target_os = "macos")]
pub fn enable_native_blur(creation_context: &eframe::CreationContext<'_>) {
    use objc2_app_kit::NSView;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = creation_context.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(app_kit) = handle.as_raw() else {
        return;
    };

    // SAFETY: eframe owns a live NSView for the duration of creation. AppKit window
    // access and the CGS call are made synchronously on the UI thread.
    unsafe {
        let view = &*app_kit.ns_view.as_ptr().cast::<NSView>();
        let Some(window) = view.window() else {
            return;
        };
        let status = cgs_set_window_background_blur_radius(
            cgs_main_connection_id(),
            window.windowNumber() as i32,
            80,
        );
        if status != 0 {
            eprintln!("Не удалось включить системный blur: CGError {status}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn enable_native_blur(_creation_context: &eframe::CreationContext<'_>) {
    // Transparent surfaces remain available; native blur is platform-specific.
}

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    #[link_name = "CGSMainConnectionID"]
    fn cgs_main_connection_id() -> u32;

    #[link_name = "CGSSetWindowBackgroundBlurRadius"]
    fn cgs_set_window_background_blur_radius(
        connection: u32,
        window_number: i32,
        radius: i32,
    ) -> i32;
}
