pub mod tui;

use anyhow::Result;
use crossterm::{
    event::{self, Event as CEvent, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use polymarket_adapter::{build_executor, spawn_live_feeds};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::sync::Arc;
use strategy_sweep::build_engine;
use tokio::sync::{mpsc, RwLock};
use trading_core::config::{AppProfile, RunMode};
use trading_core::events::{NormalizedEvent, TimerCadence};
use trading_core::replay::EventRecorder;
use trading_core::snapshot::WorldSnapshot;

pub async fn run(profile: AppProfile) -> Result<()> {
    ensure_tty()?;
    install_panic_hook();

    let (tx, mut rx) = mpsc::channel::<NormalizedEvent>(1024);
    let snapshot_cache = Arc::new(RwLock::new(WorldSnapshot::default()));
    let snapshot_writer = snapshot_cache.clone();
    let mut recorder = match profile.record_path.as_ref() {
        Some(path) => Some(EventRecorder::create(path)?),
        None => None,
    };

    let executor = build_executor(&profile)?;
    let mut engine = build_engine(&profile, RunMode::Live, executor);
    *snapshot_cache.write().await = engine.snapshot();

    let processor = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Some(recorder) = recorder.as_mut() {
                recorder.record(&event)?;
            }
            engine.apply_event(event);
            engine.refresh_dirty_markets().await;
            *snapshot_writer.write().await = engine.snapshot();
        }
        if let Some(recorder) = recorder.as_mut() {
            recorder.flush()?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let mut terminal = TerminalSession::enter()?;
    let live_handles = spawn_live_feeds(
        profile.adapter.clone(),
        Some(profile.execution.live.clone()),
        tx.clone(),
    );
    let timer_handle = tokio::spawn(live_timers(tx.clone()));
    let mut quit_rx = spawn_quit_listener();
    let render_result = render_loop(&mut terminal, snapshot_cache, &mut quit_rx).await;

    for handle in live_handles {
        handle.abort();
    }
    timer_handle.abort();
    drop(tx);
    processor.await??;
    terminal.restore()?;
    render_result
}

async fn render_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    snapshot_cache: Arc<RwLock<WorldSnapshot>>,
    quit_rx: &mut mpsc::Receiver<()>,
) -> Result<()> {
    let mut render_timer = tokio::time::interval(std::time::Duration::from_millis(200));
    render_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let snapshot = snapshot_cache.read().await.clone();
        terminal.draw(|frame| tui::render(frame, &snapshot))?;

        tokio::select! {
            _ = quit_rx.recv() => break,
            _ = render_timer.tick() => {}
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
                if tx.send(NormalizedEvent::TimerTick {
                    cadence: TimerCadence::Fast,
                    ts: std::time::Instant::now(),
                }).await.is_err() {
                    return;
                }
            }
            _ = slow.tick() => {
                if tx.send(NormalizedEvent::TimerTick {
                    cadence: TimerCadence::Slow,
                    ts: std::time::Instant::now(),
                }).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn spawn_quit_listener() -> mpsc::Receiver<()> {
    let (quit_tx, quit_rx) = mpsc::channel::<()>(1);
    std::thread::spawn(move || loop {
        if let Ok(CEvent::Key(key)) = event::read() {
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
                let _ = quit_tx.blocking_send(());
                break;
            }
        }
    });
    quit_rx
}

fn ensure_tty() -> Result<()> {
    if !crossterm::tty::IsTty::is_tty(&io::stdin()) {
        eprintln!("Run this directly in a terminal: cargo run");
        std::process::exit(1);
    }

    Ok(())
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        eprintln!("panic: {info}");
    }));
}

struct TerminalSession {
    terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            terminal: Some(terminal),
        })
    }

    fn restore(&mut self) -> Result<()> {
        if let Some(mut terminal) = self.terminal.take() {
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
