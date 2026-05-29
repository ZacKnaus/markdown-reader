# Markdown Reader

A small Rust Windows desktop Markdown reader/editor. It opens exactly one Markdown file path, shows a large resizable WebView2 window, and starts in rendered Markdown view with a slim frame, save control, view/edit toggle, and settings button.

## Usage

```powershell
cargo run -- path\to\file.md
```

The app expects UTF-8 Markdown. Passing zero arguments or more than one positional argument exits with a usage error.

## Editing And Saving

- Toggle off to edit the plaintext Markdown.
- Toggle on to render the current editor contents.
- Make small text edits directly in the formatted view when that is faster.
- Use the formatted-view toolbar for headings, bold, italic, inline code, links, bullet lists, numbered lists, blockquotes, and code blocks.
- Use the save button or press `Ctrl+S` to save.
- Closing the window autosaves the current editor contents before exit.

Formatted-view edits are converted back to conservative Markdown when saving or switching to plaintext. That path preserves common structures such as headings, paragraphs, lists, blockquotes, code blocks, links, images, and tables, but it may normalize some original spacing.

## Table Of Contents

Add `[toc]` on its own line in plaintext mode to generate a table of contents from the document headings. Switching to formatted view expands the marker into Markdown links, and switching back to plaintext keeps the populated TOC in the document.

## Themes

Use the gear button to open settings. The app includes several built-in CSS themes and can import a custom `.css` file for the current WebView profile. Theme choices are stored locally when the WebView storage backend allows it.

Rendered HTML is produced by `pulldown-cmark` and sanitized with `ammonia` before being inserted into the WebView. Raw scripts from Markdown are not kept in the rendered document.

## Validation

```powershell
cargo fmt --check
cargo test
cargo check
```
