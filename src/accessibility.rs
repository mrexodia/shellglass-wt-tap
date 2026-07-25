//! Accessibility-tree reconstruction for non-terminal foreground windows.
//!
//! Native terminal frames always win. This module is deliberately only a
//! semantic reconstruction: it renders roles, names, values, hierarchy, and
//! state reported by xa11y, never pixels or simulated input.

use std::collections::BTreeSet;
use std::io::{Stdout, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{
    Attribute, Color as TerminalColor, Print, ResetColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use serde::Deserialize;
use shellglass::model::{Color, Frame, Grid, StyledCell};
use shellglass::source::{FramePublisher, SourceSession, external_source};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use xa11y::{App, AppExt, Element, Rect, Role, StateSet, Toggled};

const FG: Color = Color::Rgb(214, 219, 233);
const BG: Color = Color::Rgb(18, 20, 28);
const MUTED: Color = Color::Rgb(113, 120, 145);
const CYAN: Color = Color::Rgb(101, 214, 232);
const GREEN: Color = Color::Rgb(151, 224, 126);
const YELLOW: Color = Color::Rgb(246, 193, 119);
const MAGENTA: Color = Color::Rgb(198, 160, 246);
const RED: Color = Color::Rgb(235, 111, 146);
const MAX_FIELD_CHARS: usize = 2_000;
const BUILTIN_DENIED_APPS: &[&str] = &["discord", "discordcanary", "discordptb"];

/// Shared CLI/configuration surface for accessibility reconstruction.
#[derive(Debug, Clone, Args)]
pub struct AccessibilityOptions {
    /// Accessibility snapshot interval in milliseconds.
    #[arg(long = "a11y-interval-ms", default_value_t = 300)]
    pub interval_ms: u64,
    /// Columns in reconstructed accessibility frames.
    #[arg(long = "a11y-cols", default_value_t = 120)]
    pub cols: u16,
    /// Rows in reconstructed accessibility frames.
    #[arg(long = "a11y-rows", default_value_t = 40)]
    pub rows: u16,
    /// Maximum accessibility-tree depth (at most 64).
    #[arg(long = "a11y-depth", default_value_t = 12)]
    pub max_depth: usize,
    /// Maximum accessibility nodes captured per snapshot (at most 100000).
    #[arg(long = "a11y-max-nodes", default_value_t = 1_000)]
    pub max_nodes: usize,
    /// TOML privacy policy; defaults to ./privacy.toml when that file exists.
    #[arg(long = "a11y-config", env = "SHELLGLASS_A11Y_CONFIG")]
    pub policy_config: Option<std::path::PathBuf>,
    /// Additional executable/application name that must never be captured.
    /// May be repeated or supplied comma-separated through the environment.
    #[arg(
        long = "a11y-deny-app",
        env = "SHELLGLASS_A11Y_DENY_APPS",
        value_delimiter = ','
    )]
    pub denied_apps: Vec<String>,
}

impl Default for AccessibilityOptions {
    fn default() -> Self {
        Self {
            interval_ms: 300,
            cols: 120,
            rows: 40,
            max_depth: 12,
            max_nodes: 1_000,
            policy_config: None,
            denied_apps: Vec::new(),
        }
    }
}

