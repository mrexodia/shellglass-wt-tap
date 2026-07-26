//! Accessibility-tree reconstruction for non-terminal foreground windows.
//!
//! Native terminal frames always win. This module is deliberately only a
//! semantic reconstruction: it renders roles, names, values, hierarchy, and
//! state reported by xa11y, never pixels or simulated input.

use std::collections::BTreeSet;
use std::io::{Stdout, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU16, Ordering};
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
use serde::{Deserialize, Serialize};
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
const MAX_FIELD_CHARS: usize = 2_000;
const MAX_MULTILINE_FIELD_CHARS: usize = 8_192;
const DEFAULT_MAX_DEPTH: usize = 40;
const DEFAULT_MAX_NODES: usize = 2_000;
const BUILTIN_DENIED_APPS: &[&str] = &["discord", "discordcanary", "discordptb"];

/// Shared CLI/configuration surface for accessibility reconstruction.
#[derive(Debug, Clone, Args)]
pub struct AccessibilityOptions {
    /// Accessibility snapshot interval in milliseconds.
    #[arg(long = "a11y-interval-ms", default_value_t = 300)]
    pub interval_ms: u64,
    /// Columns in streamed frames; standalone preview follows its terminal.
    #[arg(long = "a11y-cols", default_value_t = 200)]
    pub cols: u16,
    /// Rows in streamed frames; standalone preview follows its terminal.
    #[arg(long = "a11y-rows", default_value_t = 60)]
    pub rows: u16,
    /// Maximum accessibility-tree depth (at most 64).
    #[arg(long = "a11y-depth", default_value_t = DEFAULT_MAX_DEPTH)]
    pub max_depth: usize,
    /// Maximum accessibility nodes captured per snapshot (at most 100000).
    #[arg(long = "a11y-max-nodes", default_value_t = DEFAULT_MAX_NODES)]
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
            cols: 200,
            rows: 60,
            max_depth: DEFAULT_MAX_DEPTH,
            max_nodes: DEFAULT_MAX_NODES,
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
    /// Foreground/window geometry changed during traversal. Keep displaying
    /// the last coherent frame rather than publishing mixed coordinate spaces.
    Unstable,
}

#[derive(Default)]
struct GeometryStabilizer {
    identity: Option<String>,
    bounds: Option<Rect>,
}

impl GeometryStabilizer {
    fn should_publish(&mut self, identity: &SourceIdentity) -> bool {
        let key = identity.publication_key();
        if self.identity.as_deref() != Some(&key) {
            self.identity = Some(key);
            self.bounds = identity.bounds;
            return true;
        }
        if self.bounds != identity.bounds {
            self.bounds = identity.bounds;
            return false;
        }
        true
    }

