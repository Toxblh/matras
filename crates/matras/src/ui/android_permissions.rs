//! Asking for the microphone at runtime.
//!
//! `RECORD_AUDIO` is a "dangerous" permission: the manifest entry is necessary
//! but not sufficient, and without a runtime grant AAudio refuses to open the
//! capture stream — the call starts with "No audio for this call: Internal".
//! There is no portal on Android, so the request goes straight to the hosting
//! Activity over JNI: `checkSelfPermission`, then `requestPermissions` if it is
//! not already held. The grant dialog is asynchronous and its result is not
//! plumbed back (the GTK glue does not forward `onRequestPermissionsResult`),
//! so this is fire-and-forget: the first request shows the dialog, and once the
//! person allows it every later call — and the next capture attempt — succeeds.

use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use jni::objects::{JObject, JValue};
use jni::{jni_sig, jni_str};

const PERMISSION: &str = "android.permission.RECORD_AUDIO";
/// `PackageManager.PERMISSION_GRANTED`.
const GRANTED: i32 = 0;

/// Requests the microphone permission from `surface`'s Activity if it is not
/// already granted. Best-effort: any failure is logged, never fatal.
pub fn ensure_record_audio(surface: &gtk::gdk::Surface) {
    if let Err(e) = try_request(surface) {
        tracing::warn!(error = %e, "could not request the microphone permission");
    }
}

fn try_request(surface: &gtk::gdk::Surface) -> Result<(), String> {
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
            // SAFETY: the activity outlives this synchronous call; GDK owns the
            // reference, so it is only borrowed here, never freed.
            let activity = unsafe { JObject::from_raw(env, activity_ptr) };

            let perm = env.new_string(PERMISSION)?;
            let granted = env
                .call_method(
                    &activity,
                    jni_str!("checkSelfPermission"),
                    jni_sig!("(Ljava/lang/String;)I"),
                    &[JValue::Object(perm.as_ref())],
                )?
                .i()?;
            if granted == GRANTED {
                return Ok(Ok(()));
            }

            // new String[]{ PERMISSION }
            let perm2 = env.new_string(PERMISSION)?;
            let array = env.new_object_array(1, jni_str!("java/lang/String"), &perm2)?;
            env.call_method(
                &activity,
                jni_str!("requestPermissions"),
                jni_sig!("([Ljava/lang/String;I)V"),
                &[JValue::Object(<&JObject>::from(&array)), JValue::Int(1)],
            )?;
            tracing::info!("requested the microphone permission");
            Ok(Ok(()))
        })
        .into_outcome();

    match outcome {
        jni::Outcome::Ok(result) => result,
        jni::Outcome::Err(e) => Err(e.to_string()),
        jni::Outcome::Panic(_) => Err("the JNI permission request panicked".into()),
    }
}
