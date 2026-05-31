#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    fs,
    io::Read,
    iter::Peekable,
    net::{IpAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use pulldown_cmark::{html, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use tao::{
    dpi::LogicalSize,
    event::{Event as TaoEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::{Icon, Window, WindowBuilder},
};
use url::Url;
use wry::{http::Request, NewWindowResponse, WebView, WebViewBuilder};

const WINDOW_WIDTH: f64 = 1200.0;
const WINDOW_HEIGHT: f64 = 900.0;
const APP_ICON_WIDTH: u32 = 32;
const APP_ICON_HEIGHT: u32 = 32;
const APP_ICON_RGBA: &[u8] = include_bytes!("../assets/app-icon-32.rgba");
const LINK_BEHAVIOR_NEW_WINDOW: &str = "newWindow";
const LINK_BEHAVIOR_NAVIGATE: &str = "navigate";
const REMOTE_IMAGE_MAX_REDIRECTS: u32 = 3;
const REMOTE_IMAGE_MAX_BYTES: u64 = 10 * 1024 * 1024;
const REMOTE_IMAGE_MAX_PER_WINDOW: usize = 64;
const EMBEDDED_IMAGE_MAX_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_ALLOWED_LAUNCH_EXTENSIONS: &[&str] = &[
    "bmp", "csv", "doc", "docx", "gif", "htm", "html", "jpeg", "jpg", "json", "log", "md",
    "markdown", "odp", "ods", "odt", "pdf", "png", "ppt", "pptx", "rtf", "toml", "tsv", "txt",
    "webp", "xls", "xlsx", "xml", "yaml", "yml",
];
const DEFAULT_IMAGE_EXTENSIONS: &[&str] = &["bmp", "gif", "jpeg", "jpg", "png", "webp"];
const TRANSPARENT_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff, 0xff,
    0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

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

fn window_title_for_path(input_path: &Path) -> String {
    input_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("Markdown Reader - {name}"))
        .unwrap_or_else(|| "Markdown Reader".to_string())
}

fn launch_window(mut input_path: PathBuf, markdown: String, settings: AppSettings) -> Result<()> {
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let image_proxy = event_loop.create_proxy();
    let settings_path = app_settings_path();
    let themes_path = app_themes_path();
    let window_title = window_title_for_path(&input_path);
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
            Ok(IpcMessage::PickImage { embed_base64 }) => {
                let _ = proxy.send_event(AppEvent::PickImage { embed_base64 });
            }
            Ok(IpcMessage::OpenLink { href, behavior }) => {
                let _ = proxy.send_event(AppEvent::OpenLink { href, behavior });
            }
            Ok(IpcMessage::GoBack) => {
                let _ = proxy.send_event(AppEvent::GoBack);
            }
            Err(error) => eprintln!("invalid IPC message: {error}"),
        };

    let mut current_settings = normalize_settings(&settings);
    let mut image_cache = ImageCache::new(image_proxy).context("failed to create image cache")?;
    let initial_html = build_app_html(&markdown, &input_path, &current_settings, &mut image_cache)?;
    let builder = WebViewBuilder::new()
        .with_html(initial_html)
        .with_ipc_handler(handler)
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny);
    let webview = builder
        .build(&window)
        .context("failed to create WebView2 surface")?;
    let mut webview = Some(webview);
    let mut document_history = Vec::<PathBuf>::new();

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
                        image_cache.cleanup();
                        let _ = webview.take();
                        *control_flow = ControlFlow::Exit;
                    }
                } else {
                    image_cache.cleanup();
                    *control_flow = ControlFlow::Exit;
                }
            }
            TaoEvent::UserEvent(AppEvent::Render { markdown, marker }) => {
                if let Some(webview) = webview.as_ref() {
                    let expanded_markdown = expand_toc_markers(&markdown);
                    let html = render_markdown_for_document(
                        &expanded_markdown,
                        &input_path,
                        &current_settings,
                        &mut image_cache,
                    );
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
                image_cache.cleanup();
                let _ = webview.take();
                *control_flow = ControlFlow::Exit;
            }
            TaoEvent::UserEvent(AppEvent::SaveSettings { settings }) => {
                if let Some(webview) = webview.as_ref() {
                    let settings = normalize_settings(&settings);
                    let result = save_app_settings(&settings_path, &settings);
                    if result.is_ok() {
                        current_settings = settings;
                    }
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
            TaoEvent::UserEvent(AppEvent::PickImage { embed_base64 }) => {
                if let Some(webview) = webview.as_ref() {
                    match pick_markdown_image(&input_path, embed_base64) {
                        Ok(Some(image)) => notify_image_picked(webview, Ok(image)),
                        Ok(None) => {}
                        Err(error) => notify_image_picked(webview, Err(error)),
                    }
                }
            }
            TaoEvent::UserEvent(AppEvent::OpenLink { href, behavior }) => {
                let behavior = normalize_link_click_behavior(&behavior);
                let previous_path = input_path.clone();
                let result = if behavior == LINK_BEHAVIOR_NAVIGATE {
                    navigate_to_link_target(
                        &mut input_path,
                        webview.as_ref(),
                        &window,
                        &href,
                        &current_settings,
                        &mut image_cache,
                    )
                } else {
                    open_link_target(&input_path, &href, &current_settings)
                };
                match result {
                    Ok(()) => {
                        if behavior == LINK_BEHAVIOR_NAVIGATE && input_path != previous_path {
                            document_history.push(previous_path);
                            if let Some(webview) = webview.as_ref() {
                                notify_back_history_changed(webview, true);
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(webview) = webview.as_ref() {
                            notify_open_link_failed(webview, &error.to_string());
                        } else {
                            eprintln!("failed to open link: {error:#}");
                        }
                    }
                }
            }
            TaoEvent::UserEvent(AppEvent::GoBack) => {
                let Some(previous_path) = document_history.pop() else {
                    if let Some(webview) = webview.as_ref() {
                        notify_back_history_changed(webview, false);
                    }
                    return;
                };

                match load_document_path(
                    &mut input_path,
                    previous_path.clone(),
                    webview.as_ref(),
                    &window,
                    &current_settings,
                    &mut image_cache,
                ) {
                    Ok(()) => {
                        if let Some(webview) = webview.as_ref() {
                            notify_back_history_changed(webview, !document_history.is_empty());
                        }
                    }
                    Err(error) => {
                        document_history.push(previous_path);
                        if let Some(webview) = webview.as_ref() {
                            notify_back_history_changed(webview, !document_history.is_empty());
                            notify_open_link_failed(webview, &error.to_string());
                        } else {
                            eprintln!("failed to go back: {error:#}");
                        }
                    }
                }
            }
            TaoEvent::UserEvent(AppEvent::RemoteImageReady { source, render_src }) => {
                image_cache.mark_remote_render_src(&source, render_src.clone());
                if current_settings.allow_remote_images {
                    if let Some(webview) = webview.as_ref() {
                        notify_remote_image_ready(webview, &source, render_src.as_deref());
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
    PickImage {
        embed_base64: bool,
    },
    OpenLink {
        href: String,
        behavior: String,
    },
    GoBack,
    RemoteImageReady {
        source: String,
        render_src: Option<String>,
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
    Close,
    SaveSettings {
        settings: AppSettings,
    },
    PickCustomCss,
    PickImage {
        #[serde(rename = "embedBase64")]
        embed_base64: bool,
    },
    OpenLink {
        href: String,
        behavior: String,
    },
    GoBack,
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
    #[serde(default)]
    allowed_launch_extensions: Vec<String>,
    #[serde(default)]
    link_click_behavior: String,
    #[serde(default)]
    allow_remote_images: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImagePickResult {
    source: String,
    preview_src: String,
    alt: String,
    embedded: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ImageSizeAttributes {
    width: Option<String>,
    height: Option<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme_id: "clean".to_string(),
            custom_css: String::new(),
            allowed_launch_extensions: Vec::new(),
            link_click_behavior: LINK_BEHAVIOR_NEW_WINDOW.to_string(),
            allow_remote_images: false,
        }
    }
}

struct ImageCache {
    root: PathBuf,
    remote_urls: HashMap<String, RemoteImageState>,
    next_file_index: usize,
    placeholder_render_src: Option<String>,
    proxy: EventLoopProxy<AppEvent>,
}

impl ImageCache {
    fn new(proxy: EventLoopProxy<AppEvent>) -> Result<Self> {
        let root = create_window_image_cache_dir()?;

        Ok(Self {
            root,
            remote_urls: HashMap::new(),
            next_file_index: 0,
            placeholder_render_src: None,
            proxy,
        })
    }

    fn remote_render_src(&mut self, image_url: &str) -> Result<String> {
        if image_url.len() > 4096 {
            bail!("remote image URL is too long");
        }
        let image_url = validate_remote_image_url(image_url)?.to_string();
        match self.remote_urls.get(&image_url) {
            Some(RemoteImageState::Ready(cached_url)) => return Ok(cached_url.clone()),
            Some(RemoteImageState::Pending | RemoteImageState::Failed) => {
                return self.placeholder_render_src();
            }
            None => {}
        }
        if self.remote_urls.len() >= REMOTE_IMAGE_MAX_PER_WINDOW {
            bail!("too many remote images in this window");
        }

        let file_index = self.next_file_index;
        self.next_file_index += 1;
        self.remote_urls
            .insert(image_url.clone(), RemoteImageState::Pending);
        let root = self.root.clone();
        let proxy = self.proxy.clone();
        let source = image_url.clone();
        std::thread::spawn(move || {
            let render_src = fetch_remote_image_to_cache(&source, &root, file_index).ok();
            let _ = proxy.send_event(AppEvent::RemoteImageReady { source, render_src });
        });

        self.placeholder_render_src()
    }

    fn mark_remote_render_src(&mut self, source: &str, render_src: Option<String>) {
        let state = match render_src {
            Some(render_src) => RemoteImageState::Ready(render_src),
            None => RemoteImageState::Failed,
        };
        self.remote_urls.insert(source.to_string(), state);
    }

    fn placeholder_render_src(&mut self) -> Result<String> {
        if let Some(render_src) = &self.placeholder_render_src {
            return Ok(render_src.clone());
        }

        let render_src = format!("data:image/gif;base64,{}", encode_base64(TRANSPARENT_GIF));
        self.placeholder_render_src = Some(render_src.clone());
        Ok(render_src)
    }

    fn cleanup(&mut self) {
        if self.root.exists() {
            let _ = fs::remove_dir_all(&self.root);
        }
        self.remote_urls.clear();
        self.placeholder_render_src = None;
    }
}

enum RemoteImageState {
    Pending,
    Ready(String),
    Failed,
}

fn fetch_remote_image_to_cache(image_url: &str, root: &Path, file_index: usize) -> Result<String> {
    let request_url = validate_remote_image_url(image_url)?;
    let agent = ureq::AgentBuilder::new()
        .https_only(true)
        .redirects(0)
        .timeout_connect(Duration::from_secs(2))
        .timeout_read(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build();
    let response = fetch_remote_image_response(&agent, request_url)?;

    let content_type = response
        .header("Content-Type")
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let extension_from_type = image_extension_from_content_type(&content_type);
    if !content_type.is_empty()
        && content_type != "application/octet-stream"
        && extension_from_type.is_none()
    {
        bail!("remote image returned unsupported content type");
    }

    if let Some(length) = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
    {
        if length > REMOTE_IMAGE_MAX_BYTES {
            bail!("remote image is larger than the configured limit");
        }
    }

    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(REMOTE_IMAGE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to read remote image body")?;
    if bytes.len() as u64 > REMOTE_IMAGE_MAX_BYTES {
        bail!("remote image exceeded the configured limit");
    }
    let extension = image_extension_from_magic(&bytes)
        .context("remote image body did not match a supported image format")?;
    if let Some(content_type_extension) = extension_from_type {
        if content_type_extension != extension {
            bail!("remote image content type did not match image bytes");
        }
    }

    let path = root.join(format!("remote-image-{file_index:04}.{extension}"));
    let partial_path = path.with_extension(format!("{extension}.part"));
    fs::write(&partial_path, bytes)
        .with_context(|| format!("failed to cache remote image {}", path.display()))?;
    fs::rename(&partial_path, &path)
        .with_context(|| format!("failed to finalize cached image {}", path.display()))?;
    image_data_uri_from_path(&path)
}

fn fetch_remote_image_response(
    agent: &ureq::Agent,
    mut request_url: Url,
) -> Result<ureq::Response> {
    for redirect_count in 0..=REMOTE_IMAGE_MAX_REDIRECTS {
        validate_remote_image_host_addresses(&request_url)?;
        let response = match agent
            .get(request_url.as_str())
            .set(
                "Accept",
                "image/png,image/jpeg,image/gif,image/webp,image/bmp;q=0.8",
            )
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) if is_redirect_status(status) => response,
            Err(ureq::Error::Status(status, _)) => {
                bail!("remote image returned HTTP {status}");
            }
            Err(error) => return Err(error).context("failed to fetch remote image"),
        };

        if is_redirect_status(response.status()) {
            if redirect_count == REMOTE_IMAGE_MAX_REDIRECTS {
                bail!("remote image exceeded redirect limit");
            }
            let location = response
                .header("Location")
                .context("remote image redirect did not include a Location header")?;
            let next_url = request_url
                .join(location)
                .context("remote image redirect target was not valid")?;
            request_url = validate_remote_image_url(next_url.as_str())?;
            continue;
        }

        let response_url = validate_remote_image_url(response.get_url())?;
        validate_remote_image_host_addresses(&response_url)?;
        return Ok(response);
    }

    bail!("remote image exceeded redirect limit")
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 300 | 301 | 302 | 303 | 307 | 308)
}

impl Drop for ImageCache {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn create_window_image_cache_dir() -> Result<PathBuf> {
    let base = env::temp_dir().join("Markdown Reader");
    fs::create_dir_all(&base)
        .with_context(|| format!("failed to create image cache root {}", base.display()))?;
    cleanup_stale_image_cache_dirs(&base);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let process_id = std::process::id();

    for attempt in 0..100u32 {
        let candidate = base.join(format!("images-{process_id}-{timestamp}-{attempt}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create image cache {}", candidate.display())
                });
            }
        }
    }

    bail!("failed to create a unique image cache directory")
}

fn cleanup_stale_image_cache_dirs(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("images-"))
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified
            .elapsed()
            .is_ok_and(|age| age > Duration::from_secs(24 * 60 * 60))
        {
            let _ = fs::remove_dir_all(path);
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

fn build_app_html(
    markdown: &str,
    document_path: &Path,
    settings: &AppSettings,
    image_cache: &mut ImageCache,
) -> Result<String> {
    let expanded_markdown = expand_toc_markers(markdown);
    let rendered =
        render_markdown_for_document(&expanded_markdown, document_path, settings, image_cache);
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

fn load_document_html(webview: &WebView, markdown: &str, rendered_html: &str) -> Result<()> {
    let markdown_json = serde_json::to_string(markdown)?;
    let html_json = serde_json::to_string(rendered_html)?;
    webview
        .evaluate_script(&format!(
            "window.__mdReaderLoadDocument({markdown_json}, {html_json});"
        ))
        .context("WebView2 rejected document navigation update")
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

fn notify_image_picked(webview: &WebView, result: Result<ImagePickResult>) {
    let (ok, payload_json) = match result {
        Ok(image) => (
            true,
            serde_json::to_string(&image).unwrap_or_else(|_| "{}".into()),
        ),
        Err(error) => (
            false,
            serde_json::to_string(&error.to_string())
                .unwrap_or_else(|_| "\"image picker failed\"".into()),
        ),
    };
    let _ = webview.evaluate_script(&format!(
        "window.__mdReaderImagePicked({ok}, {payload_json});"
    ));
}

fn notify_open_link_failed(webview: &WebView, message: &str) {
    let message_json =
        serde_json::to_string(message).unwrap_or_else(|_| "\"failed to open link\"".into());
    let _ = webview.evaluate_script(&format!("window.__mdReaderOpenLinkFailed({message_json});"));
}

fn notify_back_history_changed(webview: &WebView, can_go_back: bool) {
    let _ = webview.evaluate_script(&format!(
        "window.__mdReaderSetCanGoBack && window.__mdReaderSetCanGoBack({can_go_back});"
    ));
}

fn notify_remote_image_ready(webview: &WebView, source: &str, render_src: Option<&str>) {
    let source_json = serde_json::to_string(source).unwrap_or_else(|_| "\"\"".into());
    let render_src_json =
        serde_json::to_string(&render_src.unwrap_or_default()).unwrap_or_else(|_| "\"\"".into());
    let _ = webview.evaluate_script(&format!(
        "window.__mdReaderRemoteImageReady && window.__mdReaderRemoteImageReady({source_json}, {render_src_json});"
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

fn pick_markdown_image(
    document_path: &Path,
    embed_base64: bool,
) -> Result<Option<ImagePickResult>> {
    let Some(path) = FileDialog::new()
        .set_title("Choose Markdown image")
        .add_filter("Images", DEFAULT_IMAGE_EXTENSIONS)
        .pick_file()
    else {
        return Ok(None);
    };

    ensure_image_path_is_renderable(&path)?;
    let alt = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Image")
        .to_string();

    if embed_base64 {
        let data_uri = image_data_uri_from_path(&path)?;
        return Ok(Some(ImagePickResult {
            source: data_uri.clone(),
            preview_src: data_uri,
            alt,
            embedded: true,
        }));
    }

    let source = markdown_image_source_for_path(document_path, &path);
    let preview_src = image_data_uri_from_path(&path)?;
    Ok(Some(ImagePickResult {
        source,
        preview_src,
        alt,
        embedded: false,
    }))
}

fn image_data_uri_from_path(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect image {}", path.display()))?;
    if metadata.len() > EMBEDDED_IMAGE_MAX_BYTES {
        bail!("image is too large to embed as base64");
    }
    let bytes =
        fs::read(path).with_context(|| format!("failed to read image {}", path.display()))?;
    let extension =
        image_extension_from_magic(&bytes).context("selected file is not a supported image")?;
    let mime_type = image_mime_type_for_extension(&extension)
        .context("selected file is not a supported image type")?;
    Ok(format!("data:{mime_type};base64,{}", encode_base64(&bytes)))
}

fn markdown_image_source_for_path(document_path: &Path, image_path: &Path) -> String {
    let base_dir = document_path.parent().unwrap_or_else(|| Path::new("."));
    let source = image_path
        .strip_prefix(base_dir)
        .unwrap_or(image_path)
        .to_string_lossy()
        .replace('\\', "/");
    percent_encode_file_url_path(&source)
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
        allowed_launch_extensions: normalize_allowed_launch_extensions(
            &settings.allowed_launch_extensions,
        ),
        link_click_behavior: normalize_link_click_behavior(&settings.link_click_behavior),
        allow_remote_images: settings.allow_remote_images,
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

fn normalize_allowed_launch_extensions(extensions: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();

    for extension in extensions {
        for token in extension
            .split(|character: char| character.is_whitespace() || matches!(character, ',' | ';'))
        {
            let Some(extension) = normalize_launch_extension_token(token) else {
                continue;
            };
            if !normalized.contains(&extension) {
                normalized.push(extension);
            }
            if normalized.len() >= 64 {
                return normalized;
            }
        }
    }

    normalized
}

fn normalize_launch_extension_token(token: &str) -> Option<String> {
    let extension = token
        .trim()
        .trim_start_matches('.')
        .chars()
        .take(32)
        .collect::<String>()
        .to_ascii_lowercase();
    if extension.is_empty()
        || !extension
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        None
    } else {
        Some(extension)
    }
}

fn normalize_link_click_behavior(behavior: &str) -> String {
    if behavior == LINK_BEHAVIOR_NAVIGATE {
        LINK_BEHAVIOR_NAVIGATE.to_string()
    } else {
        LINK_BEHAVIOR_NEW_WINDOW.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkTarget {
    Url(String),
    Document(PathBuf),
}

fn navigate_to_link_target(
    current_path: &mut PathBuf,
    webview: Option<&WebView>,
    window: &Window,
    href: &str,
    settings: &AppSettings,
    image_cache: &mut ImageCache,
) -> Result<()> {
    let target = resolve_link_target_with_settings(current_path, href, settings)?;
    let LinkTarget::Document(next_path) = target else {
        return open_link_target(current_path, href, settings);
    };

    load_document_path(
        current_path,
        next_path,
        webview,
        window,
        settings,
        image_cache,
    )
}

fn load_document_path(
    current_path: &mut PathBuf,
    next_path: PathBuf,
    webview: Option<&WebView>,
    window: &Window,
    settings: &AppSettings,
    image_cache: &mut ImageCache,
) -> Result<()> {
    let Some(webview) = webview else {
        bail!("reader window is not available for navigation");
    };
    let markdown = fs::read_to_string(&next_path)
        .with_context(|| format!("failed to read linked document {}", next_path.display()))?;
    let expanded_markdown = expand_toc_markers(&markdown);
    let rendered_html =
        render_markdown_for_document(&expanded_markdown, &next_path, settings, image_cache);

    load_document_html(webview, &expanded_markdown, &rendered_html)?;
    *current_path = next_path;
    window.set_title(&window_title_for_path(current_path));
    Ok(())
}

fn open_link_target(document_path: &Path, href: &str, settings: &AppSettings) -> Result<()> {
    let target = resolve_link_target_with_settings(document_path, href, settings)?;
    match target {
        LinkTarget::Url(url) => open_with_default_handler(OsStr::new(&url)),
        LinkTarget::Document(path) => open_with_default_handler(path.as_os_str()),
    }
}

#[cfg(test)]
fn resolve_link_target(document_path: &Path, href: &str) -> Result<LinkTarget> {
    resolve_link_target_with_settings(document_path, href, &AppSettings::default())
}

fn resolve_link_target_with_settings(
    document_path: &Path,
    href: &str,
    settings: &AppSettings,
) -> Result<LinkTarget> {
    let settings = normalize_settings(settings);
    let trimmed = href.trim();
    if trimmed.is_empty() {
        bail!("link target is empty");
    }
    if trimmed.starts_with('#') {
        bail!("internal document links are handled inside the reader");
    }

    if let Some(scheme) = href_scheme(trimmed) {
        return match scheme.to_ascii_lowercase().as_str() {
            "file" => {
                let file_path = file_url_to_path(trimmed)?;
                ensure_document_path_is_launchable(&file_path, &settings)?;
                Ok(LinkTarget::Document(file_path))
            }
            "http" | "https" | "mailto" => Ok(LinkTarget::Url(trimmed.to_string())),
            _ => bail!("blocked unsupported link scheme: {scheme}"),
        };
    }

    let path_part = strip_link_fragment_and_query(trimmed);
    let decoded = percent_decode_path(path_part)?;
    if decoded.trim().is_empty() {
        bail!("link target does not include a document path");
    }

    let linked_path = PathBuf::from(decoded);
    ensure_document_path_is_launchable(&linked_path, &settings)?;
    if linked_path.is_absolute() {
        Ok(LinkTarget::Document(linked_path))
    } else {
        let base_dir = document_path.parent().unwrap_or_else(|| Path::new("."));
        Ok(LinkTarget::Document(base_dir.join(linked_path)))
    }
}

fn file_url_to_path(href: &str) -> Result<PathBuf> {
    let path_part = strip_link_fragment_and_query(
        href.strip_prefix("file:")
            .context("file URL should include a file: scheme")?,
    );
    let decoded = percent_decode_path(path_part)?;
    if decoded.trim().is_empty() {
        bail!("file URL does not include a document path");
    }

    #[cfg(windows)]
    {
        let path_text = if let Some(path) = decoded.strip_prefix("///") {
            path.to_string()
        } else if let Some(path) = decoded.strip_prefix("//") {
            format!(r"\\{path}")
        } else if let Some(path) = decoded.strip_prefix('/') {
            path.to_string()
        } else {
            decoded
        };
        return Ok(PathBuf::from(path_text.replace('/', "\\")));
    }

    #[cfg(not(windows))]
    {
        let path_text = decoded
            .strip_prefix("//")
            .or_else(|| decoded.strip_prefix('/'))
            .unwrap_or(&decoded)
            .to_string();
        Ok(PathBuf::from(path_text))
    }
}

fn ensure_document_path_is_launchable(path: &Path, settings: &AppSettings) -> Result<()> {
    let Some(extension) = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
    else {
        return Ok(());
    };

    if settings
        .allowed_launch_extensions
        .iter()
        .any(|allowed| allowed == &extension)
        || DEFAULT_ALLOWED_LAUNCH_EXTENSIONS.contains(&extension.as_str())
    {
        return Ok(());
    }

    if is_blocked_launch_extension(&extension) {
        bail!("blocked potentially executable link target: .{extension}");
    }

    bail!("blocked link target extension that is not allowed by default: .{extension}")
}

fn is_blocked_launch_extension(extension: &str) -> bool {
    matches!(
        extension,
        "bat"
            | "cmd"
            | "com"
            | "cpl"
            | "exe"
            | "hta"
            | "jar"
            | "js"
            | "jse"
            | "lnk"
            | "msi"
            | "msp"
            | "ps1"
            | "psm1"
            | "py"
            | "reg"
            | "scr"
            | "url"
            | "vbe"
            | "vbs"
            | "wsf"
            | "wsh"
    )
}

fn href_scheme(href: &str) -> Option<&str> {
    let colon_index = href.find(':')?;
    let candidate = &href[..colon_index];
    if candidate.len() == 1
        && href
            .as_bytes()
            .get(colon_index + 1)
            .is_some_and(|byte| matches!(*byte, b'\\' | b'/'))
    {
        return None;
    }

    let mut chars = candidate.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if chars
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
    {
        Some(candidate)
    } else {
        None
    }
}

fn strip_link_fragment_and_query(href: &str) -> &str {
    href.find(['#', '?'])
        .map(|index| &href[..index])
        .unwrap_or(href)
}

fn percent_decode_path(path: &str) -> Result<String> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).context("link path is not valid UTF-8 after percent decoding")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_encode_file_url_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(*byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

fn image_extension_from_content_type(content_type: &str) -> Option<String> {
    match content_type.trim().to_ascii_lowercase().as_str() {
        "image/bmp" | "image/x-ms-bmp" => Some("bmp".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/jpeg" | "image/jpg" => Some("jpg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

fn image_extension_from_magic(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        return Some("png".to_string());
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg".to_string());
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif".to_string());
    }
    if bytes.starts_with(b"BM") {
        return Some("bmp".to_string());
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("webp".to_string());
    }
    None
}

fn image_mime_type_for_extension(extension: &str) -> Option<&'static str> {
    match extension {
        "bmp" => Some("image/bmp"),
        "gif" => Some("image/gif"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let triple = ((first as u32) << 16) | ((second as u32) << 8) | third as u32;

        encoded.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        encoded.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }

    encoded
}

fn normalize_image_extension(extension: &str) -> Option<String> {
    let extension = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if DEFAULT_IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        Some(extension)
    } else {
        None
    }
}

fn has_common_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(normalize_image_extension)
        .is_some()
}

fn ensure_image_path_is_renderable(path: &Path) -> Result<()> {
    if !has_common_image_extension(path) {
        bail!("blocked image with unsupported extension");
    }
    if !path.is_file() {
        bail!("image file does not exist");
    }
    Ok(())
}

fn validate_remote_image_url(image_url: &str) -> Result<Url> {
    let url = Url::parse(image_url).context("remote image URL is not valid")?;
    if url.scheme() != "https" {
        bail!("remote images must use https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("remote image URLs cannot include credentials");
    }

    match url.host().context("remote image URL must include a host")? {
        url::Host::Ipv4(address) => ensure_remote_image_ip_is_public(IpAddr::V4(address))?,
        url::Host::Ipv6(address) => ensure_remote_image_ip_is_public(IpAddr::V6(address))?,
        url::Host::Domain(host) => {
            let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
            if host.is_empty() {
                bail!("remote image URL must include a host");
            }
            if host == "localhost" || host.ends_with(".localhost") {
                bail!("remote image host is local-only");
            }
        }
    }
    Ok(url)
}

fn validate_remote_image_host_addresses(url: &Url) -> Result<()> {
    let host = remote_image_host_for_resolution(url)?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve remote image host {host}"))?;
    let mut found_address = false;
    for address in addresses {
        found_address = true;
        ensure_remote_image_ip_is_public(address.ip())?;
    }
    if !found_address {
        bail!("remote image host did not resolve");
    }
    Ok(())
}

fn remote_image_host_for_resolution(url: &Url) -> Result<String> {
    match url.host().context("remote image URL must include a host")? {
        url::Host::Ipv4(address) => Ok(address.to_string()),
        url::Host::Ipv6(address) => Ok(address.to_string()),
        url::Host::Domain(host) => Ok(host.trim().trim_end_matches('.').to_ascii_lowercase()),
    }
}

fn ensure_remote_image_ip_is_public(ip_address: IpAddr) -> Result<()> {
    match ip_address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            let is_shared = octets[0] == 100 && (octets[1] & 0b1100_0000) == 64;
            let is_benchmarking = octets[0] == 198 && matches!(octets[1], 18 | 19);
            if address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_multicast()
                || address.is_broadcast()
                || address.is_documentation()
                || address.is_unspecified()
                || is_shared
                || is_benchmarking
            {
                bail!("remote image host is not public");
            }
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return ensure_remote_image_ip_is_public(IpAddr::V4(mapped));
            }
            if address.is_loopback()
                || address.is_unspecified()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || address.is_multicast()
            {
                bail!("remote image host is not public");
            }
        }
    }
    Ok(())
}

fn is_safe_rendered_image_src(src: &str) -> bool {
    let Some(scheme) = href_scheme(src.trim()) else {
        return false;
    };

    if scheme.eq_ignore_ascii_case("data") {
        return is_safe_data_image_src(src);
    }

    if scheme.eq_ignore_ascii_case("file") {
        return file_url_to_path(src)
            .map(|path| has_common_image_extension(&path))
            .unwrap_or(false);
    }

    false
}

fn is_safe_data_image_src(src: &str) -> bool {
    if src.len() > ((EMBEDDED_IMAGE_MAX_BYTES as usize * 4) / 3 + 256) {
        return false;
    }
    let Some((metadata, encoded)) = src.split_once(',') else {
        return false;
    };
    let metadata = metadata.to_ascii_lowercase();
    let Some(media_type) = metadata.strip_prefix("data:") else {
        return false;
    };
    let Some((mime_type, parameters)) = media_type.split_once(';') else {
        return false;
    };
    if image_extension_from_content_type(mime_type).is_none() {
        return false;
    }
    if !parameters
        .split(';')
        .any(|parameter| parameter.eq_ignore_ascii_case("base64"))
    {
        return false;
    }
    encoded
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

#[cfg(windows)]
fn open_with_default_handler(target: &OsStr) -> Result<()> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            lp_operation: *const u16,
            lp_file: *const u16,
            lp_parameters: *const u16,
            lp_directory: *const u16,
            n_show_cmd: i32,
        ) -> *mut c_void;
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    let operation = wide(OsStr::new("open"));
    let file = wide(target);
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            1,
        )
    } as isize;

    if result <= 32 {
        bail!("Windows could not open link target (ShellExecuteW error {result})");
    }

    Ok(())
}

#[cfg(not(windows))]
fn open_with_default_handler(target: &OsStr) -> Result<()> {
    use std::process::Command;

    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = Command::new(opener)
        .arg(target)
        .status()
        .with_context(|| format!("failed to launch {opener}"))?;
    if !status.success() {
        bail!("{opener} failed with status {status}");
    }
    Ok(())
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

struct ImageRewriteContext<'a> {
    document_path: &'a Path,
    settings: &'a AppSettings,
    cache: Option<&'a mut ImageCache>,
}

fn render_markdown_for_document(
    markdown: &str,
    document_path: &Path,
    settings: &AppSettings,
    image_cache: &mut ImageCache,
) -> String {
    let settings = normalize_settings(settings);
    render_markdown_internal(
        markdown,
        Some(ImageRewriteContext {
            document_path,
            settings: &settings,
            cache: Some(image_cache),
        }),
    )
}

#[cfg(test)]
fn render_markdown_safely(markdown: &str) -> String {
    render_markdown_internal(markdown, None)
}

fn render_markdown_internal(
    markdown: &str,
    mut image_context: Option<ImageRewriteContext<'_>>,
) -> String {
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
    options.insert(Options::ENABLE_DEFINITION_LIST);
    options.insert(Options::ENABLE_SUBSCRIPT);
    options.insert(Options::ENABLE_SUPERSCRIPT);
    options.insert(Options::ENABLE_MATH);

    let mut parser = Parser::new_ext(&markdown, options).peekable();
    let mut next_heading_index = 0usize;
    let mut indexed_events = Vec::new();

    while let Some(event) = parser.next() {
        match event {
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
                indexed_events.push(Event::Html(
                    format!(r#"<{tag} id="{heading_id}" data-md-section-index="{index}">"#).into(),
                ));
            }
            Event::End(TagEnd::Heading(level)) => {
                indexed_events.push(Event::Html(format!("</{}>", heading_tag(level)).into()));
            }
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => {
                let alt = collect_image_alt_text(&mut parser);
                let (image_size, remainder) = consume_image_size_attributes(&mut parser);
                indexed_events.push(Event::Html(
                    markdown_image_html(
                        dest_url.as_ref(),
                        title.as_ref(),
                        &alt,
                        &image_size,
                        image_context.as_mut(),
                    )
                    .into(),
                ));
                if let Some(remainder) = remainder {
                    indexed_events.push(Event::Text(remainder.into()));
                }
            }
            Event::End(TagEnd::Image) => {}
            other => indexed_events.push(other),
        }
    }

    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, indexed_events.into_iter());
    sanitize_rendered_html(&unsafe_html)
}

fn collect_image_alt_text<'a>(parser: &mut Peekable<Parser<'a>>) -> String {
    let mut alt = String::new();
    let mut depth = 1usize;

    while let Some(event) = parser.next() {
        match event {
            Event::Start(Tag::Image { .. }) => {
                depth += 1;
            }
            Event::End(TagEnd::Image) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Text(text) | Event::Code(text) => alt.push_str(text.as_ref()),
            Event::SoftBreak | Event::HardBreak => alt.push(' '),
            _ => {}
        }
    }

    alt
}

fn consume_image_size_attributes<'a>(
    parser: &mut Peekable<Parser<'a>>,
) -> (ImageSizeAttributes, Option<String>) {
    let Some((attributes, remainder)) = parser.peek().and_then(|event| match event {
        Event::Text(text) => parse_image_size_attribute_suffix(text.as_ref())
            .map(|(attributes, remainder)| (attributes, remainder.to_string())),
        _ => None,
    }) else {
        return (ImageSizeAttributes::default(), None);
    };

    parser.next();
    let remainder = if remainder.is_empty() {
        None
    } else {
        Some(remainder.to_string())
    };
    (attributes, remainder)
}

fn parse_image_size_attribute_suffix(text: &str) -> Option<(ImageSizeAttributes, &str)> {
    let inner = text.strip_prefix('{')?;
    let end = inner.find('}')?;
    let attributes = parse_image_size_attributes(&inner[..end])?;
    Some((attributes, &inner[end + 1..]))
}

fn parse_image_size_attributes(value: &str) -> Option<ImageSizeAttributes> {
    let mut attributes = ImageSizeAttributes::default();

    for token in value.split_ascii_whitespace() {
        let Some((key, raw_value)) = token.split_once('=') else {
            continue;
        };
        let Some(dimension) = normalize_image_dimension(raw_value) else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "width" | "w" => attributes.width = Some(dimension),
            "height" | "h" => attributes.height = Some(dimension),
            _ => {}
        }
    }

    if attributes.width.is_some() || attributes.height.is_some() {
        Some(attributes)
    } else {
        None
    }
}

fn normalize_image_dimension(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }

    if let Some(percent) = value.strip_suffix('%') {
        if is_positive_decimal(percent) {
            return Some(format!("{percent}%"));
        }
        return None;
    }

    let pixels = value.strip_suffix("px").unwrap_or(&value);
    if pixels.chars().all(|ch| ch.is_ascii_digit()) {
        let parsed = pixels.parse::<u32>().ok()?;
        if (1..=10000).contains(&parsed) {
            return Some(parsed.to_string());
        }
    }

    None
}

fn is_positive_decimal(value: &str) -> bool {
    if value.is_empty() || value.matches('.').count() > 1 {
        return false;
    }
    if !value.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
        return false;
    }
    value
        .parse::<f64>()
        .is_ok_and(|parsed| parsed > 0.0 && parsed <= 1000.0)
}

fn markdown_image_html(
    original_src: &str,
    title: &str,
    alt: &str,
    image_size: &ImageSizeAttributes,
    image_context: Option<&mut ImageRewriteContext<'_>>,
) -> String {
    let resolved_src =
        image_context.and_then(
            |context| match resolve_markdown_image_src(context, original_src) {
                Ok(src) => Some(src),
                Err(_) => context
                    .cache
                    .as_deref_mut()
                    .and_then(|cache| cache.placeholder_render_src().ok()),
            },
        );
    let src_attr = resolved_src
        .as_deref()
        .filter(|src| !src.trim().is_empty())
        .map(|src| format!(r#" src="{}""#, escape_html_attribute(src)))
        .unwrap_or_default();
    let title_attr = if title.is_empty() {
        String::new()
    } else {
        format!(
            r#" title="{}" data-md-title="{}""#,
            escape_html_attribute(title),
            escape_html_attribute(title)
        )
    };
    let width_attr = image_size
        .width
        .as_deref()
        .map(|width| format!(r#" data-md-width="{}""#, escape_html_attribute(width)))
        .unwrap_or_default();
    let height_attr = image_size
        .height
        .as_deref()
        .map(|height| format!(r#" data-md-height="{}""#, escape_html_attribute(height)))
        .unwrap_or_default();

    format!(
        r#"<img{src_attr} alt="{}" data-md-src="{}"{title_attr}{width_attr}{height_attr} />"#,
        escape_html_attribute(alt),
        escape_html_attribute(original_src)
    )
}

fn resolve_markdown_image_src(
    context: &mut ImageRewriteContext<'_>,
    original_src: &str,
) -> Result<String> {
    let trimmed = original_src.trim();
    if trimmed.is_empty() {
        bail!("image source is empty");
    }

    if let Some(scheme) = href_scheme(trimmed) {
        return match scheme.to_ascii_lowercase().as_str() {
            "data" if is_safe_data_image_src(trimmed) => Ok(trimmed.to_string()),
            "file" => {
                let path = file_url_to_path(trimmed)?;
                ensure_image_path_is_renderable(&path)?;
                image_data_uri_from_path(&path)
            }
            "http" | "https" => {
                if !context.settings.allow_remote_images {
                    bail!("remote images are disabled");
                }
                let Some(cache) = context.cache.as_deref_mut() else {
                    bail!("image cache is not available");
                };
                cache.remote_render_src(trimmed)
            }
            _ => bail!("blocked unsupported image scheme: {scheme}"),
        };
    }

    let path_part = strip_link_fragment_and_query(trimmed);
    let decoded = percent_decode_path(path_part)?;
    if decoded.trim().is_empty() {
        bail!("image source does not include a path");
    }

    let image_path = PathBuf::from(decoded);
    let resolved = if image_path.is_absolute() {
        image_path
    } else {
        context
            .document_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(image_path)
    };

    ensure_image_path_is_renderable(&resolved)?;
    image_data_uri_from_path(&resolved)
}

fn escape_html_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn sanitize_rendered_html(unsafe_html: &str) -> String {
    let mut builder = ammonia::Builder::default();
    builder.add_generic_attributes(&["id"]);
    builder.add_url_schemes(&["data", "file"]);
    builder.add_tags(&["input"]);
    builder.add_tag_attributes("input", &["type", "checked", "disabled"]);
    builder.set_tag_attribute_value("input", "disabled", "");
    builder.add_tag_attributes(
        "img",
        &[
            "data-md-src",
            "data-md-title",
            "data-md-width",
            "data-md-height",
        ],
    );
    builder.add_tag_attributes("div", &["class"]);
    builder.add_tag_attributes("span", &["class"]);
    builder.add_tag_attributes("sup", &["class"]);
    for tag in ["h1", "h2", "h3", "h4", "h5", "h6"] {
        builder.add_tag_attributes(tag, &["data-md-section-index"]);
    }
    builder.attribute_filter(|element, attribute, value| match (element, attribute) {
        ("img", "src") if is_safe_rendered_image_src(value) => Some(value.into()),
        ("img", "src") => None,
        ("img", "data-md-width" | "data-md-height") => {
            normalize_image_dimension(value).map(Into::into)
        }
        ("input", "type") if value.eq_ignore_ascii_case("checkbox") => Some("checkbox".into()),
        ("input", "checked" | "disabled") => Some(String::new().into()),
        ("input", _) => None,
        ("div", "class") => allowed_css_classes(value, &["footnote-definition"]),
        ("span", "class") => allowed_css_classes(value, &["math", "math-inline", "math-display"]),
        ("sup", "class") => {
            allowed_css_classes(value, &["footnote-definition-label", "footnote-reference"])
        }
        _ => Some(value.into()),
    });
    builder.clean(unsafe_html).to_string()
}

fn allowed_css_classes(value: &str, allowed: &[&str]) -> Option<std::borrow::Cow<'static, str>> {
    let classes = value
        .split_ascii_whitespace()
        .filter(|class| allowed.contains(class))
        .collect::<Vec<_>>();
    if classes.is_empty() {
        None
    } else {
        Some(classes.join(" ").into())
    }
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

const APP_HTML_TEMPLATE: &str = r###"<!doctype html>
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

    .icon-button:disabled {
      color: var(--muted);
      cursor: default;
      opacity: 0.46;
    }

    .icon-button:disabled:hover {
      border-color: transparent;
      background: transparent;
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
      min-width: 0;
      max-width: min(820px, calc(100vw - 184px));
      overflow-x: auto;
      scrollbar-width: thin;
    }

    body.editing .format-tools {
      display: none;
    }

    .format-tools .icon-button,
    .format-tools .format-select,
    .format-tools .format-divider {
      flex: 0 0 auto;
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

    .strike-label {
      text-decoration: line-through;
    }

    .script-label {
      font-size: 12px;
      line-height: 1;
    }

    .script-icon {
      position: relative;
      width: 18px;
      height: 18px;
      display: inline-block;
      font: 600 14px/18px "Segoe UI", system-ui, sans-serif;
      text-align: left;
    }

    .script-mark {
      position: absolute;
      right: 0;
      font-size: 9px;
      line-height: 1;
    }

    .script-up {
      top: 1px;
    }

    .script-down {
      bottom: 1px;
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

    #rendered a {
      cursor: pointer;
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
      cursor: default;
      vertical-align: middle;
    }

    #rendered img.md-image-selected {
      outline: 2px solid var(--accent);
      outline-offset: 2px;
    }

    #image-resize-overlay[hidden] {
      display: none;
    }

    #image-resize-overlay {
      position: fixed;
      z-index: 20;
      border: 1px solid var(--accent);
      box-sizing: border-box;
      pointer-events: none;
    }

    .image-resize-handle {
      position: absolute;
      width: 10px;
      height: 10px;
      background: #fff;
      border: 2px solid var(--accent);
      border-radius: 2px;
      box-sizing: border-box;
      pointer-events: auto;
    }

    .image-resize-handle[data-handle="nw"] {
      left: -6px;
      top: -6px;
      cursor: nwse-resize;
    }

    .image-resize-handle[data-handle="n"] {
      left: calc(50% - 5px);
      top: -6px;
      cursor: ns-resize;
    }

    .image-resize-handle[data-handle="ne"] {
      right: -6px;
      top: -6px;
      cursor: nesw-resize;
    }

    .image-resize-handle[data-handle="e"] {
      right: -6px;
      top: calc(50% - 5px);
      cursor: ew-resize;
    }

    .image-resize-handle[data-handle="se"] {
      right: -6px;
      bottom: -6px;
      cursor: nwse-resize;
    }

    .image-resize-handle[data-handle="s"] {
      left: calc(50% - 5px);
      bottom: -6px;
      cursor: ns-resize;
    }

    .image-resize-handle[data-handle="sw"] {
      left: -6px;
      bottom: -6px;
      cursor: nesw-resize;
    }

    .image-resize-handle[data-handle="w"] {
      left: -6px;
      top: calc(50% - 5px);
      cursor: ew-resize;
    }

    .image-resize-reset {
      position: absolute;
      right: -1px;
      top: -34px;
      min-height: 26px;
      padding: 0 10px;
      border: 1px solid var(--accent);
      border-radius: 4px;
      color: #fff;
      background: var(--accent);
      font: 12px/1 "Segoe UI", system-ui, sans-serif;
      pointer-events: auto;
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

    .settings-modal,
    .insert-modal {
      width: min(420px, calc(100vw - 32px));
      border: 1px solid var(--border);
      border-radius: 8px;
      background: var(--dialog, var(--surface));
      box-shadow: 0 18px 44px var(--dialog-shadow, rgba(0, 0, 0, 0.24));
      color: var(--ink);
    }

    .settings-header,
    .insert-header {
      height: 42px;
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 0 10px 0 16px;
      border-bottom: 1px solid var(--border);
    }

    .settings-header h2,
    .insert-header h2 {
      font-size: 15px;
      font-weight: 600;
      margin: 0;
    }

    .settings-body,
    .insert-body {
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

    .field-label {
      position: relative;
      display: inline-flex;
      align-items: center;
      gap: 6px;
	    }

	    .help-button {
	      width: 18px;
	      height: 18px;
      border-radius: 50%;
      font: 600 12px/1 "Segoe UI", system-ui, sans-serif;
    }

    .help-wrap {
      position: relative;
      display: inline-flex;
      align-items: center;
    }

    .help-tooltip {
      position: absolute;
      left: calc(100% + 8px);
      top: 50%;
      z-index: 30;
      width: min(330px, calc(100vw - 96px));
      padding: 8px 10px;
      color: var(--ink);
      background: var(--dialog, var(--surface));
      border: 1px solid var(--border);
      border-radius: 4px;
      box-shadow: 0 10px 24px var(--dialog-shadow, rgba(0, 0, 0, 0.22));
      font: 12px/1.35 "Segoe UI", system-ui, sans-serif;
      transform: translateY(-50%);
      display: none;
    }

    .help-wrap:hover .help-tooltip,
    .help-wrap:focus-within .help-tooltip,
    .help-tooltip.visible {
      display: block;
    }

	    .checkbox-field {
	      grid-template-columns: minmax(0, 1fr) auto;
	      align-items: center;
	    }

	    .settings-body input[type="checkbox"] {
	      width: 18px;
	      height: 18px;
	      accent-color: var(--accent);
	    }

	    .settings-body select,
	    .settings-body textarea,
    .insert-body input,
    .insert-body select,
    .insert-body textarea,
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

    .settings-body textarea,
    .insert-body textarea {
      min-height: 68px;
      resize: vertical;
      font-family: "Cascadia Mono", Consolas, "Courier New", monospace;
      line-height: 1.45;
    }

    .insert-body input[type="number"] {
      font-family: "Segoe UI", system-ui, sans-serif;
    }

    .insert-fields {
      display: grid;
      gap: 12px;
    }

    .form-actions {
      display: flex;
      justify-content: flex-end;
      gap: 8px;
    }

    .form-button {
      min-width: 84px;
      min-height: 32px;
      border: 1px solid var(--border);
      border-radius: 4px;
      padding: 4px 12px;
      color: var(--ink);
      background: var(--surface);
      font: 14px/1.3 "Segoe UI", system-ui, sans-serif;
      cursor: pointer;
    }

    .form-button:hover,
    .form-button:focus-visible {
      background: var(--frame);
      outline: 0;
    }

    .form-button.primary {
      color: #ffffff;
      background: var(--accent);
      border-color: var(--accent);
    }

    .settings-button {
      text-align: left;
      cursor: pointer;
    }

    .settings-button:hover {
      background: var(--frame);
    }

    .link-menu {
      position: fixed;
      z-index: 20;
      min-width: 188px;
      padding: 4px;
      background: var(--dialog, var(--surface));
      border: 1px solid var(--border);
      border-radius: 4px;
      box-shadow: 0 12px 28px var(--dialog-shadow, rgba(0, 0, 0, 0.22));
    }

    .link-menu button {
      width: 100%;
      min-height: 30px;
      padding: 5px 8px;
      color: var(--ink);
      background: transparent;
      border: 0;
      border-radius: 3px;
      text-align: left;
      font: 13px/1.35 "Segoe UI", system-ui, sans-serif;
      cursor: pointer;
    }

    .link-menu button:hover,
    .link-menu button:focus-visible {
      background: var(--frame);
      outline: 0;
    }
  </style>
  <style id="theme-style"></style>
  <style id="custom-theme-style"></style>
</head>
<body>
  <nav>
    <button id="back-button" class="icon-button history-button" type="button" title="Back" aria-label="Back" disabled>
      <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
        <path d="M19 12H5"></path>
        <path d="M12 19l-7-7 7-7"></path>
      </svg>
    </button>
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
      <button class="icon-button format-button" type="button" data-command="strikeThrough" title="Strikethrough" aria-label="Strikethrough"><span class="strike-label">S</span></button>
      <button class="icon-button format-button script-label" type="button" data-command="subscript" title="Subscript" aria-label="Subscript"><span class="script-icon">x<span class="script-mark script-down">2</span></span></button>
      <button class="icon-button format-button script-label" type="button" data-command="superscript" title="Superscript" aria-label="Superscript"><span class="script-icon">x<span class="script-mark script-up">2</span></span></button>
      <button id="inline-code-button" class="icon-button format-button" type="button" title="Inline code" aria-label="Inline code">{}</button>
      <button id="code-block-button" class="icon-button" type="button" title="Code block" aria-label="Code block">
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
          <path d="m8 8-4 4 4 4"></path>
          <path d="m16 8 4 4-4 4"></path>
          <path d="m14 4-4 16"></path>
        </svg>
      </button>
      <button id="math-button" class="icon-button format-button" type="button" title="Math" aria-label="Math">{x}</button>
      <button id="link-button" class="icon-button" type="button" title="Link" aria-label="Link">
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M10 13a5 5 0 0 0 7.1 0l2-2a5 5 0 0 0-7.1-7.1l-1.1 1.1"></path>
          <path d="M14 11a5 5 0 0 0-7.1 0l-2 2A5 5 0 0 0 12 20.1l1.1-1.1"></path>
        </svg>
      </button>
      <button id="image-button" class="icon-button" type="button" title="Image" aria-label="Image">
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M4 5h16v14H4z"></path>
          <path d="m8 14 2.5-3 3 4 2-2.5L20 18"></path>
          <circle cx="8.5" cy="9" r="1"></circle>
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
      <button id="task-list-button" class="icon-button" type="button" title="Task list" aria-label="Task list">
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M4 5h4v4H4z"></path>
          <path d="m5 7 1 1 2-2"></path>
          <path d="M11 7h9"></path>
          <path d="M4 15h4v4H4z"></path>
          <path d="M11 17h9"></path>
        </svg>
      </button>
	      <button id="table-button" class="icon-button" type="button" title="Table" aria-label="Table">
	        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
	          <path d="M4 5h16v14H4z"></path>
	          <path d="M4 10h16"></path>
	          <path d="M4 15h16"></path>
	          <path d="M10 5v14"></path>
	          <path d="M16 5v14"></path>
	        </svg>
	      </button>
      <button id="definition-list-button" class="icon-button format-button" type="button" title="Definition list" aria-label="Definition list">DL</button>
      <button id="footnote-button" class="icon-button format-button" type="button" title="Footnote" aria-label="Footnote">fn</button>
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
	        <label class="field" for="link-click-behavior">
	          Link Click Behavior
	          <select id="link-click-behavior">
	            <option value="newWindow">New window</option>
	            <option value="navigate">Navigate</option>
	          </select>
	        </label>
	        <label class="field checkbox-field" for="allow-remote-images">
	          <span>Remote Images</span>
	          <input id="allow-remote-images" type="checkbox">
	        </label>
	        <label class="field" for="allowed-launch-extensions">
	          <span class="field-label">
	            Allowed Link Extensions
	            <span class="help-wrap">
	              <button id="allowed-extensions-help" class="icon-button help-button" type="button" aria-label="Show default allowed link extensions" aria-describedby="allowed-extensions-help-text" aria-expanded="false">?</button>
	              <span id="allowed-extensions-help-text" class="help-tooltip" role="tooltip">Allowed by default: no extension, .bmp, .csv, .doc, .docx, .gif, .htm, .html, .jpeg, .jpg, .json, .log, .md, .markdown, .odp, .ods, .odt, .pdf, .png, .ppt, .pptx, .rtf, .toml, .tsv, .txt, .webp, .xls, .xlsx, .xml, .yaml, .yml.</span>
	            </span>
	          </span>
	          <textarea id="allowed-launch-extensions" spellcheck="false" placeholder=".ps1, .exe"></textarea>
	        </label>
      </div>
    </section>
  </div>
  <div id="insert-backdrop" class="modal-backdrop" hidden>
    <section class="insert-modal" role="dialog" aria-modal="true" aria-labelledby="insert-title">
      <header class="insert-header">
        <h2 id="insert-title">Insert</h2>
        <button id="insert-close" class="icon-button" type="button" title="Close" aria-label="Close insert dialog">
          <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
            <path d="M18 6 6 18"></path>
            <path d="m6 6 12 12"></path>
          </svg>
        </button>
      </header>
      <form id="insert-form" class="insert-body">
        <div id="insert-fields" class="insert-fields"></div>
        <footer class="form-actions">
          <button id="insert-cancel" class="form-button" type="button">Cancel</button>
          <button id="insert-submit" class="form-button primary" type="submit">Insert</button>
        </footer>
      </form>
    </section>
  </div>
  <div id="link-menu" class="link-menu" hidden>
    <button id="link-menu-action" type="button"></button>
  </div>
  <div id="image-resize-overlay" hidden>
    <button id="image-resize-reset" class="image-resize-reset" type="button">Reset</button>
    <span class="image-resize-handle" data-handle="nw" title="Resize image"></span>
    <span class="image-resize-handle" data-handle="n" title="Resize image"></span>
    <span class="image-resize-handle" data-handle="ne" title="Resize image"></span>
    <span class="image-resize-handle" data-handle="e" title="Resize image"></span>
    <span class="image-resize-handle" data-handle="se" title="Resize image"></span>
    <span class="image-resize-handle" data-handle="s" title="Resize image"></span>
    <span class="image-resize-handle" data-handle="sw" title="Resize image"></span>
    <span class="image-resize-handle" data-handle="w" title="Resize image"></span>
  </div>
  <script>
    const initialMarkdown = __INITIAL_MARKDOWN__;
    const initialRendered = __INITIAL_RENDERED__;
    const themes = __THEMES__;
    const initialSettings = __SETTINGS__;
    const editor = document.getElementById("editor");
    const rendered = document.getElementById("rendered");
    const toggle = document.getElementById("mode-toggle");
    const backButton = document.getElementById("back-button");
    const saveButton = document.getElementById("save-button");
    const settingsButton = document.getElementById("settings-button");
    const settingsBackdrop = document.getElementById("settings-backdrop");
    const settingsClose = document.getElementById("settings-close");
    const insertBackdrop = document.getElementById("insert-backdrop");
    const insertClose = document.getElementById("insert-close");
    const insertCancel = document.getElementById("insert-cancel");
    const insertForm = document.getElementById("insert-form");
    const insertFields = document.getElementById("insert-fields");
    const insertTitle = document.getElementById("insert-title");
    const insertSubmit = document.getElementById("insert-submit");
	    const themeSelect = document.getElementById("theme-select");
	    const linkClickBehaviorSelect = document.getElementById("link-click-behavior");
	    const allowRemoteImagesInput = document.getElementById("allow-remote-images");
	    const customCssButton = document.getElementById("custom-css-button");
	    const allowedLaunchExtensionsInput = document.getElementById("allowed-launch-extensions");
	    const allowedExtensionsHelpButton = document.getElementById("allowed-extensions-help");
	    const allowedExtensionsHelpText = document.getElementById("allowed-extensions-help-text");
    const linkMenu = document.getElementById("link-menu");
    const linkMenuAction = document.getElementById("link-menu-action");
    const imageResizeOverlay = document.getElementById("image-resize-overlay");
    const imageResizeReset = document.getElementById("image-resize-reset");
    const themeStyle = document.getElementById("theme-style");
    const customThemeStyle = document.getElementById("custom-theme-style");
    const blockFormat = document.getElementById("block-format");
	    const inlineCodeButton = document.getElementById("inline-code-button");
	    const codeBlockButton = document.getElementById("code-block-button");
	    const linkButton = document.getElementById("link-button");
	    const imageButton = document.getElementById("image-button");
	    const mathButton = document.getElementById("math-button");
	    const taskListButton = document.getElementById("task-list-button");
	    const tableButton = document.getElementById("table-button");
	    const definitionListButton = document.getElementById("definition-list-button");
	    const footnoteButton = document.getElementById("footnote-button");
    let dirty = false;
    let renderedDirty = false;
    let selectedImage = null;
    let imageResizeDrag = null;
	    let customCss = sanitizeCustomCss(initialSettings.customCss || "");
	    let allowedLaunchExtensions = sanitizeAllowedLaunchExtensions(initialSettings.allowedLaunchExtensions || []);
	    let linkClickBehavior = normalizeLinkClickBehavior(initialSettings.linkClickBehavior);
	    let allowRemoteImages = Boolean(initialSettings.allowRemoteImages);
    let linkMenuHref = "";
    let insertKind = "";
    let insertReturnFocus = null;
    let savedRenderedRange = null;

    editor.value = initialMarkdown;
    rendered.innerHTML = initialRendered;
    applyImageSizes(rendered);
	    allowedLaunchExtensionsInput.value = formatAllowedLaunchExtensions(allowedLaunchExtensions);
	    linkClickBehaviorSelect.value = linkClickBehavior;
	    allowRemoteImagesInput.checked = allowRemoteImages;

    function postMessage(payload) {
      window.ipc.postMessage(JSON.stringify(payload));
    }

    function setDirty(value) {
      dirty = value;
      saveButton.classList.toggle("dirty", dirty);
      saveButton.classList.toggle("saved", !dirty);
    }

    function applyImageSizes(root) {
      for (const image of root.querySelectorAll("img")) {
        applyImageSize(image);
      }
    }

    function applyImageSize(image) {
      const width = normalizeImageDimension(image.dataset.mdWidth || image.getAttribute("width") || "");
      const height = normalizeImageDimension(image.dataset.mdHeight || image.getAttribute("height") || "");
      if (width) {
        image.dataset.mdWidth = width;
        image.style.width = cssImageDimension(width);
      } else {
        delete image.dataset.mdWidth;
        image.style.removeProperty("width");
      }
      if (height) {
        image.dataset.mdHeight = height;
        image.style.height = cssImageDimension(height);
      } else {
        delete image.dataset.mdHeight;
        image.style.height = width ? "auto" : "";
      }
    }

    function normalizeImageDimension(value) {
      const dimension = String(value || "").trim().replace(/^['"]|['"]$/g, "").trim().toLowerCase();
      if (!dimension) {
        return "";
      }
      if (/^\d+(?:\.\d+)?%$/.test(dimension)) {
        const number = Number.parseFloat(dimension);
        return number > 0 && number <= 1000 ? dimension : "";
      }
      const pixels = dimension.endsWith("px") ? dimension.slice(0, -2) : dimension;
      if (/^\d+$/.test(pixels)) {
        const parsed = Number.parseInt(pixels, 10);
        return parsed > 0 && parsed <= 10000 ? String(parsed) : "";
      }
      return "";
    }

    function cssImageDimension(value) {
      return String(value).endsWith("%") ? String(value) : `${value}px`;
    }

    function imageSizeMarkdown(image) {
      const width = normalizeImageDimension(image.dataset.mdWidth || "");
      const height = normalizeImageDimension(image.dataset.mdHeight || "");
      const parts = [];
      if (width) {
        parts.push(`width=${width}`);
      }
      if (height) {
        parts.push(`height=${height}`);
      }
      return parts.length ? `{${parts.join(" ")}}` : "";
    }

    function selectImage(image) {
      if (selectedImage && selectedImage !== image) {
        selectedImage.classList.remove("md-image-selected");
      }
      selectedImage = image;
      selectedImage.classList.add("md-image-selected");
      imageResizeOverlay.hidden = false;
      updateImageResizeOverlay();
    }

    function clearImageSelection() {
      if (imageResizeDrag) {
        return;
      }
      if (selectedImage) {
        selectedImage.classList.remove("md-image-selected");
      }
      selectedImage = null;
      imageResizeOverlay.hidden = true;
    }

    function updateImageResizeOverlay() {
      if (!selectedImage || imageResizeOverlay.hidden || !document.body.contains(selectedImage)) {
        imageResizeOverlay.hidden = true;
        return;
      }
      const rect = selectedImage.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) {
        imageResizeOverlay.hidden = true;
        return;
      }
      imageResizeOverlay.style.left = `${rect.left}px`;
      imageResizeOverlay.style.top = `${rect.top}px`;
      imageResizeOverlay.style.width = `${rect.width}px`;
      imageResizeOverlay.style.height = `${rect.height}px`;
    }

    function startImageResize(event) {
      if (!selectedImage) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      const rect = selectedImage.getBoundingClientRect();
      imageResizeDrag = {
        handle: event.currentTarget.dataset.handle || "se",
        startX: event.clientX,
        startY: event.clientY,
        startWidth: Math.max(1, rect.width),
        startHeight: Math.max(1, rect.height),
        aspectRatio: Math.max(1, rect.width) / Math.max(1, rect.height)
      };
      window.addEventListener("mousemove", resizeSelectedImage);
      window.addEventListener("mouseup", finishImageResize, { once: true });
    }

    function resizeSelectedImage(event) {
      if (!imageResizeDrag || !selectedImage) {
        return;
      }
      const drag = imageResizeDrag;
      const horizontal = drag.handle.includes("e")
        ? event.clientX - drag.startX
        : drag.handle.includes("w")
          ? drag.startX - event.clientX
          : 0;
      const vertical = drag.handle.includes("s")
        ? event.clientY - drag.startY
        : drag.handle.includes("n")
          ? drag.startY - event.clientY
          : 0;
      let width = drag.startWidth + horizontal;
      if (!horizontal && vertical) {
        width = drag.startWidth + vertical * drag.aspectRatio;
      }
      width = Math.max(24, Math.min(Math.round(width), Math.max(24, rendered.clientWidth - 16)));
      selectedImage.dataset.mdWidth = String(width);
      selectedImage.style.width = `${width}px`;
      selectedImage.style.height = "auto";
      delete selectedImage.dataset.mdHeight;
      renderedDirty = true;
      setDirty(true);
      updateImageResizeOverlay();
    }

    function finishImageResize() {
      window.removeEventListener("mousemove", resizeSelectedImage);
      imageResizeDrag = null;
      updateImageResizeOverlay();
    }

    function resetSelectedImageSize() {
      if (!selectedImage) {
        return;
      }
      delete selectedImage.dataset.mdWidth;
      delete selectedImage.dataset.mdHeight;
      selectedImage.style.removeProperty("width");
      selectedImage.style.removeProperty("height");
      renderedDirty = true;
      setDirty(true);
      updateImageResizeOverlay();
    }

    function setCanGoBack(value) {
      backButton.disabled = !Boolean(value);
    }

    function setAllowedExtensionsHelpVisible(value) {
      const visible = Boolean(value);
      allowedExtensionsHelpText.classList.toggle("visible", visible);
      allowedExtensionsHelpButton.setAttribute("aria-expanded", visible ? "true" : "false");
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
      clearImageSelection();
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
	      if (tag === "div" && isFootnoteDefinition(node)) {
	        return footnoteDefinitionMarkdown(node, depth);
	      }
	      if (tag === "dl") {
	        return definitionListMarkdown(node, depth);
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
	      if (tag === "del" || tag === "s") {
	        return text ? `~~${text}~~` : "";
	      }
	      if (tag === "code") {
	        return inlineCode(node.textContent || "");
	      }
	      if (tag === "sup") {
	        const footnote = footnoteReferenceMarkdown(node);
	        return footnote || (text ? `^${text}^` : "");
	      }
	      if (tag === "sub") {
	        return text ? `~${text}~` : "";
	      }
	      if (tag === "span" && isMathSpan(node)) {
	        return mathMarkdown(node);
	      }
	      if (tag === "a") {
	        const href = node.getAttribute("href") || "";
	        if (!href || /^\s*javascript:/i.test(href)) {
	          return text;
	        }
	        return `[${text}](${markdownDestination(href)})`;
	      }
	      if (tag === "img") {
	        const alt = normalizeText(node.getAttribute("alt") || "");
	        const src = node.getAttribute("data-md-src") || node.getAttribute("src") || "";
	        const title = node.getAttribute("data-md-title") || node.getAttribute("title") || "";
	        return src ? `![${alt}](${markdownDestination(src)}${title ? ` "${markdownTitle(title)}"` : ""})${imageSizeMarkdown(node)}` : alt;
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
	        const taskMarker = taskListPrefix(item);
	        const content = listItemMarkdown(item, depth + 1);
	        const contentLines = content.split("\n");
	        const indent = "  ".repeat(depth);
	        lines.push(`${indent}${marker}${taskMarker}${contentLines[0] || ""}`);
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
	        if (isTaskCheckbox(child)) {
	          continue;
	        }
	        if (child.nodeType === Node.ELEMENT_NODE && /^(ul|ol|p|pre|blockquote|table|dl|div)$/.test(child.tagName.toLowerCase())) {
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

	    function taskListPrefix(item) {
	      const checkbox = Array.from(item.childNodes).find(isTaskCheckbox);
	      if (!checkbox) {
	        return "";
	      }
	      return checkbox.checked || checkbox.hasAttribute("checked") ? "[x] " : "[ ] ";
	    }

	    function isTaskCheckbox(node) {
	      return node.nodeType === Node.ELEMENT_NODE
	        && node.tagName.toLowerCase() === "input"
	        && (node.getAttribute("type") || "").toLowerCase() === "checkbox";
	    }

	    function footnoteReferenceMarkdown(node) {
	      if (!node.classList.contains("footnote-reference")) {
	        return "";
	      }
	      const anchor = node.querySelector("a[href]");
	      const href = anchor ? anchor.getAttribute("href") || "" : "";
	      const label = footnoteLabelFromHref(href);
	      return label ? `[^${label}]` : "";
	    }

	    function isFootnoteDefinition(node) {
	      return node.classList.contains("footnote-definition")
	        && Boolean(footnoteLabelFromId(node.getAttribute("id") || ""));
	    }

	    function footnoteDefinitionMarkdown(node, depth) {
	      const label = footnoteLabelFromId(node.getAttribute("id") || "");
	      const clone = node.cloneNode(true);
	      const labelNode = clone.querySelector("sup");
	      if (labelNode) {
	        labelNode.remove();
	      }
	      const text = childBlocksMarkdown(clone, depth).trim();
	      const lines = text.split("\n");
	      const first = lines.shift() || "";
	      const rest = lines.map((line) => line ? `    ${line}` : "").join("\n");
	      return rest ? `[^${label}]: ${first}\n${rest}` : `[^${label}]: ${first}`;
	    }

	    function footnoteLabelFromHref(href) {
	      if (!href.startsWith("#")) {
	        return "";
	      }
	      return footnoteLabelFromId(href.slice(1));
	    }

	    function footnoteLabelFromId(id) {
	      const trimmed = normalizeText(id || "").trim();
	      if (!trimmed) {
	        return "";
	      }
	      try {
	        return decodeURIComponent(trimmed);
	      } catch (_) {
	        return trimmed;
	      }
	    }

	    function definitionListMarkdown(list, depth) {
	      const blocks = [];
	      let currentTerm = "";
	      for (const child of Array.from(list.children)) {
	        const tag = child.tagName.toLowerCase();
	        if (tag === "dt") {
	          currentTerm = inlineMarkdown(child).trim();
	          if (currentTerm) {
	            blocks.push(currentTerm);
	          }
	        } else if (tag === "dd") {
	          const definition = childBlocksMarkdown(child, depth + 1).trim() || inlineMarkdown(child).trim();
	          const lines = definition.split("\n");
	          const first = lines.shift() || "";
	          const rest = lines.map((line) => line ? `  ${line}` : "").join("\n");
	          blocks.push(rest ? `: ${first}\n${rest}` : `: ${first}`);
	        }
	      }
	      return blocks.join("\n");
	    }

	    function isMathSpan(node) {
	      return node.classList.contains("math-inline") || node.classList.contains("math-display");
	    }

	    function mathMarkdown(node) {
	      const text = normalizeText(node.textContent || "");
	      if (node.classList.contains("math-display")) {
	        return text ? `$$${text}$$` : "";
	      }
	      return text ? `$${text}$` : "";
	    }

	    function markdownDestination(destination) {
	      return normalizeText(destination).replace(/\)/g, "%29");
	    }

	    function markdownTitle(title) {
	      return normalizeText(title).replace(/\\/g, "\\\\").replace(/"/g, "\\\"");
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
        saveSettings(themeId);
      }
    }

	    function saveSettings(themeId) {
	      linkClickBehavior = normalizeLinkClickBehavior(linkClickBehaviorSelect.value);
	      allowRemoteImages = Boolean(allowRemoteImagesInput.checked);
	      postMessage({
	        kind: "saveSettings",
	        settings: {
	          themeId,
	          customCss,
	          allowedLaunchExtensions,
	          linkClickBehavior,
	          allowRemoteImages
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

    function sanitizeAllowedLaunchExtensions(value) {
      const source = Array.isArray(value) ? value.join(" ") : normalizeText(value || "");
      const result = [];
      for (const token of source.split(/[\s,;]+/)) {
        const extension = token.trim().replace(/^\.+/, "").slice(0, 32).toLowerCase();
        if (!extension || !/^[a-z0-9_-]+$/.test(extension)) {
          continue;
        }
        if (!result.includes(extension)) {
          result.push(extension);
        }
        if (result.length >= 64) {
          break;
        }
      }
      return result;
    }

    function formatAllowedLaunchExtensions(extensions) {
      return extensions.map((extension) => `.${extension}`).join(", ");
    }

    function normalizeLinkClickBehavior(value) {
      return value === "navigate" ? "navigate" : "newWindow";
    }

    function alternateLinkBehavior() {
      return linkClickBehavior === "navigate" ? "newWindow" : "navigate";
    }

    function linkBehaviorLabel(behavior) {
      return behavior === "navigate" ? "Navigate" : "Open in new window";
    }

    function saveAllowedLaunchExtensions() {
      allowedLaunchExtensions = sanitizeAllowedLaunchExtensions(allowedLaunchExtensionsInput.value);
      allowedLaunchExtensionsInput.value = formatAllowedLaunchExtensions(allowedLaunchExtensions);
      saveSettings(themeSelect.value);
    }

	    function saveLinkClickBehavior() {
	      linkClickBehavior = normalizeLinkClickBehavior(linkClickBehaviorSelect.value);
	      linkClickBehaviorSelect.value = linkClickBehavior;
	      saveSettings(themeSelect.value);
	    }

	    function saveAllowRemoteImages() {
	      allowRemoteImages = Boolean(allowRemoteImagesInput.checked);
	      saveSettings(themeSelect.value);
	      if (toggle.checked) {
	        const marker = markerFromRendered();
	        commitRenderedEditsToMarkdown();
	        postMessage({ kind: "render", markdown: editor.value, marker });
	      }
	    }

    function openSettings() {
      hideLinkMenu();
      settingsBackdrop.hidden = false;
      themeSelect.focus();
    }

    function closeSettings() {
      settingsBackdrop.hidden = true;
      settingsButton.focus();
    }

    function confirmNavigationIfNeeded(message = "Navigate without saving changes?") {
      return !dirty || window.confirm(message);
    }

    function openLinkWithBehavior(href, behavior) {
      const normalizedBehavior = normalizeLinkClickBehavior(behavior);
      if (normalizedBehavior === "navigate" && !confirmNavigationIfNeeded()) {
        return;
      }
      hideLinkMenu();
      postMessage({ kind: "openLink", href, behavior: normalizedBehavior });
    }

    function requestBack() {
      if (backButton.disabled || !confirmNavigationIfNeeded("Go back without saving changes?")) {
        return;
      }
      hideLinkMenu();
      postMessage({ kind: "goBack" });
    }

    function showLinkMenu(href, event) {
      if (!href.trim()) {
        return;
      }
      linkMenuHref = href;
      const behavior = alternateLinkBehavior();
      linkMenuAction.textContent = linkBehaviorLabel(behavior);
      linkMenu.hidden = false;
      const width = linkMenu.offsetWidth || 188;
      const height = linkMenu.offsetHeight || 38;
      linkMenu.style.left = `${Math.max(4, Math.min(event.clientX, window.innerWidth - width - 4))}px`;
      linkMenu.style.top = `${Math.max(4, Math.min(event.clientY, window.innerHeight - height - 4))}px`;
      linkMenuAction.focus();
    }

    function hideLinkMenu() {
      linkMenu.hidden = true;
      linkMenuHref = "";
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

    function openInsertForm(kind, returnFocus = null) {
      saveRenderedSelection();
      hideLinkMenu();
      insertKind = kind;
      insertReturnFocus = returnFocus;
      const selectedText = selectedRenderedText();
      const defaults = {
        link: {
          title: "Insert Link",
          submit: "Insert",
          fields: `
            <label class="field" for="insert-link-url">URL<input id="insert-link-url" name="url" type="text" autocomplete="off"></label>
            <label class="field" for="insert-link-text">Text<input id="insert-link-text" name="text" type="text" autocomplete="off" value="${escapeAttribute(selectedText)}"></label>
          `
        },
        image: {
          title: "Insert Image",
          submit: "Insert",
          fields: `
            <label class="field" for="insert-image-source">Source<input id="insert-image-source" name="source" type="text" autocomplete="off"></label>
            <input id="insert-image-preview" name="previewSrc" type="hidden">
            <label class="field checkbox-field" for="insert-image-embed"><span>Embed selected file as base64</span><input id="insert-image-embed" name="embedBase64" type="checkbox"></label>
            <button id="insert-image-browse" class="settings-button" type="button">Choose image...</button>
            <label class="field" for="insert-image-alt">Alt text<input id="insert-image-alt" name="alt" type="text" autocomplete="off" value="${escapeAttribute(selectedText)}"></label>
            <label class="field" for="insert-image-title">Title<input id="insert-image-title" name="title" type="text" autocomplete="off"></label>
            <label class="field" for="insert-image-width">Width<input id="insert-image-width" name="width" type="text" autocomplete="off" placeholder="640 or 50%"></label>
          `
        },
        table: {
          title: "Insert Table",
          submit: "Insert",
          fields: `
            <label class="field" for="insert-table-rows">Rows<input id="insert-table-rows" name="rows" type="number" inputmode="numeric" min="1" max="500" step="1" value="3"></label>
            <label class="field" for="insert-table-columns">Columns<input id="insert-table-columns" name="columns" type="number" inputmode="numeric" min="1" max="50" step="1" value="3"></label>
          `
        },
        footnote: {
          title: "Insert Footnote",
          submit: "Insert",
          fields: `
            <label class="field" for="insert-footnote-label">Label<input id="insert-footnote-label" name="label" type="text" autocomplete="off" value="${escapeAttribute(nextFootnoteLabel())}"></label>
            <label class="field" for="insert-footnote-body">Text<textarea id="insert-footnote-body" name="body" spellcheck="true">${escapeHtml(selectedText || "Footnote text")}</textarea></label>
          `
        },
        definition: {
          title: "Insert Definition List",
          submit: "Insert",
          fields: `
            <label class="field" for="insert-definition-term">Term<input id="insert-definition-term" name="term" type="text" autocomplete="off" value="${escapeAttribute(selectedText || "Term")}"></label>
            <label class="field" for="insert-definition-body">Definition<textarea id="insert-definition-body" name="definition" spellcheck="true">Definition</textarea></label>
          `
        },
        math: {
          title: "Insert Math",
          submit: "Insert",
          fields: `
            <label class="field" for="insert-math-expression">Expression<input id="insert-math-expression" name="expression" type="text" autocomplete="off" value="${escapeAttribute(selectedText || "a+b")}"></label>
            <label class="field" for="insert-math-mode">Mode<select id="insert-math-mode" name="mode"><option value="inline">Inline</option><option value="display">Display</option></select></label>
          `
        }
      };
      const definition = defaults[kind];
      if (!definition) {
        return;
      }
      insertTitle.textContent = definition.title;
      insertSubmit.textContent = definition.submit;
      insertFields.innerHTML = definition.fields;
      const browseButton = document.getElementById("insert-image-browse");
      if (browseButton) {
        browseButton.addEventListener("click", pickImageFile);
      }
      insertBackdrop.hidden = false;
      requestAnimationFrame(() => {
        const firstField = insertFields.querySelector("input, textarea, select");
        if (firstField) {
          firstField.focus();
          if (typeof firstField.select === "function") {
            firstField.select();
          }
        }
      });
    }

    function closeInsertForm(restoreFocus = true) {
      insertBackdrop.hidden = true;
      insertFields.innerHTML = "";
      insertKind = "";
      if (restoreFocus && insertReturnFocus) {
        insertReturnFocus.focus();
      }
      insertReturnFocus = null;
    }

    function submitInsertForm() {
      if (insertKind === "link") {
        applyLinkFromForm();
      } else if (insertKind === "image") {
        applyImageFromForm();
      } else if (insertKind === "table") {
        applyTableFromForm();
      } else if (insertKind === "footnote") {
        applyFootnoteFromForm();
      } else if (insertKind === "definition") {
        applyDefinitionListFromForm();
      } else if (insertKind === "math") {
        applyMathFromForm();
      }
    }

    function formValue(name) {
      const field = insertForm.elements.namedItem(name);
      return field ? normalizeText(field.value || "").trim() : "";
    }

    function setFormValue(name, value) {
      const field = insertForm.elements.namedItem(name);
      if (field) {
        field.value = value || "";
      }
    }

    function formNumber(name, fallback, min, max) {
      const field = insertForm.elements.namedItem(name);
      const parsed = Number.parseInt(field ? field.value : "", 10);
      const value = Number.isFinite(parsed) ? parsed : fallback;
      const bounded = Math.max(min, Math.min(max, value));
      if (field) {
        field.value = String(bounded);
      }
      return bounded;
    }

    function pickImageFile() {
      const embedField = insertForm.elements.namedItem("embedBase64");
      postMessage({ kind: "pickImage", embedBase64: Boolean(embedField && embedField.checked) });
    }

    function applyLinkFromForm() {
      const url = formValue("url");
      if (!url) {
        focusInsertField("url");
        return;
      }
      const label = formValue("text") || selectedRenderedText() || url;
      restoreRenderedSelection();
      const selection = window.getSelection();
      if (!selection || selection.rangeCount === 0 || selection.isCollapsed) {
        document.execCommand("insertHTML", false, `<a href="${escapeAttribute(url)}">${escapeHtml(label)}</a>`);
      } else {
        document.execCommand("createLink", false, url);
      }
      closeInsertForm(false);
      markRenderedEdited();
    }

    function applyImageFromForm() {
      const source = formValue("source");
      if (!source) {
        focusInsertField("source");
        return;
      }
      const alt = formValue("alt");
      const title = formValue("title");
      const previewSrc = formValue("previewSrc") || source;
      const width = normalizeImageDimension(formValue("width"));
      insertHtmlAtSelection(`<img src="${escapeAttribute(previewSrc)}" alt="${escapeAttribute(alt)}" data-md-src="${escapeAttribute(source)}"${title ? ` title="${escapeAttribute(title)}" data-md-title="${escapeAttribute(title)}"` : ""}${width ? ` data-md-width="${escapeAttribute(width)}"` : ""}>`);
      applyImageSizes(rendered);
      closeInsertForm(false);
    }

    function applyTableFromForm() {
      const rows = formNumber("rows", 3, 1, 500);
      const columns = formNumber("columns", 3, 1, 50);
      insertHtmlAtSelection(blockWithTrailingParagraph(tableHtml(rows, columns)));
      closeInsertForm(false);
    }

    function applyFootnoteFromForm() {
      const label = sanitizeFootnoteLabel(formValue("label") || nextFootnoteLabel());
      const body = formValue("body") || "Footnote text";
      insertHtmlAtSelection(`<sup class="footnote-reference"><a href="#${escapeAttribute(label)}">${escapeHtml(label)}</a></sup>`);
      rendered.insertAdjacentHTML("beforeend", `<div class="footnote-definition" id="${escapeAttribute(label)}"><sup class="footnote-definition-label">${escapeHtml(label)}</sup><p>${escapeHtml(body)}</p></div>`);
      closeInsertForm(false);
      markRenderedEdited();
    }

    function applyDefinitionListFromForm() {
      const term = formValue("term") || "Term";
      const definition = formValue("definition") || "Definition";
      insertHtmlAtSelection(blockWithTrailingParagraph(`<dl><dt>${escapeHtml(term)}</dt><dd>${escapeHtml(definition)}</dd></dl>`));
      closeInsertForm(false);
    }

    function applyMathFromForm() {
      const expression = formValue("expression") || "a+b";
      const mode = formValue("mode") === "display" ? "display" : "inline";
      if (mode === "display") {
        insertHtmlAtSelection(`<div><span class="math math-display">${escapeHtml(expression)}</span></div>`);
      } else {
        insertHtmlAtSelection(`<span class="math math-inline">${escapeHtml(expression)}</span>`);
      }
      closeInsertForm(false);
    }

    function applyTaskList() {
      const text = selectedRenderedText() || "Task";
      insertHtmlAtSelection(blockWithTrailingParagraph(`<ul><li><input type="checkbox" disabled> ${escapeHtml(text)}</li></ul>`));
    }

    function applyCodeBlock() {
      const text = selectedRenderedText() || "code";
      insertHtmlAtSelection(blockWithTrailingParagraph(`<pre><code>${escapeHtml(text)}</code></pre>`));
    }

    function blockWithTrailingParagraph(html) {
      return `${html}<p><br></p>`;
    }

    function insertHtmlAtSelection(html) {
      restoreRenderedSelection();
      document.execCommand("insertHTML", false, html);
      markRenderedEdited();
    }

    function focusInsertField(name) {
      const field = insertForm.elements.namedItem(name);
      if (field) {
        field.focus();
      }
    }

    function selectedRenderedText() {
      const selection = window.getSelection();
      if (!selection || selection.rangeCount === 0 || !selection.anchorNode || !rendered.contains(selection.anchorNode)) {
        return "";
      }
      return normalizeText(selection.toString()).trim();
    }

    function nextFootnoteLabel() {
      let index = 1;
      while (document.getElementById(`note-${index}`) || rendered.querySelector(`a[href="#note-${index}"]`)) {
        index += 1;
      }
      return `note-${index}`;
    }

    function sanitizeFootnoteLabel(label) {
      const cleaned = normalizeText(label)
        .replace(/^\[\^/, "")
        .replace(/\]$/, "")
        .trim()
        .replace(/[^A-Za-z0-9_-]+/g, "-")
        .replace(/^-+|-+$/g, "");
      return cleaned || nextFootnoteLabel();
    }

	    function tableHtml(rows, columns) {
	      const headCells = Array.from({ length: columns }, (_, index) => `<th>Header ${index + 1}</th>`).join("");
	      const bodyRows = Array.from({ length: Math.max(0, rows - 1) }, (_, rowIndex) => {
	        const cells = Array.from({ length: columns }, (_, columnIndex) => `<td>Cell ${rowIndex + 1}.${columnIndex + 1}</td>`).join("");
	        return `<tr>${cells}</tr>`;
	      }).join("");
	      return `<table><thead><tr>${headCells}</tr></thead><tbody>${bodyRows}</tbody></table>`;
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
      applyImageSizes(rendered);
      clearImageSelection();
      renderedDirty = false;
      if (expandedMarkdown !== previousMarkdown) {
        setDirty(true);
      }
      document.body.classList.remove("editing");
      toggle.checked = true;
      requestAnimationFrame(() => applyMarkerToRendered(marker));
    };

    window.__mdReaderLoadDocument = (expandedMarkdown, html) => {
      hideLinkMenu();
      editor.value = expandedMarkdown;
      rendered.innerHTML = html;
      applyImageSizes(rendered);
      clearImageSelection();
      renderedDirty = false;
      document.body.classList.remove("editing");
      toggle.checked = true;
      editor.scrollTop = 0;
      rendered.scrollTop = 0;
      setDirty(false);
    };

    window.__mdReaderSetCanGoBack = setCanGoBack;

    window.__mdReaderRemoteImageReady = (source, fileUrl) => {
      if (!source || !fileUrl) {
        return;
      }
      for (const image of rendered.querySelectorAll("img[data-md-src]")) {
        if (image.getAttribute("data-md-src") === source) {
          image.setAttribute("src", fileUrl);
        }
      }
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

    window.__mdReaderImagePicked = (ok, payload) => {
      if (!ok) {
        alert(payload || "Image picker failed.");
        return;
      }
      if (insertKind !== "image" || insertBackdrop.hidden || !payload) {
        return;
      }
      setFormValue("source", payload.source || "");
      setFormValue("previewSrc", payload.previewSrc || payload.source || "");
      if (payload.alt && !formValue("alt")) {
        setFormValue("alt", payload.alt);
      }
    };

    window.__mdReaderOpenLinkFailed = (message) => {
      alert(message || "Link could not be opened.");
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
    backButton.addEventListener("click", requestBack);

    settingsButton.addEventListener("click", openSettings);
    settingsClose.addEventListener("click", closeSettings);
    settingsBackdrop.addEventListener("click", (event) => {
      if (event.target === settingsBackdrop) {
        closeSettings();
      }
    });
    insertClose.addEventListener("click", () => closeInsertForm());
    insertCancel.addEventListener("click", () => closeInsertForm());
    insertBackdrop.addEventListener("click", (event) => {
      if (event.target === insertBackdrop) {
        closeInsertForm();
      }
    });
    insertForm.addEventListener("submit", (event) => {
      event.preventDefault();
      submitInsertForm();
    });

    themeSelect.addEventListener("change", () => {
      applyTheme(themeSelect.value);
    });

    linkClickBehaviorSelect.addEventListener("change", saveLinkClickBehavior);
    allowRemoteImagesInput.addEventListener("change", saveAllowRemoteImages);
    allowedExtensionsHelpButton.addEventListener("click", () => {
      setAllowedExtensionsHelpVisible(allowedExtensionsHelpButton.getAttribute("aria-expanded") !== "true");
    });
    allowedExtensionsHelpButton.addEventListener("blur", () => {
      setAllowedExtensionsHelpVisible(false);
    });

    customCssButton.addEventListener("click", pickCustomCss);
    allowedLaunchExtensionsInput.addEventListener("change", saveAllowedLaunchExtensions);

    document.querySelectorAll("[data-command]").forEach((button) => {
      button.addEventListener("mousedown", (event) => event.preventDefault());
      button.addEventListener("click", () => applyDocumentCommand(button.dataset.command));
    });

    blockFormat.addEventListener("change", () => {
      applyBlockFormat(blockFormat.value);
    });

    inlineCodeButton.addEventListener("mousedown", (event) => event.preventDefault());
    inlineCodeButton.addEventListener("click", applyInlineCode);

	    codeBlockButton.addEventListener("mousedown", (event) => event.preventDefault());
	    codeBlockButton.addEventListener("click", applyCodeBlock);

	    linkButton.addEventListener("mousedown", (event) => event.preventDefault());
	    linkButton.addEventListener("click", () => openInsertForm("link", linkButton));

	    imageButton.addEventListener("mousedown", (event) => event.preventDefault());
	    imageButton.addEventListener("click", () => openInsertForm("image", imageButton));

	    mathButton.addEventListener("mousedown", (event) => event.preventDefault());
	    mathButton.addEventListener("click", () => openInsertForm("math", mathButton));

	    taskListButton.addEventListener("mousedown", (event) => event.preventDefault());
	    taskListButton.addEventListener("click", applyTaskList);

	    tableButton.addEventListener("mousedown", (event) => event.preventDefault());
	    tableButton.addEventListener("click", () => openInsertForm("table", tableButton));

	    definitionListButton.addEventListener("mousedown", (event) => event.preventDefault());
	    definitionListButton.addEventListener("click", () => openInsertForm("definition", definitionListButton));

	    footnoteButton.addEventListener("mousedown", (event) => event.preventDefault());
	    footnoteButton.addEventListener("click", () => openInsertForm("footnote", footnoteButton));

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
      if (event.key === "Escape" && !insertBackdrop.hidden) {
        event.preventDefault();
        closeInsertForm();
      }
      if (event.key === "Escape" && !linkMenu.hidden) {
        event.preventDefault();
        hideLinkMenu();
      }
      if (event.key === "Escape" && selectedImage) {
        event.preventDefault();
        clearImageSelection();
      }
      if (event.key === "Escape" && allowedExtensionsHelpButton.getAttribute("aria-expanded") === "true") {
        event.preventDefault();
        setAllowedExtensionsHelpVisible(false);
        allowedExtensionsHelpButton.focus();
      }
    });

    rendered.addEventListener("click", (event) => {
      const image = event.target.closest("img");
      if (image && rendered.contains(image)) {
        event.preventDefault();
        selectImage(image);
        return;
      }
      clearImageSelection();
      const anchor = event.target.closest("a");
      if (anchor) {
        event.preventDefault();
        const href = anchor.getAttribute("href") || "";
        if (href.startsWith('#')) {
          const target = document.getElementById(href.slice(1));
          if (target) {
            rendered.scrollTop = Math.max(0, target.offsetTop - 28);
          }
        } else if (href.trim()) {
          openLinkWithBehavior(href, linkClickBehavior);
        }
      }
    });

    rendered.addEventListener("contextmenu", (event) => {
      const anchor = event.target.closest("a");
      if (!anchor) {
        return;
      }
      event.preventDefault();
      const href = anchor.getAttribute("href") || "";
      if (href.startsWith('#')) {
        return;
      }
      showLinkMenu(href, event);
    });

    linkMenuAction.addEventListener("click", () => {
      if (linkMenuHref) {
        openLinkWithBehavior(linkMenuHref, alternateLinkBehavior());
      }
    });

    imageResizeOverlay.querySelectorAll(".image-resize-handle").forEach((handle) => {
      handle.addEventListener("mousedown", startImageResize);
    });
    imageResizeReset.addEventListener("click", (event) => {
      event.preventDefault();
      resetSelectedImageSize();
    });

    document.addEventListener("click", (event) => {
      if (!linkMenu.hidden && !linkMenu.contains(event.target)) {
        hideLinkMenu();
      }
      if (selectedImage && !imageResizeOverlay.contains(event.target) && event.target !== selectedImage) {
        clearImageSelection();
      }
    });

    rendered.addEventListener("scroll", () => {
      hideLinkMenu();
      updateImageResizeOverlay();
    });
    window.addEventListener("resize", () => {
      hideLinkMenu();
      updateImageResizeOverlay();
    });

    populateThemes();
    const storedTheme = initialSettings.themeId || "clean";
    const themeIsKnown = storedTheme === "custom" ? Boolean(customCss) : themes.some((theme) => theme.id === storedTheme);
    const initialTheme = themeIsKnown ? storedTheme : "clean";
	    themeSelect.value = initialTheme;
	    linkClickBehaviorSelect.value = linkClickBehavior;
	    allowRemoteImagesInput.checked = allowRemoteImages;
	    applyTheme(initialTheme, false);
    document.body.classList.remove("editing");
    toggle.checked = true;
    setCanGoBack(false);
    setDirty(false);
  </script>
</body>
</html>
"###;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use tao::platform::windows::EventLoopBuilderExtWindows;

    fn test_image_cache() -> ImageCache {
        let mut builder = EventLoopBuilder::<AppEvent>::with_user_event();
        #[cfg(windows)]
        builder.with_any_thread(true);
        let event_loop = builder.build();
        let proxy = event_loop.create_proxy();
        ImageCache::new(proxy).expect("image cache should initialize")
    }

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
    fn image_picker_ipc_accepts_button_payload() {
        let message: IpcMessage =
            serde_json::from_str(r#"{"kind":"pickImage","embedBase64":true}"#)
                .expect("choose-image button payload should deserialize");

        match message {
            IpcMessage::PickImage { embed_base64 } => assert!(embed_base64),
            other => panic!("expected PickImage message, got {other:?}"),
        }
    }

    #[test]
    fn app_html_includes_theme_and_control_data() {
        let mut image_cache = test_image_cache();
        let html = build_app_html(
            "# One",
            Path::new("C:/Docs/source.md"),
            &AppSettings::default(),
            &mut image_cache,
        )
        .expect("app html should render");

        assert!(!html.contains("__THEMES__"));
        assert!(!html.contains("__SETTINGS__"));
        assert!(html.contains("back-button"));
        assert!(html.contains(r#"title="Back""#));
        assert!(html.contains(r#"kind: "goBack""#));
        assert!(html.contains("requestBack"));
        assert!(html.contains("__mdReaderSetCanGoBack"));
        assert!(html.contains("Go back without saving changes?"));
        assert!(html.contains("save-button"));
        assert!(html.contains("Save (Ctrl+S)"));
        assert!(html.contains("settings-button"));
        assert!(html.contains("format-toolbar"));
        assert!(html.contains(r#"title="Strikethrough""#));
        assert!(html.contains("data-command=\"subscript\""));
        assert!(html.contains("data-command=\"superscript\""));
        assert!(html.contains("script-down"));
        assert!(html.contains("script-up"));
        assert!(html.contains("code-block-button"));
        assert!(html.contains("math-button"));
        assert!(html.contains("image-button"));
        assert!(html.contains("image-resize-overlay"));
        assert!(html.contains("image-resize-handle"));
        assert!(html.contains("startImageResize"));
        assert!(html.contains("imageSizeMarkdown"));
        assert!(html.contains("data-md-width"));
        assert!(html.contains("insert-image-width"));
        assert!(html.contains("insert-image-browse"));
        assert!(html.contains("insert-image-embed"));
        assert!(html.contains("previewSrc"));
        assert!(html.contains(r#"kind: "pickImage""#));
        assert!(html.contains("__mdReaderImagePicked"));
        assert!(html.contains("task-list-button"));
        assert!(html.contains("table-button"));
        assert!(html.contains(r#"max="500""#));
        assert!(html.contains(r#"max="50""#));
        assert!(html.contains("definition-list-button"));
        assert!(html.contains("footnote-button"));
        assert!(html.contains("insert-backdrop"));
        assert!(html.contains("insert-form"));
        assert!(html.contains(r#"grid-template-rows: 34px minmax(0, 1fr)"#));
        assert!(html.contains("contenteditable=\"true\""));
        assert!(html.contains(r#""themeId":"clean""#));
        assert!(html.contains(r#""allowRemoteImages":false"#));
        assert!(html.contains(r#"kind: "saveSettings""#));
        assert!(html.contains("Plaintext: Alt+Left. Formatted: Alt+Right."));
        assert!(html.contains("showPlaintextView"));
        assert!(html.contains("showRenderedView"));
        assert!(html.contains("custom-css-button"));
        assert!(html.contains("pickCustomCss"));
        assert!(html.contains("__mdReaderCustomCssPicked"));
        assert!(html.contains("allowed-launch-extensions"));
        assert!(html.contains("allowedLaunchExtensions"));
        assert!(html.contains("allowed-extensions-help"));
        assert!(html.contains("Allowed by default: no extension, .bmp"));
        assert!(html.contains(".md, .markdown"));
        assert!(!html.contains("not executable or script-like"));
        assert!(html.contains("allow-remote-images"));
        assert!(html.contains("allowRemoteImages"));
        assert!(html.contains("link-click-behavior"));
        assert!(html.contains("linkClickBehavior"));
        assert!(html.contains("#rendered a"));
        assert!(html.contains("cursor: pointer"));
        assert!(html.contains("__mdReaderLoadDocument"));
        assert!(html.contains("link-menu-action"));
        assert!(html.contains("contextmenu"));
        assert!(html.contains("openLink"));
        assert!(html.contains("__mdReaderOpenLinkFailed"));
        assert!(html.contains("tableHtml"));
        assert!(html.contains("openInsertForm"));
        assert!(html.contains("applyImageFromForm"));
        assert!(html.contains("applyTaskList"));
        assert!(html.contains("applyCodeBlock"));
        assert!(html.contains("blockWithTrailingParagraph"));
        assert!(html.contains("taskListPrefix"));
        assert!(html.contains("mathMarkdown"));
        assert!(!html.contains("window.prompt"));
        assert!(!html.contains("type=\"file\""));
        assert!(!html.contains("localStorage"));
    }

    #[test]
    fn app_html_requires_explicit_save() {
        let mut image_cache = test_image_cache();
        let html = build_app_html(
            "[toc]\n\n# One",
            Path::new("C:/Docs/source.md"),
            &AppSettings::default(),
            &mut image_cache,
        )
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
    fn renderer_resolves_local_images_against_document_parent() {
        let root =
            env::temp_dir().join(format!("markdown-reader-image-test-{}", std::process::id()));
        let image_dir = root.join("images");
        fs::create_dir_all(&image_dir).expect("image test dir should be created");
        let image_path = image_dir.join("pic.png");
        fs::write(&image_path, TRANSPARENT_GIF).expect("image fixture should be written");
        let document_path = root.join("source.md");
        let mut image_cache = test_image_cache();
        let html = render_markdown_for_document(
            "![Diagram](images/pic.png \"Flow\")",
            &document_path,
            &AppSettings::default(),
            &mut image_cache,
        );
        let expected_src =
            image_data_uri_from_path(&image_path).expect("image data URI should build");

        assert!(html.contains(&format!(r#"src="{expected_src}""#)));
        assert!(!html.contains(r#"src="file:"#));
        assert!(html.contains(r#"data-md-src="images/pic.png""#));
        assert!(html.contains(r#"data-md-title="Flow""#));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn renderer_preserves_markdown_image_size_attributes() {
        let root = env::temp_dir().join(format!(
            "markdown-reader-image-size-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("image size test dir should be created");
        let image_path = root.join("pic.png");
        fs::write(&image_path, TRANSPARENT_GIF).expect("image fixture should be written");
        let document_path = root.join("source.md");
        let mut image_cache = test_image_cache();
        let html = render_markdown_for_document(
            "![Diagram](pic.png){width=320 height=240}\n\nAfter",
            &document_path,
            &AppSettings::default(),
            &mut image_cache,
        );

        assert!(html.contains(r#"data-md-src="pic.png""#));
        assert!(html.contains(r#"data-md-width="320""#));
        assert!(html.contains(r#"data-md-height="240""#));
        assert!(html.contains(">After</p>"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn image_size_attributes_accept_percent_widths_and_reject_script_like_values() {
        assert_eq!(
            parse_image_size_attribute_suffix("{width=50%}").map(|(attrs, _)| attrs.width),
            Some(Some("50%".to_string()))
        );
        assert_eq!(
            parse_image_size_attribute_suffix("{width=320px height='240'}")
                .map(|(attrs, _)| { (attrs.width, attrs.height) }),
            Some((Some("320".to_string()), Some("240".to_string())))
        );
        assert!(parse_image_size_attribute_suffix("{width=expression(alert(1))}").is_none());
    }

    #[test]
    fn renderer_does_not_fetch_remote_images_when_disabled() {
        let mut image_cache = test_image_cache();
        let html = render_markdown_for_document(
            "![Remote](https://example.com/pic.png)",
            Path::new("C:/Docs/source.md"),
            &AppSettings::default(),
            &mut image_cache,
        );

        assert!(html.contains(r#"data-md-src="https://example.com/pic.png""#));
        assert!(html.contains("data:image/gif;base64,"));
        assert!(!html.contains(r#"<img src="https://"#));
        assert!(!html.contains(r#"src="file:"#));
    }

    #[test]
    fn remote_image_url_validation_blocks_risky_hosts_and_schemes() {
        assert!(validate_remote_image_url("https://example.com/pic.png").is_ok());
        assert!(validate_remote_image_url("http://example.com/pic.png").is_err());
        assert!(validate_remote_image_url("https://user:secret@example.com/pic.png").is_err());
        assert!(validate_remote_image_url("https://localhost/pic.png").is_err());
        assert!(validate_remote_image_url("https://127.0.0.1/pic.png").is_err());
        assert!(validate_remote_image_url("https://10.0.0.4/pic.png").is_err());
        assert!(validate_remote_image_url("https://[::ffff:127.0.0.1]/pic.png").is_err());
        assert!(validate_remote_image_url("//example.com/pic.png").is_err());
    }

    #[test]
    fn image_type_detection_allows_only_default_raster_formats() {
        assert_eq!(
            image_extension_from_content_type("image/png"),
            Some("png".to_string())
        );
        assert_eq!(
            image_extension_from_magic(b"\x89PNG\r\n\x1A\nabc"),
            Some("png".to_string())
        );
        assert_eq!(
            image_extension_from_magic(&[0xFF, 0xD8, 0xFF, 0x00]),
            Some("jpg".to_string())
        );
        assert_eq!(
            image_extension_from_magic(b"GIF89aabc"),
            Some("gif".to_string())
        );
        assert_eq!(
            image_extension_from_magic(b"BMabc"),
            Some("bmp".to_string())
        );
        assert_eq!(
            image_extension_from_magic(b"RIFFxxxxWEBPabc"),
            Some("webp".to_string())
        );
        assert_eq!(image_extension_from_content_type("image/svg+xml"), None);
        assert_eq!(normalize_image_extension("svg"), None);
        assert_eq!(normalize_image_extension("ico"), None);
        assert_eq!(encode_base64(b"abc"), "YWJj");
        assert_eq!(encode_base64(b"ab"), "YWI=");
        assert!(is_safe_data_image_src("data:image/png;base64,YWJj"));
        assert!(!is_safe_data_image_src(
            "data:text/html;base64,PGgxPk5vPC9oMT4="
        ));
    }

    #[test]
    fn renderer_allows_safe_embedded_data_images() {
        let mut image_cache = test_image_cache();
        let html = render_markdown_for_document(
            "![Pixel](data:image/png;base64,YWJj)",
            Path::new("C:/Docs/source.md"),
            &AppSettings::default(),
            &mut image_cache,
        );

        assert!(html.contains(r#"src="data:image/png;base64,YWJj""#));
        assert!(html.contains(r#"data-md-src="data:image/png;base64,YWJj""#));
    }

    #[test]
    fn renderer_preserves_supported_markdown_extras() {
        let html = render_markdown_safely(
            "- [x] Done\n- [ ] Todo\n\n~~gone~~\n\nTerm\n: Definition\n\n~subscript~ and ^superscript^ and $a+b$\n\nFootnote[^note]\n\n[^note]: Body",
        );

        assert!(html.contains(r#"type="checkbox""#));
        assert!(html.contains(r#"checked=""#));
        assert!(html.contains("<del>gone</del>"));
        assert!(html.contains("<dl>"));
        assert!(html.contains("<dt>Term</dt>"));
        assert!(html.contains("<dd>Definition</dd>"));
        assert!(html.contains("<sub>subscript</sub>"));
        assert!(html.contains("<sup>superscript</sup>"));
        assert!(html.contains(r#"class="math math-inline""#));
        assert!(html.contains(r#"class="footnote-reference""#));
        assert!(html.contains(r#"class="footnote-definition""#));
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
            allowed_launch_extensions: Vec::new(),
            link_click_behavior: "missing".to_string(),
            allow_remote_images: true,
        });

        assert_eq!(settings.theme_id, "clean");
        assert!(settings.custom_css.is_empty());
        assert!(settings.allowed_launch_extensions.is_empty());
        assert_eq!(settings.link_click_behavior, LINK_BEHAVIOR_NEW_WINDOW);
        assert!(settings.allow_remote_images);
    }

    #[test]
    fn settings_allow_custom_theme_only_with_css() {
        let without_css = normalize_settings(&AppSettings {
            theme_id: "custom".to_string(),
            custom_css: String::new(),
            allowed_launch_extensions: Vec::new(),
            link_click_behavior: LINK_BEHAVIOR_NEW_WINDOW.to_string(),
            allow_remote_images: false,
        });
        let with_css = normalize_settings(&AppSettings {
            theme_id: "custom".to_string(),
            custom_css: "body { color: #123; }".to_string(),
            allowed_launch_extensions: Vec::new(),
            link_click_behavior: LINK_BEHAVIOR_NEW_WINDOW.to_string(),
            allow_remote_images: false,
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
            allowed_launch_extensions: Vec::new(),
            link_click_behavior: LINK_BEHAVIOR_NEW_WINDOW.to_string(),
            allow_remote_images: false,
        });

        assert_eq!(settings.theme_id, "custom");
        assert_eq!(settings.custom_css, "body { color: #123; }");
    }

    #[test]
    fn settings_normalize_allowed_launch_extensions() {
        let settings = normalize_settings(&AppSettings {
            theme_id: "clean".to_string(),
            custom_css: String::new(),
            allowed_launch_extensions: vec![
                ".EXE, ps1".to_string(),
                "bad!*".to_string(),
                "cmd".to_string(),
                ".cmd".to_string(),
            ],
            link_click_behavior: LINK_BEHAVIOR_NAVIGATE.to_string(),
            allow_remote_images: false,
        });

        assert_eq!(
            settings.allowed_launch_extensions,
            vec!["exe".to_string(), "ps1".to_string(), "cmd".to_string()]
        );
        assert_eq!(settings.link_click_behavior, LINK_BEHAVIOR_NAVIGATE);
    }

    #[test]
    fn settings_normalize_unknown_link_click_behavior_to_new_window() {
        let settings = normalize_settings(&AppSettings {
            theme_id: "clean".to_string(),
            custom_css: String::new(),
            allowed_launch_extensions: Vec::new(),
            link_click_behavior: "sideways".to_string(),
            allow_remote_images: false,
        });

        assert_eq!(settings.link_click_behavior, LINK_BEHAVIOR_NEW_WINDOW);
    }

    #[test]
    fn link_target_resolves_relative_documents_against_markdown_parent() {
        let target = resolve_link_target(
            Path::new("C:/Docs/source.md"),
            "references/Other%20Document.md#section",
        )
        .expect("relative document link should resolve");

        assert_eq!(
            target,
            LinkTarget::Document(PathBuf::from("C:/Docs/references/Other Document.md"))
        );
    }

    #[test]
    fn link_target_allows_common_external_schemes() {
        assert_eq!(
            resolve_link_target(Path::new("C:/Docs/source.md"), "https://example.com")
                .expect("https links should be launchable"),
            LinkTarget::Url("https://example.com".to_string())
        );
        assert_eq!(
            resolve_link_target(Path::new("C:/Docs/source.md"), "mailto:hello@example.com")
                .expect("mailto links should be launchable"),
            LinkTarget::Url("mailto:hello@example.com".to_string())
        );
        assert_eq!(
            resolve_link_target(Path::new("C:/Docs/source.md"), "file:///C:/Docs/other.md")
                .expect("file links should be launchable"),
            LinkTarget::Document(PathBuf::from("C:\\Docs\\other.md"))
        );
    }

    #[test]
    fn link_target_blocks_unsafe_schemes_and_skips_internal_anchors() {
        assert!(
            resolve_link_target(Path::new("C:/Docs/source.md"), "javascript:alert(1)").is_err()
        );
        assert!(resolve_link_target(Path::new("C:/Docs/source.md"), "data:text/html,hi").is_err());
        assert!(resolve_link_target(Path::new("C:/Docs/source.md"), "#local-heading").is_err());
    }

    #[test]
    fn link_target_blocks_potentially_executable_local_files() {
        assert!(resolve_link_target(Path::new("C:/Docs/source.md"), "tools/install.exe").is_err());
        assert!(
            resolve_link_target(Path::new("C:/Docs/source.md"), "file:///C:/Docs/run.ps1").is_err()
        );
        assert!(resolve_link_target(Path::new("C:/Docs/source.md"), "notes/custom.safe").is_err());
    }

    #[test]
    fn link_target_allows_configured_local_extensions() {
        let settings = AppSettings {
            theme_id: "clean".to_string(),
            custom_css: String::new(),
            allowed_launch_extensions: vec!["exe".to_string(), "ps1".to_string()],
            link_click_behavior: LINK_BEHAVIOR_NEW_WINDOW.to_string(),
            allow_remote_images: false,
        };

        assert_eq!(
            resolve_link_target_with_settings(
                Path::new("C:/Docs/source.md"),
                "tools/install.exe",
                &settings
            )
            .expect("configured extension should be launchable"),
            LinkTarget::Document(PathBuf::from("C:/Docs/tools/install.exe"))
        );
        assert_eq!(
            resolve_link_target_with_settings(
                Path::new("C:/Docs/source.md"),
                "file:///C:/Docs/run.ps1",
                &settings
            )
            .expect("configured file URL extension should be launchable"),
            LinkTarget::Document(PathBuf::from("C:\\Docs\\run.ps1"))
        );
    }

    #[test]
    fn file_urls_decode_to_windows_document_paths() {
        assert_eq!(
            file_url_to_path("file:///C:/Docs/Other%20Document.md").expect("file URL should parse"),
            PathBuf::from("C:\\Docs\\Other Document.md")
        );
    }

    #[test]
    fn windows_drive_paths_are_treated_as_documents_not_schemes() {
        assert_eq!(href_scheme("C:\\Docs\\Other.md"), None);
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
