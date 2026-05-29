use std::{env, fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use pulldown_cmark::{html, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use tao::{
    dpi::LogicalSize,
    event::{Event as TaoEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::{http::Request, NewWindowResponse, WebView, WebViewBuilder};

const WINDOW_WIDTH: f64 = 1200.0;
const WINDOW_HEIGHT: f64 = 900.0;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let input_path = parse_single_input_path()?;
    let markdown = fs::read_to_string(&input_path)
        .with_context(|| format!("failed to read {}", input_path.display()))?;
    launch_window(input_path, markdown)
}

fn parse_single_input_path() -> Result<PathBuf> {
    let mut args = env::args_os().skip(1);
    let Some(path) = args.next() else {
        bail!("usage: markdown-reader <path-to-markdown-file>");
    };

    if args.next().is_some() {
        bail!("usage: markdown-reader <path-to-markdown-file>");
    }

    Ok(PathBuf::from(path))
}

fn launch_window(input_path: PathBuf, markdown: String) -> Result<()> {
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window_title = input_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("Markdown Reader - {name}"))
        .unwrap_or_else(|| "Markdown Reader".to_string());
    let window = WindowBuilder::new()
        .with_title(window_title)
        .with_inner_size(LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT))
        .with_decorations(true)
        .with_resizable(true)
        .build(&event_loop)
        .context("failed to create application window")?;

    let handler =
        move |request: Request<String>| match serde_json::from_str::<IpcMessage>(request.body()) {
            Ok(IpcMessage::Render { markdown, marker }) => {
                let _ = proxy.send_event(AppEvent::Render { markdown, marker });
            }
            Ok(IpcMessage::Save { markdown }) => {
                let _ = proxy.send_event(AppEvent::Save { markdown });
            }
            Ok(IpcMessage::CloseSave { markdown }) => {
                let _ = proxy.send_event(AppEvent::CloseSave { markdown });
            }
            Err(error) => eprintln!("invalid IPC message: {error}"),
        };

    let initial_html = build_app_html(&markdown)?;
    let builder = WebViewBuilder::new()
        .with_html(initial_html)
        .with_ipc_handler(handler)
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny);
    let webview = builder
        .build(&window)
        .context("failed to create WebView2 surface")?;
    let mut webview = Some(webview);
    let mut close_pending = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            TaoEvent::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if close_pending {
                    return;
                }

                close_pending = true;
                if let Some(view) = webview.as_ref() {
                    // Closing is routed through JavaScript because the Rust side cannot
                    // synchronously read the current textarea contents from WebView2.
                    if view
                        .evaluate_script(
                            "window.__mdReaderRequestCloseSave && window.__mdReaderRequestCloseSave();",
                        )
                        .is_err()
                    {
                        let _ = webview.take();
                        *control_flow = ControlFlow::Exit;
                    }
                } else {
                    *control_flow = ControlFlow::Exit;
                }
            }
            TaoEvent::UserEvent(AppEvent::Render { markdown, marker }) => {
                if let Some(webview) = webview.as_ref() {
                    let html = render_markdown_safely(&markdown);
                    if let Err(error) = apply_rendered_html(webview, &html, &marker) {
                        eprintln!("failed to update rendered markdown: {error}");
                    }
                }
            }
            TaoEvent::UserEvent(AppEvent::Save { markdown }) => {
                if let Some(webview) = webview.as_ref() {
                    let result = fs::write(&input_path, markdown.as_bytes())
                        .with_context(|| format!("failed to save {}", input_path.display()));
                    notify_save_finished(webview, result);
                }
            }
            TaoEvent::UserEvent(AppEvent::CloseSave { markdown }) => {
                let result = fs::write(&input_path, markdown.as_bytes())
                    .with_context(|| format!("failed to save {}", input_path.display()));
                match result {
                    Ok(()) => {
                        let _ = webview.take();
                        *control_flow = ControlFlow::Exit;
                    }
                    Err(error) => {
                        close_pending = false;
                        if let Some(webview) = webview.as_ref() {
                            notify_save_finished(webview, Err(error));
                        }
                    }
                }
            }
            _ => {}
        }
    });
}

