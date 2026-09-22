//! The desktop binary. Everything lives in the library so that the Android
//! runtime, which loads the application as a shared object rather than
//! spawning a process, can reach the same code through its own entry point
//! (see `android_entry`).

fn main() -> gtk::glib::ExitCode {
    matras::run()
}
