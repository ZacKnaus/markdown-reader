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

#[derive(Serialize)]
struct ThemeDefinition {
    id: &'static str,
    name: &'static str,
    css: &'static str,
}

const THEMES: &[ThemeDefinition] = &[
    ThemeDefinition {
        id: "clean",
        name: "Clean",
        css: include_str!("../themes/clean.css"),
    },
    ThemeDefinition {
        id: "ink",
        name: "Ink",
        css: include_str!("../themes/ink.css"),
    },
    ThemeDefinition {
        id: "paper",
        name: "Paper",
        css: include_str!("../themes/paper.css"),
    },
    ThemeDefinition {
        id: "slate",
        name: "Slate",
        css: include_str!("../themes/slate.css"),
    },
];

fn build_app_html(markdown: &str) -> Result<String> {
    let rendered = render_markdown_safely(markdown);
    let markdown_json = serde_json::to_string(markdown)?;
    let rendered_json = serde_json::to_string(&rendered)?;
    let themes_json = serde_json::to_string(THEMES)?;

    Ok(APP_HTML_TEMPLATE
        .replace("__INITIAL_MARKDOWN__", &markdown_json)
        .replace("__INITIAL_RENDERED__", &rendered_json)
        .replace("__THEMES__", &themes_json))
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

    .nav-spacer {
      flex: 1 1 auto;
    }

    .icon-button {
      width: 28px;
      height: 28px;
      display: inline-grid;
      place-items: center;
      border: 1px solid transparent;
      border-radius: 4px;
      padding: 0;
      color: var(--ink);
      background: transparent;
      cursor: pointer;
    }

    .icon-button:hover {
      border-color: var(--border);
      background: rgba(0, 0, 0, 0.04);
    }

    .icon-button:focus-visible {
      outline: 2px solid #2d6cdf;
      outline-offset: 1px;
    }

    .icon-button.saved {
      color: var(--muted);
    }

    .icon-button.dirty {
      color: var(--accent);
    }

    .icon {
      width: 16px;
      height: 16px;
      stroke: currentColor;
      stroke-width: 2;
      stroke-linecap: round;
      stroke-linejoin: round;
      fill: none;
      pointer-events: none;
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

    #rendered:focus {
      outline: 0;
    }

    #rendered:focus-visible {
      box-shadow: inset 0 0 0 2px rgba(45, 108, 223, 0.35);
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

    .modal-backdrop[hidden] {
      display: none;
    }

    .modal-backdrop {
      position: fixed;
      inset: 34px 0 0;
      display: grid;
      place-items: start center;
      padding-top: 54px;
      background: rgba(0, 0, 0, 0.18);
      z-index: 10;
    }

    .settings-modal {
      width: min(420px, calc(100vw - 32px));
      border: 1px solid var(--border);
      border-radius: 8px;
      background: var(--dialog, var(--surface));
      box-shadow: 0 18px 44px var(--dialog-shadow, rgba(0, 0, 0, 0.24));
      color: var(--ink);
    }

    .settings-header {
      height: 42px;
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 0 10px 0 16px;
      border-bottom: 1px solid var(--border);
    }

    .settings-header h2 {
      font-size: 15px;
      font-weight: 600;
      margin: 0;
    }

    .settings-body {
      display: grid;
      gap: 16px;
      padding: 16px;
    }

    .field {
      display: grid;
      gap: 6px;
      font-size: 13px;
      color: var(--muted);
    }

    select,
    input[type="file"] {
      width: 100%;
      min-height: 32px;
      color: var(--ink);
      background: var(--surface);
      border: 1px solid var(--border);
      border-radius: 4px;
      padding: 4px 8px;
      font: 14px/1.3 "Segoe UI", system-ui, sans-serif;
    }
  </style>
  <style id="theme-style"></style>
  <style id="custom-theme-style"></style>
</head>
<body>
  <nav>
    <div class="nav-spacer"></div>
    <button id="save-button" class="icon-button saved" type="button" title="Save" aria-label="Save">
      <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
        <path d="M5 3h12l2 2v16H5z"></path>
        <path d="M8 3v6h8V3"></path>
        <path d="M8 21v-7h8v7"></path>
      </svg>
    </button>
    <label class="switch" title="Toggle view">
      <input id="mode-toggle" type="checkbox" aria-label="Toggle rendered Markdown view">
      <span class="track"></span>
      <span class="thumb"></span>
    </label>
    <button id="settings-button" class="icon-button" type="button" title="Settings" aria-label="Settings">
      <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
        <circle cx="12" cy="12" r="3"></circle>
        <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1-2.1 2.1-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.5V20h-3v-.2a1.7 1.7 0 0 0-1-1.5 1.7 1.7 0 0 0-1.9.3l-.1.1L6.6 16.6l.1-.1A1.7 1.7 0 0 0 7 14.6a1.7 1.7 0 0 0-1.5-1H5v-3h.2a1.7 1.7 0 0 0 1.5-1A1.7 1.7 0 0 0 6.4 7.7l-.1-.1 2.1-2.1.1.1a1.7 1.7 0 0 0 1.9.3 1.7 1.7 0 0 0 1-1.5V4h3v.2a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.9-.3l.1-.1 2.1 2.1-.1.1a1.7 1.7 0 0 0-.3 1.9 1.7 1.7 0 0 0 1.5 1h.2v3h-.2a1.7 1.7 0 0 0-1.5 1z"></path>
      </svg>
    </button>
  </nav>
  <main>
    <textarea id="editor" spellcheck="false" wrap="off"></textarea>
    <article id="rendered" contenteditable="true" spellcheck="true"></article>
  </main>
  <div id="settings-backdrop" class="modal-backdrop" hidden>
    <section class="settings-modal" role="dialog" aria-modal="true" aria-labelledby="settings-title">
      <header class="settings-header">
        <h2 id="settings-title">Settings</h2>
        <button id="settings-close" class="icon-button" type="button" title="Close" aria-label="Close settings">
          <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
            <path d="M18 6 6 18"></path>
            <path d="m6 6 12 12"></path>
          </svg>
        </button>
      </header>
      <div class="settings-body">
        <label class="field" for="theme-select">
          Theme
          <select id="theme-select"></select>
        </label>
        <label class="field" for="custom-css-input">
          Custom CSS
          <input id="custom-css-input" type="file" accept=".css,text/css">
        </label>
      </div>
    </section>
  </div>
  <script>
    const initialMarkdown = __INITIAL_MARKDOWN__;
    const initialRendered = __INITIAL_RENDERED__;
    const themes = __THEMES__;
    const editor = document.getElementById("editor");
    const rendered = document.getElementById("rendered");
    const toggle = document.getElementById("mode-toggle");
    const saveButton = document.getElementById("save-button");
    const settingsButton = document.getElementById("settings-button");
    const settingsBackdrop = document.getElementById("settings-backdrop");
    const settingsClose = document.getElementById("settings-close");
    const themeSelect = document.getElementById("theme-select");
    const customCssInput = document.getElementById("custom-css-input");
    const themeStyle = document.getElementById("theme-style");
    const customThemeStyle = document.getElementById("custom-theme-style");
    const storageKeys = {
      theme: "markdown-reader.theme",
      customCss: "markdown-reader.custom-css"
    };
    let dirty = false;
    let renderedDirty = false;
    let customCss = loadStoredValue(storageKeys.customCss, "");

    editor.value = initialMarkdown;
    rendered.innerHTML = initialRendered;

    function postMessage(payload) {
      window.ipc.postMessage(JSON.stringify(payload));
    }

    function loadStoredValue(key, fallback) {
      try {
        const value = localStorage.getItem(key);
        return value === null ? fallback : value;
      } catch (_error) {
        return fallback;
      }
    }

    function storeValue(key, value) {
      try {
        localStorage.setItem(key, value);
      } catch (_error) {
        // Some embedded WebView origins can reject storage. The setting still
        // applies to the current window, so failure here is non-fatal.
      }
    }

    function setDirty(value) {
      dirty = value;
      saveButton.classList.toggle("dirty", dirty);
      saveButton.classList.toggle("saved", !dirty);
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

    function currentMarkdownForSave() {
      if (!document.body.classList.contains("editing")) {
        commitRenderedEditsToMarkdown();
      }
      return editor.value;
    }

    function commitRenderedEditsToMarkdown() {
      if (renderedDirty) {
        editor.value = renderedToMarkdown();
        renderedDirty = false;
      }
      return editor.value;
    }

    function saveCurrentDocument() {
      postMessage({ kind: "save", markdown: currentMarkdownForSave() });
    }

    function switchToRendered() {
      const marker = markerFromEditor();
      postMessage({ kind: "render", markdown: editor.value, marker });
    }

    function switchToEditor() {
      const marker = markerFromRendered();
      commitRenderedEditsToMarkdown();
      document.body.classList.add("editing");
      toggle.checked = false;
      requestAnimationFrame(() => applyMarkerToEditor(marker));
    }

    function renderedToMarkdown() {
      const blocks = [];
      for (const child of rendered.childNodes) {
        const markdown = blockMarkdown(child, 0).trimEnd();
        if (markdown.trim()) {
          blocks.push(markdown);
        }
      }

      const documentText = blocks.join("\n\n").replace(/[ \t]+\n/g, "\n").replace(/\n{3,}/g, "\n\n").trimEnd();
      return documentText ? `${documentText}\n` : "";
    }

    function blockMarkdown(node, depth) {
      if (node.nodeType === Node.TEXT_NODE) {
        return normalizeText(node.textContent).trim();
      }
      if (node.nodeType !== Node.ELEMENT_NODE) {
        return "";
      }

      const tag = node.tagName.toLowerCase();
      if (/^h[1-6]$/.test(tag)) {
        const level = Number(tag.slice(1));
        return `${'#'.repeat(level)} ${inlineMarkdown(node).trim()}`;
      }
      if (tag === "p") {
        return inlineMarkdown(node).trim();
      }
      if (tag === "pre") {
        return fencedCode(node.innerText || node.textContent || "");
      }
      if (tag === "blockquote") {
        const text = childBlocksMarkdown(node, depth).trim();
        return text.split("\n").map((line) => line ? `> ${line}` : ">").join("\n");
      }
      if (tag === "ul" || tag === "ol") {
        return listMarkdown(node, tag === "ol", depth);
      }
      if (tag === "table") {
        return tableMarkdown(node);
      }
      if (tag === "hr") {
        return "---";
      }
      if (tag === "br") {
        return "\n";
      }
      if (hasBlockChildren(node)) {
        return childBlocksMarkdown(node, depth);
      }
      return inlineMarkdown(node).trim();
    }

    function childBlocksMarkdown(element, depth) {
      const blocks = [];
      for (const child of element.childNodes) {
        const markdown = blockMarkdown(child, depth).trimEnd();
        if (markdown.trim()) {
          blocks.push(markdown);
        }
      }
      return blocks.join("\n\n");
    }

    function hasBlockChildren(element) {
      return Array.from(element.children).some((child) => /^(address|article|aside|blockquote|div|dl|fieldset|figcaption|figure|footer|form|h[1-6]|header|hr|li|main|nav|ol|p|pre|section|table|ul)$/.test(child.tagName.toLowerCase()));
    }

    function inlineMarkdown(node) {
      if (node.nodeType === Node.TEXT_NODE) {
        return escapeInlineText(node.textContent);
      }
      if (node.nodeType !== Node.ELEMENT_NODE) {
        return "";
      }

      const tag = node.tagName.toLowerCase();
      const text = Array.from(node.childNodes).map(inlineMarkdown).join("");
      if (tag === "strong" || tag === "b") {
        return text ? `**${text}**` : "";
      }
      if (tag === "em" || tag === "i") {
        return text ? `*${text}*` : "";
      }
      if (tag === "code") {
        return inlineCode(node.textContent || "");
      }
      if (tag === "a") {
        const href = node.getAttribute("href") || "";
        if (!href || /^\s*javascript:/i.test(href)) {
          return text;
        }
        return `[${text}](${href.replace(/\)/g, "%29")})`;
      }
      if (tag === "img") {
        const alt = escapeInlineText(node.getAttribute("alt") || "");
        const src = node.getAttribute("src") || "";
        return src ? `![${alt}](${src.replace(/\)/g, "%29")})` : alt;
      }
      if (tag === "br") {
        return "\n";
      }
      return text;
    }

    function listMarkdown(list, ordered, depth) {
      const lines = [];
      let ordinal = 1;
      for (const item of Array.from(list.children).filter((child) => child.tagName.toLowerCase() === "li")) {
        const marker = ordered ? `${ordinal}. ` : "- ";
        ordinal += 1;
        const content = listItemMarkdown(item, depth + 1);
        const contentLines = content.split("\n");
        const indent = "  ".repeat(depth);
        lines.push(`${indent}${marker}${contentLines[0] || ""}`);
        for (const line of contentLines.slice(1)) {
          lines.push(`${indent}  ${line}`);
        }
      }
      return lines.join("\n");
    }

    function listItemMarkdown(item, depth) {
      const parts = [];
      let inlineParts = [];
      for (const child of item.childNodes) {
        if (child.nodeType === Node.ELEMENT_NODE && /^(ul|ol|p|pre|blockquote|table)$/.test(child.tagName.toLowerCase())) {
          if (inlineParts.join("").trim()) {
            parts.push(inlineParts.join("").trim());
            inlineParts = [];
          }
          parts.push(blockMarkdown(child, depth));
        } else {
          inlineParts.push(inlineMarkdown(child));
        }
      }
      if (inlineParts.join("").trim()) {
        parts.push(inlineParts.join("").trim());
      }
      return parts.join("\n");
    }

    function tableMarkdown(table) {
      const rows = Array.from(table.querySelectorAll("tr")).map((row) =>
        Array.from(row.children)
          .filter((cell) => /^(th|td)$/.test(cell.tagName.toLowerCase()))
          .map((cell) => inlineMarkdown(cell).replace(/\|/g, "\\|").trim())
      ).filter((row) => row.length > 0);

      if (rows.length === 0) {
        return "";
      }

      const width = Math.max(...rows.map((row) => row.length));
      const normalized = rows.map((row) => {
        const next = row.slice();
        while (next.length < width) {
          next.push("");
        }
        return next;
      });
      const header = normalized[0];
      const separator = header.map(() => "---");
      const body = normalized.slice(1);
      return [header, separator, ...body].map((row) => `| ${row.join(" | ")} |`).join("\n");
    }

    function fencedCode(text) {
      const cleaned = text.replace(/\n$/, "");
      let fence = "```";
      while (cleaned.includes(fence)) {
        fence += "`";
      }
      return `${fence}\n${cleaned}\n${fence}`;
    }

    function inlineCode(text) {
      let fence = "`";
      while (text.includes(fence)) {
        fence += "`";
      }
      return `${fence}${text}${fence}`;
    }

    function normalizeText(text) {
      return (text || "").replace(/\u00a0/g, " ");
    }

    function escapeInlineText(text) {
      return normalizeText(text).replace(/[\\`*_{}\[\]<>]/g, "\\$&");
    }

    function populateThemes() {
      themeSelect.innerHTML = "";
      for (const theme of themes) {
        const option = document.createElement("option");
        option.value = theme.id;
        option.textContent = theme.name;
        themeSelect.append(option);
      }
      if (customCss) {
        addCustomThemeOption();
      }
    }

    function addCustomThemeOption() {
      if (themeSelect.querySelector('option[value="custom"]')) {
        return;
      }
      const option = document.createElement("option");
      option.value = "custom";
      option.textContent = "Custom";
      themeSelect.append(option);
    }

    function applyTheme(themeId) {
      const fallback = themes[0];
      const selected = themes.find((theme) => theme.id === themeId) || fallback;
      if (themeId === "custom" && customCss) {
        themeStyle.textContent = fallback.css;
        customThemeStyle.textContent = customCss;
      } else {
        themeStyle.textContent = selected.css;
        customThemeStyle.textContent = "";
      }
      storeValue(storageKeys.theme, themeId);
    }

    function openSettings() {
      settingsBackdrop.hidden = false;
      themeSelect.focus();
    }

    function closeSettings() {
      settingsBackdrop.hidden = true;
      settingsButton.focus();
    }

    // Rendering stays in Rust so opened Markdown is processed by pulldown-cmark
    // and ammonia before it is assigned to innerHTML.
    window.__mdReaderApplyRendered = (html, marker) => {
      rendered.innerHTML = html;
      renderedDirty = false;
      document.body.classList.remove("editing");
      toggle.checked = true;
      requestAnimationFrame(() => applyMarkerToRendered(marker));
    };

    window.__mdReaderSaveFinished = (ok, message) => {
      if (ok) {
        setDirty(false);
        return;
      }
      alert(message || "Save failed.");
    };

    window.__mdReaderRequestCloseSave = () => {
      postMessage({ kind: "closeSave", markdown: currentMarkdownForSave() });
    };

    toggle.addEventListener("change", () => {
      if (toggle.checked) {
        switchToRendered();
      } else {
        switchToEditor();
      }
    });

    saveButton.addEventListener("click", saveCurrentDocument);

    settingsButton.addEventListener("click", openSettings);
    settingsClose.addEventListener("click", closeSettings);
    settingsBackdrop.addEventListener("click", (event) => {
      if (event.target === settingsBackdrop) {
        closeSettings();
      }
    });

    themeSelect.addEventListener("change", () => {
      applyTheme(themeSelect.value);
    });

    customCssInput.addEventListener("change", async () => {
      const file = customCssInput.files && customCssInput.files[0];
      if (!file) {
        return;
      }
      customCss = await file.text();
      storeValue(storageKeys.customCss, customCss);
      addCustomThemeOption();
      themeSelect.value = "custom";
      applyTheme("custom");
      customCssInput.value = "";
    });

    editor.addEventListener("input", () => {
      setDirty(true);
    });

    rendered.addEventListener("input", () => {
      renderedDirty = true;
      setDirty(true);
    });

    document.addEventListener("keydown", (event) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
        event.preventDefault();
        saveCurrentDocument();
      }
      if (event.key === "Escape" && !settingsBackdrop.hidden) {
        event.preventDefault();
        closeSettings();
      }
    });

    rendered.addEventListener("click", (event) => {
      const anchor = event.target.closest("a");
      if (anchor) {
        event.preventDefault();
      }
    });

    populateThemes();
    const storedTheme = loadStoredValue(storageKeys.theme, "clean");
    const themeIsKnown = storedTheme === "custom" ? Boolean(customCss) : themes.some((theme) => theme.id === storedTheme);
    const initialTheme = themeIsKnown ? storedTheme : "clean";
    themeSelect.value = initialTheme;
    applyTheme(initialTheme);
    document.body.classList.remove("editing");
    toggle.checked = true;
    setDirty(false);
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

    #[test]
    fn app_html_includes_theme_and_control_data() {
        let html = build_app_html("# One").expect("app html should render");

        assert!(!html.contains("__THEMES__"));
        assert!(html.contains("save-button"));
        assert!(html.contains("settings-button"));
        assert!(html.contains("contenteditable=\"true\""));
    }

    #[test]
    fn built_in_themes_are_embedded() {
        assert!(THEMES.len() >= 3);
        assert!(THEMES.iter().all(|theme| theme.css.contains(":root")));
    }
}
