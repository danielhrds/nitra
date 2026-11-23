//! `nitra.exe`: the app's window. A Windows (GUI) program, so opening it from
//! Explorer shows no console window; the command line is `nitra-cli.exe`.

#![windows_subsystem = "windows"]

fn main() {
    nitra_app::main_entry(nitra_app::Kind::Window);
}
