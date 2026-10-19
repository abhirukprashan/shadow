// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! CLI command implementations.

use crate::cli::app::Commands;
use crate::engine::heartbeat::Heartbeat;
use crate::engine::workspace::Engine;
use crate::storage::models::{Workspace, WorkspaceStatus};
use anyhow::{Context, Result};
use colored::*;
use rayon::prelude::*;
use std::env;
use std::io::IsTerminal;
use std::path::Path;

/// Dispatch a parsed CLI command.
pub fn handle_command(cmd: Commands) -> Result<()> {
    let cwd = env::current_dir()?;
    // Only `init`/`new` may bootstrap an engine layout in the current
    // directory; every other command must find an existing project, so a
    // stray invocation from e.g. `/tmp` can no longer litter a fresh
    // `.shadow/` tree into an unrelated directory.
    let engine = match &cmd {
        Commands::Init | Commands::New { .. } => Engine::open_discovered(&cwd)?,
        _ => {
            let root = Engine::discover(&cwd).ok_or_else(|| {
                anyhow::anyhow!(
                    "not inside a shadow project (no .shadow metadata found in this or any \
                     parent directory); run `shadow init` first"
                )
            })?;
            Engine::open(&root)?
        }
    };

    match cmd {
        Commands::Init => cmd_init(&engine),
        Commands::New { name, parent } => cmd_new(&engine, &name, parent.as_deref()),
        Commands::Checkout { name } => cmd_checkout(&engine, &name),
        Commands::Path { name } => cmd_path(&engine, &name),
        Commands::Touch { name } => cmd_touch(&engine, &name),
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

    if crate::co_w::fs_utils::is_problematic_fs(fs_name) {
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
    let lineage = describe_lineage(&engine.list().unwrap_or_default(), &ws);
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
    let ws_path = Path::new(&ws.path);

    // Non-interactive contexts (scripts, CI, AI agents) cannot use an
    // interactive subshell: print the path so callers can `cd` themselves,
    // and keep the workspace heartbeat alive while they work.
    if !std::io::stdin().is_terminal() {
        engine.touch_workspace(&ws)?;
        println!("{}", ws.path);
        eprintln!(
            "{} non-interactive checkout: workspace path printed; \
             heartbeats refresh while you work via `shadow touch '{name}'`",
            "!".yellow(),
        );
        return Ok(());
    }

    let shell = shell_path();
    println!(
        "{} entering workspace '{}' ({}) — type 'exit' to return to the project root",
        "➜".cyan(),
        name,
        ws_path.display()
    );
    // Keep the heartbeat fresh for the whole subshell session so long-lived
    // checkouts never look abandoned.
    let _guard = Heartbeat::new(ws_path).start_refresher();
    let status = checkout_command(&shell, ws_path)
        .status()
        .with_context(|| format!("failed to spawn shell '{shell}'"))?;
    if !status.success() {
        eprintln!("{} shell exited with status {status}", "!".yellow());
    }
    Ok(())
}

fn cmd_path(engine: &Engine, name: &str) -> Result<()> {
    let ws = engine.find(name)?;
    // Acting as a keep-alive: every path resolution refreshes the heartbeat,
    // so scripts/agents that poll `shadow path` keep the workspace active.
    engine.touch_workspace(&ws)?;
    println!("{}", ws.path);
    Ok(())
}

fn cmd_touch(engine: &Engine, name: &str) -> Result<()> {
    engine.touch(name)?;
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

    // Delta computation hashes every workspace tree: run them in parallel.
    // Merged workspaces have no directory left, so they skip the walk and
    // report no delta.
    let rows: Vec<(Workspace, bool, Option<usize>)> = workspaces
        .par_iter()
        .map(|ws| {
            if ws.status == WorkspaceStatus::Merged {
                return (ws.clone(), false, None);
            }
            let alive = engine.is_alive(ws);
            let delta = match (engine.baseline(ws), engine.workspace_manifest(ws)) {
                (Ok(base), Ok(cur)) => crate::engine::snapshot::count_changes(&base, &cur),
                _ => 0,
            };
            (ws.clone(), alive, Some(delta))
        })
        .collect();

    println!(
        "{:<16} {:<12} {:<10} {:<10} {}",
        "NAME".bold(),
        "STATUS".bold(),
        "AGE".bold(),
        "DELTA".bold(),
        "LINEAGE".bold()
    );
    let all = &workspaces;
    for (ws, alive, delta) in rows {
        let status = match ws.status {
            WorkspaceStatus::Merged => "merged".blue().to_string(),
            _ if alive => "active".green().to_string(),
            _ => "abandoned".yellow().to_string(),
        };
        let age = humanize_age(ws.created_at);
        let lineage = describe_lineage(all, &ws);
        let delta = delta.map_or_else(|| "-".to_string(), |d| format!("{d} files"));
        println!(
            "{:<16} {:<12} {:<10} {:<10} {}",
            ws.name, status, age, delta, lineage
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
            // Surface read errors instead of silently rendering an empty file.
            let old = std::fs::read_to_string(root.join(&change.path))
                .with_context(|| format!("reading root copy of '{}'", change.path))?;
            let new = std::fs::read_to_string(ws_path.join(&change.path))
                .with_context(|| format!("reading workspace copy of '{}'", change.path))?;
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

    // Refresh the heartbeat for the entire command runtime, however long it
    // runs, so the workspace never flips to "abandoned" mid-build.
    let _guard = Heartbeat::new(ws_path).start_refresher();

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

/// Render a workspace's ancestry as `main -> parent -> ... -> name`.
///
/// Takes the pre-loaded workspace list so callers looping over many workspaces
/// don't trigger a full metadata scan per row.
fn describe_lineage(all: &[Workspace], ws: &Workspace) -> String {
    let mut chain = vec![ws.name.clone()];
    let mut cursor = ws.parent_id;
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

/// Print a colorized, git-style line diff with 3 lines of context.
///
/// Unchanged runs longer than the context window collapse into a `...`
/// marker instead of flooding the terminal.
fn print_line_diff(old: &str, new: &str) {
    use diff::Result as DiffResult;
    let items = diff::lines(old, new);
    let n = items.len();

    // Mark which lines to display: every changed line plus 3 of context.
    let mut show = vec![false; n];
    for (i, item) in items.iter().enumerate() {
        if !matches!(item, DiffResult::Both(..)) {
            let lo = i.saturating_sub(3);
            let hi = (i + 4).min(n);
            show[lo..hi].fill(true);
        }
    }

    let mut last_shown: Option<usize> = None;
    for (i, item) in items.iter().enumerate() {
        if !show[i] {
            continue;
        }
        if matches!(last_shown, Some(l) if i > l + 1) {
            println!("  {}", "...".dimmed());
        }
        last_shown = Some(i);
        match item {
            DiffResult::Left(l) => println!("{}", format!("- {l}").red()),
            DiffResult::Right(r) => println!("{}", format!("+ {r}").green()),
            DiffResult::Both(l, _) => println!("  {l}"),
        }
    }
}
