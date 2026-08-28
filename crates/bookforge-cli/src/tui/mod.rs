//! Terminal dashboard built on [`ratatui`].
//!
//! The dashboard is a thin renderer over [`bookforge_core::RunState`]: it folds
//! [`ProgressEvent`]s into state and draws that state. The same widget tree
//! serves both the attached `translate --ui tui` mode (events arrive live over
//! a channel, see `progress::run_tui_attached`) and the detached `watch`
//! command (events are replayed/tailed from a JSONL log).

use std::time::Duration;

use anyhow::Result;
use bookforge_core::{ProgressEvent, RunState};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Gauge, List, ListItem, ListState, Paragraph},
};

use crate::presentation::{RunView, format_count, format_eta, format_rate, run_status_name};

/// One row in the job picker shown by `bookforge watch` with no job id.
pub struct JobPickerEntry {
    pub id: String,
    pub line: String,
}

/// Show an interactive list of jobs and return the selected id, or `None` if
/// the user cancelled (q/Esc/Ctrl-C). Blocks on input; restores the terminal
/// on every exit path.
pub fn pick_job(entries: Vec<JobPickerEntry>) -> Result<Option<String>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut terminal = ratatui::try_init()?;
    let result = run_picker(&mut terminal, &entries);
    let _ = ratatui::try_restore();
    result
}

fn run_picker(
    terminal: &mut DefaultTerminal,
    entries: &[JobPickerEntry],
) -> Result<Option<String>> {
    let mut selected: usize = 0;
    loop {
        terminal.draw(|frame| {
            let items: Vec<ListItem> = entries
                .iter()
                .map(|entry| ListItem::new(entry.line.clone()))
                .collect();
            let mut state = ListState::default();
            state.select(Some(selected));
            let list = List::new(items)
                .block(
                    Block::bordered()
                        .title(" Select a job to watch — ↑↓ move · Enter open · q cancel "),
                )
                .highlight_style(
                    Style::new()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
            frame.render_stateful_widget(list, frame.area(), &mut state);
        })?;

        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl_c {
                return Ok(None);
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if selected + 1 < entries.len() {
                        selected += 1;
                    }
                }
                KeyCode::Home | KeyCode::Char('g') => selected = 0,
                KeyCode::End | KeyCode::Char('G') => selected = entries.len() - 1,
                KeyCode::Enter => return Ok(Some(entries[selected].id.clone())),
                _ => {}
            }
        }
    }
}

/// How the dashboard was launched, which controls labels and quit semantics.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    /// Attached to a live `translate` run in this process. Quitting before the
    /// run finishes cancels it (see `run_tui_attached`).
    Attached,
    /// Following another process's job by tailing its event log. Quitting only
    /// stops watching; the translation keeps running.
    Watch,
}

impl TuiMode {
    fn label(self) -> &'static str {
        match self {
            TuiMode::Attached => "translate",
            TuiMode::Watch => "watch",
        }
    }
}

/// Scroll state for the event-log pane (UI-1).
///
/// While following, `offset` is re-pinned to the newest row on every frame,
/// so it must not be trusted as "the row the user is looking at". Leaving
/// follow mode with ↑ therefore starts from the *last rendered* bottom
/// position (`last_max_scroll`) and steps up exactly one line, instead of
/// falling back to a stale offset of 0 (which jumped to the oldest entry).
#[derive(Debug, Default)]
struct LogScroll {
    /// Scroll offset from the top, in lines.
    offset: u16,
    /// When true, the log pins to the newest entries.
    follow: bool,
    /// Largest legal offset seen on the previous render.
    last_max_scroll: u16,
}

impl LogScroll {
    fn new() -> Self {
        Self {
            offset: 0,
            follow: true,
            last_max_scroll: 0,
        }
    }

    fn key_up(&mut self) {
        if self.follow {
            self.offset = self.last_max_scroll.saturating_sub(1);
        } else {
            self.offset = self.offset.saturating_sub(1);
        }
        self.follow = false;
    }

    fn key_down(&mut self) {
        self.follow = false;
        self.offset = self.offset.saturating_add(1);
    }

    fn page_up(&mut self) {
        if self.follow {
            self.offset = self.last_max_scroll.saturating_sub(10);
        } else {
            self.offset = self.offset.saturating_sub(10);
        }
        self.follow = false;
    }

