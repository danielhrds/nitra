//! `ziz.exe`: the app's window. A Windows (GUI) program, so opening it from
//! Explorer shows no console window; the command line is `ziz-cli.exe`.

#![windows_subsystem = "windows"]

fn main() {
    ziz_app::main_entry(ziz_app::Kind::Window);
}
