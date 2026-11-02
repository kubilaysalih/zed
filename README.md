> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed — local AI chats and multi-repository workspaces

This fork extends Zed with a shared home for local **Codex and Claude Code conversations**, multi-repository Source Control, and configurable workspace layouts. It uses Zed's existing Rust and native GPUI architecture.

The features below describe changes in this fork. The original Zed README follows afterward.

## Local Codex and Claude chats

- Browse local histories across folders in a native **Chats** panel, with provider icons, search, provider filters, pins, and Active / Archived / Trash / Background views. Background sessions are hidden by default.
- Start a new conversation, resume an existing one, or **Continue in a copy**. Replies stream directly into the chat; permission requests and questions have explicit response controls. Sessions owned by another application are not forcibly taken over.
- Rename, pin, archive, restore, and move chats to manually created projects. Trash is reversible. These organization changes are stored locally in this fork; they do not rename, archive, or delete chats in the original applications.
- Group chats by date or by your own projects. Each project supports **Newest first** or **Custom · drag to reorder** sorting. Project-grouped rows omit the redundant project subtitle.
- Existing conversations keep their provider fixed. Model and effort can be selected for the next turn using the options reported by the installed provider.
- Attach images and local files using the attachment button, drag and drop, or clipboard paste. Images use provider image inputs; other files are referenced by their local paths and remain subject to the agent's normal permissions.
- View Markdown, tables, and supported local or embedded images, including images in Claude tool results. Click an image to open the preview, zoom with the wheel or buttons, drag to pan, and use **Fit** to reset. Zoom has no fixed UI maximum.
- Public progress updates and tool results appear in compact expandable activity groups. Background task notifications are rendered as activity rather than raw XML. Context compaction has its own aligned progress/completion row.
- Conversations open at the latest messages; scrolling upward loads older history automatically while preserving the reading position. Image thumbnails, centered loading states, message-hover copy controls, and a growing composer keep the conversation readable.
- Running chats show spinners in the sidebar and chat tabs. Completed, unread replies show persistent dots. The latest public activity is shown above the composer.
- Context and account-quota indicators share the model/effort row. They use reported values rather than guessed limits; Claude account quota is currently unavailable through this connection.
- Turn completion notifications on or off, choose native system notifications or Zed popups, and configure sound and auto-dismiss. Native notification presentation also follows the operating system's settings.

**Requirements and scope:** Codex uses its local App Server; Claude uses the official CLI's streaming JSON transport. Sending messages requires the relevant installed, signed-in client. Chats checks for those clients when opened and provides setup links when missing; saved history remains readable without them. This is a local coding-chat integration, not synchronization with ChatGPT or Claude web Projects. A forked conversation shares earlier context, but subsequent messages are independent.

## General view and persistent projects

- Choose **General** from the title-bar project picker to manage chats without an active code project. The Files / Git / Search activity bar is hidden in this mode.
- Add local projects to the General workspace and return to individual projects for their files and Git state. Already-open chat views are retained when switching, avoiding unnecessary history reloads.
- General saves its project list, chat tabs, pane arrangement, drafts, selected pane, and sidebar width for reopening the app.
- Create your own chat projects and add existing Codex or Claude conversations through the project picker. **Workspace projects** uses your manual project assignments; provider working directories are separate, optional context and do not decide membership.
- Chat-to-project assignments and per-folder project selections are stored in the profile's shared `chat-library.json`. Opening a folder separately preserves those assignments, so projects do not need to be linked again.
- Pending **Trust and Continue** prompts remain visible when General becomes active, including prompts from projects restored later. Trust or Restricted Mode still applies to the original project; switching views never grants trust automatically.

## Chat tabs and panes

- Choose **Editor tabs** or **Separate chat panel** in Chats settings. The separate panel can sit beside the code editor, with the file tree on the left and independent code-editor splits in the center.
- Open multiple chats as distinct tabs with editor-style appearance. Drag tabs to reorder or move between chat panes, split right/down, resize dividers, and zoom a pane.
- Move the chat panel between supported dock positions. Chat-only layouts fill the available area when the window resizes, while the left sidebar remains accessible and resizable.
- The duplicate conversation header is removed. The project-directory shortcut and conversation options are available in the composer.

## Multi-repository Source Control

- Repositories under an opened folder appear in separate collapsible sections, each with its own Changes / History view, staging controls, and commit draft.
- Repository sections use distinct backgrounds and readable headers with compact spacing. Duplicate entries are removed, and changed-file totals include nested folders and repositories in the Git activity badge.
- Click a repository's branch name to switch branches for that repository. Changes lists grow with their contents and scroll through the panel rather than being clipped to a small fixed height.
- In **View Options → Open Changed Files**, choose **All Changes in One Tab**, **Separate File Diffs (Side by Side)**, or **Open File Without Diff**. Preferences persist; the combined diff view remains the default.
- Right-click a folder and choose **Hide Folder** to hide it and nested repositories from both Files and Git. Use **Hidden Folders** or **Show All Hidden Folders** to restore them. This does not delete files or modify `.gitignore`.
- Invalid or empty `.git` directories are no longer presented as ownership/trust failures. Actual Git ownership and project-trust checks remain in place.

## Navigation and appearance

- Files, Git, Search, and Chats use a vertical activity bar on the left. **Project Search** opens in the left dock with streamed results grouped by file; clicking a result opens the matching file and range in the editor. Case, whole-word, regex, ignored-file, and include/exclude controls are available.
- A single-folder file tree shows the folder's contents directly. The title bar shows the project name before its branch, with readable text and icons on colored backgrounds.
- The existing Threads sidebar opens on demand instead of automatically: **Ctrl+Alt+J** or **Ctrl+Shift+P → Toggle Workspace Sidebar**. Its visibility is restored. Zed's existing Agent panel remains available through **View → Agent Panel**.
- Open **Ctrl+Shift+P → Folder Color** for a visual picker with live preview, hue, presets, and HEX input. **Apply** saves, **Cancel** restores the previous color, and **Theme** resets to theme colors. The title, activity, and status bars use the selected color. Color changes work in Restricted Mode without granting trust.
- Open **Ctrl+Shift+P → Pane Layout** and choose **Flat** or **Separated**. Separated adds rounded panel boundaries and folder-colored spacing around the file panel, editor, bottom terminal, and right panel. Flat remains the default.
- Activity buttons have square corners and no gaps. Hover states, branch labels, diagnostic icons, and status-bar separators have been adjusted for colored bars. Resize handles remain usable with separated panes and chat-only layouts.

## Windows builds

Custom Windows builds are distributed separately as a signed **Zed Setup** executable, with Start menu integration and **Open with Zed** Explorer integration. The installer keeps application settings separate from its binaries; personal chat histories and profiles are not bundled for distribution. The custom signing certificate is not Zed Industries' certificate.

For a local optimized build, first follow the [Windows development prerequisites](./docs/src/development/windows.md), then run:

```powershell
cargo build --profile release-fast -p zed --bin zed -p cli --bin cli --locked
```

The resulting executables are in `target/release-fast`. A standalone distribution also needs the Windows terminal support files and license notices; an EXE alone is not the full installer.

The download links in the original README below point to **upstream Zed**, not this fork.

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