    fn page_down(&mut self) {
        self.follow = false;
        self.offset = self.offset.saturating_add(10);
    }

    fn top(&mut self) {
        self.follow = false;
        self.offset = 0;
    }

    fn bottom(&mut self) {
        self.follow = true;
    }

    /// Re-pin/clamp the offset for this frame. Returns `(offset, max_scroll)`.
    fn pre_render(&mut self, total_lines: u16, inner_height: u16) -> (u16, u16) {
        let max_scroll = total_lines.saturating_sub(inner_height);
        self.last_max_scroll = max_scroll;
        if self.follow || self.offset > max_scroll {
            self.offset = max_scroll;
        }
        (self.offset, max_scroll)
    }
}

/// A user-requested action the host loop should perform. Kept store-agnostic so
/// the TUI does not depend on the persistence layer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TuiAction {
    /// Mark this job's failed + needs-review segments for retry.
    RetryFlagged,
    PauseJob,
    ResumeJob,
    StopJob,
}

/// Owns the terminal and the folded run view; renders the dashboard.
pub struct TuiApp {
    terminal: DefaultTerminal,
    mode: TuiMode,
    /// Canonical presentation view (UI-31): the one RunState+EpochTracker
    /// pairing shared with the bars, `tail`, and serve folds.
    pub view: RunView,
    /// Event-log pane scroll/follow state (UI-1).
    log_scroll: LogScroll,
    quit: bool,
    /// Actions requested by the user, drained by the host loop.
    actions: Vec<TuiAction>,
    /// Transient one-line status shown in the footer (e.g. "marked 3 for retry").
    status_message: Option<String>,
}

/// Static metadata shown by the attached audiobook dashboard.
pub struct AudioTuiInfo {
    pub title: String,
    pub input: String,
    pub output: String,
    pub provider: String,
    pub model: String,
    pub voice: String,
    pub cost_line: Option<String>,
    pub chapters_total: usize,
    pub total: usize,
}

/// Attached terminal dashboard for audiobook synthesis. The durable source of
/// truth remains `manifest.json`; this renderer consumes the same per-chunk
/// notifications that update that checkpoint.
pub struct AudioTuiApp {
    terminal: DefaultTerminal,
    info: AudioTuiInfo,
    done: usize,
    cached: usize,
    current_chapter: String,
    status: String,
    quit: bool,
}

impl AudioTuiApp {
    pub fn new(info: AudioTuiInfo) -> Result<Self> {
        Ok(Self {
            terminal: ratatui::try_init()?,
            info,
            done: 0,
            cached: 0,
            current_chapter: "Planning audio".to_string(),
            status: "running".to_string(),
            quit: false,
        })
    }

    pub fn update(&mut self, progress: &bookforge_audio::Progress) {
        self.done = progress.done;
        self.current_chapter = progress.chapter_title.clone();
        if progress.skipped {
            self.cached += 1;
        }
    }

    pub fn finish(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    pub fn pump_input(&mut self) -> Result<bool> {
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    self.quit = true;
                }
            }
        }
        Ok(self.quit)
    }

    pub fn draw(&mut self) -> Result<()> {
        let Self {
            terminal,
            info,
            done,
            cached,
            current_chapter,
            status,
            ..
        } = self;
        terminal.draw(|frame| {
            render_audio_dashboard(frame, info, *done, *cached, current_chapter, status);
        })?;
        Ok(())
    }

    pub fn restore(&mut self) -> Result<()> {
        ratatui::try_restore()?;
        Ok(())
    }
}

impl Drop for AudioTuiApp {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
    }
}

