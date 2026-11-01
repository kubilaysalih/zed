> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

## Changes in this fork

This fork builds on Zed's existing Rust and native GPUI architecture, with a focus on file browsing and working across multiple Git repositories.

1. **Activity bar and file tree:** Files, Git, and Search controls have been moved into a vertical activity bar on the left. The Threads sidebar and the Agent panel's bottom-left button have been removed. Activity buttons are contiguous with square corners, and overlapping hover backgrounds have been fixed. The redundant project name has been removed from the title bar. When a single folder is open, the file tree shows its contents directly without an extra root row.

2. **Source Control for multiple repositories:** Git repositories under the opened folder appear in separate collapsible sections. Each section keeps its changes, history, staging operations, and commit draft tied to its repository. Repository headers and section backgrounds are more distinct, and the active Changes / History tab is clearer. Unnecessary card borders and large gaps have been removed, while rows have more vertical padding. Duplicate repository entries have been fixed. Sections grow with their file lists, and long lists can be scrolled through the panel. Clicking a repository's branch name opens the branch selector for that repository. Invalid or empty `.git` directories are no longer misreported as ownership or trust errors; actual Git trust checks remain in place.

3. **Hide and restore folders:** Right-click a folder in the file tree and select **Hide Folder** to hide it and its nested repositories from both the file tree and Git panel. Restore folders individually through **Hidden Folders**, or restore them together with **Show All Hidden Folders**. Visibility choices persist without deleting files or changing `.gitignore`.

4. **Folder colors with live preview:** Open **Ctrl+Shift+P → Folder Color** to use the visual color picker. Choose a color using the color field, hue strip, presets, or a HEX value such as `#FF6600`. Changes preview immediately; **Apply** saves them, **Cancel** restores the previous color, and **Theme** returns to the theme's colors. The chosen color is applied to the title bar, activity bar, and status bar. Color saving and application in Restricted Mode have been fixed; choosing a color does not grant project trust.

5. **Consistent hover states and readability:** Shared hover, active, and selected states use neutral colors. Menu controls, branch labels, and icons are more readable on colored bars. The unnecessary vertical border beside the activity bar has been removed. When a folder color is set, separators between the bottom-right panel button groups use translucent white in dark themes and translucent black in light themes.

6. **Optional pane and diff layouts:** Open **Ctrl+Shift+P → Pane Layout** to choose **Flat** or **Separated**. Separated gives the left panel, editor, bottom terminal, and right panel rounded boundaries, with the folder color visible between them and along the editor's right edge. The Git panel's **View Options → Open Changed Files** menu exposes three choices: **All Changes in One Tab**, **Separate File Diffs (Side by Side)**, and **Open File Without Diff**. File clicks can open the combined changes view, a separate side-by-side diff tab for the selected file, or the file itself. These preferences persist; the defaults remain Flat and the combined diff view.

The download links below point to upstream Zed. Custom Windows builds of this fork are distributed separately.

---

# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
