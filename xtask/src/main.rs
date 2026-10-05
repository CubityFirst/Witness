//! Witness build, signing and release tooling.
//!
//! Run as `cargo run -p xtask -- <command>`. The commands are implemented by
//! the release-tooling unit; until then this only prints usage.

const USAGE: &str = "\
usage: cargo run -p xtask -- <command>

commands:
  version X.Y.Z            set the workspace version (root Cargo.toml + Cargo.lock)
  check                    verify the release configuration
  dist [--sign] [--notes]  build the release exe and the NSIS installer
  sign <file>              minisign-sign a file with the updater key";

fn main() {
    let command = std::env::args().nth(1);
    match command.as_deref() {
        None | Some("help" | "-h" | "--help") => println!("{USAGE}"),
        Some(other) => {
            eprintln!("xtask: `{other}` is not implemented yet\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}
