> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed — local AI chats and multi-repository workspaces

This fork extends Zed with a shared home for local **Codex and Claude Code conversations**, multi-repository Source Control, and configurable workspace layouts. It uses Zed's existing Rust and native GPUI architecture.

The features below describe changes in this fork. The original Zed README follows afterward.

## Latest updates — custom Windows build 1.28.0.33

- **Agent Mode** in the current workspace, preserving the open folders and showing their names and paths. Switch back through the command palette without going through General.
- A shared compact sidebar in normal and Agent modes, with smaller date headings and separators. Project, agent, status, and grouping controls live in the top-right settings menu; filters use aligned native submenus.
- Drag chat tabs between separate chat panes and the editor's tab bar while retaining the open conversation and its running/unread indicators.
- Compact editor gutters with separate fold and AI hit areas, AI actions for hovered lines, and restored selection actions. Breakpoint indicators share the line-number area.
- Correct workspace colors on initial opening and mode changes, readable **Sign In** text, and consistent outer spacing in separated pane layouts.
- Improved background-task activity indicators, compact Claude continuation summaries, and local Markdown-link opening. Project Search results now include file icons, match highlights, counts, and a viewport scrollbar.

## Local Codex and Claude chats

- Browse local histories across folders in a native **Chats** panel, with provider icons, search, provider filters, pins, and Active / Archived / Trash / Background views. Background sessions are hidden by default.
- Start a new conversation, resume an existing one, or **Continue in a copy**. Replies stream directly into the chat; permission requests and questions have explicit response controls. Sessions owned by another application are not forcibly taken over.
- Questions support selectable answers, free text, and skipping. Permission cards show the reason, command, and working directory, with technical details collapsed. Codex connections respect the installed client's approval and sandbox configuration.
- Use **`/goal <objective>`** in Codex chats to start a goal, optionally with **`/goal --budget <tokens> <objective>`**. Status, pause, resume, and clear commands are supported; the goal indicator appears only when a goal exists.
- Rename, pin, archive, restore, and move chats to manually created projects. Trash is reversible. These organization changes are stored locally in this fork; they do not rename, archive, or delete chats in the original applications.
- Group chats by date, by your own projects, or **By status**: Pinned, In progress, Input required, Unread, and Ready. Each project supports **Newest first** or **Custom · drag to reorder** sorting. Project-grouped rows omit the redundant project subtitle.
- Open the filter/settings button at the top right of **Chats** or **Agent Mode** to choose **Project**, **Agent**, and **Status**. Each opens a native submenu; the current selection appears in its label. Under **View**, **Chats** groups by date, **Projects** groups by project, and **By status** groups by activity. Date headings use smaller muted text, shortened dates, and separators.
- Time-based ordering follows the latest human message, so streaming replies and background activity do not continually rearrange the sidebar.
- Existing conversations keep their provider fixed. Model and effort can be selected for the next turn using the options reported by the installed provider.
- Attach images and local files using the attachment button, drag and drop, or clipboard paste. Images use provider image inputs; other files are referenced by their local paths and remain subject to the agent's normal permissions.
- View Markdown, tables, and supported local or embedded images, including images in Claude tool results. Click an image to open the preview, zoom with the wheel or buttons, drag to pan, and use **Fit** to reset. Zoom has no fixed UI maximum.
- Local file attachments and Codex file citations appear as file cards. Citations stay in their original position alongside the message text. Click to open with the system's default application, or right-click to open in Zed, reveal in File Explorer, or copy the path or filename.
- Local file links in Markdown messages resolve relative to the conversation's working directory and open in the editor, including generated `.md` files.
- Public progress updates and tool results appear in compact expandable activity groups. Background task notifications are rendered as activity rather than raw XML. Context compaction has its own aligned progress/completion row.
- Plans appear as selectable-text checklists with pending, running, and completed steps. Claude subtasks can open their linked saved history separately. Automatic goal check-ins and internal continuation messages appear as activity; pasted-content wrappers are removed while preserving their contents.
- Fenced code blocks have syntax highlighting and a separate toolbar for copying, **Run in Terminal**, and transferring code to the terminal without submitting it. Terminal startup preserves the shell prompt position; multiline transfer requires bracketed-paste support. Temporary file-opening errors can be dismissed.
- Conversations open at the latest messages; scrolling upward loads older history automatically while preserving the reading position. Image thumbnails, centered loading states, message-hover copy controls, and a growing composer keep the conversation readable.
- Sent messages appear immediately. Failed sends retain their bubble with retry controls; writer conflicts also offer sending in a new copy. Streaming updates preserve Markdown views, and the activity indicator has reserved space above the composer to avoid shifting the conversation.
- Running chats show spinners in the sidebar and chat tabs, including tracked background Bash tasks and monitors. Completed, unread replies show persistent dots. The latest public activity is shown above the composer; internal Claude continuation summaries stay out of normal message bubbles.
- Context and account-quota details, refresh actions, and conversation options are available in the composer's **…** menu. Claude usage reports and rate-limit events are supported when exposed by the installed client. Missing limits remain unknown rather than being guessed.
- Chat text uses the existing **Agent Panel Font** settings: **UI Font Size** for agent replies and **Buffer Font Size** for user messages.
- Codex chats with saved schedules show a clock icon with cadence and status details and a shortcut to the schedule configuration. Execution remains the responsibility of the provider's scheduler.
- Turn completion notifications on or off, choose native system notifications or Zed popups, and configure sound and auto-dismiss. Native notification presentation also follows the operating system's settings.