impl AccessibilityOptions {
    pub fn validate(&self) -> Result<()> {
        if self.interval_ms < 50 {
            bail!("--a11y-interval-ms must be at least 50");
        }
        if !(20..=500).contains(&self.cols) {
            bail!("--a11y-cols must be between 20 and 500");
        }
        if !(6..=200).contains(&self.rows) {
            bail!("--a11y-rows must be between 6 and 200");
        }
        if self.max_depth > 64 {
            bail!("--a11y-depth must not exceed 64");
        }
        if !(1..=100_000).contains(&self.max_nodes) {
            bail!("--a11y-max-nodes must be between 1 and 100000");
        }
        if self
            .denied_apps
            .iter()
            .any(|name| normalize_app(name).is_empty())
        {
            bail!("--a11y-deny-app must not be empty");
        }
        Ok(())
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AccessibilityConfigFile {
    privacy: PrivacyFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PrivacyFile {
    deny_apps: Vec<String>,
}

#[derive(Debug, Clone)]
struct PrivacyPolicy {
    denied_apps: BTreeSet<String>,
}

impl PrivacyPolicy {
    fn load(options: &AccessibilityOptions) -> Result<Self> {
        let mut additional = options.denied_apps.clone();
        let working_directory =
            std::env::current_dir().context("resolving accessibility config working directory")?;
        let config_path =
            privacy_config_path(options.policy_config.as_deref(), &working_directory)?;
        if let Some(path) = config_path {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading accessibility config {}", path.display()))?;
            let config: AccessibilityConfigFile = toml::from_str(&text)
                .with_context(|| format!("parsing accessibility config {}", path.display()))?;
            additional.extend(config.privacy.deny_apps);
        }
        if additional.iter().any(|name| normalize_app(name).is_empty()) {
            bail!("accessibility privacy deny_apps entries must not be empty");
        }
        Ok(Self::new(&additional))
    }

    fn new(additional: &[String]) -> Self {
        let denied_apps = BUILTIN_DENIED_APPS
            .iter()
            .map(|name| (*name).to_string())
            .chain(additional.iter().map(|name| normalize_app(name)))
            .collect();
        Self { denied_apps }
    }

    fn blocks(&self, app: &App) -> Result<bool> {
        if self.matches(&app.name, None) {
            return Ok(true);
        }
        let pid = app
            .pid
            .context("foreground application has no process ID; capture denied by policy")?;
        let executable = executable_name(pid)
            .with_context(|| {
                format!("identifying foreground process {pid}; capture denied by policy")
            })?
            .context("foreground executable has no file name; capture denied by policy")?;
        Ok(self.matches(&app.name, Some(&executable)))
    }

    fn matches(&self, app_name: &str, executable: Option<&str>) -> bool {
        self.denied_apps.contains(&normalize_app(app_name))
            || executable.is_some_and(|name| self.denied_apps.contains(&normalize_app(name)))
    }
}

fn privacy_config_path(
    explicit: Option<&Path>,
    working_directory: &Path,
) -> Result<Option<std::path::PathBuf>> {
    if let Some(path) = explicit {
        return Ok(Some(path.to_path_buf()));
    }
    let default_path = working_directory.join("privacy.toml");
    Ok(default_path
        .try_exists()
        .with_context(|| format!("checking for {}", default_path.display()))?
        .then_some(default_path))
}

enum CaptureOutcome {
    Visible(SourceIdentity, Box<Snapshot>),
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceIdentity {
    pid: Option<u32>,
    stable_id: Option<String>,
    class_name: Option<String>,
    bounds: Option<Rect>,
}

impl SourceIdentity {
    fn publication_key(&self) -> String {
        format!(
            "pid={:?};id={:?};class={:?};bounds={:?}",
            self.pid, self.stable_id, self.class_name, self.bounds
        )
    }
}

struct Snapshot {
    app_name: String,
    pid: Option<u32>,
    window_name: String,
    root: SnapshotNode,
    node_count: usize,
    truncated: bool,
    capture_time: Duration,
}

struct SnapshotNode {
    role: Role,
    name: Option<String>,
    value: Option<String>,
    description: Option<String>,
    states: StateSet,
    children: Vec<SnapshotNode>,
}

#[derive(Clone, Copy, Default)]
struct Style {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
}

struct Span {
    text: String,
    style: Style,
}

struct DisplayLine {
    spans: Vec<Span>,
    focused: bool,
}

impl Span {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// Start a pure accessibility source. Used on non-Windows platforms and by the
/// standalone terminal preview.
pub fn start(options: AccessibilityOptions) -> Result<SourceSession> {
    options.validate()?;
    let (publisher, source) = external_source(message_frame(
        options.cols,
        options.rows,
        "shellglass accessibility",
        "Waiting for a foreground accessibility window…",
        CYAN,
    ));
    let current_identity = Arc::new(Mutex::new(None::<String>));
    spawn(
        options,
        || true,
        move |identity, frame| {
            publish_source(&publisher, &current_identity, identity, frame);
        },
    )?;
    Ok(source)
}

/// Start the capture worker. `wanted` is checked before every expensive xa11y
/// traversal; `publish` must repeat any precedence check atomically because the
/// foreground source can change while a snapshot is being built.
pub fn spawn<W, P>(options: AccessibilityOptions, wanted: W, publish: P) -> Result<()>
where
    W: Fn() -> bool + Send + Sync + 'static,
    P: Fn(String, Frame) + Send + Sync + 'static,
{
    options.validate()?;
    let policy = PrivacyPolicy::load(&options)?;
    std::thread::Builder::new()
        .name("shellglass-accessibility".into())
        .spawn(move || capture_loop(options, policy, wanted, publish))
        .context("starting accessibility capture worker")?;
    Ok(())
}

fn capture_loop<W, P>(options: AccessibilityOptions, policy: PrivacyPolicy, wanted: W, publish: P)
where
    W: Fn() -> bool,
    P: Fn(String, Frame),
{
    let mut last_error = None;
    loop {
        let tick = Instant::now();
        if wanted() {
            match capture(&options, &policy) {
                Ok(CaptureOutcome::Visible(identity, snapshot)) => {
                    publish(
                        identity.publication_key(),
                        render_snapshot(&snapshot, options.cols, options.rows, options.max_depth),
                    );
                    last_error = None;
                }
                Ok(CaptureOutcome::Blocked) => {
                    publish(
                        "accessibility-blocked".into(),
                        message_frame(
                            options.cols,
                            options.rows,
                            "shellglass accessibility — blocked",
                            "Foreground application blocked by privacy policy.",
                            YELLOW,
                        ),
                    );
                    last_error = None;
                }
                Err(error) => {
                    let message = format!("Accessibility capture failed: {error:#}");
                    if last_error.as_deref() != Some(message.as_str()) {
                        eprintln!("shellglass accessibility: {message}");
                    }
                    publish(
                        "accessibility-error".into(),
                        message_frame(
                            options.cols,
                            options.rows,
                            "shellglass accessibility — capture error",
                            &message,
                            RED,
                        ),
                    );
                    last_error = Some(message);
                }
            }
        }
        std::thread::sleep(options.interval().saturating_sub(tick.elapsed()));
    }
}

fn publish_source(
    publisher: &FramePublisher,
    current_identity: &Mutex<Option<String>>,
    identity: String,
    frame: Frame,
) {
    let mut current = current_identity
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if current.as_deref() == Some(identity.as_str()) {
        publisher.publish(frame);
    } else {
        publisher.switch_source(frame);
        *current = Some(identity);
    }
}

fn capture(options: &AccessibilityOptions, policy: &PrivacyPolicy) -> Result<CaptureOutcome> {
    let started = Instant::now();
    let app = App::foreground(Duration::ZERO).context("resolving the foreground application")?;
    if policy.blocks(&app)? {
        return Ok(CaptureOutcome::Blocked);
    }
    let window = active_window(&app)?;
    let identity = SourceIdentity {
        pid: window.pid,
        stable_id: bounded_option(&window.stable_id),
        class_name: window
            .raw
            .get("class_name")
            .and_then(|value| value.as_str())
            .map(bounded),
        bounds: window.bounds,
    };

    let mut node_count = 0;
    let mut truncated = false;
    let root = capture_node(
        &window,
        0,
        options.max_depth,
        options.max_nodes,
        &mut node_count,
        &mut truncated,
    )?
    .context("the active window disappeared before it could be captured")?;

    Ok(CaptureOutcome::Visible(
        identity,
        Box::new(Snapshot {
            app_name: bounded(&app.name),
            pid: window.pid,
            window_name: window
                .name
                .as_deref()
                .map(bounded)
                .unwrap_or_else(|| "untitled window".into()),
            root,
            node_count,
            truncated,
            capture_time: started.elapsed(),
        }),
    ))
}

fn active_window(app: &App) -> Result<Element> {
    let root = app.as_element();
    if matches!(root.role, Role::Window | Role::Dialog) {
        return Ok(root);
    }

    let active: Vec<Element> = app
        .children()
        .context("enumerating the foreground application's top-level elements")?
        .into_iter()
        .filter(|element| {
            matches!(element.role, Role::Window | Role::Dialog) && element.states.active
        })
        .collect();

    match active.as_slice() {
        [window] => Ok(window.clone()),
        [] => bail!(
            "foreground application {:?} exposes no active Window or Dialog",
            app.name
        ),
        windows => bail!(
            "foreground application {:?} exposes {} active windows; refusing to guess",
            app.name,
            windows.len()
        ),
    }
}

fn capture_node(
    element: &Element,
    depth: usize,
    max_depth: usize,
    max_nodes: usize,
    node_count: &mut usize,
    truncated: &mut bool,
) -> Result<Option<SnapshotNode>> {
    if *node_count >= max_nodes {
        *truncated = true;
        return Ok(None);
    }
    *node_count += 1;

    let mut children = Vec::new();
    if depth < max_depth {
        for child in element.children().with_context(|| {
            format!(
                "enumerating children of {} {:?}",
                element.role,
                element.name.as_deref().unwrap_or("")
            )
        })? {
            let Some(child) = capture_node(
                &child,
                depth + 1,
                max_depth,
                max_nodes,
                node_count,
                truncated,
            )?
            else {
                break;
            };
            children.push(child);
        }
    }

    Ok(Some(SnapshotNode {
        role: element.role,
        name: bounded_option(&element.name),
        value: bounded_option(&element.value),
        description: bounded_option(&element.description),
        states: element.states.clone(),
        children,
    }))
}

fn render_snapshot(snapshot: &Snapshot, cols: u16, rows: u16, max_depth: usize) -> Frame {
    let mut tree_lines = Vec::new();
    flatten_node(&snapshot.root, &[], true, true, &mut tree_lines);

    let viewport_rows = usize::from(rows.saturating_sub(3));
    // App::foreground marks the application/window root focused as a
    // foreground tag. Prefer the last focused descendant when available.
    let focused = tree_lines
        .iter()
        .rposition(|line| line.focused)
        .unwrap_or(0);
    let max_start = tree_lines.len().saturating_sub(viewport_rows);
    let start = focused.saturating_sub(viewport_rows / 2).min(max_start);
    let end = (start + viewport_rows).min(tree_lines.len());

    let header = vec![
        Span::new(
            " xa11y live ",
            Style {
                fg: BG,
                bg: CYAN,
                bold: true,
                ..Style::default()
            },
        ),
        Span::new(
            format!(
                " {} › {} ",
                clean(&snapshot.app_name),
                clean(&snapshot.window_name)
            ),
            Style {
                fg: FG,
                bg: Color::Rgb(37, 40, 54),
                bold: true,
                ..Style::default()
            },
        ),
        Span::new(
            format!(
                " pid {}",
                snapshot
                    .pid
                    .map_or_else(|| "?".into(), |pid| pid.to_string())
            ),
            Style {
                fg: MUTED,
                bg: Color::Rgb(37, 40, 54),
                ..Style::default()
            },
        ),
    ];
    let divider = vec![Span::new(
        "─".repeat(usize::from(cols)),
        Style {
            fg: CYAN,
            ..Style::default()
        },
    )];

    let mut rendered_rows = vec![cells(&header, cols), cells(&divider, cols)];
    for line in &tree_lines[start..end] {
        rendered_rows.push(cells(&line.spans, cols));
    }
    while rendered_rows.len() + 1 < usize::from(rows) {
        rendered_rows.push(blank_row(cols));
    }

    let truncation = if snapshot.truncated {
        " · node limit reached"
    } else {
        ""
    };
    let footer = vec![Span::new(
        format!(
            " rows {}–{} / {} · {} nodes · depth ≤ {} · captured in {} ms{} ",
            if tree_lines.is_empty() { 0 } else { start + 1 },
            end,
            tree_lines.len(),
            snapshot.node_count,
            max_depth,
            snapshot.capture_time.as_millis(),
            truncation,
        ),
        Style {
            fg: MUTED,
            bg: Color::Rgb(29, 32, 44),
            dim: true,
            ..Style::default()
        },
    )];
    rendered_rows.push(cells(&footer, cols));
    rendered_rows.truncate(usize::from(rows));

    frame(
        cols,
        rendered_rows,
        format!("xa11y — {}", clean(&snapshot.window_name)),
    )
}

fn flatten_node(
    node: &SnapshotNode,
    parent_last: &[bool],
    is_last: bool,
    is_root: bool,
    lines: &mut Vec<DisplayLine>,
) {
    let mut spans = Vec::new();
    for ancestor_last in parent_last {
        spans.push(Span::new(
            if *ancestor_last { "   " } else { "│  " },
            Style {
                fg: MUTED,
                dim: true,
                ..Style::default()
            },
        ));
    }
    if !is_root {
        spans.push(Span::new(
            if is_last { "└─ " } else { "├─ " },
            Style {
                fg: MUTED,
                dim: true,
                ..Style::default()
            },
        ));
    }

    spans.push(Span::new(
        node.role.to_string(),
        Style {
            fg: if node.states.focused { YELLOW } else { CYAN },
            bold: true,
            dim: !node.states.enabled || !node.states.visible,
            ..Style::default()
        },
    ));
    if let Some(name) = nonempty(&node.name) {
        spans.push(Span::new("  ", Style::default()));
        spans.push(Span::new(
            format!("“{}”", clean(name)),
            Style {
                fg: FG,
                bold: node.states.focused,
                ..Style::default()
            },
        ));
    }
    if let Some(value) = nonempty(&node.value) {
        if node.name.as_deref() != Some(value) {
            spans.push(Span::new(
                "  = ",
                Style {
                    fg: MUTED,
                    ..Style::default()
                },
            ));
            spans.push(Span::new(
                format!("“{}”", clean(value)),
                Style {
                    fg: GREEN,
                    ..Style::default()
                },
            ));
        }
    } else if let Some(description) = nonempty(&node.description) {
        spans.push(Span::new(
            "  — ",
            Style {
                fg: MUTED,
                ..Style::default()
            },
        ));
        spans.push(Span::new(
            clean(description),
            Style {
                fg: MUTED,
                italic: true,
                ..Style::default()
            },
        ));
    }

    let state = state_label(&node.states);
    if !state.is_empty() {
        spans.push(Span::new(
            format!("  [{state}]"),
            Style {
                fg: MAGENTA,
                ..Style::default()
            },
        ));
    }
    lines.push(DisplayLine {
        spans,
        focused: node.states.focused,
    });

    let mut child_prefix = parent_last.to_vec();
    if !is_root {
        child_prefix.push(is_last);
    }
    let last_index = node.children.len().saturating_sub(1);
    for (index, child) in node.children.iter().enumerate() {
        flatten_node(child, &child_prefix, index == last_index, false, lines);
    }
}

fn state_label(states: &StateSet) -> String {
    let mut labels = Vec::new();
    if states.focused {
        labels.push("focused".to_string());
    }
    if states.active {
        labels.push("active".to_string());
    }
    if !states.enabled {
        labels.push("disabled".to_string());
    }
    if !states.visible {
        labels.push("hidden".to_string());
    }
    if states.selected {
        labels.push("selected".to_string());
    }
    if let Some(checked) = states.checked {
        labels.push(
            match checked {
                Toggled::Off => "unchecked",
                Toggled::On => "checked",
                Toggled::Mixed => "mixed",
            }
            .to_string(),
        );
    }
    if let Some(expanded) = states.expanded {
        labels.push(if expanded { "expanded" } else { "collapsed" }.to_string());
    }
    if states.editable {
        labels.push("editable".to_string());
    }
    if states.busy {
        labels.push("busy".to_string());
    }
    labels.join(" ")
}

fn message_frame(cols: u16, rows: u16, title: &str, message: &str, color: Color) -> Frame {
    let mut rendered_rows = vec![cells(
        &[Span::new(
            format!(" {title} "),
            Style {
                fg: BG,
                bg: color,
                bold: true,
                ..Style::default()
            },
        )],
        cols,
    )];
    rendered_rows.push(cells(
        &[Span::new(
            "─".repeat(usize::from(cols)),
            Style {
                fg: color,
                ..Style::default()
            },
        )],
        cols,
    ));
    rendered_rows.push(cells(
        &[Span::new(
            format!(" {}", clean(message)),
            Style {
                fg: color,
                ..Style::default()
            },
        )],
        cols,
    ));
    while rendered_rows.len() < usize::from(rows) {
        rendered_rows.push(blank_row(cols));
    }
    frame(cols, rendered_rows, title.to_string())
}

fn frame(cols: u16, rows: Vec<Vec<StyledCell>>, title: String) -> Frame {
    Frame::Screen(Grid {
        source_epoch: 0,
        cols,
        rows,
        cursor: None,
        cursor_style: 0,
        default_colors: (FG, BG),
        title,
        links: Default::default(),
        images: Vec::new(),
        image_data: Default::default(),
    })
}

fn cells(spans: &[Span], cols: u16) -> Vec<StyledCell> {
    let mut output: Vec<StyledCell> = Vec::new();
    let mut occupied = 0usize;
    let width = usize::from(cols);

    'spans: for span in spans {
        for grapheme in clean(&span.text).graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if grapheme_width == 0 {
                if let Some(previous) = output.last_mut() {
                    previous.text.push_str(grapheme);
                }
                continue;
            }
            if occupied + grapheme_width > width {
                break 'spans;
            }
            if grapheme_width > 2 {
                if occupied == width {
                    break 'spans;
                }
                output.push(styled_cell("�", span.style, false));
                occupied += 1;
                continue;
            }
            output.push(styled_cell(grapheme, span.style, grapheme_width == 2));
            occupied += grapheme_width;
        }
    }
    while occupied < width {
        output.push(StyledCell::default());
        occupied += 1;
    }
    output
}

fn styled_cell(text: &str, style: Style, wide: bool) -> StyledCell {
    StyledCell {
        text: text.to_string(),
        fg: style.fg,
        bg: style.bg,
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
        wide,
        ..StyledCell::default()
    }
}

fn blank_row(cols: u16) -> Vec<StyledCell> {
    vec![StyledCell::default(); usize::from(cols)]
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.trim().is_empty())
}

fn bounded_option(value: &Option<String>) -> Option<String> {
    value.as_deref().map(bounded)
}

fn normalize_app(value: &str) -> String {
    let name = Path::new(value.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value.trim())
        .to_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}

#[cfg(windows)]
fn executable_name(pid: u32) -> Result<Option<String>> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    // SAFETY: pid is supplied by UIA; the returned handle is checked and closed
    // before this function returns.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error()).context("opening foreground process");
    }
    let result = (|| {
        let mut buffer = vec![0u16; 32_768];
        let mut length = u32::try_from(buffer.len()).context("process path buffer is too large")?;
        // SAFETY: process is valid and buffer has `length` writable UTF-16 units.
        if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) } == 0
        {
            return Err(std::io::Error::last_os_error()).context("querying foreground executable");
        }
        let path = OsString::from_wide(&buffer[..length as usize]);
        Ok(Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()))
    })();
    // SAFETY: process is a live owned handle returned by OpenProcess.
    unsafe { CloseHandle(process) };
    result
}

