//! Dendro — a fast, WizTree-style disk space analyzer for Linux.

mod format;
mod helper;
mod mounts;
mod scanner;
mod settings;
mod tree;
mod treemap;
mod ui;

use std::ffi::OsString;

const USAGE: &str = "\
Usage: dendro [OPTIONS] [FOLDER]

A fast disk space analyzer. Opens a window and, if FOLDER is given,
starts scanning it right away.

Options:
  -h, --help       Show this help
  -V, --version    Show the version
";

fn main() -> std::process::ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == helper::HELPER_ARG) {
        let code = helper::run(&args[2..]);
        return std::process::ExitCode::from(code as u8);
    }
    for a in &args[1..] {
        match a.to_str() {
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return std::process::ExitCode::SUCCESS;
            }
            Some("-V" | "--version") => {
                println!("dendro {}", ui::VERSION);
                return std::process::ExitCode::SUCCESS;
            }
            _ => {}
        }
    }
    scanner::raise_fd_limit();
    let code = ui::run(args);
    if code == gtk::glib::ExitCode::SUCCESS {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