**Requirements and scope:** Codex uses its local App Server; Claude uses the official CLI's streaming JSON transport. Sending messages requires the relevant installed, signed-in client. Chats checks for those clients when opened and provides setup links when missing; saved history remains readable without them. This is a local coding-chat integration, not synchronization with ChatGPT or Claude web Projects. A forked conversation shares earlier context, but subsequent messages are independent.

## Inline editing and Quick chats

- Enable **Agent → Use Local Chats for Editor** in Settings (`agent.use_local_chats_for_editor`) to use local Codex or Claude sessions for inline requests and **Add to Agent Thread**. Turn it off to use Zed's built-in agent integration.
- In the inline prompt, choose **Codex** or **Claude**, then **Ask** for an explanation or **Edit** for a selection replacement. Edits use the editor's existing diff, accept, reject, and undo controls. An answer based on a document that changed while waiting is rejected instead of overwriting newer edits.
- The last selected inline provider is remembered across workspaces and restarts. New inline prompts open with it already selected; sending does not require choosing a provider again.
- Hover a code line's gutter to reveal its AI action, or use **Ask AI Inline** in the editor context menu. With code selected, AI and available code-action controls appear near the selection. Hovering does not move the cursor or change the selection; clicking opens the inline request. Follow-up messages keep earlier questions and answers visible, with a spinner while waiting.
- Type **`@`** in an inline prompt to choose an existing chat. Results filter as you type by conversation title, project, and provider, including multiword searches. Sending includes the selected code and file/line reference in that chat; without a recipient, the inline request uses a Quick chat.
- Composer suggestions can be accepted with **Tab** or the **Tab to accept** button. **Ratata mode** in the conversation menu automatically sends available suggestions until paused or interrupted by an error or approval. It is off by default, does not overwrite drafts, and does not automatically approve requests.
- Inline requests are saved as **Quick chats**. Select **Quick chats** from the Chats status menu to browse them, see running work, or continue a conversation. Repeated requests in the same inline prompt retain the same provider session.
- Regular agents can discover and read Quick chats through `zed_chats.list_chats` and `zed_chats.read_chat`; the catalog identifies them as `quick_chat`. These histories show the question, file, and answer without the inline transport envelope.

## Collaboration between local chats