    fn reset(&mut self) {
        self.identity = None;
        self.bounds = None;
    }
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
        // Window position is deliberately excluded: moving a window does not
        // create a new source, and changing the epoch on every drag tick makes
        // the viewer flash full snapshots.
        format!(
            "pid={:?};id={:?};class={:?}",
            self.pid, self.stable_id, self.class_name
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    app_name: String,
    pid: Option<u32>,
    window_name: String,
    root: SnapshotNode,
    node_count: usize,
    truncated: bool,
    capture_time: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotNode {
    role: Role,
    bounds: Option<Rect>,
    name: Option<String>,
    value: Option<String>,
    description: Option<String>,
    states: StateSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    numeric_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_value: Option<f64>,
    children: Vec<SnapshotNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutFixture {
    schema_version: u32,
    cols: u16,
    rows: u16,
    snapshot: Snapshot,
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
        || Some(0),
        move |_ticket, identity, frame| {
            publish_source(&publisher, &current_identity, identity, frame);
        },
        |_ticket| {},
    )?;
    Ok(source)
}

/// Start the capture worker. `wanted` is checked before every expensive xa11y
/// traversal; `publish` must repeat any precedence check atomically because the
/// foreground source can change while a snapshot is being built.
pub fn spawn<W, P, B>(
    options: AccessibilityOptions,
    wanted: W,
    publish: P,
    blocked: B,
) -> Result<()>
where
    W: Fn() -> Option<u64> + Send + Sync + 'static,
    P: Fn(u64, String, Frame) + Send + Sync + 'static,
    B: Fn(u64) + Send + Sync + 'static,
{
    let dimensions = (options.cols, options.rows);
    spawn_with_dimensions(options, wanted, move || dimensions, publish, blocked)
}

fn spawn_with_dimensions<W, D, P, B>(
    options: AccessibilityOptions,
    wanted: W,
    dimensions: D,
    publish: P,
    blocked: B,
) -> Result<()>
where
    W: Fn() -> Option<u64> + Send + Sync + 'static,
    D: Fn() -> (u16, u16) + Send + Sync + 'static,
    P: Fn(u64, String, Frame) + Send + Sync + 'static,
    B: Fn(u64) + Send + Sync + 'static,
{
    options.validate()?;
    let policy = PrivacyPolicy::load(&options)?;
    std::thread::Builder::new()
        .name("shellglass-accessibility".into())
        .spawn(move || capture_loop(options, policy, wanted, dimensions, publish, blocked))
        .context("starting accessibility capture worker")?;
    Ok(())
}

fn capture_loop<W, D, P, B>(
    options: AccessibilityOptions,
    policy: PrivacyPolicy,
    wanted: W,
    dimensions: D,
    publish: P,
    blocked: B,
) where
    W: Fn() -> Option<u64>,
    D: Fn() -> (u16, u16),
    P: Fn(u64, String, Frame),
    B: Fn(u64),
{
    let mut last_error = None;
    let mut geometry = GeometryStabilizer::default();
    loop {
        let tick = Instant::now();
        if let Some(ticket) = wanted() {
            let (cols, rows) = dimensions();
            match capture(&options, &policy) {
                Ok(CaptureOutcome::Visible(identity, snapshot)) => {
                    if geometry.should_publish(&identity) {
                        publish(
                            ticket,
                            identity.publication_key(),
                            render_snapshot(&snapshot, cols, rows, options.max_depth),
                        );
                    }
                    last_error = None;
                }
                Ok(CaptureOutcome::Blocked) => {
                    // Privacy policy deliberately leaves the previously
                    // published frame untouched. Publishing even a generic
                    // replacement reveals focus activity in a denied app.
                    blocked(ticket);
                    geometry.reset();
                    last_error = None;
                }
                Ok(CaptureOutcome::Unstable) => {
                    // Moving/resizing can make a provider expose the window in
                    // one coordinate space and descendants in another during
                    // the same traversal. Preserve the last coherent frame.
                    last_error = None;
                }
                Err(error) => {
                    geometry.reset();
                    let message = format!("Accessibility capture failed: {error:#}");
                    if last_error.as_deref() != Some(message.as_str()) {
                        eprintln!("shellglass accessibility: {message}");
                    }
                    // Foreground transitions can briefly expose the taskbar,
                    // desktop, popup quickbars, or a window whose provider is
                    // between roots. Keep the last coherent publication rather
                    // than replacing it with an error screen. This also fails
                    // closed when process identity cannot be established: no
                    // details from the new foreground target are published.
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
    let identity = source_identity(&window);

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

    // Providers expose snapshot values one element at a time. If the window
    // moves during traversal, root and descendant bounds can therefore refer
    // to different moments. Re-resolve the foreground window and reject that
    // mixed snapshot instead of letting controls jump around while dragging.
    let latest_app = App::foreground(Duration::ZERO)
        .context("rechecking the foreground application after capture")?;
    if latest_app.pid != app.pid {
        return Ok(CaptureOutcome::Unstable);
    }
    let latest_window = active_window(&latest_app)?;
    if source_identity(&latest_window) != identity {
        return Ok(CaptureOutcome::Unstable);
    }

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

fn source_identity(window: &Element) -> SourceIdentity {
    SourceIdentity {
        pid: window.pid,
        stable_id: bounded_option(&window.stable_id),
        class_name: window
            .raw
            .get("class_name")
            .and_then(|value| value.as_str())
            .map(bounded),
        bounds: window.bounds,
    }
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
            // Hidden/offscreen subtrees cannot contribute to the spatial
            // frame. Chromium exposes the entire offscreen document here;
            // pruning it keeps deep web trees bounded without losing pixels.
            if !child.states.visible {
                continue;
            }
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
        bounds: element.bounds,
        name: bounded_option(&element.name),
        value: bounded_value_option(&element.value),
        description: bounded_option(&element.description),
        states: element.states.clone(),
        numeric_value: element.numeric_value,
        min_value: element.min_value,
        max_value: element.max_value,
        children,
    }))
}

fn render_snapshot(snapshot: &Snapshot, cols: u16, rows: u16, max_depth: usize) -> Frame {
    if let Some(frame) = render_spatial_snapshot(snapshot, cols, rows, max_depth) {
        frame
    } else {
        render_tree_snapshot(snapshot, cols, rows, max_depth)
    }
}

#[derive(Clone, Copy)]
struct CellRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

#[derive(Clone, Default)]
struct CanvasCell {
    cell: StyledCell,
    continuation: bool,
}

struct Canvas {
    cols: usize,
    rows: usize,
    cells: Vec<Vec<CanvasCell>>,
}

impl Canvas {
    fn new(cols: u16, rows: u16) -> Self {
        let cols = usize::from(cols);
        let rows = usize::from(rows);
        Self {
            cols,
            rows,
            cells: vec![vec![CanvasCell::default(); cols]; rows],
        }
    }

    fn text(&mut self, x: usize, y: usize, width: usize, text: &str, style: Style) {
        if y >= self.rows || x >= self.cols || width == 0 {
            return;
        }
        let end = x.saturating_add(width).min(self.cols);
        let mut column = x;
        for grapheme in clean(text).graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if grapheme_width == 0 {
                if column > x {
                    self.cells[y][column - 1].cell.text.push_str(grapheme);
                }
                continue;
            }
            let grapheme_width = grapheme_width.min(2);
            if column + grapheme_width > end {
                break;
            }
            self.clear_footprint(column, y);
            self.cells[y][column].cell = styled_cell(
                if UnicodeWidthStr::width(grapheme) > 2 {
                    "�"
                } else {
                    grapheme
                },
                style,
                grapheme_width == 2,
            );
            if grapheme_width == 2 {
                self.clear_footprint(column + 1, y);
                self.cells[y][column + 1].continuation = true;
            }
            column += grapheme_width;
        }
    }

    fn clear_footprint(&mut self, x: usize, y: usize) {
        if x >= self.cols || y >= self.rows {
            return;
        }
        if self.cells[y][x].continuation && x > 0 {
            self.cells[y][x - 1] = CanvasCell::default();
        }
        if self.cells[y][x].cell.wide && x + 1 < self.cols {
            self.cells[y][x + 1] = CanvasCell::default();
        }
        self.cells[y][x] = CanvasCell::default();
    }

    fn flow_text(&mut self, preferred_x: usize, y: usize, text: &str, style: Style) {
        if preferred_x >= self.cols || y >= self.rows {
            return;
        }
        let text = clean(text);
        let desired = UnicodeWidthStr::width(text.as_str()).saturating_add(1);
        let mut start = preferred_x;
        while start < self.cols {
            let end = start.saturating_add(desired).min(self.cols);
            if let Some(occupied) = (start..end).find(|column| {
                let slot = &self.cells[y][*column];
                slot.continuation || !slot.cell.text.is_empty()
            }) {
                start = occupied + 1;
                continue;
            }
            break;
        }
        if start >= self.cols {
            return;
        }
        let available = self.cols - start;
        let max_label_width = available.saturating_sub(1);
        let fitted = elide(&text, max_label_width);
        let label_width = UnicodeWidthStr::width(fitted.as_str());
        self.text(start, y, label_width, &fitted, style);
        if start + label_width < self.cols {
            self.text(start + label_width, y, 1, " ", style);
        }
    }

    fn decoration(&mut self, x: usize, y: usize, ch: &str, style: Style) {
        if x >= self.cols || y >= self.rows {
            return;
        }
        let slot = &self.cells[y][x];
        if slot.continuation || !slot.cell.text.is_empty() {
            return;
        }
        self.text(x, y, 1, ch, style);
    }

    fn horizontal(&mut self, x: usize, y: usize, width: usize, ch: &str, style: Style) {
        for column in x..x.saturating_add(width).min(self.cols) {
            self.decoration(column, y, ch, style);
        }
    }

    fn vertical(&mut self, x: usize, y: usize, height: usize, ch: &str, style: Style) {
        for row in y..y.saturating_add(height).min(self.rows) {
            self.decoration(x, row, ch, style);
        }
    }

    fn border(&mut self, rect: CellRect, style: Style) {
        if rect.width < 2 || rect.height < 2 {
            return;
        }
        self.horizontal(rect.x + 1, rect.y, rect.width - 2, "─", style);
        self.horizontal(
            rect.x + 1,
            rect.y + rect.height - 1,
            rect.width - 2,
            "─",
            style,
        );
        self.vertical(rect.x, rect.y + 1, rect.height - 2, "│", style);
        self.vertical(
            rect.x + rect.width - 1,
            rect.y + 1,
            rect.height - 2,
            "│",
            style,
        );
        self.decoration(rect.x, rect.y, "┌", style);
        self.decoration(rect.x + rect.width - 1, rect.y, "┐", style);
        self.decoration(rect.x, rect.y + rect.height - 1, "└", style);
        self.decoration(
            rect.x + rect.width - 1,
            rect.y + rect.height - 1,
            "┘",
            style,
        );
    }

    fn into_rows(self) -> Vec<Vec<StyledCell>> {
        self.cells
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .filter_map(|slot| (!slot.continuation).then_some(slot.cell))
                    .collect()
            })
            .collect()
    }
}

fn render_spatial_snapshot(
    snapshot: &Snapshot,
    cols: u16,
    rows: u16,
    max_depth: usize,
) -> Option<Frame> {
    let root_bounds = snapshot.root.bounds?;
    if root_bounds.width == 0 || root_bounds.height == 0 || rows <= 3 || cols == 0 {
        return None;
    }

    let content_rows = rows - 3;
    let mut canvas = Canvas::new(cols, content_rows);
    let mut positioned = 0usize;
    render_spatial_node(
        &snapshot.root,
        root_bounds,
        &mut canvas,
        true,
        snapshot.root.role == Role::Window,
        None,
        &mut positioned,
    );
    if positioned == 0 {
        return None;
    }

    let mut rendered_rows = vec![
        cells(&snapshot_header(snapshot), cols),
        cells(&snapshot_divider(cols), cols),
    ];
    rendered_rows.extend(canvas.into_rows());
    let truncation = if snapshot.truncated {
        " · node limit reached"
    } else {
        ""
    };
    rendered_rows.push(cells(
        &[Span::new(
            format!(
                " spatial · {positioned} positioned / {} nodes · depth ≤ {} · captured in {} ms{} ",
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
        )],
        cols,
    ));
    Some(frame(
        cols,
        rendered_rows,
        format!("xa11y — {}", clean(&snapshot.window_name)),
    ))
}

fn render_spatial_node(
    node: &SnapshotNode,
    root: Rect,
    canvas: &mut Canvas,
    is_root: bool,
    fit_window: bool,
    editor_host: Option<Rect>,
    positioned: &mut usize,
) {
    if !node.states.visible
        || is_window_chrome(node)
        || matches!(node.role, Role::ScrollBar | Role::ScrollThumb)
    {
        return;
    }
    let child_editor_host = if node.role == Role::Group
        && let Some(bounds) = node.bounds
        && bounds.height >= 64
    {
        Some(bounds)
    } else {
        editor_host
    };
    let mut rendered_here = false;
    if !is_root && let Some(bounds) = node.bounds {
        let multiline_field = node.role == Role::TextField
            && nonempty(&node.value)
                .or_else(|| nonempty(&node.name))
                .or_else(|| nonempty(&node.description))
                .is_some_and(|text| text.contains('\n'));
        let mut expanded_multiline = false;
        let effective_bounds = if multiline_field && bounds.height < 64 {
            editor_host
                .filter(|host| host.height >= bounds.height.saturating_mul(3))
                .inspect(|_| expanded_multiline = true)
                .unwrap_or(bounds)
        } else {
            bounds
        };
        if let Some(rect) =
            project_bounds(effective_bounds, root, canvas.cols, canvas.rows, fit_window)
            && draw_spatial_control(canvas, rect, node, expanded_multiline)
        {
            *positioned += 1;
            rendered_here = true;
        }
    }
    if rendered_here && owns_descendant_text(node) {
        // Named semantic controls and HTML flow containers already represent
        // their descendant text. Drawing both parent and child is what turns
        // Chromium headings, links, and paragraphs into overlapping fragments.
        return;
    }
    if node.role == Role::List {
        if node
            .children
            .iter()
            .any(|child| child.role == Role::TreeItem)
            || infer_grid_columns(node).len() <= 1
        {
            render_collection_children(node, root, canvas, fit_window, positioned);
        } else {
            render_table_children(node, root, canvas, fit_window, positioned);
        }
    } else if node.role == Role::Table {
        render_table_children(node, root, canvas, fit_window, positioned);
    } else if node.role == Role::Group
        && render_mixed_inline_group(node, root, canvas, fit_window, positioned)
    {
    } else {
        for child in &node.children {
            render_spatial_node(
                child,
                root,
                canvas,
                false,
                fit_window,
                child_editor_host,
                positioned,
            );
        }
    }
}

fn render_mixed_inline_group(
    group: &SnapshotNode,
    root: Rect,
    canvas: &mut Canvas,
    fit_window: bool,
    positioned: &mut usize,
) -> bool {
    let is_inline = |node: &SnapshotNode| matches!(node.role, Role::StaticText | Role::Link);
    if !group.children.iter().any(|child| child.role == Role::List)
        || group
            .children
            .iter()
            .filter(|child| is_inline(child))
            .count()
            < 2
    {
        return false;
    }

    let mut index = 0;
    while index < group.children.len() {
        if !is_inline(&group.children[index]) {
            render_spatial_node(
                &group.children[index],
                root,
                canvas,
                false,
                fit_window,
                None,
                positioned,
            );
            index += 1;
            continue;
        }

        let start = index;
        let mut bottom = group.children[index]
            .bounds
            .map(|bounds| i64::from(bounds.y) + i64::from(bounds.height));
        index += 1;
        while index < group.children.len() && is_inline(&group.children[index]) {
            let Some(bounds) = group.children[index].bounds else {
                break;
            };
            if bottom.is_some_and(|previous| i64::from(bounds.y) > previous + 8) {
                break;
            }
            bottom = Some(
                bottom
                    .unwrap_or(i64::from(bounds.y))
                    .max(i64::from(bounds.y) + i64::from(bounds.height)),
            );
            index += 1;
        }

        let run = &group.children[start..index];
        if run.len() == 1 {
            render_spatial_node(&run[0], root, canvas, false, fit_window, None, positioned);
            continue;
        }
        let Some(mut union) = run[0].bounds else {
            continue;
        };
        let mut text = String::new();
        let mut complete = true;
        for child in run {
            complete &= collect_inline_text(child, &mut text);
            if let Some(bounds) = child.bounds {
                let right = (i64::from(union.x) + i64::from(union.width))
                    .max(i64::from(bounds.x) + i64::from(bounds.width));
                let bottom = (i64::from(union.y) + i64::from(union.height))
                    .max(i64::from(bounds.y) + i64::from(bounds.height));
                union.x = union.x.min(bounds.x);
                union.y = union.y.min(bounds.y);
                union.width = u32::try_from(right - i64::from(union.x)).unwrap_or(u32::MAX);
                union.height = u32::try_from(bottom - i64::from(union.y)).unwrap_or(u32::MAX);
            }
        }
        if complete
            && !text.trim().is_empty()
            && let Some(rect) = project_bounds(union, root, canvas.cols, canvas.rows, fit_window)
        {
            draw_flow_text(canvas, rect, &text, Style::default());
            *positioned += 1;
        }
    }
    true
}

fn render_collection_children(
    collection: &SnapshotNode,
    root: Rect,
    canvas: &mut Canvas,
    fit_window: bool,
    positioned: &mut usize,
) {
    if collection.children.len() == 1
        && collection.children[0].role == Role::Group
        && !collection.children[0].children.is_empty()
        && collection.children[0]
            .children
            .iter()
            .all(|child| child.role == Role::TreeItem)
    {
        render_collection_children(
            &collection.children[0],
            root,
            canvas,
            fit_window,
            positioned,
        );
        return;
    }
    let collection_rect = collection
        .bounds
        .and_then(|bounds| project_bounds(bounds, root, canvas.cols, canvas.rows, fit_window));
    let collection_left = collection_rect.map_or(0, |rect| rect.x);
    let bottom = collection_rect.map_or(canvas.rows, |rect| rect.y + rect.height);
    let collection_right = collection_rect.map_or(canvas.cols, |rect| rect.x + rect.width);
    let collection_source_right = collection
        .bounds
        .map(|bounds| i64::from(bounds.x) + i64::from(bounds.width));
    let header_bottom = collection
        .children
        .iter()
        .filter(|child| {
            child.role == Role::Group
                && child
                    .children
                    .iter()
                    .any(|cell| cell.role == Role::TableCell)
        })
        .filter_map(|child| child.bounds)
        .map(|bounds| i64::from(bounds.y) + i64::from(bounds.height))
        .max();
    let mut source_row = None;
    let mut source_bottom = None;
    let mut output_row = None;

    for child in &collection.children {
        if !child.states.visible || is_collection_chrome(child, collection) {
            continue;
        }
        let Some(bounds) = child.bounds else {
            render_spatial_node(child, root, canvas, false, fit_window, None, positioned);
            continue;
        };
        if collection_source_right.is_some_and(|right| i64::from(bounds.x) >= right)
            || (child.role == Role::ListItem
                && header_bottom
                    .is_some_and(|bottom| i64::from(bounds.y) + i64::from(bounds.height) <= bottom))
        {
            continue;
        }
        let Some(mut rect) = project_bounds(bounds, root, canvas.cols, canvas.rows, fit_window)
        else {
            continue;
        };
        if rect.x >= collection_right {
            continue;
        }
        rect.width = rect.width.min(collection_right - rect.x);
        let row = if source_row == Some(bounds.y) {
            output_row.unwrap_or(rect.y)
        } else {
            let contiguous = source_bottom.is_some_and(|bottom| bounds.y <= bottom + 2);
            let row = output_row.map_or(rect.y, |previous| {
                if contiguous {
                    previous + 1
                } else {
                    rect.y.max(previous + 1)
                }
            });
            source_row = Some(bounds.y);
            source_bottom = Some(
                i32::try_from(i64::from(bounds.y) + i64::from(bounds.height)).unwrap_or(i32::MAX),
            );
            output_row = Some(row);
            row
        };
        if row >= bottom || row >= canvas.rows {
            continue;
        }
        rect.y = row;
        if let Some(label) = bullet_list_label(child) {
            let available_rows = bottom.min(canvas.rows).saturating_sub(row);
            let wrapped = wrap_flow_text(&label, rect.width, available_rows);
            rect.height = rect.height.max(wrapped.len()).max(1);
        } else {
            rect.height = 1;
            if child.role == Role::TreeItem {
                // Some native tree controls report only their painted text
                // width. The rest of the containing tree row is still usable.
                rect.width = rect.width.max(collection_right.saturating_sub(rect.x));
                let required = nonempty(&child.name)
                    .map(UnicodeWidthStr::width)
                    .unwrap_or(0)
                    .saturating_add(3);
                let borrow = required
                    .saturating_sub(rect.width)
                    .min(rect.x.saturating_sub(collection_left));
                rect.x -= borrow;
                rect.width += borrow;
            } else if child.role == Role::ListItem && compact_list_row_label(child).is_none() {
                // A sole list item on its source row may use the empty
                // remainder of the containing pane. Multi-column rows retain
                // narrow bounds so collision-aware flow places flags safely.
                let same_row_count = collection
                    .children
                    .iter()
                    .filter_map(|sibling| sibling.bounds)
                    .filter(|sibling| sibling.y == bounds.y)
                    .count();
                if same_row_count == 1 {
                    rect.width = rect.width.max(collection_right.saturating_sub(rect.x));
                }
            }
        }
        // `row` is the item's first output row. Advance the packed-row cursor
        // past every wrapped line so the next list item cannot overwrite this
        // item's continuation lines.
        output_row = Some(row.saturating_add(rect.height.saturating_sub(1)));
        let owns_text = owns_descendant_text(child);
        if is_tabular_list_item(child) {
            let mut rendered = false;
            for grandchild in &child.children {
                let Some(cell_bounds) = grandchild.bounds else {
                    continue;
                };
                let Some(mut cell_rect) =
                    project_bounds(cell_bounds, root, canvas.cols, canvas.rows, fit_window)
                else {
                    continue;
                };
                if cell_rect.x >= collection_right {
                    continue;
                }
                let next_column = child
                    .children
                    .iter()
                    .filter_map(|sibling| sibling.bounds)
                    .filter(|sibling| sibling.x > cell_bounds.x)
                    .filter_map(|sibling| {
                        project_bounds(sibling, root, canvas.cols, canvas.rows, fit_window)
                    })
                    .map(|sibling| sibling.x)
                    .min()
                    .unwrap_or(collection_right);
                cell_rect.y = row;
                cell_rect.width = cell_rect
                    .width
                    .max(next_column.saturating_sub(cell_rect.x))
                    .min(collection_right - cell_rect.x);
                cell_rect.height = 1;
                let mut cell = grandchild.clone();
                cell.states.selected |= child.states.selected;
                rendered |= draw_spatial_control(canvas, cell_rect, &cell, false);
            }
            if rendered {
                *positioned += 1;
            }
            continue;
        }
        let rendered = if child.role == Role::ListItem && !child.children.is_empty() && !owns_text {
            // Composite cards expose a concatenated ListItem name as well as
            // richer positioned descendants. Render only the descendants;
            // simple bullet rows take the parent-name path above.
            false
        } else {
            draw_spatial_control(canvas, rect, child, false)
        };
        if rendered {
            *positioned += 1;
        }
        if rendered && owns_text {
            continue;
        }
        for grandchild in &child.children {
            render_spatial_node(
                grandchild, root, canvas, false, fit_window, None, positioned,
            );
        }
    }
}

fn infer_grid_columns(collection: &SnapshotNode) -> Vec<i32> {
    let first_y = collection
        .children
        .iter()
        .filter(|child| child.states.visible && !is_collection_chrome(child, collection))
        .filter_map(|child| child.bounds.map(|bounds| bounds.y))
        .min();
    let mut columns = collection
        .children
        .iter()
        .filter(|child| child.states.visible && !is_collection_chrome(child, collection))
        .filter_map(|child| child.bounds)
        .filter(|bounds| Some(bounds.y) == first_y)
        .map(|bounds| bounds.x)
        .collect::<Vec<_>>();
    columns.sort_unstable();
    columns.dedup();
    columns
}

fn render_table_children(
    table: &SnapshotNode,
    root: Rect,
    canvas: &mut Canvas,
    fit_window: bool,
    positioned: &mut usize,
) {
    let columns = infer_grid_columns(table);
    if columns.len() < 2 && table.role != Role::Table {
        render_collection_children(table, root, canvas, fit_window, positioned);
        return;
    }
    let Some(table_bounds) = table.bounds else {
        render_collection_children(table, root, canvas, fit_window, positioned);
        return;
    };
    let Some(table_rect) = project_bounds(table_bounds, root, canvas.cols, canvas.rows, fit_window)
    else {
        return;
    };
    if columns.is_empty() {
        render_collection_children(table, root, canvas, fit_window, positioned);
        return;
    }

    let direct_cells = table
        .children
        .iter()
        .filter(|child| {
            child.states.visible && child.bounds.is_some() && !is_collection_chrome(child, table)
        })
        .collect::<Vec<_>>();
    let mut cells_by_column = vec![Vec::new(); columns.len()];
    for cell in &direct_cells {
        let x = cell.bounds.map_or(columns[0], |bounds| bounds.x);
        let column = columns
            .partition_point(|start| *start <= x)
            .saturating_sub(1);
        cells_by_column[column].push(*cell);
    }

    let mut preferred = Vec::with_capacity(columns.len());
    let mut minimum = Vec::with_capacity(columns.len());
    let source_right = i64::from(table_bounds.x) + i64::from(table_bounds.width);
    for (index, cells) in cells_by_column.iter().enumerate() {
        let right = columns
            .get(index + 1)
            .map_or(source_right, |next| i64::from(*next));
        let source_width = right.saturating_sub(i64::from(columns[index])).max(1) as u64;
        let projected = (source_width * table_rect.width as u64
            / u64::from(table_bounds.width.max(1))) as usize;
        preferred.push(projected.max(1));

        let mut text_widths = cells
            .iter()
            .map(|cell| {
                nonempty(&cell.value)
                    .or_else(|| nonempty(&cell.name))
                    .map_or(0, UnicodeWidthStr::width)
            })
            .collect::<Vec<_>>();
        text_widths.sort_unstable();
        let percentile = text_widths
            .get(text_widths.len().saturating_sub(1) * 3 / 4)
            .copied()
            .unwrap_or(0);
        minimum.push(percentile.saturating_add(1).max(1));
    }
    let widths = allocate_table_widths(&preferred, &minimum, table_rect.width);
    let mut starts = Vec::with_capacity(widths.len());
    let mut next = table_rect.x;
    for width in &widths {
        starts.push(next);
        next += *width;
    }

    let mut source_row = None;
    let mut source_bottom = None;
    let mut output_row = None;
    let table_bottom = table_rect.y + table_rect.height;
    for cell in direct_cells {
        let Some(bounds) = cell.bounds else {
            continue;
        };
        let column = columns
            .partition_point(|start| *start <= bounds.x)
            .saturating_sub(1);
        let projected = project_bounds(bounds, root, canvas.cols, canvas.rows, fit_window);
        let desired_row = projected.map_or(table_rect.y, |rect| rect.y);
        let row = if source_row == Some(bounds.y) {
            output_row.unwrap_or(desired_row)
        } else {
            let contiguous = source_bottom.is_some_and(|bottom| bounds.y <= bottom + 2);
            let row = output_row.map_or(desired_row, |previous| {
                if contiguous {
                    previous + 1
                } else {
                    desired_row.max(previous + 1)
                }
            });
            source_row = Some(bounds.y);
            source_bottom = Some(
                i32::try_from(i64::from(bounds.y) + i64::from(bounds.height)).unwrap_or(i32::MAX),
            );
            output_row = Some(row);
            row
        };
        if row >= table_bottom || row >= canvas.rows {
            continue;
        }
        // Providers can expose a spanning/outlier cell slightly left of the
        // first inferred column. Clamp that offset instead of casting a
        // negative coordinate delta to a huge unsigned value.
        let source_indent = bounds.x.saturating_sub(columns[column]).max(0) as u64;
        let indent = (source_indent * table_rect.width as u64
            / u64::from(table_bounds.width.max(1))) as usize;
        let indent = indent.min(widths[column].saturating_sub(1));
        let rect = CellRect {
            x: starts[column] + indent,
            y: row,
            width: widths[column] - indent,
            height: 1,
        };
        if draw_spatial_control(canvas, rect, cell, false) {
            *positioned += 1;
        }
        for grandchild in &cell.children {
            render_spatial_node(
                grandchild, root, canvas, false, fit_window, None, positioned,
            );
        }
    }
}

fn allocate_table_widths(preferred: &[usize], minimum: &[usize], total: usize) -> Vec<usize> {
    let mut widths = minimum.to_vec();
    if widths.is_empty() || total == 0 {
        return widths;
    }
    while widths.iter().sum::<usize>() > total {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > 1)
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        widths[index] -= 1;
    }

    let mut remaining = total.saturating_sub(widths.iter().sum::<usize>());
    while remaining != 0 {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(index, width)| **width < preferred[*index])
            .max_by_key(|(index, width)| preferred[*index] - **width)
        else {
            break;
        };
        widths[index] += 1;
        remaining -= 1;
    }
    if remaining != 0 {
        let last = widths.len() - 1;
        widths[last] += remaining;
    }
    widths
}

