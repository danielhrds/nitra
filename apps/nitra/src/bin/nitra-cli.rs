//! `nitra-cli.exe`: the command line (tests and measurements): host or viewer
//! with `--role`, the window without it. A console program, so it prints and
//! PowerShell waits for it.

fn main() {
    nitra_app::main_entry(nitra_app::Kind::Cli);
}
