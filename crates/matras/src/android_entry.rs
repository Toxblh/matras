//! The Android entry point.
//!
//! The GTK Android runtime (`gdkandroidruntime.c`) loads `lib<target>.so`,
//! looks up the symbol `main` with `g_module_symbol`, calls it on its own
//! thread and waits for `g_application_get_default()` to come alive; if it
//! returns before then, the process exits. The `main` in `.dynsym` comes from
//! `build-aux/android/shim.c`, which forwards here — a Rust `fn main` in a
//! cdylib is never exported, and the staticlib route needs a C-visible name
//! anyway.
//!
//! The runtime's argv is just `["android-gtk"]`, so there is nothing to parse.

use std::ffi::{c_char, c_int};

#[unsafe(no_mangle)]
pub extern "C" fn matras_main(
    _argc: c_int,
    _argv: *mut *mut c_char,
    _envp: *mut *mut c_char,
) -> c_int {
    // `run()` blocks in `g_application_run` for the life of the app, which is
    // exactly the not-returning the runtime's barrier expects.
    crate::run().get() as c_int
}