fn render_audio_dashboard(
    frame: &mut Frame,
    info: &AudioTuiInfo,
    done: usize,
    cached: usize,
    current_chapter: &str,
    status: &str,
) {
    let areas = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .split(frame.area());
    let header = Text::from(vec![
        Line::from(vec![
            Span::styled(
                "BookForge Audiobook",
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {}", info.title)),
        ]),
        Line::from(format!("{} → {}", info.input, info.output)),
        Line::from(format!(
            "{} / {} · voice {}",
            info.provider, info.model, info.voice
        )),
    ]);
    frame.render_widget(Paragraph::new(header).block(Block::bordered()), areas[0]);

    let ratio = if info.total == 0 {
        0.0
    } else {
        (done as f64 / info.total as f64).clamp(0.0, 1.0)
    };
    frame.render_widget(
        Gauge::default()
            .block(Block::bordered().title(" Synthesis "))
            .gauge_style(Style::new().fg(Color::Cyan))
            .ratio(ratio)
            .label(format!("{done}/{}", info.total)),
        areas[1],
    );

    let synthesized = done.saturating_sub(cached);
    let mut detail_lines = vec![
        Line::from(format!("Current: {current_chapter}")),
        Line::from(format!("Resolved model: {}", info.model)),
        Line::from(format!(
            "Synthesized: {synthesized}   Cached: {cached}   Remaining: {}",
            info.total.saturating_sub(done)
        )),
        Line::from(format!("Chapters: {} total", info.chapters_total)),
        Line::from(format!("Status: {status}")),
        Line::from("Progress is checkpointed to manifest.json after every chunk."),
    ];
    if let Some(cost_line) = &info.cost_line {
        detail_lines.insert(2, Line::from(cost_line.clone()));
    }
    let details = Text::from(detail_lines);
    frame.render_widget(Paragraph::new(details).block(Block::bordered()), areas[2]);
    frame.render_widget(
        Paragraph::new(" q/Esc cancel and exit ").style(Style::new().fg(Color::DarkGray)),
        areas[3],
    );
}

impl TuiApp {
    /// Enter the alternate screen + raw mode and create the dashboard.
    pub fn new(mode: TuiMode) -> Result<Self> {
        let terminal = ratatui::try_init()?;
        Ok(Self {
            terminal,
            mode,
            view: RunView::new(),
            log_scroll: LogScroll::new(),
            quit: false,
            actions: Vec::new(),
            status_message: None,
        })
    }

    /// Restore the terminal to its normal state. Safe to call more than once.
    pub fn restore(&mut self) -> Result<()> {
        ratatui::try_restore()?;
        Ok(())
    }

    /// Fold one event into the displayed state.
    pub fn fold(&mut self, event: &ProgressEvent) {
        self.view.fold(event);
    }

    /// Drain and handle all currently-pending key input without blocking.
    /// Returns true if the user has asked to quit.
    pub fn pump_input(&mut self) -> Result<bool> {
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                self.on_key(key);
            }
        }
        Ok(self.quit)
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl_c {
            self.quit = true;
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.log_scroll.key_up(),
            KeyCode::Down | KeyCode::Char('j') => self.log_scroll.key_down(),
            KeyCode::PageUp => self.log_scroll.page_up(),
            KeyCode::PageDown => self.log_scroll.page_down(),
            KeyCode::Home | KeyCode::Char('g') => self.log_scroll.top(),
            KeyCode::End | KeyCode::Char('G') => self.log_scroll.bottom(),
            // Retry is only meaningful when watching a persisted job, not while
            // attached to a run that is still producing segments.
            KeyCode::Char('r') if self.mode == TuiMode::Watch => {
                self.actions.push(TuiAction::RetryFlagged)
            }
            KeyCode::Char('p') if self.mode == TuiMode::Watch => {
                self.actions.push(TuiAction::PauseJob)
            }
            KeyCode::Char('u') if self.mode == TuiMode::Watch => {
                self.actions.push(TuiAction::ResumeJob)
            }
            KeyCode::Char('s') if self.mode == TuiMode::Watch => {
                self.actions.push(TuiAction::StopJob)
            }
            _ => {}
        }
    }

    /// Remove and return any pending user actions for the host loop to perform.
    pub fn take_actions(&mut self) -> Vec<TuiAction> {
        std::mem::take(&mut self.actions)
    }

    /// Set the transient footer status line (e.g. the result of a retry).
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
    }

    /// Redraw the dashboard from the current state.
    pub fn draw(&mut self) -> Result<()> {
        // Destructure so the draw closure borrows the data fields while
        // `terminal` is borrowed mutably (disjoint borrows).
        let Self {
            terminal,
            mode,
            view,
            log_scroll,
            status_message,
            ..
        } = self;
        let mode = *mode;
        let status = status_message.as_deref();
        // Rate/ETA come from the canonical epoch-aware view so resumed runs do
        // not inherit timing baselines from earlier epochs (UI-9/31).
        let per_min = view.segments_per_minute();
        let eta = view.eta_secs();
        let ratio = view.progress_ratio();
        terminal.draw(|frame| {
            render_dashboard(
                frame,
                &view.state,
                mode,
                per_min,
                eta,
                ratio,
                log_scroll,
                status,
            )
        })?;
        Ok(())
    }
}

