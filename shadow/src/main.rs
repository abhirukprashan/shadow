// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

use clap::Parser;
use shadow::cli::app::App;

fn main() {
    if let Err(e) = run() {
        eprintln!("shadow: error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let app = App::parse();
    shadow::cli::commands::handle_command(app.command)
}