fn is_collection_chrome(child: &SnapshotNode, collection: &SnapshotNode) -> bool {
    if matches!(child.role, Role::ScrollBar | Role::ScrollThumb) {
        return true;
    }
    let Some(name) = nonempty(&child.name) else {
        return false;
    };
    let (Some(child_bounds), Some(collection_bounds)) = (child.bounds, collection.bounds) else {
        return false;
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "vertical" | "horizontal"
    ) && child_bounds.width.saturating_mul(20) < collection_bounds.width
}

fn is_window_chrome(node: &SnapshotNode) -> bool {
    node.role == Role::Group
        && node.children.iter().any(|child| {
            child.role == Role::Button
                && nonempty(&child.name).is_some_and(|name| {
                    matches!(
                        name.to_ascii_lowercase().as_str(),
                        "minimise" | "minimize" | "maximise" | "maximize" | "close"
                    )
                })
        })
}

fn project_bounds(
    bounds: Rect,
    root: Rect,
    cols: usize,
    rows: usize,
    fit_window: bool,
) -> Option<CellRect> {
    const CELL_HEIGHT_PX: f64 = 17.0;
    const CELL_ASPECT: f64 = 0.5;

    if bounds.width == 0
        || bounds.height == 0
        || root.width == 0
        || root.height == 0
        || cols == 0
        || rows == 0
    {
        return None;
    }
    let root_left = i64::from(root.x);
    let root_top = i64::from(root.y);
    let root_right = root_left + i64::from(root.width);
    let root_bottom = root_top + i64::from(root.height);
    let left = i64::from(bounds.x).max(root_left);
    let top = i64::from(bounds.y).max(root_top);
    let right = (i64::from(bounds.x) + i64::from(bounds.width)).min(root_right);
    let bottom = (i64::from(bounds.y) + i64::from(bounds.height)).min(root_bottom);
    if left >= right || top >= bottom {
        return None;
    }

    // Preserve GUI aspect ratio instead of independently stretching every
    // window to the full grid. Logical accessibility pixels correspond closely
    // to browser CSS pixels, so also avoid enlarging beyond the default
    // Shellglass cell size. Small dialogs then remain compact rather than
    // turning two checkbox columns into opposite sides of a 1080p display.
    let fit_scale = (rows as f64 / f64::from(root.height))
        .min(cols as f64 * CELL_ASPECT / f64::from(root.width));
    let row_scale = if fit_window {
        fit_scale
    } else {
        (1.0 / CELL_HEIGHT_PX).min(fit_scale)
    };
    let column_scale = row_scale / CELL_ASPECT;
    let projected_width = (f64::from(root.width) * column_scale).floor().max(1.0) as usize;
    let projected_height = (f64::from(root.height) * row_scale).floor().max(1.0) as usize;
    let x_offset = cols.saturating_sub(projected_width) / 2;
    let y_offset = rows.saturating_sub(projected_height) / 2;

    let project_x = |coordinate: i64| {
        x_offset + ((coordinate - root_left) as f64 * column_scale).floor() as usize
    };
    let project_y =
        |coordinate: i64| y_offset + ((coordinate - root_top) as f64 * row_scale).floor() as usize;
    let x = project_x(left).min(cols - 1);
    let y = project_y(top).min(rows - 1);
    let right_cell = project_x(right).min(cols);
    let bottom_cell = project_y(bottom).min(rows);
    Some(CellRect {
        x,
        y,
        width: right_cell.saturating_sub(x).max(1),
        height: bottom_cell.saturating_sub(y).max(1),
    })
}

