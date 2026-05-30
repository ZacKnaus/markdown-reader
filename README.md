# Markdown Reader

A small Rust Windows desktop Markdown reader/editor. It opens exactly one Markdown file path, shows a large resizable WebView2 window, and starts in rendered Markdown view with a slim frame, save control, view/edit toggle, and settings button.

## Usage

```powershell
cargo run -- path\to\file.md
```

The app expects UTF-8 Markdown. Passing zero arguments or more than one positional argument exits with a usage error.

Windows builds use the GUI subsystem, so launching `markdown-reader.exe` opens the app window without a background console window.

For a no-registry `Open with` entry that shows a friendly filename, build the Windows copy:

```powershell
.\scripts\build-windows.cmd --release
```

Use `target\release\Markdown Reader.exe` for the Windows picker. The plain Cargo binary remains `target\release\markdown-reader.exe`.
The helper initializes Visual Studio Build Tools when they are installed but not already loaded in the current shell.

## Editing And Saving

- Toggle off to edit the plaintext Markdown.
- Toggle on to render the current editor contents.
- Press `Alt+Left` for plaintext view or `Alt+Right` for formatted view.
- Make small text edits directly in the formatted view when that is faster.
- Use the formatted-view toolbar for headings, bold, italic, inline code, links, bullet lists, numbered lists, blockquotes, and code blocks.
- Use the save button or press `Ctrl+S` to save.
- Closing the window does not autosave. Unsaved changes prompt before close.

Formatted-view edits are converted back to conservative Markdown when saving or switching to plaintext. That path preserves common structures such as headings, paragraphs, lists, blockquotes, code blocks, links, images, and tables, but it may normalize some original spacing.

Click links in formatted view to open safe targets with the Windows default handler. Relative document links are resolved from the folder containing the opened Markdown file, internal `#heading` links scroll inside the reader, and local executable/script-like targets are blocked.

## Table Of Contents

Add `[toc]` on its own line in plaintext mode to generate a table of contents from the document headings. Switching to formatted view expands the marker into Markdown links in memory; use Save to write the populated TOC to the file.

## Themes

Use the gear button to open settings. The app includes several built-in CSS themes and can import a custom `.css` file. Theme settings are stored outside the document at `%APPDATA%\Markdown Reader\settings.json` so the same theme follows you across opened Markdown files without registry writes.

The app also writes the built-in theme CSS files to `%APPDATA%\Markdown Reader\themes`. The Custom CSS picker opens in that folder so those files can be used as starting templates.

Rendered HTML is produced by `pulldown-cmark` and sanitized with `ammonia` before being inserted into the WebView. Raw scripts from Markdown are not kept in the rendered document. The WebView content security policy blocks network access, remote images, frames, and object/embed content; imported custom CSS is also filtered to remove remote imports and script-like legacy CSS patterns.

## Validation

```powershell
cargo fmt --check
cargo test
cargo check
cargo build --release
```
