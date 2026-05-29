#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use pulldown_cmark::{html, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use tao::{
    dpi::LogicalSize,
    event::{Event as TaoEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::{Icon, WindowBuilder},
};
use wry::{http::Request, NewWindowResponse, WebView, WebViewBuilder};

const WINDOW_WIDTH: f64 = 1200.0;
const WINDOW_HEIGHT: f64 = 900.0;
const APP_ICON_WIDTH: u32 = 32;
const APP_ICON_HEIGHT: u32 = 32;
const APP_ICON_RGBA: &[u8] = include_bytes!("../assets/app-icon-32.rgba");

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
    let settings = load_app_settings();
    if let Err(error) = export_builtin_theme_templates(&app_themes_path()) {
        eprintln!("failed to write default theme templates: {error:#}");
    }
    launch_window(input_path, markdown, settings)
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

fn app_window_icon() -> Result<Icon> {
    Icon::from_rgba(APP_ICON_RGBA.to_vec(), APP_ICON_WIDTH, APP_ICON_HEIGHT)
        .context("failed to load runtime window icon")
}

fn launch_window(input_path: PathBuf, markdown: String, settings: AppSettings) -> Result<()> {
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let settings_path = app_settings_path();
    let themes_path = app_themes_path();
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
        .with_window_icon(Some(app_window_icon()?))
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
            Ok(IpcMessage::Close) => {
                let _ = proxy.send_event(AppEvent::Close);
            }
            Ok(IpcMessage::SaveSettings { settings }) => {
                let _ = proxy.send_event(AppEvent::SaveSettings { settings });
            }
            Ok(IpcMessage::PickCustomCss) => {
                let _ = proxy.send_event(AppEvent::PickCustomCss);
            }
            Err(error) => eprintln!("invalid IPC message: {error}"),
        };

    let initial_html = build_app_html(&markdown, &settings)?;
    let builder = WebViewBuilder::new()
        .with_html(initial_html)
        .with_ipc_handler(handler)
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny);
    let webview = builder
        .build(&window)
        .context("failed to create WebView2 surface")?;
    let mut webview = Some(webview);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            TaoEvent::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if let Some(view) = webview.as_ref() {
                    // Closing is routed through JavaScript because the Rust side cannot
                    // synchronously inspect the dirty state from WebView2.
                    if view
                        .evaluate_script(
                            "window.__mdReaderRequestClose && window.__mdReaderRequestClose();",
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
                    let expanded_markdown = expand_toc_markers(&markdown);
                    let html = render_markdown_safely(&expanded_markdown);
                    if let Err(error) =
                        apply_rendered_html(webview, &html, &expanded_markdown, &marker)
                    {
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
            TaoEvent::UserEvent(AppEvent::Close) => {
                let _ = webview.take();
                *control_flow = ControlFlow::Exit;
            }
            TaoEvent::UserEvent(AppEvent::SaveSettings { settings }) => {
                if let Some(webview) = webview.as_ref() {
                    let result = save_app_settings(&settings_path, &settings);
                    notify_settings_finished(webview, result);
                }
            }
            TaoEvent::UserEvent(AppEvent::PickCustomCss) => {
                if let Some(webview) = webview.as_ref() {
                    match pick_custom_css(&themes_path) {
                        Ok(Some(css)) => notify_custom_css_picked(webview, Ok(css)),
                        Ok(None) => {}
                        Err(error) => notify_custom_css_picked(webview, Err(error)),
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
    Close,
    SaveSettings {
        settings: AppSettings,
    },
    PickCustomCss,
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
    Close,
    SaveSettings {
        settings: AppSettings,
    },
    PickCustomCss,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ScrollMarker {
    section: Option<usize>,
    ratio: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    theme_id: String,
    custom_css: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme_id: "clean".to_string(),
            custom_css: String::new(),
        }
    }
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

fn build_app_html(markdown: &str, settings: &AppSettings) -> Result<String> {
    let expanded_markdown = expand_toc_markers(markdown);
    let rendered = render_markdown_safely(&expanded_markdown);
    let markdown_json = serde_json::to_string(&expanded_markdown)?;
    let rendered_json = serde_json::to_string(&rendered)?;
    let themes_json = serde_json::to_string(THEMES)?;
    let settings_json = serde_json::to_string(&normalize_settings(settings))?;

    Ok(APP_HTML_TEMPLATE
        .replace("__INITIAL_MARKDOWN__", &markdown_json)
        .replace("__INITIAL_RENDERED__", &rendered_json)
        .replace("__THEMES__", &themes_json)
        .replace("__SETTINGS__", &settings_json))
}

fn apply_rendered_html(
    webview: &WebView,
    rendered_html: &str,
    expanded_markdown: &str,
    marker: &ScrollMarker,
) -> Result<()> {
    let html_json = serde_json::to_string(rendered_html)?;
    let markdown_json = serde_json::to_string(expanded_markdown)?;
    let marker_json = serde_json::to_string(marker)?;
    webview
        .evaluate_script(&format!(
            "window.__mdReaderApplyRendered({html_json}, {markdown_json}, {marker_json});"
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

fn notify_settings_finished(webview: &WebView, result: Result<()>) {
    let (ok, message) = match result {
        Ok(()) => (true, String::new()),
        Err(error) => (false, error.to_string()),
    };
    let message_json =
        serde_json::to_string(&message).unwrap_or_else(|_| "\"settings save failed\"".into());
    let _ = webview.evaluate_script(&format!(
        "window.__mdReaderSettingsFinished({ok}, {message_json});"
    ));
}

fn notify_custom_css_picked(webview: &WebView, result: Result<String>) {
    let (ok, payload) = match result {
        Ok(css) => (true, css),
        Err(error) => (false, error.to_string()),
    };
    let payload_json =
        serde_json::to_string(&payload).unwrap_or_else(|_| "\"custom CSS picker failed\"".into());
    let _ = webview.evaluate_script(&format!(
        "window.__mdReaderCustomCssPicked({ok}, {payload_json});"
    ));
}

fn app_settings_dir() -> PathBuf {
    let base = env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| {
            env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|path| path.join("AppData").join("Roaming"))
        })
        .unwrap_or_else(env::temp_dir);
    base.join("Markdown Reader")
}

fn app_settings_path() -> PathBuf {
    app_settings_dir().join("settings.json")
}

fn app_themes_path() -> PathBuf {
    app_settings_dir().join("themes")
}

fn load_app_settings() -> AppSettings {
    let path = app_settings_path();
    let Ok(body) = fs::read_to_string(path) else {
        return AppSettings::default();
    };
    serde_json::from_str::<AppSettings>(&body)
        .map(|settings| normalize_settings(&settings))
        .unwrap_or_default()
}

fn save_app_settings(path: &Path, settings: &AppSettings) -> Result<()> {
    let settings = normalize_settings(settings);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create settings directory {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&settings)?;
    fs::write(path, body.as_bytes())
        .with_context(|| format!("failed to save settings {}", path.display()))
}

fn export_builtin_theme_templates(themes_path: &Path) -> Result<()> {
    fs::create_dir_all(themes_path).with_context(|| {
        format!(
            "failed to create theme template directory {}",
            themes_path.display()
        )
    })?;

    for theme in THEMES {
        let path = themes_path.join(format!("{}.css", theme.id));
        if path.exists() {
            continue;
        }
        fs::write(&path, theme.css.as_bytes())
            .with_context(|| format!("failed to save theme template {}", path.display()))?;
    }

    Ok(())
}

fn pick_custom_css(themes_path: &Path) -> Result<Option<String>> {
    export_builtin_theme_templates(themes_path)?;
    let Some(path) = FileDialog::new()
        .set_title("Choose custom Markdown theme CSS")
        .set_directory(themes_path)
        .add_filter("CSS", &["css"])
        .pick_file()
    else {
        return Ok(None);
    };

    fs::read_to_string(&path)
        .with_context(|| format!("failed to read custom CSS {}", path.display()))
        .map(Some)
}

fn normalize_settings(settings: &AppSettings) -> AppSettings {
    let custom_css = sanitize_settings_css(
        &settings
            .custom_css
            .chars()
            .take(262_144)
            .collect::<String>(),
    );
    let theme_id = if settings.theme_id == "custom" && !custom_css.trim().is_empty() {
        "custom".to_string()
    } else if THEMES.iter().any(|theme| theme.id == settings.theme_id) {
        settings.theme_id.clone()
    } else {
        "clean".to_string()
    };

    AppSettings {
        theme_id,
        custom_css,
    }
}

fn sanitize_settings_css(css: &str) -> String {
    css.lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            !lower.contains("@import")
                && !lower.contains("http://")
                && !lower.contains("https://")
                && !lower.contains("javascript:")
                && !lower.contains("expression(")
                && !lower.contains("behavior:")
                && !lower.contains("-moz-binding")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeadingEntry {
    level: usize,
    text: String,
    slug: String,
}

fn expand_toc_markers(markdown: &str) -> String {
    if !markdown
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("[toc]"))
    {
        return markdown.to_string();
    }

    let headings = collect_markdown_headings(markdown);
    if headings.is_empty() {
        return markdown.to_string();
    }

    let toc = render_markdown_toc(&headings);
    let mut expanded = Vec::new();
    for line in markdown.lines() {
        if line.trim().eq_ignore_ascii_case("[toc]") {
            expanded.push(toc.clone());
        } else {
            expanded.push(line.to_string());
        }
    }

    let mut result = expanded.join("\n");
    if markdown.ends_with('\n') || markdown.ends_with('\r') {
        result.push('\n');
    }
    result
}

fn collect_markdown_headings(markdown: &str) -> Vec<HeadingEntry> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut headings = Vec::new();
    let mut used_slugs = HashMap::new();
    let mut in_fence = false;
    let mut fence_char = '\0';
    let mut index = 0;

    while index < lines.len() {
        let trimmed = lines[index].trim();
        if let Some(marker) = fence_marker(trimmed) {
            if in_fence && marker == fence_char {
                in_fence = false;
            } else if !in_fence {
                in_fence = true;
                fence_char = marker;
            }
            index += 1;
            continue;
        }

        if in_fence {
            index += 1;
            continue;
        }

        if let Some((level, text)) = parse_atx_heading(lines[index]) {
            let slug = unique_heading_slug(&text, headings.len(), &mut used_slugs);
            headings.push(HeadingEntry { level, text, slug });
            index += 1;
            continue;
        }

        if index + 1 < lines.len() {
            if let Some(level) = parse_setext_underline(lines[index + 1]) {
                let text = clean_heading_text(lines[index].trim());
                if !text.is_empty() {
                    let slug = unique_heading_slug(&text, headings.len(), &mut used_slugs);
                    headings.push(HeadingEntry { level, text, slug });
                    index += 2;
                    continue;
                }
            }
        }

        index += 1;
    }

    headings
}

fn render_markdown_toc(headings: &[HeadingEntry]) -> String {
    let min_level = headings
        .iter()
        .map(|heading| heading.level)
        .min()
        .unwrap_or(1);
    headings
        .iter()
        .map(|heading| {
            let indent = "  ".repeat(heading.level.saturating_sub(min_level));
            format!(
                "{indent}- [{}](#{})",
                escape_toc_link_text(&heading.text),
                heading.slug
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_atx_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    let level = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if !(1..=6).contains(&level) {
        return None;
    }

    let remainder = &trimmed[level..];
    if !remainder.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }

    let mut text = remainder.trim();
    if let Some(stripped) = text.strip_suffix('#') {
        text = stripped.trim_end_matches('#').trim_end();
    }
    Some((level, clean_heading_text(text)))
}

fn parse_setext_underline(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    if trimmed.len() < 2 {
        return None;
    }

    if trimmed.chars().all(|character| character == '=') {
        return Some(1);
    }
    if trimmed.chars().all(|character| character == '-') {
        return Some(2);
    }
    None
}

fn fence_marker(trimmed_line: &str) -> Option<char> {
    if trimmed_line.starts_with("```") {
        return Some('`');
    }
    if trimmed_line.starts_with("~~~") {
        return Some('~');
    }
    None
}

fn clean_heading_text(text: &str) -> String {
    let mut cleaned = text.trim().to_string();
    if let Some(start) = cleaned.rfind(" {#") {
        if cleaned.ends_with('}') {
            cleaned.truncate(start);
        }
    }
    cleaned.trim().to_string()
}

fn escape_toc_link_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

fn unique_heading_slug(
    text: &str,
    section_index: usize,
    used_slugs: &mut HashMap<String, usize>,
) -> String {
    let base = slugify_heading(text).unwrap_or_else(|| format!("section-{section_index}"));
    let count = used_slugs.entry(base.clone()).or_insert(0);
    let slug = if *count == 0 {
        base
    } else {
        format!("{base}-{count}")
    };
    *count += 1;
    slug
}

fn slugify_heading(text: &str) -> Option<String> {
    let mut slug = String::new();
    let mut last_was_separator = false;

    for character in text.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            last_was_separator = false;
        } else if (character.is_whitespace() || matches!(character, '-' | '_' | '.' | '/' | ':'))
            && !last_was_separator
            && !slug.is_empty()
        {
            slug.push('-');
            last_was_separator = true;
        }
    }

    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn render_markdown_safely(markdown: &str) -> String {
    let markdown = expand_toc_markers(markdown);
    let heading_ids = collect_markdown_headings(&markdown)
        .into_iter()
        .map(|heading| heading.slug)
        .collect::<Vec<_>>();
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);

    let parser = Parser::new_ext(&markdown, options);
    let mut next_heading_index = 0usize;
    let indexed_events = parser.map(move |event| match event {
        Event::Start(Tag::Heading { level, .. }) => {
            let tag = heading_tag(level);
            let index = next_heading_index;
            next_heading_index += 1;
            let heading_id = heading_ids
                .get(index)
                .cloned()
                .unwrap_or_else(|| format!("section-{index}"));
            // The data attribute is the bridge between Rust-rendered headings and
            // JavaScript scroll restoration. The id is human-readable for TOC
            // links; the data index remains stable while the user edits headings.
            Event::Html(
                format!(r#"<{tag} id="{heading_id}" data-md-section-index="{index}">"#).into(),
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
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; base-uri 'none'; object-src 'none'; frame-src 'none'; connect-src 'none'; img-src data: file:; media-src data: file:; font-src data: file:; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
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

    .format-tools {
      display: flex;
      align-items: center;
      gap: 4px;
    }

    body.editing .format-tools {
      display: none;
    }

    .format-select {
      width: 70px;
      min-height: 28px;
      padding: 2px 6px;
      color: var(--ink);
      background: var(--surface);
      border: 1px solid var(--border);
      border-radius: 4px;
      font: 13px/1.2 "Segoe UI", system-ui, sans-serif;
    }

    .format-divider {
      width: 1px;
      height: 20px;
      margin: 0 3px;
      background: var(--border);
    }

    .format-button {
      font: 600 13px/1 "Segoe UI", system-ui, sans-serif;
    }

    .format-button.italic-label {
      font-style: italic;
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

    .settings-body select,
    .settings-button {
      width: 100%;
      min-height: 32px;
      color: var(--ink);
      background: var(--surface);
      border: 1px solid var(--border);
      border-radius: 4px;
      padding: 4px 8px;
      font: 14px/1.3 "Segoe UI", system-ui, sans-serif;
    }

    .settings-button {
      text-align: left;
      cursor: pointer;
    }

    .settings-button:hover {
      background: var(--frame);
    }
  </style>
  <style id="theme-style"></style>
  <style id="custom-theme-style"></style>
</head>
<body>
  <nav>
    <div id="format-toolbar" class="format-tools" aria-label="Formatting">
      <select id="block-format" class="format-select" title="Block format" aria-label="Block format">
        <option value="p">P</option>
        <option value="h1">H1</option>
        <option value="h2">H2</option>
        <option value="h3">H3</option>
        <option value="h4">H4</option>
        <option value="blockquote">Quote</option>
        <option value="pre">Code</option>
      </select>
      <span class="format-divider"></span>
      <button class="icon-button format-button" type="button" data-command="bold" title="Bold" aria-label="Bold">B</button>
      <button class="icon-button format-button italic-label" type="button" data-command="italic" title="Italic" aria-label="Italic">I</button>
      <button id="inline-code-button" class="icon-button format-button" type="button" title="Inline code" aria-label="Inline code">{}</button>
      <button id="link-button" class="icon-button" type="button" title="Link" aria-label="Link">
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M10 13a5 5 0 0 0 7.1 0l2-2a5 5 0 0 0-7.1-7.1l-1.1 1.1"></path>
          <path d="M14 11a5 5 0 0 0-7.1 0l-2 2A5 5 0 0 0 12 20.1l1.1-1.1"></path>
        </svg>
      </button>
      <span class="format-divider"></span>
      <button class="icon-button" type="button" data-command="insertUnorderedList" title="Bullet list" aria-label="Bullet list">
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M8 6h12"></path>
          <path d="M8 12h12"></path>
          <path d="M8 18h12"></path>
          <path d="M4 6h.01"></path>
          <path d="M4 12h.01"></path>
          <path d="M4 18h.01"></path>
        </svg>
      </button>
      <button class="icon-button" type="button" data-command="insertOrderedList" title="Numbered list" aria-label="Numbered list">
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M10 6h10"></path>
          <path d="M10 12h10"></path>
          <path d="M10 18h10"></path>
          <path d="M4 6h1v4"></path>
          <path d="M4 10h2"></path>
          <path d="M4 14h2l-2 4h2"></path>
        </svg>
      </button>
    </div>
    <div class="nav-spacer"></div>
    <button id="save-button" class="icon-button saved" type="button" title="Save (Ctrl+S)" aria-label="Save">
      <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
        <path d="M5 3h12l2 2v16H5z"></path>
        <path d="M8 3v6h8V3"></path>
        <path d="M8 21v-7h8v7"></path>
      </svg>
    </button>
    <label class="switch" title="Plaintext: Alt+Left. Formatted: Alt+Right.">
      <input id="mode-toggle" type="checkbox" aria-label="Toggle rendered Markdown view" title="Plaintext: Alt+Left. Formatted: Alt+Right.">
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
        <label class="field" for="custom-css-button">
          Custom CSS
          <button id="custom-css-button" class="settings-button" type="button">Choose CSS...</button>
        </label>
      </div>
    </section>
  </div>
  <script>
    const initialMarkdown = __INITIAL_MARKDOWN__;
    const initialRendered = __INITIAL_RENDERED__;
    const themes = __THEMES__;
    const initialSettings = __SETTINGS__;
    const editor = document.getElementById("editor");
    const rendered = document.getElementById("rendered");
    const toggle = document.getElementById("mode-toggle");
    const saveButton = document.getElementById("save-button");
    const settingsButton = document.getElementById("settings-button");
    const settingsBackdrop = document.getElementById("settings-backdrop");
    const settingsClose = document.getElementById("settings-close");
    const themeSelect = document.getElementById("theme-select");
    const customCssButton = document.getElementById("custom-css-button");
    const themeStyle = document.getElementById("theme-style");
    const customThemeStyle = document.getElementById("custom-theme-style");
    const blockFormat = document.getElementById("block-format");
    const inlineCodeButton = document.getElementById("inline-code-button");
    const linkButton = document.getElementById("link-button");
    let dirty = false;
    let renderedDirty = false;
    let customCss = sanitizeCustomCss(initialSettings.customCss || "");
    let savedRenderedRange = null;

    editor.value = initialMarkdown;
    rendered.innerHTML = initialRendered;

    function postMessage(payload) {
      window.ipc.postMessage(JSON.stringify(payload));
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

    function showRenderedView() {
      if (toggle.checked) {
        return;
      }
      toggle.checked = true;
      switchToRendered();
    }

    function showPlaintextView() {
      if (!toggle.checked) {
        return;
      }
      switchToEditor();
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
        return protectParagraphMarkdown(normalizeText(node.textContent).trim());
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
        return protectParagraphMarkdown(inlineMarkdown(node).trim());
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
        return normalizeText(node.textContent);
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
        const alt = normalizeText(node.getAttribute("alt") || "");
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

    function protectParagraphMarkdown(text) {
      if (/^\s*(#{1,6}\s|[-+*]\s+|\d+[.)]\s+|>\s+|\[toc\]\s*$)/i.test(text)) {
        return text.replace(/^(\s*)(\S)/, "$1\\$2");
      }
      return text;
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

    function applyTheme(themeId, persist = true) {
      const fallback = themes[0];
      const selected = themes.find((theme) => theme.id === themeId) || fallback;
      if (themeId === "custom" && customCss) {
        themeStyle.textContent = fallback.css;
        customThemeStyle.textContent = customCss;
      } else {
        themeStyle.textContent = selected.css;
        customThemeStyle.textContent = "";
      }
      if (persist) {
        saveThemeSettings(themeId);
      }
    }

    function saveThemeSettings(themeId) {
      postMessage({
        kind: "saveSettings",
        settings: {
          themeId,
          customCss
        }
      });
    }

    function pickCustomCss() {
      postMessage({ kind: "pickCustomCss" });
    }

    function sanitizeCustomCss(css) {
      return normalizeText(css)
        .slice(0, 262144)
        .split(/\r\n|\r|\n/)
        .filter((line) => !/@import|https?:\/\/|javascript:|expression\s*\(|behavior:|-moz-binding/i.test(line))
        .join("\n");
    }

    function openSettings() {
      settingsBackdrop.hidden = false;
      themeSelect.focus();
    }

    function closeSettings() {
      settingsBackdrop.hidden = true;
      settingsButton.focus();
    }

    function markRenderedEdited() {
      renderedDirty = true;
      setDirty(true);
      saveRenderedSelection();
    }

    function saveRenderedSelection() {
      const selection = window.getSelection();
      if (!selection || selection.rangeCount === 0) {
        return;
      }
      if (selection.anchorNode && selection.focusNode && rendered.contains(selection.anchorNode) && rendered.contains(selection.focusNode)) {
        savedRenderedRange = selection.getRangeAt(0).cloneRange();
      }
    }

    function restoreRenderedSelection() {
      rendered.focus();
      if (!savedRenderedRange) {
        return;
      }
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(savedRenderedRange);
    }

    function applyDocumentCommand(command, value = null) {
      restoreRenderedSelection();
      document.execCommand(command, false, value);
      markRenderedEdited();
    }

    function applyBlockFormat(tag) {
      restoreRenderedSelection();
      document.execCommand("formatBlock", false, `<${tag}>`);
      markRenderedEdited();
    }

    function applyInlineCode() {
      restoreRenderedSelection();
      const selection = window.getSelection();
      if (!selection || selection.rangeCount === 0 || !selection.anchorNode || !rendered.contains(selection.anchorNode)) {
        return;
      }

      const selectedText = selection.toString() || "code";
      document.execCommand("insertHTML", false, `<code>${escapeHtml(selectedText)}</code>`);
      markRenderedEdited();
    }

    function applyLink() {
      restoreRenderedSelection();
      const url = window.prompt("URL");
      if (!url) {
        return;
      }

      const selection = window.getSelection();
      if (!selection || selection.rangeCount === 0 || selection.isCollapsed) {
        const label = window.prompt("Text") || url;
        document.execCommand("insertHTML", false, `<a href="${escapeAttribute(url)}">${escapeHtml(label)}</a>`);
      } else {
        document.execCommand("createLink", false, url);
      }
      markRenderedEdited();
    }

    function escapeHtml(text) {
      return normalizeText(text)
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;");
    }

    function escapeAttribute(text) {
      return escapeHtml(text).replace(/"/g, "&quot;");
    }

    // Rendering stays in Rust so opened Markdown is processed by pulldown-cmark
    // and ammonia before it is assigned to innerHTML.
    window.__mdReaderApplyRendered = (html, expandedMarkdown, marker) => {
      const previousMarkdown = editor.value;
      editor.value = expandedMarkdown;
      rendered.innerHTML = html;
      renderedDirty = false;
      if (expandedMarkdown !== previousMarkdown) {
        setDirty(true);
      }
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

    window.__mdReaderSettingsFinished = (ok, message) => {
      if (!ok) {
        alert(message || "Settings save failed.");
      }
    };

    window.__mdReaderCustomCssPicked = (ok, payload) => {
      if (!ok) {
        alert(payload || "Custom CSS picker failed.");
        return;
      }
      customCss = sanitizeCustomCss(payload);
      addCustomThemeOption();
      themeSelect.value = "custom";
      applyTheme("custom");
    };

    window.__mdReaderRequestClose = () => {
      if (!dirty || window.confirm("Close without saving changes?")) {
        postMessage({ kind: "close" });
      }
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

    customCssButton.addEventListener("click", pickCustomCss);

    document.querySelectorAll("[data-command]").forEach((button) => {
      button.addEventListener("mousedown", (event) => event.preventDefault());
      button.addEventListener("click", () => applyDocumentCommand(button.dataset.command));
    });

    blockFormat.addEventListener("change", () => {
      applyBlockFormat(blockFormat.value);
    });

    inlineCodeButton.addEventListener("mousedown", (event) => event.preventDefault());
    inlineCodeButton.addEventListener("click", applyInlineCode);

    linkButton.addEventListener("mousedown", (event) => event.preventDefault());
    linkButton.addEventListener("click", applyLink);

    editor.addEventListener("input", () => {
      setDirty(true);
    });

    rendered.addEventListener("input", () => {
      renderedDirty = true;
      setDirty(true);
      saveRenderedSelection();
    });

    rendered.addEventListener("keyup", saveRenderedSelection);
    rendered.addEventListener("mouseup", saveRenderedSelection);

    document.addEventListener("selectionchange", () => {
      if (!document.body.classList.contains("editing")) {
        saveRenderedSelection();
      }
    });

    document.addEventListener("keydown", (event) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
        event.preventDefault();
        saveCurrentDocument();
      }
      if (event.altKey && !event.ctrlKey && !event.metaKey && event.key === "ArrowLeft") {
        event.preventDefault();
        showPlaintextView();
      }
      if (event.altKey && !event.ctrlKey && !event.metaKey && event.key === "ArrowRight") {
        event.preventDefault();
        showRenderedView();
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
        const href = anchor.getAttribute("href") || "";
        if (href.startsWith('#')) {
          const target = document.getElementById(href.slice(1));
          if (target) {
            rendered.scrollTop = Math.max(0, target.offsetTop - 28);
          }
        }
      }
    });

    populateThemes();
    const storedTheme = initialSettings.themeId || "clean";
    const themeIsKnown = storedTheme === "custom" ? Boolean(customCss) : themes.some((theme) => theme.id === storedTheme);
    const initialTheme = themeIsKnown ? storedTheme : "clean";
    themeSelect.value = initialTheme;
    applyTheme(initialTheme, false);
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
        assert!(html.contains(r#"id="one""#));
    }

    #[test]
    fn renderer_strips_raw_scripts() {
        let html = render_markdown_safely("# Safe\n\n<script>alert('x')</script>");

        assert!(!html.to_lowercase().contains("<script"));
        assert!(!html.contains("alert"));
    }

    #[test]
    fn app_html_includes_theme_and_control_data() {
        let html =
            build_app_html("# One", &AppSettings::default()).expect("app html should render");

        assert!(!html.contains("__THEMES__"));
        assert!(!html.contains("__SETTINGS__"));
        assert!(html.contains("save-button"));
        assert!(html.contains("Save (Ctrl+S)"));
        assert!(html.contains("settings-button"));
        assert!(html.contains("format-toolbar"));
        assert!(html.contains(r#"grid-template-rows: 34px minmax(0, 1fr)"#));
        assert!(html.contains("contenteditable=\"true\""));
        assert!(html.contains(r#""themeId":"clean""#));
        assert!(html.contains(r#"kind: "saveSettings""#));
        assert!(html.contains("Plaintext: Alt+Left. Formatted: Alt+Right."));
        assert!(html.contains("showPlaintextView"));
        assert!(html.contains("showRenderedView"));
        assert!(html.contains("custom-css-button"));
        assert!(html.contains("pickCustomCss"));
        assert!(html.contains("__mdReaderCustomCssPicked"));
        assert!(!html.contains("type=\"file\""));
        assert!(!html.contains("localStorage"));
    }

    #[test]
    fn app_html_requires_explicit_save() {
        let html = build_app_html("[toc]\n\n# One", &AppSettings::default())
            .expect("app html should render");

        assert!(html.contains("__mdReaderRequestClose"));
        assert!(html.contains("Close without saving changes?"));
        assert!(!html.contains("closeSave"));
    }

    #[test]
    fn built_in_themes_are_embedded() {
        assert!(THEMES.len() >= 3);
        assert!(THEMES.iter().all(|theme| theme.css.contains(":root")));
        assert!(THEMES.iter().all(|theme| !theme.css.contains("#rendered")));
    }

    #[test]
    fn runtime_window_icon_bytes_match_declared_size() {
        assert_eq!(
            APP_ICON_RGBA.len(),
            (APP_ICON_WIDTH * APP_ICON_HEIGHT * 4) as usize
        );
    }

    #[test]
    fn toc_marker_expands_to_markdown_links() {
        let expanded = expand_toc_markers("[toc]\n\n# One\n\n## Two\n");

        assert!(expanded.contains("- [One](#one)"));
        assert!(expanded.contains("  - [Two](#two)"));
        assert!(!expanded.contains("[toc]"));
    }

    #[test]
    fn renderer_expands_toc_marker_to_internal_links() {
        let html = render_markdown_safely("[toc]\n\n# One");

        assert!(html.contains(r##"href="#one""##));
        assert!(html.contains(r#"data-md-section-index="0""#));
    }

    #[test]
    fn formatted_serializer_no_longer_uses_global_inline_escaping() {
        assert!(APP_HTML_TEMPLATE.contains("function protectParagraphMarkdown"));
        assert!(!APP_HTML_TEMPLATE.contains(r#"replace(/[\\`*_{}\[\]<>]/g"#));
    }

    #[test]
    fn settings_default_unknown_themes_to_clean() {
        let settings = normalize_settings(&AppSettings {
            theme_id: "missing".to_string(),
            custom_css: String::new(),
        });

        assert_eq!(settings.theme_id, "clean");
        assert!(settings.custom_css.is_empty());
    }

    #[test]
    fn settings_allow_custom_theme_only_with_css() {
        let without_css = normalize_settings(&AppSettings {
            theme_id: "custom".to_string(),
            custom_css: String::new(),
        });
        let with_css = normalize_settings(&AppSettings {
            theme_id: "custom".to_string(),
            custom_css: "body { color: #123; }".to_string(),
        });

        assert_eq!(without_css.theme_id, "clean");
        assert_eq!(with_css.theme_id, "custom");
        assert_eq!(with_css.custom_css, "body { color: #123; }");
    }

    #[test]
    fn settings_css_sanitizer_removes_remote_and_script_like_css() {
        let settings = normalize_settings(&AppSettings {
            theme_id: "custom".to_string(),
            custom_css: "@import url('https://example.com/x.css');\nbody { color: #123; }\na { background: url(http://example.com/x.png); }\ndiv { width: expression(alert(1)); }"
                .to_string(),
        });

        assert_eq!(settings.theme_id, "custom");
        assert_eq!(settings.custom_css, "body { color: #123; }");
    }

    #[test]
    fn exports_default_theme_templates_without_overwriting_existing_files() {
        let themes_path =
            env::temp_dir().join(format!("markdown-reader-theme-test-{}", std::process::id()));
        if themes_path.exists() {
            fs::remove_dir_all(&themes_path).expect("old theme test dir should be removable");
        }

        export_builtin_theme_templates(&themes_path).expect("theme templates should export");
        for theme in THEMES {
            let path = themes_path.join(format!("{}.css", theme.id));
            assert!(path.exists(), "{} should exist", path.display());
            assert_eq!(
                fs::read_to_string(&path).expect("theme template should be readable"),
                theme.css
            );
        }

        let clean_path = themes_path.join("clean.css");
        fs::write(&clean_path, "custom edit").expect("test theme should be writable");
        export_builtin_theme_templates(&themes_path).expect("theme templates should export again");
        assert_eq!(
            fs::read_to_string(&clean_path).expect("test theme should be readable"),
            "custom edit"
        );

        fs::remove_dir_all(&themes_path).expect("theme test dir should be removable");
    }
}