fn owns_descendant_text(node: &SnapshotNode) -> bool {
    match node.role {
        Role::Group => inline_flow_text(node).is_some(),
        Role::ListItem => {
            bullet_list_label(node).is_some()
                || compact_list_row_label(node).is_some()
                || (node.children.len() == 1
                    && matches!(node.children[0].role, Role::Link | Role::StaticText))
        }
        Role::TreeItem => true,
        Role::Button
        | Role::CheckBox
        | Role::RadioButton
        | Role::Switch
        | Role::TextField
        | Role::TextArea
        | Role::ComboBox
        | Role::SpinButton
        | Role::ProgressBar
        | Role::Slider
        | Role::Image
        | Role::Link
        | Role::MenuItem
        | Role::StaticText
        | Role::Heading => true,
        _ => false,
    }
}

fn bullet_list_label(node: &SnapshotNode) -> Option<String> {
    if node.role != Role::ListItem {
        return None;
    }
    if let Some(name) = nonempty(&node.name) {
        let lines = name
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if lines.len() >= 2 {
            let mut label = format!("• {}", lines[0]);
            for line in &lines[1..] {
                label.push_str("\n  ◦ ");
                label.push_str(line);
            }
            return Some(label);
        }
    }
    if node.children.iter().any(|child| child.role == Role::List) {
        // A semantic list is sufficient evidence for hierarchy even when a
        // legacy provider omits its painted bullet from the accessibility tree.
        let mut lines = vec![format!("• {}", direct_list_item_text(node)?)];
        for list in node
            .children
            .iter()
            .filter(|child| child.role == Role::List)
        {
            append_nested_list_labels(list, 1, &mut lines);
        }
        return Some(lines.join("\n"));
    }
    let has_marker = nonempty(&node.name).is_some_and(|name| name.starts_with('•'))
        || node.children.iter().any(|child| {
            child.role == Role::StaticText
                && nonempty(&child.name)
                    .or_else(|| nonempty(&child.value))
                    .is_some_and(|text| text.trim() == "•")
        });
    if !has_marker {
        return None;
    }
    let label = nonempty(&node.name)?;
    Some(format!("• {}", label.trim_start_matches('•').trim_start()))
}

