//! Terminal dashboard built on [`ratatui`].
//!
//! The dashboard is a thin renderer over [`bookforge_core::RunState`]: it folds
//! [`ProgressEvent`]s into state and draws that state. The same widget tree
//! serves both the attached `translate --ui tui` mode (events arrive live over
//! a channel, see `progress::run_tui_attached`) and the detached `watch`
//! command (events are replayed/tailed from a JSONL log).

use std::time::Duration;

use anyhow::Result;
use bookforge_core::{IssueLevel, ProgressEvent, RunState};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Gauge, List, ListItem, ListState, Paragraph},
};

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

/// Owns the terminal and the folded [`RunState`]; renders the dashboard.
pub struct TuiApp {
    terminal: DefaultTerminal,
    mode: TuiMode,
    pub state: RunState,
    /// Log scroll offset from the top, in lines.
    scroll: u16,
    /// When true, the log pins to the newest entries.
    follow: bool,
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
    let details = Text::from(vec![
        Line::from(format!("Current: {current_chapter}")),
        Line::from(format!(
            "Synthesized: {synthesized}   Cached: {cached}   Remaining: {}",
            info.total.saturating_sub(done)
        )),
        Line::from(format!("Status: {status}")),
        Line::from("Progress is checkpointed to manifest.json after every chunk."),
    ]);
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
            state: RunState::default(),
            scroll: 0,
            follow: true,
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
        self.state.fold(event);
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
            KeyCode::Up | KeyCode::Char('k') => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.follow = false;
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.follow = false;
                self.scroll = self.scroll.saturating_add(10);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.follow = false;
                self.scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => self.follow = true,
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
            state,
            scroll,
            follow,
            status_message,
            ..
        } = self;
        let mode = *mode;
        let follow = *follow;
        let scroll_ref = scroll;
        let status = status_message.as_deref();
        terminal.draw(|frame| render_dashboard(frame, state, mode, scroll_ref, follow, status))?;
        Ok(())
    }
}

fn render_dashboard(
    frame: &mut Frame,
    state: &RunState,
    mode: TuiMode,
    scroll: &mut u16,
    follow: bool,
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
    render_gauge(frame, chunks[1], state);
    render_stats(frame, chunks[2], state);
    render_log(frame, chunks[3], state, scroll, follow);
    render_footer(frame, chunks[4], state, mode, status);
}

fn render_header(frame: &mut Frame, area: ratatui::layout::Rect, state: &RunState, mode: TuiMode) {
    let status = if state.finished {
        "done"
    } else if state.paused {
        "paused"
    } else if state.total_segments > 0 {
        "running"
    } else {
        "starting"
    };
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

fn render_gauge(frame: &mut Frame, area: ratatui::layout::Rect, state: &RunState) {
    let ratio = if state.total_segments > 0 {
        (state.done_segments as f64 / state.total_segments as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
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

fn render_stats(frame: &mut Frame, area: ratatui::layout::Rect, state: &RunState) {
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
        format_duration(state.eta_secs())
    };
    let rates = Line::from(format!(
        "{:.1} seg/min · ETA {} · tokens {} in / {} out · flushed {}",
        state.segments_per_minute(),
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
    scroll: &mut u16,
    follow: bool,
) {
    let lines: Vec<Line> = state.recent_events.iter().map(format_event_line).collect();
    let total = lines.len() as u16;
    // Inner height excludes the top/bottom border rows.
    let inner_h = area.height.saturating_sub(2);
    let max_scroll = total.saturating_sub(inner_h);
    // Pin to the newest line when following, and never scroll past the end.
    if follow || *scroll > max_scroll {
        *scroll = max_scroll;
    }
    let title = if follow {
        " Events (following) ".to_string()
    } else {
        format!(" Events ({}/{}) ", (*scroll).min(max_scroll), max_scroll)
    };
    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title(title))
        .scroll((*scroll, 0));
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
        (TuiMode::Attached, false) => format!(" q/Ctrl-C abort & quit · {keys}"),
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
        // Quieter / high-frequency events are not worth a log line each.
        _ => return Line::from(Span::raw("")),
    };
    Line::from(Span::styled(text, style))
}

/// Issue level → display colour, kept for any future issues panel.
#[allow(dead_code)]
fn issue_style(level: IssueLevel) -> Style {
    match level {
        IssueLevel::Warning => Style::new().fg(Color::Yellow),
        IssueLevel::Error => Style::new().fg(Color::Red),
    }
}

fn format_duration(secs: f64) -> String {
    if secs <= 0.0 {
        return "—".to_string();
    }
    if secs > 3600.0 {
        format!("{:.1}h", secs / 3600.0)
    } else if secs > 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{secs:.0}s")
    }
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_formatting_is_compact() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(12_345), "12.3k");
        assert_eq!(format_count(2_000_000), "2.0M");
    }

    #[test]
    fn duration_formatting_buckets_by_scale() {
        assert_eq!(format_duration(0.0), "—");
        assert_eq!(format_duration(45.0), "45s");
        assert_eq!(format_duration(150.0), "2m");
        assert_eq!(format_duration(7200.0), "2.0h");
    }

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
        let mut scroll = 0u16;
        terminal
            .draw(|frame| {
                render_dashboard(frame, &state, TuiMode::Watch, &mut scroll, true, None);
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