#[derive(Debug)]
enum AppEvent {
    Render {
        markdown: String,
        marker: ScrollMarker,
    },
    Save {
        markdown: String,
    },
    CloseSave {
        markdown: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum IpcMessage {
    Render {
        markdown: String,
        marker: ScrollMarker,
    },
    Save {
        markdown: String,
    },
    CloseSave {
        markdown: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ScrollMarker {
    section: Option<usize>,
    ratio: f64,
}

fn build_app_html(markdown: &str) -> Result<String> {
    let rendered = render_markdown_safely(markdown);
    let markdown_json = serde_json::to_string(markdown)?;
    let rendered_json = serde_json::to_string(&rendered)?;

    Ok(APP_HTML_TEMPLATE
        .replace("__INITIAL_MARKDOWN__", &markdown_json)
        .replace("__INITIAL_RENDERED__", &rendered_json))
}

fn apply_rendered_html(
    webview: &WebView,
    rendered_html: &str,
    marker: &ScrollMarker,
) -> Result<()> {
    let html_json = serde_json::to_string(rendered_html)?;
    let marker_json = serde_json::to_string(marker)?;
    webview
        .evaluate_script(&format!(
            "window.__mdReaderApplyRendered({html_json}, {marker_json});"
        ))
        .context("WebView2 rejected rendered markdown update")
}

fn notify_save_finished(webview: &WebView, result: Result<()>) {
    let (ok, message) = match result {
        Ok(()) => (true, String::new()),
        Err(error) => (false, error.to_string()),
    };
    let message_json = serde_json::to_string(&message).unwrap_or_else(|_| "\"save failed\"".into());
    let _ = webview.evaluate_script(&format!(
        "window.__mdReaderSaveFinished({ok}, {message_json});"
    ));
}

fn render_markdown_safely(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);

    let parser = Parser::new_ext(markdown, options);
    let mut next_heading_index = 0usize;
    let indexed_events = parser.map(move |event| match event {
        Event::Start(Tag::Heading { level, .. }) => {
            let tag = heading_tag(level);
            let index = next_heading_index;
            next_heading_index += 1;
            // The data attribute is the bridge between Rust-rendered headings and
            // JavaScript scroll restoration. It avoids relying on user heading text
            // or generated slugs, both of which can collide during editing.
            Event::Html(
                format!(r#"<{tag} id="section-{index}" data-md-section-index="{index}">"#).into(),
            )
        }
        Event::End(TagEnd::Heading(level)) => {
            Event::Html(format!("</{}>", heading_tag(level)).into())
        }
        other => other,
    });

    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, indexed_events);
    sanitize_rendered_html(&unsafe_html)
}

fn sanitize_rendered_html(unsafe_html: &str) -> String {
    let mut builder = ammonia::Builder::default();
    builder.add_generic_attributes(&["id"]);
    for tag in ["h1", "h2", "h3", "h4", "h5", "h6"] {
        builder.add_tag_attributes(tag, &["data-md-section-index"]);
    }
    builder.clean(unsafe_html).to_string()
}

fn heading_tag(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "h1",
        HeadingLevel::H2 => "h2",
        HeadingLevel::H3 => "h3",
        HeadingLevel::H4 => "h4",
        HeadingLevel::H5 => "h5",
        HeadingLevel::H6 => "h6",
    }
}

const APP_HTML_TEMPLATE: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data: file: http: https:; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
  <style>
    :root {
      color-scheme: light;
      --frame: #f7f7f4;
      --border: #d5d8d2;
      --ink: #20231f;
      --muted: #697067;
      --track: #cfd5ca;
      --thumb: #fffaf0;
      --accent: #1f7a68;
      --surface: #ffffff;
      --code: #f2f3f0;
    }

    * {
      box-sizing: border-box;
    }

    html,
    body {
      height: 100%;
      margin: 0;
      overflow: hidden;
      background: var(--surface);
      color: var(--ink);
      font-family: "Segoe UI", system-ui, sans-serif;
    }

    body {
      display: grid;
      grid-template-rows: 34px minmax(0, 1fr);
    }

    nav {
      display: flex;
      justify-content: flex-end;
      align-items: center;
      gap: 8px;
      height: 34px;
      padding: 0 12px;
      background: var(--frame);
      border-bottom: 1px solid var(--border);
      user-select: none;
    }

    .switch {
      position: relative;
      width: 44px;
      height: 22px;
      display: inline-flex;
      align-items: center;
      cursor: pointer;
    }

    .switch input {
      position: absolute;
      opacity: 0;
      pointer-events: none;
    }

    .track {
      position: absolute;
      inset: 0;
      border-radius: 999px;
      background: var(--track);
      border: 1px solid #aab3a8;
      transition: background 120ms ease;
    }

    .thumb {
      position: absolute;
      top: 3px;
      left: 3px;
      width: 16px;
      height: 16px;
      border-radius: 50%;
      background: var(--thumb);
      box-shadow: 0 1px 2px rgba(0, 0, 0, 0.24);
      transition: transform 120ms ease;
    }

    .switch input:checked + .track {
      background: var(--accent);
    }

    .switch input:checked ~ .thumb {
      transform: translateX(22px);
    }

    .switch input:focus-visible + .track {
      outline: 2px solid #2d6cdf;
      outline-offset: 2px;
    }

    main {
      min-height: 0;
      display: grid;
    }

    #editor,
    #rendered {
      grid-area: 1 / 1;
      width: 100%;
      height: 100%;
    }

