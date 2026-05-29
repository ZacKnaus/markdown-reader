# Markdown Reader

A small Rust Windows desktop Markdown reader/editor. It opens exactly one Markdown file path, shows a large resizable WebView2 window, and starts in rendered Markdown view with a slim frame, save control, view/edit toggle, and settings button.

## Usage

```powershell
cargo run -- path\to\file.md
```

The app expects UTF-8 Markdown. Passing zero arguments or more than one positional argument exits with a usage error.

Windows builds use the GUI subsystem, so launching `markdown-reader.exe` opens the app window without a background console window.

## Editing And Saving

- Toggle off to edit the plaintext Markdown.
- Toggle on to render the current editor contents.
- Press `Alt+Left` for plaintext view or `Alt+Right` for formatted view.
- Make small text edits directly in the formatted view when that is faster.
- Use the formatted-view toolbar for headings, bold, italic, inline code, links, bullet lists, numbered lists, blockquotes, and code blocks.
- Use the save button or press `Ctrl+S` to save.
- Closing the window does not autosave. Unsaved changes prompt before close.

Formatted-view edits are converted back to conservative Markdown when saving or switching to plaintext. That path preserves common structures such as headings, paragraphs, lists, blockquotes, code blocks, links, images, and tables, but it may normalize some original spacing.

## Table Of Contents

Add `[toc]` on its own line in plaintext mode to generate a table of contents from the document headings. Switching to formatted view expands the marker into Markdown links in memory; use Save to write the populated TOC to the file.

## Themes

Use the gear button to open settings. The app includes several built-in CSS themes and can import a custom `.css` file. Theme settings are stored outside the document at `%APPDATA%\Markdown Reader\settings.json` so the same theme follows you across opened Markdown files without registry writes.

Rendered HTML is produced by `pulldown-cmark` and sanitized with `ammonia` before being inserted into the WebView. Raw scripts from Markdown are not kept in the rendered document. The WebView content security policy blocks network access, remote images, frames, and object/embed content; imported custom CSS is also filtered to remove remote imports and script-like legacy CSS patterns.

## Validation

```powershell
cargo fmt --check
cargo test
cargo check
cargo build --release
```