#[cfg(target_os = "linux")]
fn executable_name(pid: u32) -> Result<Option<String>> {
    let path = std::fs::read_link(format!("/proc/{pid}/exe"))
        .with_context(|| format!("reading /proc/{pid}/exe"))?;
    Ok(path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned()))
}

#[cfg(target_os = "macos")]
fn executable_name(pid: u32) -> Result<Option<String>> {
    unsafe extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut core::ffi::c_void, size: u32) -> i32;
    }

    let mut buffer = vec![0u8; 4_096];
    // SAFETY: the buffer is writable for the supplied size and pid came from AX.
    let length = unsafe {
        proc_pidpath(
            i32::try_from(pid).context("foreground pid exceeds i32")?,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).context("process path buffer is too large")?,
        )
    };
    if length <= 0 {
        return Err(std::io::Error::last_os_error()).context("querying foreground executable");
    }
    let path = std::str::from_utf8(&buffer[..length as usize])?;
    Ok(Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned()))
}

fn bounded(value: &str) -> String {
    value.chars().take(MAX_FIELD_CHARS).collect()
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(MAX_FIELD_CHARS)
        .collect()
}

/// Render the exact accessibility `Frame` producer into a local alternate
/// screen. This is a test/debug viewer, not a second capture implementation.
pub async fn preview(options: AccessibilityOptions) -> Result<()> {
    let mut source = start(options)?;
    let mut terminal = TerminalPreview::enter()?;
    loop {
        let frame = source.frames.borrow_and_update().clone();
        terminal.draw(&frame)?;
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("waiting for Ctrl-C")?;
                break;
            }
            changed = source.frames.changed() => {
                changed.context("accessibility preview source stopped")?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalStyle {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: u8,
    strike: bool,
    concealed: bool,
    blink: bool,
    inverse: bool,
}

impl From<&StyledCell> for TerminalStyle {
    fn from(cell: &StyledCell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            bold: cell.bold,
            dim: cell.dim,
            italic: cell.italic,
            underline: cell.underline,
            strike: cell.strike,
            concealed: cell.concealed,
            blink: cell.blink,
            inverse: cell.inverse,
        }
    }
}

struct TerminalPreview {
    stdout: Stdout,
}

impl TerminalPreview {
    fn enter() -> Result<Self> {
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide, Clear(ClearType::All))?;
        Ok(Self { stdout })
    }

    fn draw(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Screen(grid) = frame;
        for (row_index, row) in grid.rows.iter().enumerate() {
            let row_index = u16::try_from(row_index).context("preview has too many rows")?;
            queue!(self.stdout, MoveTo(0, row_index))?;
            let mut active_style = None;
            for cell in row {
                let style = TerminalStyle::from(cell);
                if active_style.as_ref() != Some(&style) {
                    queue_terminal_style(&mut self.stdout, &style)?;
                    active_style = Some(style);
                }
                queue!(
                    self.stdout,
                    Print(if cell.text.is_empty() {
                        " "
                    } else {
                        cell.text.as_str()
                    })
                )?;
            }
            queue!(self.stdout, ResetColor, SetAttribute(Attribute::Reset))?;
        }
        queue!(self.stdout, Clear(ClearType::FromCursorDown))?;
        self.stdout.flush()?;
        Ok(())
    }
}