    #editor {
      display: none;
      padding: 28px 34px 60px;
      border: 0;
      outline: 0;
      resize: none;
      color: var(--ink);
      background: var(--surface);
      font: 15px/1.55 "Cascadia Mono", Consolas, "Courier New", monospace;
      tab-size: 4;
      white-space: pre;
      overflow: auto;
    }

    #rendered {
      overflow: auto;
      padding: 24px 34px 72px;
      font: 16px/1.58 "Segoe UI", system-ui, sans-serif;
    }

    body.editing #editor {
      display: block;
    }

    body.editing #rendered {
      display: none;
    }

    #rendered > :first-child {
      margin-top: 0;
    }

    #rendered h1,
    #rendered h2,
    #rendered h3,
    #rendered h4,
    #rendered h5,
    #rendered h6 {
      line-height: 1.2;
      margin: 1.65em 0 0.45em;
    }

    #rendered h1 {
      font-size: 2rem;
      border-bottom: 1px solid var(--border);
      padding-bottom: 0.25em;
    }

    #rendered h2 {
      font-size: 1.55rem;
      border-bottom: 1px solid #e4e6e1;
      padding-bottom: 0.2em;
    }

    #rendered h3 {
      font-size: 1.25rem;
    }

    #rendered p,
    #rendered ul,
    #rendered ol,
    #rendered blockquote,
    #rendered pre,
    #rendered table {
      margin: 0 0 1em;
    }

    #rendered pre,
    #rendered code {
      background: var(--code);
      font-family: "Cascadia Mono", Consolas, "Courier New", monospace;
    }

    #rendered pre {
      overflow: auto;
      padding: 14px 16px;
      border-radius: 6px;
    }

    #rendered code {
      border-radius: 4px;
      padding: 0.1em 0.28em;
    }

    #rendered pre code {
      padding: 0;
      background: transparent;
    }

    #rendered blockquote {
      color: var(--muted);
      border-left: 3px solid var(--border);
      padding-left: 14px;
    }

    #rendered table {
      border-collapse: collapse;
      width: max-content;
      max-width: 100%;
    }

    #rendered th,
    #rendered td {
      border: 1px solid var(--border);
      padding: 6px 10px;
    }

    #rendered img {
      max-width: 100%;
      height: auto;
    }
  </style>
