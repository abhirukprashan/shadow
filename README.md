# Shadow

version control and workspace orchestration engine built for human developers and autonomous AI agents.

Shadow is an ultra-fast version control engine leveraging operating system Copy-on-Write (CoW) reflinks (`FICLONE` `ioctl` on Linux) to spawn isolated development sandboxes in sub-milliseconds with zero initial disk footprint.

---

## Key Features

- **Copy-on-Write Workspaces**: Instant physical directories via kernel reflink cloning (`FICLONE`). Storage starts at ~0 bytes — files share disk extents with the project until you edit them, then only the modified extents are copied. On filesystems without reflink support, shadow transparently falls back to full byte copies.
- **Selective Cloning**: Engine state, `.git`, `target/`, and `node_modules/` are never copied into workspaces; builds and dependencies stay isolated per workspace. `shadow new` warns before cloning very large trees (> 1 GiB or > 50k tracked files), where copy-fallback filesystems would pay the full cost.
- **Interactive Subshell Checkout**: `shadow checkout <name>` spawns an interactive shell locked inside the workspace directory. Type `exit` to return to the root.
- **Dual Merge Architecture**:
  - **Root Promotion**: Squashed HEAD promotion back to project root `main` via atomic staging swaps (`std::fs::rename`).
  - **Integration Merges**: Combine two finished agent workspaces into a brand-new, isolated integration sandbox (`shadow merge ws1 ws2 --out combined`).
- **Surgical Conflict Engine**:
  - Automatic `diff3` hunk merging for non-overlapping changes.
  - Injects standard Git-compatible conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) for native IDE UI support.
  - Outputs machine-readable `.shadow/conflicts.json` manifests for headless AI agent parsing.
  - Provides TTY line-targeting to jump directly to conflicting lines in your editor (e.g., `code --goto file:line`).
- **Liveness Heartbeats**: `.shadow-lock` timestamp tracking eliminates raw PID-reuse risks when pruning abandoned workspaces. Long-running `checkout`/`exec` sessions keep heartbeats fresh automatically; `shadow touch`/`shadow path` provide keep-alives for agents working in the directory by other means.
- **Embedded Metadata**: Backed by `redb` for atomic transactions and concurrent thread safety.

---

## System Requirements

- **Operating System**: 
  - **Linux**: Kernel 4.5+ with `FICLONE` support (recommended: Btrfs, XFS).
- **Rust Toolchain**: Rust 1.70+ (stable).

---

## Installation

### Build from Source (Cargo)

```bash
git clone https://github.com/your-username/shadow.git
cd shadow
cargo build --release
cp target/release/shadow ~/.local/bin/
```

### Build via Makefile

```bash
make build
make install
```

---

## Quick Start Guide

### 1. Initialize Project
```bash
cd /path/to/project
shadow init
```

### 2. Create & Enter Workspaces
```bash
# Spawn a new CoW workspace
shadow new feature-auth

# Enter interactive subshell
shadow checkout feature-auth
# (Run commands, build code, type 'exit' to return to project root)
```

### 3. Isolated Execution & Checkpoints
```bash
# Run isolated commands directly from project root
shadow exec feature-auth -- cargo test

# Record a checkpoint snapshot
shadow commit -m "Implemented feature X"
```

### 4. Merge Workflows
```bash
# Promote workspace back to root main
shadow merge feature-auth

# Combine two workspaces into a new integration sandbox
shadow merge ws1 ws2 --out combined
```

---

## CLI Reference

