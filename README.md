# Markdown Reader

A small Rust Windows desktop Markdown reader/editor. It opens exactly one Markdown file path, shows a large resizable WebView2 window, and starts in rendered Markdown view with a slim frame and a single side-to-side toggle for edit/view mode.

## Usage

```powershell
cargo run -- path\to\file.md
```

The app expects UTF-8 Markdown. Passing zero arguments or more than one positional argument exits with a usage error.

## Editing And Saving

- Toggle off to edit the plaintext Markdown.
- Toggle on to render the current editor contents.
- Press `Ctrl+S` to save.
- Closing the window autosaves the current editor contents before exit.

Rendered HTML is produced by `pulldown-cmark` and sanitized with `ammonia` before being inserted into the WebView. Raw scripts from Markdown are not kept in the rendered document.

## Validation

```powershell
cargo fmt --check
cargo test
cargo check
```
