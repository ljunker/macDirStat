# macDirStat

`macDirStat` is a fast terminal interface for finding large files and directories
on macOS. Directory sizes are calculated in parallel in the background while
navigation, search, and filtering remain responsive.

## Installation

Rust 1.85 or newer is required.

```bash
cargo install --path . --locked
```

Afterwards, the program can be launched using its exact name:

```bash
macDirStat
macDirStat ~/Library
macDirStat --workers 4 --size-mode physical ~/Library
```

To run it directly from the source directory:

```bash
cargo run --release -- ~/Library
```

### Prebuilt macOS binaries

GitHub Releases contain separate archives for Apple Silicon (`arm64`) and
Intel Macs (`x86_64`). After downloading, the appropriate archive can be
installed like this, for example:

```bash
tar -xzf macDirStat-v0.3.1-macos-arm64.tar.gz
mkdir -p ~/.local/bin
install -m 755 macDirStat-v0.3.1-macos-arm64/macDirStat ~/.local/bin/macDirStat
```

## Usage

Press `?` at any time to open the full scrollable help view.

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab`, `1`…`5` | Switch between Tree, Largest, Types, Duplicates, and Changes |
| `↑` / `↓`, `k` / `j` | Move the selection |
| `→` / `l` | Open a directory |
| `←` / `h` | Close a directory or jump to the parent node |
| `Enter` | Expand/collapse a directory or reveal an analysis path in the Tree view |
| `g` / `G` | Jump to the first or last visible entry |
| `Home` / `End`, `PgUp` / `PgDn` | Navigate directly or one page at a time |
| `Backspace` | Open the parent directory as the new scan root |
| `/`, `n` / `N` | Search loaded nodes and move between matches |
| `f` / `F` | Set or clear the combined filter |
| `s` / `S` | Change the sort mode or reverse the sort direction |
| `z` | Switch between logical and physical size |
| `i` | Toggle the adaptive detail panel |
| `t` | Change the theme |
| `m` | Toggle mouse controls |
| `w` | Toggle the native file system watcher |
| `e` | Export the full analysis index as JSON |
| `Esc` | Cancel running scan jobs |
| `Space` | Mark or unmark an entry for a multi-item action |
| `x` | Move all marked entries to the Trash together |
| `d` | Move the selected entry to the Trash after confirmation |
| `r` | Fully rescan the root without using the cache |
| `q` / `Ctrl+C` | Quit |

The regular Tree view stays fast by scanning directories on demand. The first
time an analysis tab is opened, macDirStat builds a complete index once in the
background. `Largest` shows individual files, `Types` groups by category,
extension, and age, `Duplicates` verifies candidates exactly using BLAKE3, and
`Changes` compares the index with the most recent complete run. Hard links are
treated as aliases of the same file during duplicate detection; empty files are
not considered duplicate candidates.

Filter predicates are combined with AND. In addition to names and quoted
phrases, supported examples include `>1GiB`, `size>500MB`, `age>30d`,
`ext:log`, `ext:none`, and `type:image`. Categories are `image`, `video`,
`audio`, `archive`, `document`, `code`, and `other`. Search and filtering in the
Tree view operate only on loaded nodes; analysis tabs filter the index that has
already been built. On wide terminals, the detail panel appears on the right;
on narrower terminals, it appears below the list.

Mouse controls are disabled by default. After pressing `m`, a click selects a
row, clicking the checkbox marks it, double-clicking toggles a directory, and
the mouse wheel moves the selection.

## Sizes, mount points, and cache

A scan records all of the following at the same time:

- logical size based on file length,
- physical size based on allocated file system blocks,
- number of regular files per directory.

Symbolic links are displayed and their own metadata is counted, but they are
never followed. Hard links are counted once per visible path entry; there is no
global inode deduplication.

By default, a scan stays on the file system of the starting path. Volumes mounted
below it remain visible as mount points, but are not traversed. Use
`--cross-filesystems` to explicitly disable this behavior.

Completed directory values are cached for 24 hours by default in
`~/Library/Caches/macDirStat/scan-cache-v1.json`. Cache hits are marked with `≈`
in the Tree view and as `cached` in the detail panel. Validation uses the path,
device, inode, and modification time; deep content changes may therefore remain
stale until the cache entry expires. `r` discards the cache for the current root
and forces a fresh scan. After Trash operations, only removed paths are
invalidated and the affected ancestors are rescanned.

The native watcher enabled with `w` or `--watch` uses FSEvents on macOS. Events
are collected for 750 ms by default; only affected loaded subtrees and their
ancestors are refreshed. If an overflow occurs or more than 1,000 events are
received, exactly one root refresh is performed. There is no periodic background
scan.

Complete analyses write a rolling comparison by default under
`~/Library/Application Support/macDirStat/snapshots/`. The key contains the
lossless root path and the mount policy. An incomplete or cancelled run does not
replace the baseline; inaccessible subtrees are not incorrectly treated as
removed. `--no-snapshots` disables this behavior.

## Configuration

The optional configuration file is located on macOS at:

```text
~/Library/Application Support/macDirStat/config.toml
```

Example:

```toml
workers = 6
size_mode = "logical"
one_file_system = true
cache = true
cache_ttl_hours = 24
mouse = false
theme = "default"
detail_panel = true
sort = "size"
sort_direction = "descending"
watch = false
watch_debounce_ms = 750
snapshots = true
```

Available themes are `default`, `monochrome`, and `high-contrast`. Sorting can be
performed by `size`, `name`, `files`, or `kind`, in either `ascending` or
`descending` order.

The precedence is: built-in defaults, TOML file, CLI arguments. A different
configuration path can be selected with `--config PATH`. To see all options, run:

```bash
macDirStat --help
```

Available options include `--one-file-system`/`--cross-filesystems`,
`--cache`/`--no-cache`, `--mouse`/`--no-mouse`,
`--details`/`--no-details`, `--watch`/`--no-watch`,
`--snapshots`/`--no-snapshots`, `--theme`, `--sort`, and `--sort-direction`.

A complete JSON export without the TUI is also available. Diagnostics and the
short statistics summary are written to stderr so that stdout remains valid JSON
when `-` is used:

```bash
macDirStat --export-json scan.json ~/Library
macDirStat --export-json - --detect-duplicates ~/Library >scan.json
macDirStat --export-json changes.json --compare-snapshot scan.json ~/Library
```

The schema contains files, directories, both size values, file and age
statistics, errors, optional duplicate groups, and changes. Paths are stored both
in readable form and losslessly as Base64-encoded OS bytes.

## Deletion and safety

Single-item and multi-item deletions are confirmed exclusively with `y`. `n` and
`Esc` cancel the dialog. When a directory is marked, `macDirStat` removes any
existing marks on its children so that no path is deleted twice. All paths in a
multi-selection are validated again before the first entry is moved to the Trash.

- Deletion is not permanent: `macDirStat` uses the macOS Trash.
- The scan root itself cannot be deleted.
- Symbolic links are followed neither during scanning nor during deletion validation.
- Permission errors and other I/O errors do not stop the rest of the scan.
- Protected locations may require Full Disk Access for the terminal application
  being used. `sudo` is not required and is not used automatically.
- File names do not need to be valid UTF-8; cache paths are stored losslessly.

## Development

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
```

## Creating a release

The [`.github/workflows/release.yml`](.github/workflows/release.yml) workflow
automatically creates a GitHub Release with native macOS binaries and SHA-256
checksums whenever a version tag is pushed. The tag must follow the format
`vMAJOR.MINOR.PATCH` and exactly match the leading version in [`VERSION`](VERSION).

For a new version, first update `VERSION` and the copy required by Cargo in
`Cargo.toml`. The build script prevents builds when the values differ. Then update
`Cargo.lock` with a normal Cargo command and run the checks.
