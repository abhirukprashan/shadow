# Shadow VCS

A high-performance, OS-native version control and workspace orchestration engine built on filesystem copy-on-write (CoW) reflinks. Shadow provides isolated development workspaces that share filesystem blocks, fast branching, checkpointing, and atomic merges.

## Features

- **Copy-on-Write Workspaces**: Creates isolated workspaces using `FICLONE` reflinks on btrfs/XFS (falls back to copies when unsupported)
- **Fast Creation**: Near-instant workspace creation by sharing unchanged files
- **Lineage Tracking**: Parent-child relationships between workspaces
- **Three-way Merging**: Diff3-based merging with Git-compatible conflict markers
- **Integration Merges**: Merge two workspaces into a new combined workspace
- **Atomic Promotions**: Merge workspace changes back to root atomically
- **Heartbeat Monitoring**: Detects abandoned/stale workspaces
- **Checkpoint System**: Commit snapshots with history tracking

## Requirements

- Linux with `clone3`/`FICLONE` support for native CoW (recommended: btrfs, XFS)
- Rust 1.70+ (stable)

## Installation

### Build from source

```bash
cd shadow
cargo build --release
cp target/release/shadow ~/.local/bin/
```

Or using Makefile:

```bash
make build
make install
```

## Usage

### Initialize in a project

```bash
cd /path/to/project
shadow init
```

### Create a workspace

```bash
shadow new feature-branch
# or with a parent
shadow new child --parent feature-branch
```

### Work in isolation

```bash
shadow exec feature-branch -- make test
shadow exec feature-branch -- cargo build
```

### Check status and differences

```bash
shadow status
shadow diff feature-branch
```

### Create checkpoints

```bash
# From within a workspace directory or project root
shadow commit -m "Implemented feature X"
```

### View history

```bash
shadow log
```

### Merge changes

**Promote workspace to root (squash/merge back):**
```bash
shadow merge feature-branch
```

**Integrate two workspaces into a new one:**
```bash
shadow merge ws1 ws2 --out combined
```

### Switch/enter a workspace (spawns a subshell)

```bash
shadow checkout feature-branch
```

### Undo last operation

```bash
shadow undo
```

### Drop a workspace

```bash
shadow drop feature-branch
```

## Architecture

- **`co_w/`**: Low-level reflink/copy operations and filesystem detection
- **`engine/`**: Workspace lifecycle, discovery, heartbeat, snapshot/diff
- **`storage/`**: Embedded redb metadata store with retry logic
- **`merger/`**: Diff3 merging, conflict handling, atomic application
- **`cli/`**: Command-line interface (clap)

## Workspace Layout

- `.shadow/` - Engine metadata (redb database)
- `.shadow-workspaces/` - Isolated workspace directories
- `.shadow-staging/` - Staging area for atomic merges
- `.shadow-artifacts/` - Per-workspace build caches (isolated)

## Conflict Resolution

When merge conflicts occur, Shadow:
1. Aborts the merge preserving both sources and target
2. Writes Git-compatible conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) to staged files
3. Generates `.shadow/conflicts.json` with machine-readable conflict metadata
4. Suggests editor commands (e.g., `code --goto file:line`) when in a TTY

Resolve conflicts in the staged files, then re-attempt the merge.

## Testing

```bash
cargo test
cargo test --test integration
```

## License

See repository for license details.