- Local Codex and Claude agents can discover conversations through `zed_chats.list_chats`, read them with `zed_chats.read_chat`, and consult them with `zed_chats.send_message`. No manual linking is required; a conversation can consult multiple recipients, and Zed opens a recipient when needed.
- Right-click a sidebar conversation and choose **Copy chat reference** to copy its title, provider, conversation identifiers, and working directory for another agent.
- Each recipient has its own message queue. Delivery waits until that chat is ready, preserving its existing writer and leaving drafts, pending questions, and permissions undisturbed. Repeated message IDs are deduplicated, and another application's writer lock is never forcibly bypassed.
- Peer messages display their source with a provider icon and a compact card instead of raw transport instructions. There is no fixed conversation-turn limit; agents send follow-ups when needed rather than automatically echoing every response.
- Use **Fork conversation** in the sidebar menu to continue with the same earlier context in an independent conversation. Chats settings also includes a **Codex ↔ Claude demo** that creates two new chats for a design discussion.

Collaboration is local to the running app. Pending deliveries are held in memory and are not restored after restarting Zed. Normal chat histories remain saved by their providers. Agents can also use `zed_chats.list_terminals` and `zed_chats.read_terminal` to inspect recent output from terminals in their chat's Zed window, including commands launched through code-block controls. These tools only read retained output; they do not execute commands or infer an exit code.

## General view and persistent projects

- Choose **General** from the title-bar project picker to manage chats without an active code project. The Files / Git / Search activity bar is hidden in this mode.
- Add local projects to the General workspace and return to individual projects for their files and Git state. Already-open chat views are retained when switching, avoiding unnecessary history reloads.
- General saves its project list, chat tabs, pane arrangement, drafts, selected pane, and sidebar width for reopening the app.
- Create your own chat projects and add existing Codex or Claude conversations through the project picker. **Workspace projects** uses your manual project assignments; provider working directories are separate, optional context and do not decide membership.
- Chat-to-project assignments and per-folder project selections are stored in the profile's shared `chat-library.json`. Opening a folder separately preserves those assignments, so projects do not need to be linked again.
- Pending **Trust and Continue** prompts remain visible when General becomes active, including prompts from projects restored later. Trust or Restricted Mode still applies to the original project; switching views never grants trust automatically.

## Agent Mode in the current workspace

- Open **Ctrl+Shift+P → Toggle Agent Mode** to switch the current project into a chat-focused layout. The Files / Git / Search activity bar is hidden, chats occupy the left sidebar, and conversation panes remain on the right.
- The sidebar displays the current folder names and paths. Entering Agent Mode starts with **Current folder** scope; the settings menu also offers all projects, workspace projects, individual chat projects, and agent working folders.
- Normal mode and Agent Mode share the same compact chat rows, date formatting, search, filter submenus, and grouping controls. Existing chat projects and project assignments remain available in either mode.
- Open generated Markdown and other files in editor panes while staying in Agent Mode. Multiple conversation tabs and independently split chat panes remain available.
- Use the same command to return to normal mode. The previous editor layout, panel visibility, and chat scope are restored, and the workspace color applies in both modes.

## Chat tabs and panes

- Choose **Editor tabs** or **Separate chat panel** in Chats settings. The separate panel can sit beside the code editor, with the file tree on the left and independent code-editor splits in the center.
- Open multiple chats as distinct tabs with editor-style appearance. Drag tabs to reorder or move between chat panes, split right/down, resize dividers, and zoom a pane.
- Drag a chat tab onto the editor's tab bar to place it alongside files, or drag it back into a chat pane. The same conversation view is moved, preserving its state; clicking it in the sidebar focuses its existing location.
- Splits can be nested independently, including two chats above one wider chat. Tab context menus provide closing controls, and the saved layout retains the split tree.
- Move the chat panel between supported dock positions. Chat-only layouts fill the available area when the window resizes, while the left sidebar remains accessible and resizable.
- The duplicate conversation header is removed. The project-directory shortcut and conversation options are available in the composer.

## Multi-repository Source Control