fn render_dashboard(
    frame: &mut Frame,
    state: &RunState,
    mode: TuiMode,
    segments_per_minute: f64,
    eta_secs: f64,
    ratio: f64,
    scroll: &mut LogScroll,
    status: Option<&str>,
) {
    let chunks = Layout::vertical([
        Constraint::Length(5), // header
        Constraint::Length(3), // progress gauge
        Constraint::Length(4), // stats
        Constraint::Min(3),    // event log
        Constraint::Length(1), // footer
    ])
    .split(frame.area());

    render_header(frame, chunks[0], state, mode);
    // Gauge ratio comes from the canonical epoch-aware view (UI-31).
    render_gauge(frame, chunks[1], state, ratio);
    render_stats(frame, chunks[2], state, segments_per_minute, eta_secs);
    render_log(frame, chunks[3], state, scroll);
    render_footer(frame, chunks[4], state, mode, status);
}

fn render_header(frame: &mut Frame, area: ratatui::layout::Rect, state: &RunState, mode: TuiMode) {
    // Status naming comes from the shared presentation vocabulary (UI-31).
    let status = run_status_name(state);
    let job = state.job_id.as_deref().unwrap_or("(no job yet)");
    let model = match (state.provider.as_deref(), state.model.as_deref()) {
        (Some(p), Some(m)) => format!("{p} / {m}"),
        (Some(p), None) => p.to_string(),
        _ => "—".to_string(),
    };
    let paths = match (state.input_path.as_deref(), state.output_path.as_deref()) {
        (Some(i), Some(o)) => format!("{i}  →  {o}"),
        _ => String::new(),
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                job,
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   status: "),
            Span::styled(status, status_style(state)),
        ]),
        Line::from(format!(
            "{model}   ·   concurrency {}",
            state.target_concurrency
        )),
        Line::from(paths),
    ];
    let block = Block::bordered().title(format!(" BookForge — {} ", mode.label()));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_gauge(frame: &mut Frame, area: ratatui::layout::Rect, state: &RunState, ratio: f64) {
    let stage = state.stage.as_deref().unwrap_or("…");
    let label = format!(
        "{}/{} ({} cached)",
        state.done_segments, state.total_segments, state.cached
    );
    let gauge = Gauge::default()
        .block(Block::bordered().title(format!(" Segments — {stage} ")))
        .gauge_style(Style::new().fg(Color::Cyan).bg(Color::Black))
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, area);
}

fn render_stats(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    state: &RunState,
    segments_per_minute: f64,
    eta_secs: f64,
) {
    let counts = Line::from(vec![
        Span::styled(
            format!("✓ {} ok", state.succeeded),
            Style::new().fg(Color::Green),
        ),
        Span::raw("   "),
        Span::styled(
            format!("⚠ {} review", state.needs_review),
            Style::new().fg(Color::Yellow),
        ),
        Span::raw("   "),
        Span::styled(
            format!("✗ {} failed", state.failed),
            Style::new().fg(Color::Red),
        ),
        Span::raw("   "),
        Span::raw(format!(
            "{}/{} active",
            state.active_requests, state.target_concurrency
        )),
    ]);
    let eta = if state.finished {
        "—".to_string()
    } else {
        format_eta(eta_secs)
    };
    let rates = Line::from(format!(
        "{} · ETA {} · tokens {} in / {} out · flushed {}",
        format_rate(segments_per_minute),
        eta,
        format_count(state.input_tokens),
        format_count(state.output_tokens),
        state.checkpoint_flushed,
    ));
    frame.render_widget(
        Paragraph::new(vec![counts, rates]).block(Block::bordered().title(" Stats ")),
        area,
    );
}

