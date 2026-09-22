//! Opening a URL in the person's browser.
//!
//! The desktop asks GIO for the default handler. Android has no GAppInfo
//! registry — the platform's way is an ACTION_VIEW intent — and the GTK
//! Android backend exports the two pieces needed to fire one from native
//! code: the GTK thread's JNI environment and an activity launcher on the
//! toplevel. Both halves report failure the same way, so callers show one
//! message either way.

#[cfg(not(target_os = "android"))]
pub fn open(url: &str) -> Result<(), String> {
    gtk::gio::AppInfo::launch_default_for_uri(url, None::<&gtk::gio::AppLaunchContext>)
        .map_err(|e| format!("Could not open a browser: {e}"))
}

#[cfg(target_os = "android")]
pub fn open(url: &str) -> Result<(), String> {
    use gtk::glib::translate::ToGlibPtr;
    use gtk::prelude::*;
    use jni::{jni_sig, jni_str, JValue};

    // From gdk/android/gdkandroiddisplay.h and gdkandroidtoplevel.h; both are
    // exported by libgtk-4.so (checked with llvm-nm). On Android the window's
    // GdkSurface *is* the GdkAndroidToplevel, so the surface pointer is the
    // right receiver.
    unsafe extern "C" {
        fn gdk_android_display_get_env(
            display: *mut gtk::gdk::ffi::GdkDisplay,
        ) -> *mut jni::sys::JNIEnv;
        fn gdk_android_toplevel_launch_activity(
            toplevel: *mut gtk::gdk::ffi::GdkSurface,
            intent: jni::sys::jobject,
            error: *mut *mut gtk::glib::ffi::GError,
        ) -> gtk::glib::ffi::gboolean;
    }

    let window = gtk::gio::Application::default()
        .and_downcast::<gtk::Application>()
        .and_then(|app| app.active_window())
        .ok_or("no window to launch the browser from")?;
    let surface = window.surface().ok_or("the window is not on screen yet")?;

    let env_ptr = unsafe { gdk_android_display_get_env(surface.display().to_glib_none().0) };
    if env_ptr.is_null() {
        return Err("the display has no JNI environment".into());
    }

    // GDK owns this thread's attachment; EnvUnowned::with_env borrows it and
    // manages the local frame and any thrown exception around the closure.
    let mut unowned = unsafe { jni::EnvUnowned::from_raw(env_ptr) };
    let outcome = unowned
        .with_env(|env| -> Result<Result<(), String>, jni::errors::Error> {
            let uri_str = env.new_string(url)?;
            let uri = env
                .call_static_method(
                    jni_str!("android/net/Uri"),
                    jni_str!("parse"),
                    jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
                    &[JValue::Object(uri_str.as_ref())],
                )?
                .l()?;
            let action = env.new_string("android.intent.action.VIEW")?;
            let intent = env.new_object(
                jni_str!("android/content/Intent"),
                jni_sig!("(Ljava/lang/String;Landroid/net/Uri;)V"),
                &[JValue::Object(action.as_ref()), JValue::Object(&uri)],
            )?;

            let mut gerr: *mut gtk::glib::ffi::GError = std::ptr::null_mut();
            let ok = unsafe {
                gdk_android_toplevel_launch_activity(
                    surface.to_glib_none().0,
                    intent.as_raw(),
                    &mut gerr,
                )
            };
            Ok(if ok == gtk::glib::ffi::GFALSE {
                let error: gtk::glib::Error = unsafe { gtk::glib::translate::from_glib_full(gerr) };
                Err(format!("Could not open a browser: {error}"))
            } else {
                Ok(())
            })
        })
        .into_outcome();

    match outcome {
        jni::Outcome::Ok(result) => result,
        jni::Outcome::Err(e) => Err(format!("Could not open a browser: {e}")),
        jni::Outcome::Panic(_) => Err("Could not open a browser: the JNI call panicked".into()),
    }
}