- Repositories under an opened folder appear in separate collapsible sections, each with its own changes, history, staging controls, and commit draft. The **History** icon sits beside the view-options button and **Stage All**; a back button returns to changes without a permanent tab row.
- Tree mode is the default, with file-type icons, folder disclosure arrows, compact rows, and sticky ancestor folders that track the scroll position. Explicit existing view preferences remain respected.
- Repository sections use distinct backgrounds and readable headers with compact spacing. Duplicate entries are removed, and changed-file totals include nested folders and repositories in the Git activity badge.
- Click a repository's branch name to switch branches for that repository. Changes lists grow with their contents and scroll through the panel rather than being clipped to a small fixed height.
- Long multi-repository lists keep their vertical scrollbar attached to the panel viewport, so it remains reachable while scrolling through large change sets.
- In **View Options → Open Changed Files**, choose **All Changes in One Tab**, **Separate File Diffs (Side by Side)**, or **Open File Without Diff**. Preferences persist; the combined diff view remains the default.
- Right-click a folder and choose **Hide Folder** to hide it and nested repositories from both Files and Git. Use **Hidden Folders** or **Show All Hidden Folders** to restore them. This does not delete files or modify `.gitignore`.
- Invalid or empty `.git` directories are no longer presented as ownership/trust failures. Actual Git ownership and project-trust checks remain in place.

## Navigation and appearance

- **Neutral Dark** is bundled with the app and available in the theme selector. It uses neutral charcoal editor and panel backgrounds; the existing **One Dark** theme remains available unchanged.
- Breadcrumbs, editor quick actions, and tab-bar back/forward buttons are hidden by default. The tab-bar controls button reveals editor actions on demand, and the corresponding toolbar/tab-bar settings can restore persistent controls.
- Files, Git, Search, and Chats use a vertical activity bar on the left. **Project Search** opens in the left dock with streamed results grouped by file; clicking a result opens the matching file and range in the editor. Case, whole-word, regex, ignored-file, and include/exclude controls are available.
- A single-folder file tree shows the folder's contents directly. The title bar shows the project name before its branch, with readable text and icons on colored backgrounds.
- The existing Threads sidebar opens on demand instead of automatically: **Ctrl+Alt+J** or **Ctrl+Shift+P → Toggle Workspace Sidebar**. Its visibility is restored. Zed's existing Agent panel remains available through **View → Agent Panel**.
- Open **Ctrl+Shift+P → Folder Color** for a visual picker with live preview, hue, presets, and HEX input. **Apply** saves, **Cancel** restores the previous color, and **Theme** resets to theme colors. The title, activity, and status bars use the selected color. Color changes work in Restricted Mode without granting trust.
- Open **Ctrl+Shift+P → Pane Layout** and choose **Flat** or **Separated**. Separated adds rounded panel boundaries and folder-colored spacing around the file panel, editor, bottom terminal, and right panel. Flat remains the default.
- Separated layouts use matching outer margins and pane separators in normal and Agent modes. Tab icons have additional leading space, and compact gutter controls keep their hover areas clear of line numbers and folding arrows.
- Activity buttons have square corners and no gaps. Hover states, branch labels, diagnostic icons, and status-bar separators have been adjusted for colored bars. Resize handles remain usable with separated panes and chat-only layouts.

## Windows builds

Custom Windows builds are distributed separately as a signed **Zed Setup** executable, with Start menu integration and **Open with Zed** Explorer integration. The installer keeps application settings separate from its binaries; personal chat histories and profiles are not bundled for distribution. The custom signing certificate is not Zed Industries' certificate.

For a local optimized build, first follow the [Windows development prerequisites](./docs/src/development/windows.md), then run:

```powershell
pwsh -File script/build-windows-update.ps1
```

The resulting executables are in `target/release-fast`. A standalone distribution also needs the Windows terminal support files and license notices; an EXE alone is not the full installer.

The update script reuses the repository's `target` directory and enables incremental compilation. The `release-fast` profile retains release optimization with limited debug information. Keep the build cache between updates; changing the Rust toolchain or profile settings can require a large rebuild. Use `-Jobs 6` to adjust parallelism and `-Offline` when all dependencies are already cached. Cargo writes build timing reports to `target/cargo-timings`. Installer packaging and signing are separate steps and do not require recompilation.

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
