//! CLI command implementations.

use crate::cli::app::Commands;
use crate::engine::workspace::Engine;
use anyhow::{Context, Result};
use colored::*;
use std::env;
use std::io::IsTerminal;
use std::path::Path;

/// Dispatch a parsed CLI command.
pub fn handle_command(cmd: Commands) -> Result<()> {
    let cwd = env::current_dir()?;
    let engine = Engine::open_discovered(&cwd)?;

    match cmd {
        Commands::Init => cmd_init(&engine),
        Commands::New { name, parent } => cmd_new(&engine, &name, parent.as_deref()),
        Commands::Checkout { name } => cmd_checkout(&engine, &name),
        Commands::Status => cmd_status(&engine),
        Commands::Diff { name } => cmd_diff(&engine, &name),
        Commands::Exec { name, cmd } => cmd_exec(&engine, &name, cmd),
        Commands::Commit { message } => cmd_commit(&engine, &cwd, &message),
        Commands::Log => cmd_log(&engine),
        Commands::Merge { workspaces, out } => cmd_merge(&engine, &workspaces, out.as_deref()),
        Commands::Drop { name } => cmd_drop(&engine, &name),
        Commands::Undo => cmd_undo(&engine),
    }
}

fn cmd_init(engine: &Engine) -> Result<()> {
    let fs_name = engine.fs_name();
    println!("{} initialized shadow metadata", "✓".green());
    println!("  root:       {}", engine.root().display());
    println!("  metadata:   {}", engine.db_path().display());
    println!("  filesystem: {fs_name}");

    if crate::co_w::fs_utils::is_problematic_fs(&fs_name) {
        println!(
            "  {} filesystem '{}' does not support native CoW reflinks; \
             falling back to copy-based workspaces (\".shadow/staging\")",
            "!".yellow(),
            fs_name
        );
    } else {
        println!("  {} native reflink CoW available", "✓".green());
    }
    Ok(())
}

fn cmd_new(engine: &Engine, name: &str, parent: Option<&str>) -> Result<()> {
    let ws = engine.create(name, parent)?;
    let lineage = describe_lineage(engine, &ws);
    println!("{} created workspace '{}' ({})", "✓".green(), name, lineage);
    println!("  path: {}", ws.path);
    Ok(())
}