fn render_log(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    state: &RunState,
    scroll: &mut LogScroll,
) {
    // Events that have no dedicated log line render as empty strings; skip
    // them so the visible log (and its scroll math) is not polluted with
    // blank rows.
    let lines: Vec<Line> = state
        .recent_events
        .iter()
        .map(format_event_line)
        .filter(|line| !line.spans.iter().all(|span| span.content.is_empty()))
        .collect();
    let total = lines.len() as u16;
    // Inner height excludes the top/bottom border rows.
    let inner_h = area.height.saturating_sub(2);
    let (offset, max_scroll) = scroll.pre_render(total, inner_h);
    let title = if scroll.follow {
        " Events (following) ".to_string()
    } else {
        format!(" Events ({}/{}) ", offset.min(max_scroll), max_scroll)
    };
    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title(title))
        .scroll((offset, 0));
    frame.render_widget(paragraph, area);
}

fn render_footer(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    state: &RunState,
    mode: TuiMode,
    status: Option<&str>,
) {
    // A transient status (e.g. retry result) takes precedence over the key hints.
    if let Some(status) = status {
        frame.render_widget(
            Paragraph::new(format!(" {status}")).style(Style::new().fg(Color::Cyan)),
            area,
        );
        return;
    }
    let keys = "↑↓ scroll · g/G top/bottom";
    let text = match (mode, state.finished) {
        (TuiMode::Attached, true) => {
            let review = state
                .job_id
                .as_deref()
                .map(|j| format!(" · review: bookforge review {j}"))
                .unwrap_or_default();
            format!(" finished · q to exit{review}")
        }
        // Quitting cancels the run: the token is shared with the worker for
        // both `translate --ui tui` and `resume --ui tui` (UI-2). The run
        // stops at its next safe boundary and progress stays checkpointed.
        (TuiMode::Attached, false) => format!(" q/Ctrl-C cancel run & quit · {keys}"),
        (TuiMode::Watch, _) => format!(" q quit · p pause · u resume · s stop · r retry · {keys}"),
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::new().fg(Color::DarkGray)),
        area,
    );
}

fn status_style(state: &RunState) -> Style {
    if state.finished {
        if state.failed > 0 {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Green)
        }
    } else {
        Style::new().fg(Color::Yellow)
    }
}