</head>
<body>
  <nav>
    <label class="switch" title="Toggle view">
      <input id="mode-toggle" type="checkbox" aria-label="Toggle rendered Markdown view">
      <span class="track"></span>
      <span class="thumb"></span>
    </label>
  </nav>
  <main>
    <textarea id="editor" spellcheck="false" wrap="off"></textarea>
    <article id="rendered"></article>
  </main>
  <script>
    const initialMarkdown = __INITIAL_MARKDOWN__;
    const initialRendered = __INITIAL_RENDERED__;
    const editor = document.getElementById("editor");
    const rendered = document.getElementById("rendered");
    const toggle = document.getElementById("mode-toggle");
    let dirty = false;

    editor.value = initialMarkdown;
    rendered.innerHTML = initialRendered;

    function postMessage(payload) {
      window.ipc.postMessage(JSON.stringify(payload));
    }

    function markerFromEditor() {
      const lineHeight = editorLineHeight();
      const line = Math.max(0, Math.floor(editor.scrollTop / lineHeight));
      return {
        section: sectionIndexForLine(line),
        ratio: scrollRatio(editor)
      };
    }

    function markerFromRendered() {
      const headings = rendered.querySelectorAll("[data-md-section-index]");
      let current = null;
      const top = rendered.scrollTop + 36;

      for (const heading of headings) {
        if (heading.offsetTop <= top) {
          current = Number(heading.dataset.mdSectionIndex);
        } else {
          break;
        }
      }

      return { section: current, ratio: scrollRatio(rendered) };
    }

    function markerForCurrentMode() {
      return document.body.classList.contains("editing") ? markerFromEditor() : markerFromRendered();
    }

    function applyMarkerToRendered(marker) {
      if (marker && marker.section !== null && marker.section !== undefined) {
        const target = rendered.querySelector(`[data-md-section-index="${marker.section}"]`);
        if (target) {
          rendered.scrollTop = Math.max(0, target.offsetTop - 28);
          return;
        }
      }
      applyRatio(rendered, marker ? marker.ratio : 0);
    }

    function applyMarkerToEditor(marker) {
      if (marker && marker.section !== null && marker.section !== undefined) {
        const line = lineForSectionIndex(marker.section);
        if (line !== null) {
          editor.scrollTop = Math.max(0, line * editorLineHeight() - 20);
          return;
        }
      }
      applyRatio(editor, marker ? marker.ratio : 0);
    }

    function scrollRatio(element) {
      const limit = element.scrollHeight - element.clientHeight;
      return limit > 0 ? element.scrollTop / limit : 0;
    }

    function applyRatio(element, ratio) {
      const limit = element.scrollHeight - element.clientHeight;
      element.scrollTop = Math.max(0, limit * Math.min(1, Math.max(0, ratio || 0)));
    }

    function editorLineHeight() {
      const parsed = Number.parseFloat(getComputedStyle(editor).lineHeight);
      return Number.isFinite(parsed) && parsed > 0 ? parsed : 23;
    }

    function sectionIndexForLine(line) {
      const headingLines = collectHeadingLines();
      let section = null;
      for (let index = 0; index < headingLines.length; index += 1) {
        if (headingLines[index] <= line) {
          section = index;
        } else {
          break;
        }
      }
      return section;
    }

    function lineForSectionIndex(section) {
      const headingLines = collectHeadingLines();
      return headingLines[section] ?? null;
    }

    function collectHeadingLines() {
      const lines = editor.value.split(/\r\n|\r|\n/);
      const result = [];
      let fenced = false;
      let fenceMarker = "";

      for (let index = 0; index < lines.length; index += 1) {
        const trimmed = lines[index].trim();
        const fence = trimmed.match(/^(```+|~~~+)/);
        if (fence) {
          const marker = fence[1][0];
          if (!fenced) {
            fenced = true;
            fenceMarker = marker;
          } else if (marker === fenceMarker) {
            fenced = false;
          }
          continue;
        }

        if (fenced) {
          continue;
        }

        if (/^#{1,6}\s+\S/.test(trimmed)) {
          result.push(index);
          continue;
        }

        const next = lines[index + 1] ? lines[index + 1].trim() : "";
        if (trimmed && /^(=+|-+)$/.test(next)) {
          result.push(index);
        }
      }

      return result;
    }

    function switchToRendered() {
      const marker = markerFromEditor();
      postMessage({ kind: "render", markdown: editor.value, marker });
    }

    function switchToEditor() {
      const marker = markerFromRendered();
      document.body.classList.add("editing");
      toggle.checked = false;
      requestAnimationFrame(() => applyMarkerToEditor(marker));
    }

    // Rendering stays in Rust so opened Markdown is processed by pulldown-cmark
    // and ammonia before it is assigned to innerHTML.
    window.__mdReaderApplyRendered = (html, marker) => {
      rendered.innerHTML = html;
      document.body.classList.remove("editing");
      toggle.checked = true;
      requestAnimationFrame(() => applyMarkerToRendered(marker));
    };

    window.__mdReaderSaveFinished = (ok, message) => {
      if (ok) {
        dirty = false;
        return;
      }
      alert(message || "Save failed.");
    };

    window.__mdReaderRequestCloseSave = () => {
      postMessage({ kind: "closeSave", markdown: editor.value });
    };

    toggle.addEventListener("change", () => {
      if (toggle.checked) {
        switchToRendered();
      } else {
        switchToEditor();
      }
    });

    editor.addEventListener("input", () => {
      dirty = true;
    });

    document.addEventListener("keydown", (event) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
        event.preventDefault();
        postMessage({ kind: "save", markdown: editor.value });
      }
    });

    rendered.addEventListener("click", (event) => {
      const anchor = event.target.closest("a");
      if (anchor) {
        event.preventDefault();
      }
    });

    document.body.classList.remove("editing");
    toggle.checked = true;
  </script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_adds_stable_heading_markers() {
        let html = render_markdown_safely("# One\n\n## Two");

        assert!(html.contains(r#"data-md-section-index="0""#));
        assert!(html.contains(r#"data-md-section-index="1""#));
        assert!(html.contains(r#"id="section-0""#));
    }

    #[test]
    fn renderer_strips_raw_scripts() {
        let html = render_markdown_safely("# Safe\n\n<script>alert('x')</script>");

        assert!(!html.to_lowercase().contains("<script"));
        assert!(!html.contains("alert"));
    }
}