| Command | Description |
| :--- | :--- |
| `shadow init` | Initialize `.shadow/` metadata store and verify filesystem support. |
| `shadow new <name>` | Create a new CoW workspace sandbox (optional `--parent <ws>`). Warns when the project tree is large (> 1 GiB or > 50k files), since every workspace clones all of it. |
| `shadow checkout <name>` | Spawn an interactive terminal subshell inside the workspace directory. In non-interactive contexts (scripts/agents) prints the workspace path instead. |
| `shadow path <name>` | Print the workspace directory path; also refreshes its heartbeat (keep-alive). Ideal for `cd "$(shadow path <name>)"`. |
| `shadow touch <name>` | Refresh a workspace's heartbeat without entering it. |
| `shadow exec <name> -- <cmd>` | Execute a non-interactive command inside the workspace sandbox. |
| `shadow status` | Display workspaces, ages, lineages, and modified file deltas (statuses: `active`, `abandoned`, `merged`). |
| `shadow diff <name>` | Perform a fast 3-way file comparison against root `main`. |
| `shadow commit -m "<msg>"` | Record an internal checkpoint snapshot in the timeline. |
| `shadow log` | View the internal commit timeline for the workspace. |
| `shadow merge <source>` | Promotes squashed HEAD state atomically back to project root. The workspace directory is reclaimed and its record stays listed as `merged`. |
| `shadow merge <s> <t> --out <o>` | Combines two workspaces into a new integration sandbox. |
| `shadow undo` | Roll back the last metadata transaction pointer. |
| `shadow drop <name>` | Clean up and delete a workspace directory and its metadata. |

---

## Disk Footprint

On reflink-capable filesystems (Btrfs, XFS), creating a workspace — including
its pristine merge-base snapshot — costs only a few kilobytes of metadata:

| | Real disk cost |
| :--- | :--- |
| Working tree | ~0 (extents shared with the project) |
| Baseline (merge-base) copy | ~0 (also reflinked) |
| Files you edit in the workspace | Only the modified extents (CoW) |

Two notes:

- `du` double-counts shared extents and will show the full logical size.
  For the *actual* cost on Btrfs, use `btrfs filesystem du -s <workspace>/*`.
- On filesystems without reflink support (ext4, tmpfs, overlayfs), shadow
  falls back to full copies — each workspace then costs roughly twice the
  tracked tree (working copy + baseline). `shadow init` reports which mode
  your filesystem uses, and `shadow new` warns for very large trees.

```bash
shadow new demo
btrfs filesystem du -s .shadow-workspaces/demo_*/   # Exclusive ≈ 0
shadow drop demo
```

---

## Project Architecture

```text
shadow/
├── src/
│   ├── co_w/         # Low-level reflink/copy operations and filesystem detection
│   ├── engine/       # Workspace lifecycle, discovery, heartbeats, snapshotting
│   ├── storage/      # Embedded redb metadata store with atomic transaction retry logic
│   ├── merger/       # Diff3 merging, conflict handling, atomic root staging swaps
│   └── cli/          # Clap-powered CLI interface and interactive subshell runner
├── Makefile          # Standard build, test, and installation targets
└── Cargo.toml        # Workspace dependencies & package metadata
```

---

## Workspace Layout

```text
my-project/
├── .shadow/             # Engine metadata (redb database & transaction log)
├── .shadow-workspaces/  # Isolated CoW physical workspace directories
├── .shadow-staging/     # Staging area for atomic root promotion swaps
└── .shadow-artifacts/   # Per-workspace isolated build/dependency caches
```

---

## Conflict Resolution Pipeline

When merge conflicts occur, Shadow:
1. **Aborts Merge**: Preserves both source and target environments to prevent corruption.
2. **Injects Markers**: Writes standard Git-compatible conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) into target files for native IDE plugin compatibility.
3. **Generates Manifest**: Writes `.shadow/conflicts.json` with machine-readable hunk line ranges for headless AI agents.
4. **TTY Editor Targets**: Suggests precise editor commands (e.g., `code --goto file:line`) when executed in an interactive terminal.

*Resolve conflicts in the staged files, then re-attempt the merge command.*

---

## Testing

```bash
# Run unit tests
cargo test

# Run integration tests
cargo test --test integration
```

---

## License

Licensed under the GNU General Public License v3.0 only (GPL-3.0-only). See `LICENSE` for details.

Copyright (c) 2026 Abhiruk Perera.