/// The shell used for `shadow checkout`, honouring `$SHELL`.
pub fn shell_path() -> String {
    env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// Build the subshell command used by `shadow checkout`.
pub fn checkout_command(shell: &str, dir: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(shell);
    command.current_dir(dir);
    command.env("SHADOW_WORKSPACE_PATH", dir);
    command
}

fn cmd_checkout(engine: &Engine, name: &str) -> Result<()> {
    let ws = engine.find(name)?;
    engine.touch(name)?;
    let ws_path = Path::new(&ws.path);
    let shell = shell_path();
    println!(
        "{} entering workspace '{}' ({}) — type 'exit' to return to the project root",
        "➜".cyan(),
        name,
        ws_path.display()
    );
    let status = checkout_command(&shell, ws_path)
        .status()
        .with_context(|| format!("failed to spawn shell '{shell}'"))?;
    if !status.success() {
        eprintln!("{} shell exited with status {status}", "!".yellow());
    }
    Ok(())
}

fn cmd_status(engine: &Engine) -> Result<()> {
    let report = engine.reconcile()?;
    if report.pruned > 0 {
        println!(
            "{} pruned {} ghost workspace record(s) with missing directories",
            "!".yellow(),
            report.pruned
        );
    }

    let workspaces = engine.list()?;
    if workspaces.is_empty() {
        println!("No workspaces. Create one with `shadow new <name>`.");
        return Ok(());
    }

    println!(
        "{:<16} {:<12} {:<10} {:<10} {}",
        "NAME".bold(),
        "STATUS".bold(),
        "AGE".bold(),
        "DELTA".bold(),
        "LINEAGE".bold()
    );
    for ws in workspaces {
        let alive = engine.is_alive(&ws);
        let status = if alive {
            "active".green().to_string()
        } else {
            "abandoned".yellow().to_string()
        };
        let age = humanize_age(ws.created_at);
        let delta = match (engine.baseline(&ws), engine.workspace_manifest(&ws)) {
            (Ok(base), Ok(cur)) => crate::engine::snapshot::diff(&base, &cur).len(),
            _ => 0,
        };
        let lineage = describe_lineage(engine, &ws);
        println!(
            "{:<16} {:<12} {:<10} {:<10} {}",
            ws.name,
            status,
            age,
            format!("{delta} files"),
            lineage
        );
    }
    Ok(())
}

fn cmd_diff(engine: &Engine, name: &str) -> Result<()> {
    let ws = engine.find(name)?;
    let base = engine.baseline(&ws)?;
    let ours = engine.workspace_manifest(&ws)?;

    let changes = crate::engine::snapshot::diff(&base, &ours);
    if changes.is_empty() {
        println!("No changes in '{}' relative to base.", name);
        return Ok(());
    }

    let ws_path = Path::new(&ws.path);
    let root = engine.root();
    for change in &changes {
        let (label, marker) = match change.kind {
            crate::engine::snapshot::ChangeKind::Added => ("added", "+".green()),
            crate::engine::snapshot::ChangeKind::Modified => ("modified", "~".yellow()),
            crate::engine::snapshot::ChangeKind::Deleted => ("deleted", "-".red()),
        };
        println!("{} {} ({label})", marker, change.path.bold());

        if change.kind == crate::engine::snapshot::ChangeKind::Modified {
            let old = std::fs::read_to_string(root.join(&change.path)).unwrap_or_default();
            let new = std::fs::read_to_string(ws_path.join(&change.path)).unwrap_or_default();
            print_line_diff(&old, &new);
        }
    }
    Ok(())
}

fn cmd_exec(engine: &Engine, name: &str, cmd: Vec<String>) -> Result<()> {
    if cmd.is_empty() {
        anyhow::bail!("no command specified; usage: shadow exec <name> -- <cmd...>");
    }
    let ws = engine.find(name)?;
    engine.touch(name)?;
    let ws_path = Path::new(&ws.path);

    // Local artifact isolation: keep compiler/test caches inside the workspace.
    let artifacts = ws_path.join(".shadow-artifacts");
    std::fs::create_dir_all(&artifacts)?;

    let mut command = std::process::Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    command.current_dir(ws_path);
    command.env("CARGO_TARGET_DIR", artifacts.join("target"));
    command.env("npm_config_cache", artifacts.join("npm"));
    command.env("NODE_PATH", ws_path.join("node_modules"));
    command.env("SHADOW_WORKSPACE", &ws.name);
    command.env("SHADOW_WORKSPACE_PATH", ws_path);

    // Simple heartbeat refresher while the command runs.
    let hb = crate::engine::heartbeat::Heartbeat::new(ws_path);
    let _ = hb.beat();

    let status = command
        .status()
        .with_context(|| format!("failed to spawn '{}'", cmd[0]))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn cmd_commit(engine: &Engine, cwd: &std::path::Path, message: &str) -> Result<()> {
    let commit = engine.commit(cwd, message)?;
    println!(
        "{} checkpoint {} {}",
        "✓".green(),
        &commit.id.to_string()[..8],
        message
    );
    Ok(())
}

fn cmd_log(engine: &Engine) -> Result<()> {
    let commits = engine.commits()?;
    if commits.is_empty() {
        println!("No checkpoints yet. Use `shadow commit -m \"...\"`.");
        return Ok(());
    }
    for c in commits {
        println!(
            "{} {} {}",
            c.timestamp.format("%Y-%m-%d %H:%M:%S"),
            c.id.to_string()[..8].yellow(),
            c.message
        );
    }
    Ok(())
}

fn cmd_merge(engine: &Engine, workspaces: &[String], out: Option<&str>) -> Result<()> {
    let report = match (workspaces.len(), out) {
        (1, None) => crate::merger::conflicts::promote(engine, &workspaces[0])?,
        (2, Some(out)) => {
            crate::merger::conflicts::integrate(engine, &workspaces[0], &workspaces[1], out)?
        }
        (1, Some(_)) => anyhow::bail!("--out requires two source workspaces"),
        (2, None) => anyhow::bail!("integration merge requires --out <new-workspace>"),
        _ => anyhow::bail!("usage: shadow merge <ws> | shadow merge <ws1> <ws2> --out <new>"),
    };

    let verb = report.kind.as_str();
    if report.clean {
        let dest = report
            .output_workspace
            .as_deref()
            .map(|w| format!(" into new workspace '{w}'"))
            .unwrap_or_else(|| " into the project root".to_string());
        println!(
            "{} {verb}: applied {} file(s), {} deletion(s){dest}",
            "✓".green(),
            report.written.len(),
            report.deleted.len(),
        );
        return Ok(());
    }

    let headless = env::var_os("SHADOW_HEADLESS").is_some() || !std::io::stdout().is_terminal();
    eprintln!(
        "{} {verb} aborted: {} conflicted file(s); target left untouched",
        "✗".red(),
        report.conflicts.len(),
    );

    if headless {
        if let Some(json) = &report.conflicts_json {
            eprintln!("  conflicts manifest: {}", json.display());
        }
    } else {
        for c in &report.conflicts {
            eprintln!("  {} {}", "!".yellow(), c.path.bold());
            for h in &c.hunks {
                eprintln!(
                    "      conflict at ours lines {}-{} vs theirs lines {}-{}",
                    h.ours_start, h.ours_end, h.theirs_start, h.theirs_end
                );
            }
            eprintln!("      staged with markers: {}", c.staged_path.display());
            if let Some(cmd) = editor_suggestion(c) {
                eprintln!("      resolve with: {cmd}");
            }
        }
        if let Some(json) = &report.conflicts_json {
            eprintln!("  conflicts manifest: {}", json.display());
        }
    }
    Ok(())
}

/// Build an editor invocation that jumps to the first conflicted hunk.
fn editor_suggestion(conflict: &crate::merger::conflicts::FileConflict) -> Option<String> {
    let editor = env::var("SHADOW_EDITOR")
        .or_else(|_| env::var("EDITOR"))
        .ok()?;
    let line = conflict.hunks.first().map(|h| h.ours_start).unwrap_or(1);
    let path = conflict.staged_path.display().to_string();
    if editor.contains("code") || editor.contains("cursor") {
        Some(format!("{editor} --goto {path}:{line}"))
    } else {
        Some(format!("{editor} {path}"))
    }
}

fn cmd_drop(engine: &Engine, name: &str) -> Result<()> {
    engine.drop(name)?;
    println!("{} dropped '{}'", "✓".green(), name);
    Ok(())
}

fn cmd_undo(engine: &Engine) -> Result<()> {
    let op = engine.undo()?;
    println!(
        "{} rolled back last transaction ({})",
        "↶".yellow(),
        &op.id.to_string()[..8]
    );
    Ok(())
}

fn describe_lineage(engine: &Engine, ws: &crate::storage::models::Workspace) -> String {
    let mut chain = vec![ws.name.clone()];
    let mut cursor = ws.parent_id;
    let all = engine.list().unwrap_or_default();
    while let Some(pid) = cursor {
        match all.iter().find(|w| w.id == pid) {
            Some(parent) => {
                chain.push(parent.name.clone());
                cursor = parent.parent_id;
            }
            None => break,
        }
    }
    chain.push("main".to_string());
    chain.reverse();
    chain.join(" -> ")
}

fn humanize_age(created: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - created).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Print a colorized, git-style line diff using the `diff` crate.
fn print_line_diff(old: &str, new: &str) {
    use diff::Result as DiffResult;
    for item in diff::lines(old, new) {
        match item {
            DiffResult::Left(l) => println!("{}", format!("- {l}").red()),
            DiffResult::Right(r) => println!("{}", format!("+ {r}").green()),
            DiffResult::Both(l, _) => println!("  {l}"),
        }
    }
}
