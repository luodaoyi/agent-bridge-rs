mod app;
mod gui_fonts;
mod hooks;
mod install;
mod protocol;
mod server;
mod session;
mod terminal_gui;
mod transport;
mod tui;
mod version_check;
mod worker;

fn main() {
    app::main();
}
