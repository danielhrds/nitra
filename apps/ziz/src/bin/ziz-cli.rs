//! `ziz-cli.exe`: the command line (tests and measurements): host or viewer
//! with `--role`, the window without it. A console program, so it prints and
//! PowerShell waits for it.

fn main() {
    ziz_app::main_entry(ziz_app::Kind::Cli);
}