impl Drop for TerminalPreview {
    fn drop(&mut self) {
        let _ = execute!(
            self.stdout,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show,
            LeaveAlternateScreen
        );
    }
}

fn queue_terminal_style(stdout: &mut Stdout, style: &TerminalStyle) -> Result<()> {
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    if let Some(fg) = terminal_color(style.fg) {
        queue!(stdout, SetForegroundColor(fg))?;
    }
    if let Some(bg) = terminal_color(style.bg) {
        queue!(stdout, SetBackgroundColor(bg))?;
    }
    for (enabled, attribute) in [
        (style.bold, Attribute::Bold),
        (style.dim, Attribute::Dim),
        (style.italic, Attribute::Italic),
        (style.underline != 0, Attribute::Underlined),
        (style.strike, Attribute::CrossedOut),
        (style.concealed, Attribute::Hidden),
        (style.blink, Attribute::SlowBlink),
        (style.inverse, Attribute::Reverse),
    ] {
        if enabled {
            queue!(stdout, SetAttribute(attribute))?;
        }
    }
    Ok(())
}

fn terminal_color(color: Color) -> Option<TerminalColor> {
    match color {
        Color::Default => None,
        Color::Idx(index) => Some(TerminalColor::AnsiValue(index)),
        Color::Rgb(r, g, b) => Some(TerminalColor::Rgb { r, g, b }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_are_rendered_as_spaces() {
        assert_eq!(clean("hello\nworld\t!"), "hello world !");
    }

    #[test]
    fn cell_rows_preserve_terminal_width_with_wide_text() {
        let row = cells(&[Span::new("a界b", Style::default())], 5);
        assert_eq!(
            row.iter()
                .map(|cell| if cell.wide { 2 } else { 1 })
                .sum::<usize>(),
            5
        );
        assert!(row.iter().any(|cell| cell.text == "界" && cell.wide));
    }

    #[test]
    fn status_labels_include_accessibility_state() {
        let mut states = StateSet::default();
        states.focused = true;
        states.checked = Some(Toggled::Mixed);
        states.expanded = Some(false);
        assert_eq!(state_label(&states), "focused mixed collapsed");
    }

    #[test]
    fn invalid_capture_limits_are_rejected() {
        let options = AccessibilityOptions {
            max_nodes: 0,
            ..AccessibilityOptions::default()
        };
        assert!(options.validate().is_err());
    }

    #[test]
    fn privacy_policy_denies_builtin_and_custom_executables_exactly() {
        let policy = PrivacyPolicy::new(&["Signal.exe".into()]);
        assert!(policy.matches("unrelated title", Some("Discord.exe")));
        assert!(policy.matches("SIGNAL", None));
        assert!(!policy.matches("Discordant", Some("Discordant.exe")));
        assert!(!policy.matches("Slack", Some("slack.exe")));
    }

    #[test]
    fn privacy_policy_loads_toml_config() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock must be after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "shellglass-a11y-policy-{}-{nonce}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "[privacy]\ndeny_apps = [\"Slack\", \"Signal.exe\"]\n",
        )
        .expect("write privacy fixture");
        let options = AccessibilityOptions {
            policy_config: Some(path.clone()),
            ..AccessibilityOptions::default()
        };
        let policy = PrivacyPolicy::load(&options).expect("load privacy fixture");
        std::fs::remove_file(path).expect("remove privacy fixture");
        assert!(policy.matches("Slack", None));
        assert!(policy.matches("other", Some("signal.exe")));
    }

    #[test]
    fn working_directory_privacy_toml_is_discovered_when_present() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock must be after Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "shellglass-a11y-config-dir-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).expect("create config fixture directory");
        assert_eq!(
            privacy_config_path(None, &directory).expect("check absent default config"),
            None
        );
        let expected = directory.join("privacy.toml");
        std::fs::write(&expected, "[privacy]\n").expect("write default config fixture");
        assert_eq!(
            privacy_config_path(None, &directory).expect("discover default config"),
            Some(expected.clone())
        );
        std::fs::remove_file(expected).expect("remove config fixture");
        std::fs::remove_dir(directory).expect("remove config fixture directory");
    }

    #[test]
    fn current_process_executable_can_be_identified() {
        let executable = executable_name(std::process::id())
            .expect("the test process executable should be queryable")
            .expect("the test process should have a file name");
        assert!(!executable.is_empty());
    }
}
