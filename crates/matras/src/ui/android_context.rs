//! Publishing the Android `Context` and `JavaVM` to `ndk_context`.
//!
//! cpal's AAudio backend enumerates devices through `AudioManager`, which it
//! reaches with `ndk_context::android_context()`. That call *panics* —
//! "android context was not initialized" — unless something has first handed
//! `ndk_context` the VM and a `Context`. There is no `ndk-glue`/NativeActivity
//! here to do it (the app is a GTK Activity), so we do it ourselves over JNI,
//! using the pieces the GTK Android backend exports. Without this the very
//! first thing `AudioIo::start` touches blows up, and — because a Rust panic
//! goes to stderr, i.e. /dev/null — it does so invisibly.
//!
//! Three things the wider lore warns about, all handled here: the Context must
//! be a *global* reference (a local one dies with the JNI frame), init must run
//! exactly once (`ndk_context` asserts it), and the Activity context is the
//! right one to publish.

use std::ffi::c_void;
use std::sync::OnceLock;

use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use jni::objects::JObject;

static INITED: OnceLock<()> = OnceLock::new();

/// Initialises `ndk_context` from `surface`'s Activity, once. Best-effort:
/// failure is logged, and the audio path will still surface its own error.
pub fn ensure(surface: &gtk::gdk::Surface) {
    if INITED.get().is_some() {
        return;
    }
    if let Err(e) = try_init(surface) {
        tracing::warn!(error = %e, "could not initialise the android context for audio");
    }
}

fn try_init(surface: &gtk::gdk::Surface) -> Result<(), String> {
    unsafe extern "C" {
        fn gdk_android_display_get_env(
            display: *mut gtk::gdk::ffi::GdkDisplay,
        ) -> *mut jni::sys::JNIEnv;
        fn gdk_android_toplevel_get_activity(
            toplevel: *mut gtk::gdk::ffi::GdkSurface,
        ) -> jni::sys::jobject;
    }

    let env_ptr = unsafe { gdk_android_display_get_env(surface.display().to_glib_none().0) };
    if env_ptr.is_null() {
        return Err("the display has no JNI environment".into());
    }
    let activity_ptr = unsafe { gdk_android_toplevel_get_activity(surface.to_glib_none().0) };
    if activity_ptr.is_null() {
        return Err("the toplevel has no activity".into());
    }

    let mut unowned = unsafe { jni::EnvUnowned::from_raw(env_ptr) };
    let outcome = unowned
        .with_env(|env| -> Result<Result<(), String>, jni::errors::Error> {
            // The process-global VM; observing this env has initialised it.
            let vm_ptr = jni::JavaVM::singleton()?.get_raw();

            // SAFETY: GDK owns this activity reference; we only borrow it to
            // make our own global ref from it.
            let activity = unsafe { JObject::from_raw(env, activity_ptr) };
            // Leaked on purpose: `into_raw` hands out the pointer without
            // deleting the global ref, so the Context object stays alive for
            // the whole process — which is exactly ndk_context's contract.
            let context_ptr = env.new_global_ref(&activity)?.into_raw();

            // OnceLock rather than trusting the caller: initialize_android_context
            // asserts it is called at most once.
            if INITED.set(()).is_ok() {
                unsafe {
                    ndk_context::initialize_android_context(
                        vm_ptr as *mut c_void,
                        context_ptr as *mut c_void,
                    );
                }
                tracing::info!("android context initialised for audio");
            }
            Ok(Ok(()))
        })
        .into_outcome();

    match outcome {
        jni::Outcome::Ok(result) => result,
        jni::Outcome::Err(e) => Err(e.to_string()),
        jni::Outcome::Panic(_) => Err("the JNI android-context init panicked".into()),
    }
}