fn format_event_line(event: &ProgressEvent) -> Line<'static> {
    let (text, style) = match event {
        ProgressEvent::JobCreated { job_id, .. } => (
            format!("job created: {job_id}"),
            Style::new().fg(Color::Cyan),
        ),
        ProgressEvent::StageStarted { stage, .. } => {
            (format!("▶ stage: {stage}"), Style::new().fg(Color::Blue))
        }
        ProgressEvent::StageFinished { stage, .. } => (
            format!("■ stage done: {stage}"),
            Style::new().fg(Color::Blue),
        ),
        ProgressEvent::SegmentationFinished { segment_count, .. } => (
            format!("segmented into {segment_count} segments"),
            Style::new().fg(Color::Gray),
        ),
        ProgressEvent::CacheScanFinished { hits, misses, .. } => (
            format!("cache scan: {hits} hits, {misses} misses"),
            Style::new().fg(Color::Gray),
        ),
        ProgressEvent::JobPaused { .. } => ("paused".to_string(), Style::new().fg(Color::Yellow)),
        ProgressEvent::JobResumed { .. } => ("resumed".to_string(), Style::new().fg(Color::Green)),
        ProgressEvent::BatchQueued {
            batch_id,
            item_count,
            ..
        } => (
            format!("batch {batch_id} queued ({item_count} items)"),
            Style::new().fg(Color::Gray),
        ),
        ProgressEvent::SegmentFinished {
            segment_id, status, ..
        } => {
            let style = match status.as_str() {
                "failed" => Style::new().fg(Color::Red),
                "needs_review" => Style::new().fg(Color::Yellow),
                _ => Style::new().fg(Color::Green),
            };
            (format!("· segment {segment_id}: {status}"), style)
        }
        ProgressEvent::RequestFinished { status, .. } if status != "ok" => (
            format!("← request {status}"),
            Style::new().fg(Color::Yellow),
        ),
        ProgressEvent::CheckpointFlushed { flushed_count, .. } => (
            format!("checkpoint flushed ({flushed_count})"),
            Style::new().fg(Color::Gray),
        ),
        ProgressEvent::ConcurrencyChanged {
            current, reason, ..
        } => (
            format!("concurrency → {current} ({reason})"),
            Style::new().fg(Color::Gray),
        ),
        ProgressEvent::RuntimeConfigChanged {
            revision,
            changed_fields,
            application,
            ..
        } => (
            format!(
                "runtime r{revision}: {} → {}",
                changed_fields.join(", "),
                application.join(", ")
            ),
            Style::new().fg(Color::Cyan),
        ),
        ProgressEvent::RuntimeConfigRejected {
            revision, message, ..
        } => (
            format!(
                "runtime{} rejected: {message}",
                revision.map_or_else(String::new, |value| format!(" r{value}"))
            ),
            Style::new().fg(Color::Red),
        ),
        ProgressEvent::Warning { message, .. } => {
            (format!("[warn] {message}"), Style::new().fg(Color::Yellow))
        }
        ProgressEvent::Error { message, .. } => {
            (format!("[error] {message}"), Style::new().fg(Color::Red))
        }
        ProgressEvent::TranslationFinished {
            succeeded,
            needs_review,
            failed,
            ..
        } => (
            format!("✓ finished: {succeeded} ok, {needs_review} review, {failed} failed"),
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        ProgressEvent::DroppedEvents { count, .. } => (
            format!("[dropped] {count} event(s) lost to queue overflow"),
            Style::new().fg(Color::DarkGray),
        ),
        // Quieter / high-frequency events are not worth a log line each.
        _ => return Line::from(Span::raw("")),
    };
    Line::from(Span::styled(text, style))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_renders_key_fields_into_buffer() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut state = RunState::default();
        state.fold(&ProgressEvent::JobCreated {
            job_id: "job_demo".into(),
            input_path: "a.epub".into(),
            output_path: "b.epub".into(),
            timestamp_ms: 1_000,
        });
        state.fold(&ProgressEvent::SegmentationFinished {
            segment_count: 10,
            timestamp_ms: 1_100,
        });
        for i in 0..3u64 {
            state.fold(&ProgressEvent::SegmentFinished {
                segment_id: format!("s{i}"),
                status: "succeeded".into(),
                input_tokens: None,
                output_tokens: None,
                timestamp_ms: 2_000 + i * 1_000,
            });
        }

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let mut scroll = LogScroll::new();
        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    &state,
                    TuiMode::Watch,
                    0.0,
                    0.0,
                    0.0,
                    &mut scroll,
                    None,
                );
            })
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("BookForge"), "header title missing: {text}");
        assert!(text.contains("Segments"), "gauge title missing");
        assert!(text.contains("3 ok"), "succeeded count missing");
        assert!(text.contains("3/10"), "gauge label missing");
        assert!(text.contains("q quit"), "watch footer missing");
    }

    /// UI-2: the attached footer must tell the truth — quitting cancels the
    /// run (the token is shared with the worker for translate and resume).
    #[test]
    fn attached_footer_promises_cancel_and_quit_truthfully() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut state = RunState::default();
        state.fold(&ProgressEvent::JobCreated {
            job_id: "job_footer".into(),
            input_path: "a.epub".into(),
            output_path: "b.epub".into(),
            timestamp_ms: 1,
        });

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let mut scroll = LogScroll::new();
        // An unfinished attached run shows the cancel-and-quit hint...
        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    &state,
                    TuiMode::Attached,
                    0.0,
                    0.0,
                    0.0,
                    &mut scroll,
                    None,
                );
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("cancel run & quit"),
            "attached footer must describe cancel semantics: {text}"
        );
        assert!(
            !text.contains("abort"),
            "old misleading wording must be gone"
        );

        // ...and a finished one only offers plain exit.
        state.fold(&ProgressEvent::TranslationFinished {
            succeeded: 1,
            cached: 0,
            needs_review: 0,
            failed: 0,
            input_tokens: 0,
            output_tokens: 0,
            elapsed_ms: 1,
            timestamp_ms: 2,
        });
        let mut scroll = LogScroll::new();
        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    &state,
                    TuiMode::Attached,
                    0.0,
                    0.0,
                    0.0,
                    &mut scroll,
                    None,
                );
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("finished · q to exit"));
        assert!(!text.contains("cancel run"));
    }

    /// UI-1: leaving follow mode with ↑ must step up exactly one line from
    /// the bottom, not jump to the oldest entry via a stale offset.
    #[test]
    fn arrow_up_from_follow_moves_up_one_row() {
        // Simulate a log whose window bottom is at offset 40.
        let mut scroll = LogScroll::new();
        let total_lines = 60u16;
        let inner_height = 20u16;
        let (offset, max_scroll) = scroll.pre_render(total_lines, inner_height);
        assert_eq!(
            (offset, max_scroll),
            (40, 40),
            "follow pins to the newest row"
        );

        scroll.key_up();
        let (offset, _) = scroll.pre_render(total_lines, inner_height);
        assert_eq!(offset, 39, "↑ must move up exactly one line");
        assert!(!scroll.follow);

        scroll.key_down();
        let (offset, _) = scroll.pre_render(total_lines, inner_height);
        assert_eq!(offset, 40, "↓ returns to the bottom; follow stays off");

        // G/End re-pins to follow; ↑ afterwards still steps up by one.
        scroll.bottom();
        scroll.key_up();
        let (offset, _) = scroll.pre_render(total_lines, inner_height);
        assert_eq!(offset, 39);
    }

    #[test]
    fn event_log_rendering_skips_blank_rows_from_unhandled_events() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut state = RunState::default();
        // RequestStarted has no dedicated log line and previously rendered as
        // an empty row.
        state.fold(&ProgressEvent::RequestStarted {
            request_id: "r1".into(),
            batch_id: None,
            segment_id: None,
            provider: None,
            model: None,
            prompt_template: None,
            items: 1,
            estimated_input_tokens: 0,
            max_output_tokens: None,
            active_requests: 1,
            target_concurrency: 2,
            runtime_config_revision: None,
            provider_max_attempts: None,
            effective_timeout_seconds: None,
            timestamp_ms: 1,
        });
        state.fold(&ProgressEvent::Warning {
            kind: "test".into(),
            message: "visible".into(),
            timestamp_ms: 2,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let mut scroll = LogScroll::new();
        terminal
            .draw(|frame| {
                render_dashboard(
                    frame,
                    &state,
                    TuiMode::Watch,
                    0.0,
                    0.0,
                    0.0,
                    &mut scroll,
                    None,
                );
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("[warn] visible"));

        // DroppedEvents now renders as an honest marker line (UI-10).
        let line = format_event_line(&ProgressEvent::DroppedEvents {
            count: 5,
            timestamp_ms: 3,
        });
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("[dropped] 5 event(s) lost"));
    }

    #[test]
    fn audiobook_dashboard_renders_progress_and_provider_details() {
        use ratatui::{Terminal, backend::TestBackend};

        let info = AudioTuiInfo {
            title: "Source Book".to_string(),
            input: "source.epub".to_string(),
            output: "source.audiobook".to_string(),
            provider: "gemini".to_string(),
            model: "gemini-tts".to_string(),
            voice: "Kore".to_string(),
            cost_line: Some("Estimated cost: ~$1.23".to_string()),
            chapters_total: 4,
            total: 12,
        };
        let mut terminal = Terminal::new(TestBackend::new(90, 18)).unwrap();
        terminal
            .draw(|frame| render_audio_dashboard(frame, &info, 5, 2, "Chapter 3", "running"))
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("BookForge Audiobook"));
        assert!(text.contains("gemini / gemini-tts · voice Kore"));
        assert!(text.contains("5/12"));
        assert!(text.contains("Synthesized: 3"));
        assert!(text.contains("Cached: 2"));
        assert!(text.contains("Resolved model: gemini-tts"));
        assert!(text.contains("Estimated cost: ~$1.23"));
        assert!(text.contains("Chapters: 4 total"));
        assert!(text.contains("q/Esc cancel"));
    }

    #[test]
    fn finished_event_renders_a_summary_line() {
        let line = format_event_line(&ProgressEvent::TranslationFinished {
            succeeded: 3,
            cached: 1,
            needs_review: 2,
            failed: 0,
            input_tokens: 0,
            output_tokens: 0,
            elapsed_ms: 0,
            timestamp_ms: 0,
        });
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("3 ok"));
        assert!(rendered.contains("2 review"));
    }

    #[test]
    fn runtime_change_event_renders_revision_fields_and_boundary() {
        let line = format_event_line(&ProgressEvent::RuntimeConfigChanged {
            revision: 4,
            changed_fields: vec!["concurrency".to_string(), "qa".to_string()],
            application: vec!["next_request".to_string(), "next_stage".to_string()],
            timestamp_ms: 0,
        });
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.contains("runtime r4"));
        assert!(rendered.contains("concurrency, qa"));
        assert!(rendered.contains("next_request, next_stage"));
    }
}