fn direct_list_item_text(node: &SnapshotNode) -> Option<String> {
    let mut parts = Vec::new();
    for child in &node.children {
        let text = match child.role {
            Role::List => continue,
            Role::Link => single_descendant_static_text(child).or_else(|| {
                nonempty(&child.name)
                    .or_else(|| nonempty(&child.description))
                    .map(str::to_string)
            }),
            Role::StaticText => nonempty(&child.name)
                .or_else(|| nonempty(&child.value))
                .filter(|text| !matches!(text.trim(), "•" | "◦"))
                .map(str::to_string),
            _ => None,
        };
        if let Some(text) = text {
            parts.push(text);
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn append_nested_list_labels(list: &SnapshotNode, depth: usize, lines: &mut Vec<String>) {
    for item in list
        .children
        .iter()
        .filter(|child| child.role == Role::ListItem)
    {
        let Some(label) = direct_list_item_text(item).or_else(|| nonempty(&item.name).map(clean))
        else {
            continue;
        };
        lines.push(format!("{}◦ {label}", "  ".repeat(depth)));
        for nested in item
            .children
            .iter()
            .filter(|child| child.role == Role::List)
        {
            append_nested_list_labels(nested, depth + 1, lines);
        }
    }
}

fn tree_item_status(node: &SnapshotNode) -> Option<&'static str> {
    fn find_explicit(node: &SnapshotNode) -> Option<&'static str> {
        if node.role == Role::StaticText
            && let Some(text) = nonempty(&node.name).or_else(|| nonempty(&node.value))
        {
            return match text.trim() {
                "U" => Some("U"),
                "M" => Some("M"),
                "A" => Some("A"),
                "D" => Some("D"),
                "R" => Some("R"),
                "C" => Some("C"),
                _ => None,
            };
        }
        node.children.iter().find_map(find_explicit)
    }

    find_explicit(node).or_else(|| {
        fn has_emphasis(node: &SnapshotNode) -> bool {
            nonempty(&node.name).is_some_and(|name| name.contains("Contains emphasized items"))
                || node.children.iter().any(has_emphasis)
        }
        has_emphasis(node).then_some("●")
    })
}

fn is_tabular_list_item(node: &SnapshotNode) -> bool {
    node.role == Role::ListItem
        && node.children.len() >= 2
        && node
            .children
            .iter()
            .any(|child| child.role == Role::TextField)
        && node
            .children
            .iter()
            .all(|child| matches!(child.role, Role::TextField | Role::StaticText))
}

fn compact_list_row_label(node: &SnapshotNode) -> Option<String> {
    if node.role != Role::ListItem || node.children.len() < 2 {
        return None;
    }
    let mut parts = Vec::new();
    for child in &node.children {
        let text = match child.role {
            Role::StaticText => nonempty(&child.name)
                .or_else(|| nonempty(&child.value))
                .map(str::to_string),
            Role::Link => single_descendant_static_text(child).or_else(|| {
                nonempty(&child.name)
                    .or_else(|| nonempty(&child.description))
                    .map(str::to_string)
            }),
            _ => return None,
        };
        let Some(text) = text
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        if text.starts_with("http://")
            || text.starts_with("https://")
            || text.eq_ignore_ascii_case("View evaluation results")
        {
            continue;
        }
        parts.push(text);
    }
    (parts.len() >= 2).then(|| parts.join(" "))
}

fn inline_flow_text(node: &SnapshotNode) -> Option<String> {
    if node.children.is_empty() || !matches!(node.role, Role::Group | Role::StaticText) {
        return None;
    }
    if node
        .children
        .iter()
        .any(|child| !matches!(child.role, Role::StaticText | Role::Link | Role::Group))
    {
        return None;
    }
    let mut text = String::new();
    for child in &node.children {
        if !collect_inline_text(child, &mut text) {
            return None;
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

fn collect_inline_text(node: &SnapshotNode, output: &mut String) -> bool {
    match node.role {
        Role::Link => {
            let descendant = single_descendant_static_text(node);
            let Some(text) = descendant
                .as_deref()
                .or_else(|| nonempty(&node.name))
                .or_else(|| nonempty(&node.description))
                .or_else(|| nonempty(&node.value))
            else {
                return false;
            };
            output.push_str(text);
            true
        }
        Role::StaticText | Role::Group => {
            if node.children.is_empty() {
                let Some(text) = nonempty(&node.name).or_else(|| nonempty(&node.value)) else {
                    return false;
                };
                output.push_str(text);
                true
            } else {
                node.children
                    .iter()
                    .all(|child| collect_inline_text(child, output))
            }
        }
        _ => false,
    }
}

fn single_descendant_static_text(node: &SnapshotNode) -> Option<String> {
    fn collect(node: &SnapshotNode, values: &mut Vec<String>) {
        if node.role == Role::StaticText
            && let Some(text) = nonempty(&node.name).or_else(|| nonempty(&node.value))
        {
            values.push(text.to_string());
        } else {
            for child in &node.children {
                collect(child, values);
            }
        }
    }

    let mut values = Vec::new();
    for child in &node.children {
        collect(child, &mut values);
    }
    (values.len() == 1).then(|| values.remove(0))
}

fn draw_spatial_control(
    canvas: &mut Canvas,
    rect: CellRect,
    node: &SnapshotNode,
    expanded_multiline: bool,
) -> bool {
    let mut style = Style {
        fg: if node.states.focused { YELLOW } else { FG },
        bold: node.states.focused || matches!(node.role, Role::Heading),
        dim: !node.states.enabled,
        ..Style::default()
    };
    if node.states.selected {
        style.bg = Color::Rgb(61, 48, 78);
    }
    let raw_label = nonempty(&node.value)
        .or_else(|| nonempty(&node.name))
        .or_else(|| nonempty(&node.description))
        .unwrap_or("");
    let name = nonempty(&node.name).map(clean);
    let value = nonempty(&node.value).map(clean);
    let description = nonempty(&node.description).map(clean);
    let label = value
        .as_deref()
        .or(name.as_deref())
        .or(description.as_deref())
        .unwrap_or("");
    let line = rect.y + rect.height.saturating_sub(1) / 2;

    match node.role {
        Role::Application | Role::Window | Role::WebArea | Role::TableRow => false,
        Role::Group => {
            if let Some(text) = inline_flow_text(node) {
                draw_flow_text(canvas, rect, &text, style);
                true
            } else if node.children.is_empty() && !label.is_empty() {
                draw_inline_label(canvas, rect, label, style, true);
                true
            } else if !node.children.is_empty()
                && node
                    .children
                    .iter()
                    .all(|child| matches!(child.role, Role::ScrollBar | Role::ScrollThumb))
            {
                canvas.text(
                    rect.x,
                    rect.y,
                    rect.width,
                    "⟦ list not exposed ⟧",
                    Style { fg: MUTED, ..style },
                );
                true
            } else {
                false
            }
        }
        Role::Button => {
            // Qt and other providers frequently use anonymous Button nodes as
            // layout wrappers around whole panes. Drawing those as controls
            // creates synthetic borders through their descendants.
            if label.is_empty() {
                false
            } else {
                if rect.width <= 3 && name.as_deref() == Some("Open") {
                    canvas.text(rect.x, line, rect.width, "▼", style);
                } else {
                    draw_labeled_box(canvas, rect, label, style);
                }
                true
            }
        }
        Role::CheckBox => {
            let mark = match node.states.checked {
                Some(Toggled::On) => "x",
                Some(Toggled::Mixed) => "-",
                _ => " ",
            };
            draw_intrinsic_label(canvas, rect, &format!("[{mark}] {label}"), style);
            true
        }
        Role::RadioButton => {
            let mark = if node.states.checked == Some(Toggled::On) {
                "●"
            } else {
                " "
            };
            draw_intrinsic_label(canvas, rect, &format!("({mark}) {label}"), style);
            true
        }
        Role::Switch => {
            let state = if node.states.checked == Some(Toggled::On) {
                "ON"
            } else {
                "OFF"
            };
            draw_intrinsic_label(canvas, rect, &format!("[{state}] {label}"), style);
            true
        }
        Role::TextField if raw_label.contains('\n') => {
            // UIA commonly maps multiline code editors to Edit/TextField
            // rather than TextArea. Newlines are stronger evidence than the
            // nominal role; centering this value as a one-line field destroys
            // the editor layout. Virtualized editors such as Monaco expose the
            // value on the current-line field, so use its containing editor
            // group without inventing a border that the host does not have.
            if expanded_multiline {
                draw_multiline_text(
                    canvas,
                    rect,
                    raw_label,
                    style,
                    vertical_scroll_fraction(node),
                );
            } else {
                draw_text_area(
                    canvas,
                    rect,
                    raw_label,
                    style,
                    vertical_scroll_fraction(node),
                );
            }
            true
        }
        Role::TextField | Role::ComboBox | Role::SpinButton => {
            draw_labeled_box(canvas, rect, label, style);
            true
        }
        Role::TextArea => {
            draw_text_area(
                canvas,
                rect,
                raw_label,
                style,
                vertical_scroll_fraction(node),
            );
            true
        }
        Role::Dialog | Role::Alert | Role::SplitGroup => {
            canvas.border(rect, style);
            if !label.is_empty() {
                let width = rect.width.saturating_sub(2);
                canvas.text(
                    rect.x.saturating_add(1),
                    rect.y,
                    width,
                    &elide(label, width),
                    style,
                );
            }
            true
        }
        Role::List | Role::Table if !node.children.is_empty() => {
            // Bounds identify the content viewport, not proof of a visible
            // border. Synthesizing one puts its top edge through the first row
            // in providers such as Total Commander and x64dbg.
            false
        }
        Role::ListItem if bullet_list_label(node).is_some() => {
            let label = bullet_list_label(node).expect("bullet label checked above");
            draw_flow_text(canvas, rect, &label, style);
            true
        }
        Role::ListItem if compact_list_row_label(node).is_some() => {
            let label = compact_list_row_label(node).expect("compact row label checked above");
            draw_inline_label(canvas, rect, &label, style, false);
            true
        }
        Role::TreeItem => {
            if label.is_empty() {
                false
            } else {
                let disclosure = match node.states.expanded {
                    Some(true) => "▾ ",
                    Some(false) => "▸ ",
                    None => "  ",
                };
                let row_width = rect.width.saturating_sub(1).max(1);
                let mut row = format!("{disclosure}{label}");
                if let Some(status) = tree_item_status(node) {
                    let status_width = UnicodeWidthStr::width(status);
                    let label_width = row_width.saturating_sub(status_width + 1);
                    row = format!(
                        "{}{}{}",
                        elide(&row, label_width),
                        " ".repeat(
                            label_width.saturating_sub(UnicodeWidthStr::width(row.as_str())) + 1
                        ),
                        status
                    );
                } else {
                    row = elide(&row, row_width);
                }
                canvas.text(rect.x, line, rect.width, &row, style);
                true
            }
        }
        Role::List | Role::Table => {
            if label.is_empty() {
                false
            } else {
                draw_inline_label(canvas, rect, label, style, false);
                true
            }
        }
        Role::TableCell => {
            if rect.width >= 4 && rect.height >= 3 {
                canvas.border(rect, Style { fg: MUTED, ..style });
                let width = rect.width.saturating_sub(2);
                canvas.text(
                    rect.x.saturating_add(1),
                    line,
                    width,
                    &elide(label, width),
                    style,
                );
            } else {
                draw_inline_label(canvas, rect, label, style, true);
            }
            true
        }
        Role::ProgressBar => {
            let inner = rect.width.saturating_sub(2);
            canvas.text(
                rect.x,
                line,
                rect.width,
                &format!("[{}]", "━".repeat(inner)),
                style,
            );
            true
        }
        Role::Slider => {
            let width = rect.width.saturating_sub(1);
            canvas.text(
                rect.x,
                line,
                rect.width,
                &format!(
                    "{}●{}",
                    "─".repeat(width / 2),
                    "─".repeat(width.saturating_sub(width / 2 + 1))
                ),
                style,
            );
            true
        }
        Role::Separator => {
            canvas.horizontal(rect.x, line, rect.width, "─", Style { fg: MUTED, ..style });
            true
        }
        Role::ScrollBar | Role::ScrollThumb => {
            // A one-pixel scrollbar frequently quantizes onto adjacent content,
            // and a static TUI cannot represent its thumb position usefully.
            false
        }
        Role::Image => {
            draw_inline_label(
                canvas,
                rect,
                &format!("▧ {label}"),
                Style { fg: CYAN, ..style },
                true,
            );
            true
        }
        Role::Link => {
            style.fg = CYAN;
            style.italic = true;
            let descendant = single_descendant_static_text(node);
            let link_label = descendant
                .as_deref()
                .or(name.as_deref())
                .or(description.as_deref())
                .or(value.as_deref())
                .unwrap_or("");
            draw_inline_label(canvas, rect, link_label, style, true);
            true
        }
        Role::MenuBar | Role::TabGroup | Role::Toolbar | Role::Navigation
            if !node.children.is_empty() =>
        {
            false
        }
        Role::MenuItem => {
            if label.is_empty() {
                false
            } else {
                canvas.flow_text(rect.x, line, label, style);
                true
            }
        }
        Role::Tab => {
            if label.is_empty() {
                false
            } else {
                draw_inline_label(canvas, rect, label, style, true);
                true
            }
        }
        Role::ListItem => {
            if let Some(list_label) = bullet_list_label(node) {
                draw_flow_text(canvas, rect, &list_label, style);
                true
            } else if label.is_empty() {
                false
            } else {
                let label_width = UnicodeWidthStr::width(label);
                if label_width > rect.width && label_width <= 12 {
                    canvas.flow_text(rect.x, line, label, style);
                } else {
                    draw_inline_label(canvas, rect, label, style, false);
                }
                true
            }
        }
        Role::StaticText if inline_flow_text(node).is_some() => {
            let text = inline_flow_text(node).expect("flow text checked above");
            draw_flow_text(canvas, rect, &text, style);
            true
        }
        Role::StaticText if raw_label.contains('\n') => {
            draw_multiline_text(
                canvas,
                rect,
                raw_label,
                style,
                vertical_scroll_fraction(node),
            );
            true
        }
        Role::StaticText
        | Role::Heading
        | Role::Menu
        | Role::MenuBar
        | Role::TabGroup
        | Role::Toolbar
        | Role::Tooltip
        | Role::Status
        | Role::Navigation
        | Role::Unknown => {
            if label.is_empty() {
                false
            } else {
                draw_inline_label(canvas, rect, label, style, false);
                true
            }
        }
    }
}

fn draw_flow_text(canvas: &mut Canvas, rect: CellRect, value: &str, style: Style) {
    for (offset, line) in wrap_flow_text(value, rect.width, rect.height)
        .into_iter()
        .enumerate()
    {
        canvas.text(rect.x, rect.y + offset, rect.width, &line, style);
    }
}

fn wrap_flow_text(value: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let bounded = value
        .chars()
        .take(MAX_MULTILINE_FIELD_CHARS)
        .collect::<String>();
    let mut lines = Vec::new();
    for source_line in bounded.lines() {
        let indent = source_line
            .chars()
            .take_while(|character| *character == ' ')
            .count()
            .min(width.saturating_sub(1));
        let prefix = " ".repeat(indent);
        let content_width = width.saturating_sub(indent).max(1);
        let mut current = String::new();
        let mut occupied = 0usize;
        for word in source_line.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            if !current.is_empty() && occupied + 1 + word_width > content_width {
                lines.push(format!("{prefix}{current}"));
                current.clear();
                occupied = 0;
                if lines.len() == max_lines {
                    return lines;
                }
            }
            if word_width <= content_width {
                if !current.is_empty() {
                    current.push(' ');
                    occupied += 1;
                }
                current.push_str(word);
                occupied += word_width;
            } else {
                if !current.is_empty() {
                    lines.push(format!("{prefix}{current}"));
                    current.clear();
                    occupied = 0;
                    if lines.len() == max_lines {
                        return lines;
                    }
                }
                let chunks = wrap_text(word, content_width, max_lines - lines.len());
                let chunk_count = chunks.len();
                for (index, chunk) in chunks.into_iter().enumerate() {
                    if index + 1 == chunk_count {
                        occupied = UnicodeWidthStr::width(chunk.as_str());
                        current = chunk;
                    } else {
                        lines.push(format!("{prefix}{chunk}"));
                        if lines.len() == max_lines {
                            return lines;
                        }
                    }
                }
            }
        }
        if !current.is_empty() {
            lines.push(format!("{prefix}{current}"));
        } else if source_line.is_empty() {
            lines.push(String::new());
        }
        if lines.len() >= max_lines {
            lines.truncate(max_lines);
            return lines;
        }
    }
    lines
}

fn draw_multiline_text(
    canvas: &mut Canvas,
    rect: CellRect,
    value: &str,
    style: Style,
    scroll_fraction: Option<f64>,
) {
    // Some editor providers expose every logical line padded to the viewport's
    // pixel-column width. Those trailing spaces must not hard-wrap into a
    // second visually blank terminal row.
    let unpadded = value
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t', '\r']))
        .collect::<Vec<_>>()
        .join("\n");
    let lines = wrap_text(&unpadded, rect.width, MAX_MULTILINE_FIELD_CHARS);
    let overflow = lines.len().saturating_sub(rect.height);
    let start = scroll_fraction
        .map(|fraction| (overflow as f64 * fraction).round() as usize)
        .unwrap_or(0)
        .min(overflow);
    for (offset, line) in lines.into_iter().skip(start).take(rect.height).enumerate() {
        canvas.text(rect.x, rect.y + offset, rect.width, &line, style);
    }
}

fn vertical_scroll_fraction(node: &SnapshotNode) -> Option<f64> {
    if node.role == Role::ScrollBar
        && node
            .bounds
            .is_none_or(|bounds| bounds.height >= bounds.width)
        && let (Some(current), Some(minimum), Some(maximum)) =
            (node.numeric_value, node.min_value, node.max_value)
        && current.is_finite()
        && minimum.is_finite()
        && maximum.is_finite()
        && maximum > minimum
    {
        return Some(((current - minimum) / (maximum - minimum)).clamp(0.0, 1.0));
    }
    node.children.iter().find_map(vertical_scroll_fraction)
}

fn draw_text_area(
    canvas: &mut Canvas,
    rect: CellRect,
    value: &str,
    style: Style,
    scroll_fraction: Option<f64>,
) {
    let (x, y, width, height) = if rect.width >= 4 && rect.height >= 3 {
        canvas.border(rect, style);
        (rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2)
    } else {
        (rect.x, rect.y, rect.width, rect.height)
    };
    draw_multiline_text(
        canvas,
        CellRect {
            x,
            y,
            width,
            height,
        },
        value,
        style,
        scroll_fraction,
    );
}

fn wrap_text(value: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut occupied = 0usize;
    for grapheme in value
        .chars()
        .take(MAX_MULTILINE_FIELD_CHARS)
        .collect::<String>()
        .graphemes(true)
    {
        if grapheme.contains('\n') {
            lines.push(std::mem::take(&mut line));
            occupied = 0;
        } else {
            let rendered = if grapheme.chars().all(char::is_control) {
                " "
            } else {
                grapheme
            };
            let grapheme_width = UnicodeWidthStr::width(rendered).min(2);
            if occupied + grapheme_width > width {
                lines.push(std::mem::take(&mut line));
                occupied = 0;
            }
            line.push_str(rendered);
            occupied += grapheme_width;
        }
        if lines.len() == max_lines {
            return lines;
        }
    }
    if lines.len() < max_lines && (!line.is_empty() || lines.is_empty()) {
        lines.push(line);
    }
    lines
}

fn draw_labeled_box(canvas: &mut Canvas, rect: CellRect, label: &str, style: Style) {
    if rect.width >= 4 && rect.height >= 3 {
        canvas.border(rect, style);
        let width = rect.width - 2;
        canvas.text(
            rect.x + 1,
            rect.y + rect.height.saturating_sub(1) / 2,
            width,
            &elide(label, width),
            style,
        );
    } else {
        draw_inline_label(canvas, rect, label, style, true);
    }
}

fn draw_intrinsic_label(canvas: &mut Canvas, rect: CellRect, label: &str, style: Style) {
    let line = rect.y + rect.height.saturating_sub(1) / 2;
    let width = UnicodeWidthStr::width(label)
        .max(rect.width)
        .min(canvas.cols.saturating_sub(rect.x));
    canvas.text(rect.x, line, width, label, style);
}

fn draw_inline_label(
    canvas: &mut Canvas,
    rect: CellRect,
    label: &str,
    style: Style,
    trailing_gutter: bool,
) {
    let line = rect.y + rect.height.saturating_sub(1) / 2;
    let gutter = usize::from(trailing_gutter && rect.width > 1);
    let width = rect.width.saturating_sub(gutter);
    canvas.text(rect.x, line, width, &elide(label, width), style);
    if gutter != 0 {
        canvas.clear_footprint(rect.x + rect.width - 1, line);
    }
}

fn elide(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let value = clean(value);
    if UnicodeWidthStr::width(value.as_str()) <= width {
        return value;
    }
    if width == 1 {
        return "…".into();
    }

    let mut output = String::new();
    let mut occupied = 0usize;
    for grapheme in value.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).min(2);
        if occupied + grapheme_width + 1 > width {
            break;
        }
        output.push_str(grapheme);
        occupied += grapheme_width;
    }
    output.push('…');
    output
}

fn snapshot_heading(snapshot: &Snapshot) -> String {
    let app = clean(&snapshot.app_name);
    let window = clean(&snapshot.window_name);
    if app == window {
        app
    } else {
        format!("{app} › {window}")
    }
}

fn snapshot_header(snapshot: &Snapshot) -> Vec<Span> {
    vec![
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
            format!(" {} ", snapshot_heading(snapshot)),
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
    ]
}

fn snapshot_divider(cols: u16) -> Vec<Span> {
    vec![Span::new(
        "─".repeat(usize::from(cols)),
        Style {
            fg: CYAN,
            ..Style::default()
        },
    )]
}

fn render_tree_snapshot(snapshot: &Snapshot, cols: u16, rows: u16, max_depth: usize) -> Frame {
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
            format!(" {} ", snapshot_heading(snapshot)),
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
    rendered_rows.truncate(usize::from(rows));
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

fn bounded_value_option(value: &Option<String>) -> Option<String> {
    value.as_deref().map(|value| {
        let limit = if value.contains('\n') {
            MAX_MULTILINE_FIELD_CHARS
        } else {
            MAX_FIELD_CHARS
        };
        value.chars().take(limit).collect()
    })
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

/// Capture a development-only screenshot/tree pair and the corresponding
/// plain-text TUI render. Live streaming never calls this path.
pub fn capture_layout_fixture(
    options: AccessibilityOptions,
    output: &Path,
    delay: Duration,
) -> Result<()> {
    options.validate()?;
    let policy = PrivacyPolicy::load(&options)?;
    if !delay.is_zero() {
        eprintln!(
            "shellglass accessibility: focus the fixture window; capturing in {} ms",
            delay.as_millis()
        );
        std::thread::sleep(delay);
    }

    for _ in 0..10 {
        match capture(&options, &policy)? {
            CaptureOutcome::Visible(identity, snapshot) => {
                let bounds = snapshot
                    .root
                    .bounds
                    .context("fixture window has no screenshot bounds")?;
                let screenshot = xa11y::screenshot_region(bounds)
                    .context("capturing fixture window screenshot")?;

                let latest_app = App::foreground(Duration::ZERO)
                    .context("rechecking fixture foreground application")?;
                if policy.blocks(&latest_app)? {
                    bail!("fixture capture blocked by accessibility privacy policy");
                }
                let latest_window = active_window(&latest_app)?;
                if source_identity(&latest_window) != identity {
                    std::thread::sleep(options.interval());
                    continue;
                }

                std::fs::create_dir_all(output)
                    .with_context(|| format!("creating fixture directory {}", output.display()))?;
                let fixture = LayoutFixture {
                    schema_version: 1,
                    cols: options.cols,
                    rows: options.rows,
                    snapshot: (*snapshot).clone(),
                };
                let tree = serde_json::to_vec_pretty(&fixture)
                    .context("serializing accessibility layout fixture")?;
                std::fs::write(output.join("tree.json"), tree)
                    .with_context(|| format!("writing fixture tree in {}", output.display()))?;
                screenshot
                    .save_png(output.join("reference.png"))
                    .context("writing fixture reference screenshot")?;
                let rendered =
                    render_snapshot(&snapshot, options.cols, options.rows, options.max_depth);
                std::fs::write(output.join("render.txt"), plain_frame(&rendered)).with_context(
                    || format!("writing fixture TUI render in {}", output.display()),
                )?;
                return Ok(());
            }
            CaptureOutcome::Blocked => {
                bail!("fixture capture blocked by accessibility privacy policy");
            }
            CaptureOutcome::Unstable => std::thread::sleep(options.interval()),
        }
    }
    bail!("fixture window did not remain stable long enough to capture")
}

/// Replay a captured tree without touching the live accessibility or screenshot
/// APIs. Used to iterate on layout deterministically.
pub fn render_layout_fixture(
    input: &Path,
    output: Option<&Path>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<()> {
    let bytes = std::fs::read(input)
        .with_context(|| format!("reading layout fixture {}", input.display()))?;
    let fixture: LayoutFixture = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing layout fixture {}", input.display()))?;
    if fixture.schema_version != 1 {
        bail!(
            "unsupported layout fixture schema version {}",
            fixture.schema_version
        );
    }
    let rendered = render_snapshot(
        &fixture.snapshot,
        cols.unwrap_or(fixture.cols),
        rows.unwrap_or(fixture.rows),
        DEFAULT_MAX_DEPTH,
    );
    let text = plain_frame(&rendered);
    if let Some(path) = output {
        std::fs::write(path, text)
            .with_context(|| format!("writing rendered fixture {}", path.display()))?;
    } else {
        print!("{text}");
    }
    Ok(())
}

fn plain_frame(frame: &Frame) -> String {
    let Frame::Screen(grid) = frame;
    let mut output = String::new();
    for row in &grid.rows {
        let mut line = row
            .iter()
            .map(|cell| {
                if cell.text.is_empty() {
                    " "
                } else {
                    cell.text.as_str()
                }
            })
            .collect::<String>();
        line.truncate(line.trim_end().len());
        output.push_str(&line);
        output.push('\n');
    }
    output
}

/// Render the exact accessibility `Frame` producer into a local alternate
/// screen. This is a test/debug viewer, not a second capture implementation.
pub async fn preview(options: AccessibilityOptions) -> Result<()> {
    options.validate()?;
    let mut terminal = TerminalPreview::enter()?;
    let (initial_cols, initial_rows) = terminal.size()?;
    let dimensions = Arc::new((AtomicU16::new(initial_cols), AtomicU16::new(initial_rows)));
    let (publisher, mut source) = external_source(message_frame(
        initial_cols,
        initial_rows,
        "shellglass accessibility",
        "Waiting for a foreground accessibility window…",
        CYAN,
    ));
    let current_identity = Arc::new(Mutex::new(None::<String>));
    let worker_dimensions = Arc::clone(&dimensions);
    spawn_with_dimensions(
        options,
        || Some(0),
        move || {
            (
                worker_dimensions.0.load(Ordering::Relaxed),
                worker_dimensions.1.load(Ordering::Relaxed),
            )
        },
        move |_ticket, identity, frame| {
            publish_source(&publisher, &current_identity, identity, frame);
        },
        |_ticket| {},
    )?;

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
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                let (cols, rows) = terminal.size()?;
                dimensions.0.store(cols, Ordering::Relaxed);
                dimensions.1.store(rows, Ordering::Relaxed);
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

    fn size(&self) -> Result<(u16, u16)> {
        let (cols, rows) = crossterm::terminal::size().context("reading preview terminal size")?;
        Ok((cols.clamp(1, 500), rows.clamp(1, 200)))
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
    fn text_areas_preserve_newlines_and_wrap_long_lines() {
        assert_eq!(
            wrap_text("first line\nsecond line is long", 10, 4),
            vec!["first line", "second lin", "e is long"]
        );
    }

    #[test]
    fn multiline_log_values_render_as_lines_from_the_visible_tail() {
        let mut canvas = Canvas::new(16, 3);
        let mut log = test_node(
            Role::StaticText,
            Rect {
                x: 0,
                y: 0,
                width: 16,
                height: 3,
            },
            "Log",
        );
        let value = format!(
            "{}recent one\nrecent two\ncurrent",
            "old line\n".repeat(300)
        );
        log.value = Some(value);
        let mut scrollbar = test_node(
            Role::ScrollBar,
            Rect {
                x: 15,
                y: 0,
                width: 1,
                height: 3,
            },
            "",
        );
        scrollbar.numeric_value = Some(100.0);
        scrollbar.min_value = Some(0.0);
        scrollbar.max_value = Some(100.0);
        log.children.push(scrollbar);
        assert_eq!(vertical_scroll_fraction(&log), Some(1.0));
        assert!(draw_spatial_control(
            &mut canvas,
            CellRect {
                x: 0,
                y: 0,
                width: 16,
                height: 3,
            },
            &log,
            false,
        ));
        let rows = canvas.into_rows();
        let text = |row: usize| {
            rows[row]
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        };
        assert!(text(0).starts_with("recent one"));
        assert!(text(1).starts_with("recent two"));
        assert!(text(2).starts_with("current"));
    }

    #[test]
    fn nested_html_link_wrappers_join_once_in_document_order() {
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 20,
        };
        let mut link = test_node(Role::Link, bounds, "really lame");
        link.children
            .push(test_node(Role::StaticText, bounds, "really lame"));
        let paragraph = test_node_with_children(
            Role::Group,
            bounds,
            "",
            vec![
                test_node(Role::StaticText, bounds, "These agents are "),
                test_node_with_children(Role::Group, bounds, "", vec![link]),
                test_node(Role::StaticText, bounds, ". That's the whole thing."),
            ],
        );
        assert_eq!(
            inline_flow_text(&paragraph).as_deref(),
            Some("These agents are really lame. That's the whole thing.")
        );

        let navigation = test_node_with_children(
            Role::Link,
            bounds,
            "Search and explore",
            vec![test_node_with_children(
                Role::Group,
                bounds,
                "",
                vec![test_node(Role::StaticText, bounds, "Explore")],
            )],
        );
        assert_eq!(
            single_descendant_static_text(&navigation).as_deref(),
            Some("Explore")
        );
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
    fn moving_window_requires_one_stable_sample_before_publication() {
        let mut stabilizer = GeometryStabilizer::default();
        let mut identity = SourceIdentity {
            pid: Some(7),
            stable_id: Some("window".into()),
            class_name: Some("fixture".into()),
            bounds: Some(Rect {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            }),
        };
        assert!(stabilizer.should_publish(&identity));
        identity.bounds.as_mut().expect("fixture bounds").x = 30;
        assert!(!stabilizer.should_publish(&identity));
        assert!(stabilizer.should_publish(&identity));
        identity.stable_id = Some("other-window".into());
        assert!(stabilizer.should_publish(&identity));
    }

    #[test]
    fn late_borders_and_scrollbars_never_overwrite_content() {
        let mut canvas = Canvas::new(20, 3);
        canvas.border(
            CellRect {
                x: 1,
                y: 0,
                width: 18,
                height: 3,
            },
            Style::default(),
        );
        canvas.text(1, 1, 18, "RAX = 0000", Style::default());
        // Models a narrow scrollbar three pixels left of a list whose bounds
        // collapse onto the same terminal column, as exposed by x64dbg.
        canvas.vertical(1, 0, 3, "│", Style::default());
        let rows = canvas.into_rows();
        assert_eq!(rows[1][1].text, "R", "register name must remain intact");
    }

    #[test]
    fn adjacent_pixel_bounds_do_not_overlap_after_projection() {
        let root = Rect {
            x: 2_277,
            y: 155,
            width: 1_100,
            height: 800,
        };
        let files = project_bounds(
            Rect {
                x: 2_285,
                y: 186,
                width: 37,
                height: 19,
            },
            root,
            126,
            60,
            false,
        )
        .expect("Files menu bounds");
        let mark = project_bounds(
            Rect {
                x: 2_322,
                y: 186,
                width: 41,
                height: 19,
            },
            root,
            126,
            60,
            false,
        )
        .expect("Mark menu bounds");
        assert!(files.x + files.width <= mark.x);
    }

    #[test]
    fn small_dialog_keeps_css_pixel_scale_in_a_large_viewport() {
        let root = Rect {
            x: 1_368,
            y: 285,
            width: 445,
            height: 620,
        };
        let projected = project_bounds(root, root, 200, 57, false).expect("dialog projection");
        assert!(
            projected.width < 60,
            "dialog should not stretch to 200 columns"
        );
        assert!(
            projected.height < 40,
            "dialog should not stretch to 57 rows"
        );
        assert!(projected.x > 60, "dialog should be horizontally centered");

        let first = project_bounds(
            Rect {
                x: 1_394,
                y: 379,
                width: 125,
                height: 18,
            },
            root,
            200,
            57,
            false,
        )
        .expect("first checkbox");
        let second = project_bounds(
            Rect {
                x: 1_394,
                y: 402,
                width: 125,
                height: 18,
            },
            root,
            200,
            57,
            false,
        )
        .expect("second checkbox");
        assert!(second.y - first.y <= 2, "checkbox rows should stay compact");
    }

    #[test]
    fn adjacent_menu_items_reflow_into_available_space() {
        let mut canvas = Canvas::new(30, 1);
        canvas.flow_text(0, 0, "Files", Style::default());
        canvas.flow_text(4, 0, "Mark", Style::default());
        canvas.flow_text(8, 0, "Commands", Style::default());
        let row = canvas.into_rows().remove(0);
        let text = row
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>();
        assert!(text.starts_with("Files Mark Commands "));
    }

    #[test]
    fn populated_lists_do_not_invent_a_border_through_the_first_row() {
        let mut canvas = Canvas::new(40, 10);
        let list = test_node_with_children(
            Role::List,
            Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 100,
            },
            "Files",
            vec![test_node(
                Role::ListItem,
                Rect {
                    x: 0,
                    y: 0,
                    width: 400,
                    height: 10,
                },
                "README.md",
            )],
        );
        assert!(!draw_spatial_control(
            &mut canvas,
            CellRect {
                x: 0,
                y: 0,
                width: 40,
                height: 10,
            },
            &list,
            false,
        ));
        assert!(
            canvas.into_rows()[0]
                .iter()
                .all(|cell| cell.text.is_empty())
        );
    }

    #[test]
    fn compact_labels_are_elided_instead_of_overwriting_neighbors() {
        assert_eq!(elide("Files", 4), "Fil…");
        assert_eq!(elide("Mark", 4), "Mark");
    }

    #[test]
    fn x64dbg_fixture_preserves_dense_registers_and_menu_labels() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/x64dbg-cpu/tree.json"
        ))
        .expect("parse x64dbg fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            12,
        ));
        assert!(rendered.contains(
            "File View Debug Tracing Plugins Favourites Options Help Jul 20 2026 (TitanEngine)"
        ));
        for register in ["RAX =", "RBX =", "RCX =", "RDX =", "RSP =", "RIP ="] {
            assert!(rendered.contains(register), "missing {register}");
        }
        for flags in [
            "ZF = 0 PF = 0 AF = 0",
            "OF = 0 SF = 0 DF = 0",
            "CF = 0 TF = 0 IF = 1",
            "GS = 002B FS = 0053",
            "ES = 002B DS = 002B",
            "CS = 0033 SS = 002B",
        ] {
            assert!(rendered.contains(flags), "truncated flag row: {flags}");
        }
        assert!(rendered.contains("LastError = 00000000 (ERROR_SUCCESS)"));
        assert!(rendered.contains("LastStatus = 00000000 (STATUS_SUCCESS)"));
        for index in 0..=4 {
            let register = format!("ST({index}) = 00000000000000000000");
            assert!(
                rendered.contains(&register),
                "truncated x87 register: {register}"
            );
        }
        assert!(rendered.contains("00007FFB54D43D5A E8 4DB1F8FF"));
        assert!(rendered.contains("BOOLEAN InheritedAddressSpace"));
        assert!(rendered.contains("000000569881B000"));
        assert!(!rendered.contains("│AX ="));
        // Rich-text tags are intentionally preserved: they are exposed by
        // x64dbg's accessibility provider and must not be guessed away here.
        assert!(rendered.contains("<u>struct</u>"));
        let tabs = rendered
            .lines()
            .find(|line| line.contains("Dump 1"))
            .expect("x64dbg dump tabs");
        assert!(!tabs.contains('─'));
    }

    #[test]
    fn x64dbg_log_fixture_preserves_newlines_and_follows_its_scrollbar() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/x64dbg-log/tree.json"
        ))
        .expect("parse x64dbg log fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            12,
        ));
        assert!(rendered.contains("Initializing debugger functions..."));
        assert!(rendered.contains("Registering debugger commands..."));
        assert!(rendered.contains("[DotX64Dbg] Generating project for MapFile"));
        assert!(
            !rendered.contains("System breakpoint reached!"),
            "the scrollbar is near the top, so the renderer must not guess the tail"
        );
        assert!(
            !rendered.contains("Initializing wait objects... Initializing debugger..."),
            "provider newlines must not be flattened into one line"
        );
    }

    #[test]
    fn chrome_fixture_flows_inline_html_text_without_overlap() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/chrome-striga/tree.json"
        ))
        .expect("parse Chrome article fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains(
            "The goal of this post is to lower the barrier of entry and let you experiment"
        ));
        assert!(
            normalized.contains("Static Devirtualization of Themida post that was just released")
        );
        for item in [
            "• SMT-LIB, used by Triton (symbolic execution)",
            "• VEX, used by angr",
            "• Sleigh, used by Ghidra, Remill and Icicle",
            "• BNIL, used by Binary Ninja (proprietary)",
        ] {
            assert!(
                normalized.contains(item),
                "missing intact list item: {item}"
            );
        }
        assert_eq!(rendered.matches("Triton").count(), 1);
        assert_eq!(rendered.matches("Binary Ninja").count(), 1);
        assert!(!rendered.contains("https://github.com"));
    }

    #[test]
    fn chm_fixture_flows_legacy_html_and_marks_unexposed_lists() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/chm-x64dbg/tree.json"
        ))
        .expect("parse CHM fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains(
            "If you came here because someone told you to read the manual, start by reading all sections of the introduction. See commands for an overview of the available commands and how they work (the arguments are comma separated)."
        ));
        assert!(rendered.contains("⟦ list not exposed ⟧"));
        let lines = rendered.lines().collect::<Vec<_>>();
        let navigation = lines
            .iter()
            .position(|line| line.contains("Introduction"))
            .expect("navigation introduction");
        assert!(lines[navigation + 1].contains("GUI manual"));
        assert!(lines[navigation + 2].contains("Commands"));
        let values = lines
            .iter()
            .position(|line| line.trim_end().ends_with("Values"))
            .expect("nested contents values");
        assert!(lines[values + 1].contains("Expressions"));
        assert!(lines[values + 2].contains("Expression Functions"));
    }

    #[test]
    fn chm_contents_fixture_expands_native_tree_rows_and_semantic_lists() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/chm-x64dbg-contents/tree.json"
        ))
        .expect("parse CHM contents fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        let lines = rendered.lines().collect::<Vec<_>>();
        let tree = lines
            .iter()
            .position(|line| line.trim_start().starts_with("x64dbg documentation"))
            .expect("CHM contents root");
        assert!(lines[tree + 1].contains("▸ Introduction"));
        assert!(lines[tree + 2].contains("▸ GUI manual"));
        assert!(lines[tree + 3].contains("▸ Commands"));
        assert!(lines[tree + 4].contains("▸ Developers"));
        assert!(lines[tree + 5].contains("Licenses"));
        assert!(!rendered.contains("▸ Intr…"));

        let contents = lines
            .iter()
            .position(|line| line.contains("• Introduction"))
            .expect("document contents root");
        assert!(lines[contents + 1].contains("◦ Values"));
        assert!(lines[contents + 2].contains("◦ Expressions"));
        assert!(lines[contents + 3].contains("◦ Expression Functions"));
        assert!(rendered.contains("• GUI manual"));
        assert!(rendered.contains("◦ Menus"));
    }

    #[test]
    fn x64dbg_plugins_fixture_places_nested_lists_on_indented_rows() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/chrome-x64dbg-plugins/tree.json"
        ))
        .expect("parse x64dbg plugins fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        assert_eq!(rendered.matches("◦ arguments").count(), 3);
        assert_eq!(rendered.matches("◦ result").count(), 3);
        let lines = rendered.lines().collect::<Vec<_>>();
        let parent = lines
            .iter()
            .position(|line| line.contains("• StartScylla/scylla/imprec"))
            .expect("parent plugin list item");
        let parent_column = lines[parent]
            .find('•')
            .expect("parent plugin bullet column");
        assert!(lines[parent + 1].contains("◦ arguments"));
        assert!(lines[parent + 2].contains("◦ result"));
        assert!(
            lines[parent + 1]
                .rfind('◦')
                .is_some_and(|column| column > parent_column)
        );
    }

    #[test]
    fn x64dbg_commands_fixture_keeps_wrapped_list_item_continuations() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/chrome-x64dbg-commands/tree.json"
        ))
        .expect("parse x64dbg commands fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        for continuation in [
            "also means a variable cannot begin with letters from A to F.",
            "content in the memory pointer, don’t add “[” and “]”.",
            "arguments. Do not use a space to separate the arguments.",
            "use an appropriate plugin which provides such feature.",
            "is used to transfer the value of the expression to the destination.",
        ] {
            assert!(
                rendered.contains(continuation),
                "missing wrapped list continuation: {continuation}"
            );
        }
    }

    #[test]
    fn hugging_face_fixture_wraps_bullets_and_compacts_result_rows() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/chrome-huggingface-list/tree.json"
        ))
        .expect("parse Hugging Face fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains(
            "• Mixed SWA and global attention layout: 48 layers in a 1:3 global-to-SWA ratio"
        ));
        assert!(
            normalized
                .contains("• Native reasoning support: interleaved thinking between tool calls")
        );
        assert!(normalized.contains("with per-request control via enable_thinking"));
        assert!(normalized.contains(
            "• Attention: grouped-query, 8 KV heads, head dim 128; per-head softplus output gating"
        ));
        assert!(rendered.contains("datacurve/deep-swe · Deep Swe leaderboard 40.4"));
        assert!(rendered.contains("ScaleAI/SWE-bench_Pro · SWE Bench Pro leaderboard 59.4"));
    }

    #[test]
    fn vscode_fixture_expands_virtualized_multiline_field_to_editor_bounds() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/vscode-screen-reader/tree.json"
        ))
        .expect("parse VS Code screen-reader fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        let lines = rendered.lines().collect::<Vec<_>>();
        let docstring = lines
            .iter()
            .position(|line| line.contains("Convenience entry point. Prefer: pyhhc"))
            .expect("editor docstring");
        assert!(lines[docstring + 2].contains("import sys"));
        assert!(lines[docstring + 4].contains("from hhc_compiler.cli import main"));
        assert!(lines[docstring + 6].contains("if __name__ == \"__main__\":"));
        assert!(lines[docstring + 7].contains("sys.exit(main())"));

        let first_tree_row = lines
            .iter()
            .position(|line| line.contains("▸ .ruff_cache"))
            .expect("first Explorer tree row");
        for (offset, name) in [
            ".ruff_cache",
            "hhc_compiler",
            "htmlhelp",
            "reference",
            "routines",
            ".gitignore",
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                lines[first_tree_row + offset].contains(name),
                "missing packed Explorer row: {name}"
            );
        }
        assert!(lines[first_tree_row + 5].contains('U'));
        assert!(!rendered.contains("hhchhc_compiler"));
    }

    #[test]
    fn ida_fixture_discards_editor_padding_without_inventing_blank_rows() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/ida-pseudocode/tree.json"
        ))
        .expect("parse IDA fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        let lines = rendered.lines().collect::<Vec<_>>();
        let first = lines
            .iter()
            .position(|line| line.contains("LODWORD(v4->Ptr) = v2;"))
            .expect("first pseudocode line");
        assert!(lines[first + 1].contains("if ( (unsigned int)VidMapVpStatePage"));
        assert!(lines[first + 2].contains("wil::details::in1diag3::_Throw_GetLastError("));
        assert!(lines[first - 1].contains("Function name"));
        assert!(!lines[first - 1].contains("Segment"));
        assert!(lines[first].contains("▾ d:/os"));
        assert!(lines[first + 1].contains("▸ obj/amd64fre"));
        assert!(lines[first + 2].contains("▾ public/amd64fre/onecore"));
        assert!(lines[first + 3].contains("initialize_printf_standa"));
    }

    #[test]
    fn dataexplorer_fixture_treats_multiline_text_fields_as_editors() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/dataexplorer/tree.json"
        ))
        .expect("parse DataExplorer fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        let lines = rendered.lines().collect::<Vec<_>>();
        let foo = lines
            .iter()
            .position(|line| line.contains("struct Foo {"))
            .expect("Foo declaration");
        assert!(lines[foo + 1].contains("u32 x;"));
        assert!(lines[foo + 2].contains("u8 y;"));
        assert!(!lines[foo].contains("struct Cat"));
        assert!(rendered.contains("Test t @ 0x77761000;"));
    }

    #[test]
    fn visual_studio_fixture_handles_rows_left_of_inferred_table_columns() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/visual-studio-2022/tree.json"
        ))
        .expect("parse Visual Studio fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        assert!(rendered.contains("Solution 'TooltipNotes'"));
        assert!(rendered.contains("Text Editor"));
        assert!(rendered.contains("current Visual Studio version does not support targeting"));
    }

    #[test]
    fn seven_zip_fixture_packs_visible_tabular_list_items() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/7zip-file-list/tree.json"
        ))
        .expect("parse 7-Zip fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            40,
        ));
        let lines = rendered.lines().collect::<Vec<_>>();
        let first = lines
            .iter()
            .position(|line| line.contains("2-iw4mp.map"))
            .expect("first visible archive row");
        for (offset, name) in [
            "2-iw4mp.map",
            "2-iw4mp.pdb",
            "3-iw4sp_fast_server.exe",
            "3-iw4sp_fast_server.map",
            "3-iw4sp_fast_server.pdb",
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                lines[first + offset].contains(name),
                "missing packed 7-Zip row: {name}"
            );
        }
        let selected = lines
            .iter()
            .find(|line| line.contains("5-iw4mp_demo.pdb"))
            .expect("selected archive row");
        let name = selected.find("5-iw4mp_demo.pdb").expect("name column");
        let size = selected.find("19 901 440").expect("size column");
        let modified = selected.find("2009-07-13").expect("modified column");
        let crc = selected.find("63C05667").expect("CRC column");
        assert!(name < size && size < modified && modified < crc);
        assert!(!rendered.contains("1-iw4sp.pdb"));
        assert!(!rendered.contains("2-iw4mp.exe"));
    }

    #[test]
    fn total_commander_fixture_packs_file_rows_and_keeps_complete_menus() {
        let fixture: LayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/accessibility/total-commander/tree.json"
        ))
        .expect("parse Total Commander fixture");
        let rendered = plain_frame(&render_snapshot(
            &fixture.snapshot,
            fixture.cols,
            fixture.rows,
            12,
        ));
        let normalized = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains("Files Mark Commands Net Show Configuration Start"),
            "{rendered}"
        );
        let lines = rendered.lines().collect::<Vec<_>>();
        let parent = lines
            .iter()
            .position(|line| line.contains(".. <DIR>"))
            .expect("parent directory row");
        let cache = lines
            .iter()
            .position(|line| line.contains(".cache <DIR>"))
            .expect("cache row");
        let codex = lines
            .iter()
            .position(|line| line.contains(".codex <DIR>"))
            .expect("codex row");
        assert_eq!(cache, parent + 1);
        assert_eq!(codex, cache + 1);
        assert!(!lines[parent].contains('─'));
    }

    #[test]
    fn layout_fixture_round_trips_and_replays() {
        let fixture = LayoutFixture {
            schema_version: 1,
            cols: 100,
            rows: 43,
            snapshot: test_snapshot(vec![test_node(
                Role::Button,
                Rect {
                    x: 100,
                    y: 100,
                    width: 200,
                    height: 50,
                },
                "Run",
            )]),
        };
        let encoded = serde_json::to_vec(&fixture).expect("serialize layout fixture");
        let decoded: LayoutFixture =
            serde_json::from_slice(&encoded).expect("deserialize layout fixture");
        let Frame::Screen(grid) =
            render_snapshot(&decoded.snapshot, decoded.cols, decoded.rows, 12);
        assert!(locate_text(&grid, "Run").is_some());
    }

    #[test]
    fn accessibility_stream_defaults_target_a_1080p_viewer() {
        let options = AccessibilityOptions::default();
        assert_eq!((options.cols, options.rows), (200, 60));
        assert_eq!((options.max_depth, options.max_nodes), (40, 2_000));
    }

    #[test]
    fn spatial_renderer_preserves_relative_control_positions() {
        let snapshot = test_snapshot(vec![
            test_node(
                Role::StaticText,
                Rect {
                    x: 100,
                    y: 100,
                    width: 100,
                    height: 40,
                },
                "Left",
            ),
            test_node(
                Role::Button,
                Rect {
                    x: 700,
                    y: 350,
                    width: 200,
                    height: 80,
                },
                "Right",
            ),
        ]);
        let Frame::Screen(grid) = render_snapshot(&snapshot, 100, 43, 12);
        let left = locate_text(&grid, "Left").expect("left label should be rendered");
        let right = locate_text(&grid, "Right").expect("right button should be rendered");
        assert!(left.1 < right.1, "horizontal geometry should be preserved");
        assert!(left.0 < right.0, "vertical geometry should be preserved");
    }

    #[test]
    fn table_widths_borrow_slack_for_content_heavy_columns() {
        let widths = allocate_table_widths(&[12, 30, 29, 57], &[17, 14, 30, 1], 128);
        assert_eq!(widths.iter().sum::<usize>(), 128);
        assert!(widths[0] >= 17, "address column must fit a full address");
        assert!(widths[1] >= 14, "byte column must retain useful content");
        assert!(widths[2] >= 30, "instruction column must fit typical text");
    }

    #[test]
    fn spatial_renderer_reconstructs_table_cells_as_a_grid() {
        let snapshot = test_snapshot(vec![test_node_with_children(
            Role::Table,
            Rect {
                x: 100,
                y: 50,
                width: 800,
                height: 400,
            },
            "Results",
            vec![
                test_node(
                    Role::TableCell,
                    Rect {
                        x: 100,
                        y: 50,
                        width: 400,
                        height: 200,
                    },
                    "R0C0",
                ),
                test_node(
                    Role::TableCell,
                    Rect {
                        x: 500,
                        y: 50,
                        width: 400,
                        height: 200,
                    },
                    "R0C1",
                ),
                test_node(
                    Role::TableCell,
                    Rect {
                        x: 100,
                        y: 250,
                        width: 400,
                        height: 200,
                    },
                    "R1C0",
                ),
                test_node(
                    Role::TableCell,
                    Rect {
                        x: 500,
                        y: 250,
                        width: 400,
                        height: 200,
                    },
                    "R1C1",
                ),
            ],
        )]);
        let Frame::Screen(grid) = render_snapshot(&snapshot, 100, 43, 12);
        let r0c0 = locate_text(&grid, "R0C0").expect("first table cell");
        let r0c1 = locate_text(&grid, "R0C1").expect("second table cell");
        let r1c0 = locate_text(&grid, "R1C0").expect("third table cell");
        assert_eq!(r0c0.0, r0c1.0, "same table row should share a TUI row");
        assert!(r0c0.1 < r0c1.1, "table columns should retain their order");
        assert!(r0c0.0 < r1c0.0, "table rows should retain their order");
    }

    fn test_snapshot(children: Vec<SnapshotNode>) -> Snapshot {
        Snapshot {
            app_name: "Fixture".into(),
            pid: Some(1),
            window_name: "Fixture window".into(),
            root: test_node_with_children(
                Role::Window,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1_000,
                    height: 500,
                },
                "Fixture window",
                children,
            ),
            node_count: 5,
            truncated: false,
            capture_time: Duration::ZERO,
        }
    }

    fn test_node(role: Role, bounds: Rect, name: &str) -> SnapshotNode {
        test_node_with_children(role, bounds, name, Vec::new())
    }

    fn test_node_with_children(
        role: Role,
        bounds: Rect,
        name: &str,
        children: Vec<SnapshotNode>,
    ) -> SnapshotNode {
        SnapshotNode {
            role,
            bounds: Some(bounds),
            name: Some(name.into()),
            value: None,
            description: None,
            states: StateSet::default(),
            numeric_value: None,
            min_value: None,
            max_value: None,
            children,
        }
    }

    fn locate_text(grid: &Grid, needle: &str) -> Option<(usize, usize)> {
        grid.rows.iter().enumerate().find_map(|(row_index, row)| {
            let text = row
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>();
            text.find(needle).map(|column| (row_index, column))
        })
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
