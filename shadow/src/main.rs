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
