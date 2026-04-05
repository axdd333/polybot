mod offline;
pub mod planner;
pub mod risk;
pub mod snapshot;
pub mod strategy;
pub mod tui;

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use polymarket_adapter::{build_executor, spawn_live_feeds};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use trading_core::config::{AppProfile, RunMode};
use trading_core::events::{NormalizedEvent, TimerCadence};
use trading_core::replay::EventRecorder;
use trading_core::snapshot::WorldSnapshot;

pub async fn run(profile: AppProfile) -> Result<()> {
    run_live(profile).await
}

pub async fn run_live(profile: AppProfile) -> Result<()> {
    ensure_tty()?;
    install_panic_hook();

    let (tx, mut rx) = mpsc::channel::<NormalizedEvent>(1024);
    let snapshot_cache = Arc::new(RwLock::new(WorldSnapshot::default()));
    let snapshot_writer = snapshot_cache.clone();
    let mut recorder = build_recorder(&profile)?;
    let executor = build_executor(&profile)?;
    let mut engine = strategy::build_engine(&profile, RunMode::Live, executor);
    let sweep = profile.sweep.clone();
    *snapshot_cache.write().await = snapshot::build_snapshot(&engine, &sweep);

    let processor = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            record_event(&mut recorder, &event)?;
            engine.apply_event(event);
            engine.refresh_dirty_markets().await;
            *snapshot_writer.write().await = snapshot::build_snapshot(&engine, &sweep);
        }
        flush_recorder(&mut recorder)?;
        Ok::<(), anyhow::Error>(())
    });

    let mut terminal = TerminalSession::enter()?;
    let live_handles = spawn_live_feeds(
        profile.adapter.clone(),
        Some(profile.execution.live.clone()),
        tx.clone(),
    );
    let timer_handle = tokio::spawn(live_timers(tx.clone()));
    let mut input_rx = spawn_input_listener();
    let render = render_loop(&mut terminal, snapshot_cache, &mut input_rx).await;

    for handle in live_handles {
        handle.abort();
    }
    timer_handle.abort();
    drop(tx);
    processor.await??;
    terminal.restore()?;
    render
}

pub async fn run_offline(profile: AppProfile, events_path: PathBuf, mode: RunMode) -> Result<()> {
    ensure_tty()?;
    install_panic_hook();
    offline::run(profile, events_path, mode).await
}

async fn render_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    snapshot_cache: Arc<RwLock<WorldSnapshot>>,
    input_rx: &mut mpsc::Receiver<UiCmd>,
) -> Result<()> {
    let mut timer = tokio::time::interval(std::time::Duration::from_millis(200));
    let mut ui = tui::UiState::default();
    let mut render_state = tui::RenderState::default();
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let snapshot = snapshot_cache.read().await.clone();
        terminal.draw(|frame| {
            render_state = tui::render(frame, &snapshot, &ui);
        })?;
        tokio::select! {
            maybe_cmd = input_rx.recv() => {
                let Some(cmd) = maybe_cmd else {
                    continue;
                };
                let Some(action) = ui.handle(cmd, &render_state) else {
                    continue;
                };
                if matches!(action, tui::UiAction::Quit) {
                    break;
                }
            }
            _ = timer.tick() => {}
        }
    }

    Ok(())
}

async fn live_timers(tx: mpsc::Sender<NormalizedEvent>) {
    let mut fast = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut slow = tokio::time::interval(std::time::Duration::from_secs(2));
    fast.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    slow.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = fast.tick() => {
                if send_timer(&tx, TimerCadence::Fast).await.is_err() {
                    return;
                }
            }
            _ = slow.tick() => {
                if send_timer(&tx, TimerCadence::Slow).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn send_timer(
    tx: &mpsc::Sender<NormalizedEvent>,
    cadence: TimerCadence,
) -> Result<(), mpsc::error::SendError<NormalizedEvent>> {
    tx.send(NormalizedEvent::TimerTick {
        cadence,
        ts: std::time::Instant::now(),
    })
    .await
}

fn build_recorder(profile: &AppProfile) -> Result<Option<EventRecorder>> {
    profile
        .record_path
        .as_ref()
        .map(EventRecorder::create)
        .transpose()
}

fn record_event(rec: &mut Option<EventRecorder>, event: &NormalizedEvent) -> Result<()> {
    if let Some(rec) = rec.as_mut() {
        rec.record(event)?;
    }
    Ok(())
}

fn flush_recorder(rec: &mut Option<EventRecorder>) -> Result<()> {
    if let Some(rec) = rec.as_mut() {
        rec.flush()?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiCmd {
    Quit,
    Prev,
    Next,
    Activate,
    PagePrev,
    PageNext,
    TogglePause,
    Faster,
    Slower,
    Step,
    MousePress(u16, u16),
}

pub(crate) fn spawn_input_listener() -> mpsc::Receiver<UiCmd> {
    let (tx, rx) = mpsc::channel::<UiCmd>(32);
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(CEvent::Key(key)) => {
                let Some(cmd) = key_cmd(key.code) else {
                    continue;
                };
                let is_quit = cmd == UiCmd::Quit;
                let _ = tx.blocking_send(cmd);
                if is_quit {
                    break;
                }
            }
            Ok(CEvent::Mouse(mouse)) => {
                if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
                    continue;
                }
                let _ = tx.blocking_send(UiCmd::MousePress(mouse.column, mouse.row));
            }
            _ => {}
        }
    });
    rx
}

fn key_cmd(code: KeyCode) -> Option<UiCmd> {
    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(UiCmd::Quit),
        KeyCode::Left | KeyCode::BackTab => Some(UiCmd::Prev),
        KeyCode::Right | KeyCode::Tab => Some(UiCmd::Next),
        KeyCode::Up => Some(UiCmd::PagePrev),
        KeyCode::Down => Some(UiCmd::PageNext),
        KeyCode::Enter => Some(UiCmd::Activate),
        KeyCode::Char(' ') => Some(UiCmd::TogglePause),
        KeyCode::Char('+') | KeyCode::Char('=') => Some(UiCmd::Faster),
        KeyCode::Char('-') | KeyCode::Char('_') => Some(UiCmd::Slower),
        KeyCode::Char('n') | KeyCode::Char('N') => Some(UiCmd::Step),
        _ => None,
    }
}

pub(crate) fn ensure_tty() -> Result<()> {
    if !crossterm::tty::IsTty::is_tty(&io::stdin()) {
        eprintln!("Run this directly in a terminal: cargo run");
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            crossterm::cursor::Show
        );
        eprintln!("panic: {info}");
    }));
}

pub(crate) struct TerminalSession {
    terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,
}

impl TerminalSession {
    pub(crate) fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            terminal: Some(terminal),
        })
    }

    pub(crate) fn restore(&mut self) -> Result<()> {
        if let Some(mut terminal) = self.terminal.take() {
            disable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture
            )?;
            terminal.show_cursor()?;
        }
        Ok(())
    }
}

impl std::ops::Deref for TerminalSession {
    type Target = Terminal<CrosstermBackend<io::Stdout>>;

    fn deref(&self) -> &Self::Target {
        self.terminal
            .as_ref()
            .expect("terminal session should be active")
    }
}

impl std::ops::DerefMut for TerminalSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.terminal
            .as_mut()
            .expect("terminal session should be active")
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
