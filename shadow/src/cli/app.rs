use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about = "Shadow VCS - High performance CoW workspace orchestration", long_about = None)]
pub struct App {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Init,
    New {
        name: String,
        #[arg(long)]
        parent: Option<String>,
    },
    Checkout {
        name: String,
    },
    Status,
    Diff {
        name: String,
    },
    Exec {
        name: String,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    Commit {
        #[arg(short, long)]
        message: String,
    },
    Log,
    Merge {
        /// One workspace promotes into the root; two workspaces integrate into
        /// a new workspace (requires `--out`).
        workspaces: Vec<String>,
        #[arg(long)]
        out: Option<String>,
    },
    Drop {
        name: String,
    },
    Undo,
}
