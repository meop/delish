// a windows subsystem binary opens no console window: it is started in the desktop session, where one would show
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if matches!(std::env::args().nth(1).as_deref(), Some("-V" | "--version")) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(e) = delish::server::serve() {
        delish::server::log(&format!("serve: {e}"));
        eprintln!("delishd: {e}");
        std::process::exit(1);
    }
}
