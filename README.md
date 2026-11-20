# Cleanup Mac

Finds what a developer Mac can give back (caches, build output, tool cleanups) and says why each item is listed
and what losing it costs. A desktop window and a CLI in one binary.

## What it looks for

| Category | Found by |
|---|---|
| Caches | folders tagged with [`CACHEDIR.TAG`](https://bford.info/cachedir/), `~/Library/Caches/*` (not `com.apple.*`), `~/.cache/*`, and cache rules (npm, bun, pnpm, Cargo, Go, Gradle, Xcode…) |
| Build output | artifact rules: a folder name next to a project marker (`node_modules` next to `package.json`, `target` next to `Cargo.toml`, …), plus Cargo's shared `build.build-dir` |
| Tool cleanups | commands the tool runs itself (`brew cleanup`, `docker builder prune`, `xcrun simctl delete unavailable`, …), shown only when the tool is installed |
| Large & old | hidden tool folders and Application Support folders over 1 GB, Downloads untouched for 30 days: listed for you to judge, never selected |

Every item has a safety level:

- **Safe**: rebuilt or re-downloaded on demand. Selected from the start; build output only once unused for 7 days (change it in the window or with `--days`).
- **Review**: rebuildable, but at a cost, or it may belong to an installed tool. Build output outside a git project, and tagged caches outside cache folders and git projects, land here.
- **Manual**: you decide. Never selected, and *Select all* skips it.

Build output inside another dependency folder (`node_modules`, `site-packages`) is skipped: it belongs to a package, not to you.

## Safety

- Items go to the Trash by default, so Finder's *Put Back* works. Permanent delete is a per-item choice (`--delete` in the CLI).
- Before every action the path is checked again: absolute, inside your home or Cargo's build dir, not a symlink, and not a protected folder (Documents, Desktop, `.ssh`, Keychains, Mail, …) or one of their parents.
- Every action is logged to `~/Library/Application Support/Cleanup Mac/actions.log`.

## Your own rules

What counts as cleanable is data: [`rules/default.toml`](rules/default.toml) is built in, and
`~/Library/Application Support/Cleanup Mac/rules.toml` adds to it. A rule with the same `id` (or `dir` for build
output) replaces the built-in one; `enabled = false` turns it off.

```toml
[[cache]]
id = "my-tool"
name = "My tool's downloads"
paths = ["~/.my-tool/downloads"]
safety = "safe"
note = "Downloaded again on next use."

[[artifact]]
dir = "dist"
markers = ["package.json"]
safety = "review"

[[cache]]
id = "maven"
enabled = false
```

`cleanup-mac --rules` prints the file's location and how many rules are in effect.

## CLI

```
cleanup-mac                     open the window
cleanup-mac --plan              list what would be cleaned and why; change nothing
cleanup-mac --clean [-y] [--delete] [--days N]
cleanup-mac --sweep-rust-build  delete Cargo shared build dirs unused 3+ days or over 50 GiB (for a LaunchAgent)
cleanup-mac --rules
```

## Install

Download the latest `Cleanup-Mac-<version>.zip` from [Releases](https://github.com/nguyenvanduocit/cleanup-mac/releases)
(universal, signed and notarized), unzip it, and move **Cleanup Mac** to Applications.

### From source

Needs the Rust toolchain in `rust-toolchain.toml`.

```sh
SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" scripts/install-app.sh
```

This builds, bundles and signs `/Applications/Cleanup Mac.app`, and links `~/.cargo/bin/cleanup-mac` to its binary.
`scripts/release.sh` builds the universal app, notarizes it and publishes a GitHub release (needs `SIGN_IDENTITY`
and a `NOTARY_PROFILE` saved with `xcrun notarytool store-credentials`).

Grant the app Full Disk Access (the window has a button for it); without it macOS hides parts of `~/Library` and
the scan comes up short. Signing with a stable identity keeps that grant across reinstalls.

## Code

| File | Does |
|---|---|
| `src/model.rs` | item types, safety, the path guard (pure) |
| `src/rules.rs` | loading and merging rules |
| `src/scan.rs` | finding and classifying items |
| `src/session.rs` | window state: items, selection, run progress (pure) |
| `src/execute.rs` | running the chosen actions, logging |
| `src/fsops.rs` | filesystem and process edge |
| `src/app.rs` | the window ([shadcn-rs](https://github.com/nguyenvanduocit/shadcn-rs)) |
| `src/main.rs` | CLI |
