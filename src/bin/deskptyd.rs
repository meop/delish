// a windows subsystem binary opens no console window: it is started in the desktop session, where one would show
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if let Err(e) = deskpty::server::serve() {
        deskpty::server::log(&format!("serve: {e}"));
        eprintln!("deskptyd: {e}");
        std::process::exit(1);
    }
}
