use self::runtime::BotRuntime;
use crate::domain::trading::snapshot::WorldSnapshot;
use crate::presentation::tui;
use anyhow::Result;
use crossterm::{
    event::{self, Event as CEvent, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::env;
use std::fs;
use std::io;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

pub async fn run() -> Result<()> {
    load_env_file(".env.live")?;
    enforce_paper_mode()?;
    ensure_tty()?;
    install_panic_hook();

    let mut terminal = TerminalSession::enter()?;
    let runtime = BotRuntime::spawn();
    let mut quit_rx = spawn_quit_listener();
    let render_result = render_loop(&mut terminal, runtime.snapshot_cache(), &mut quit_rx).await;

    runtime.shutdown().await;
    terminal.restore()?;
    render_result
}

async fn render_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    snapshot_cache: std::sync::Arc<tokio::sync::RwLock<WorldSnapshot>>,
    quit_rx: &mut mpsc::Receiver<()>,
) -> Result<()> {
    let mut render_timer = tokio::time::interval(std::time::Duration::from_millis(200));
    render_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

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

fn load_env_file(path: &str) -> Result<()> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        env::set_var(key.trim(), value.trim());
    }

    Ok(())
}

fn enforce_paper_mode() -> Result<()> {
    // V5 Live Execution Unlocked
    Ok(())
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

pub mod engine;
pub mod events;
pub mod runtime;
