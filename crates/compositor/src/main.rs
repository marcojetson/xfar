//! xfar compositor entry point.
//!
//! Binary entry point for [`xfar_compositor`]. `xfar validate` checks the
//! configuration without starting the compositor.

fn main() {
    if std::env::args().nth(1).as_deref() == Some("validate") {
        std::process::exit(xfar_compositor::validate_config());
    }
    if let Err(err) = xfar_compositor::run() {
        eprintln!("xfar: fatal error: {err}");
        std::process::exit(1);
    }
}
