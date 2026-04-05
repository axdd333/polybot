use crate::{spawn_input_listener, tui, TerminalSession, UiCmd};
use anyhow::Result;
use polymarket_adapter::build_executor;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use trading_core::config::{AppProfile, RunMode, SweepProfile};
use trading_core::events::NormalizedEvent;
use trading_core::portfolio::ClosedTradeStats;
use trading_core::replay::read_recorded_events;
use trading_core::snapshot::{BacktestPhase, BacktestSnapshot, WorldSnapshot};

const RENDER_MS: u64 = 120;
const TRACE_CAP: usize = 240;
const SPEEDS: [usize; 6] = [1, 5, 25, 100, 500, 2_000];

pub async fn run(profile: AppProfile, path: PathBuf, mode: RunMode) -> Result<()> {
    let events = read_recorded_events(&path)?;
    let executor = build_executor(&profile)?;
    let mut engine = crate::strategy::build_engine(&profile, mode, executor);
    let sweep = profile.sweep.clone();
    let mut term = TerminalSession::enter()?;
    let mut input_rx = spawn_input_listener();
    let mut timer = render_timer();
    let mut run = OfflineRun::new(profile.sweep.starting_cash, &events);
    let mut ui = tui::UiState::default();
    let mut render = tui::RenderState::default();

    loop {
        drain_input(&mut input_rx, &mut run, &mut ui, &render);
        if run.quit {
            break;
        }
        let batch = run.next_batch();
        if batch > 0 {
            apply_batch(&mut engine, &events, &mut run, batch, &sweep).await;
        }
        let snap = make_snapshot(&engine, mode, &run, &sweep);
        term.draw(|frame| {
            render = tui::render(frame, &snap, &ui);
        })?;
        wait_next(&mut timer, &mut input_rx, &mut run, &mut ui, &render).await;
        if run.quit {
            break;
        }
    }

    term.restore()?;
    Ok(())
}

fn render_timer() -> tokio::time::Interval {
    let mut timer = tokio::time::interval(Duration::from_millis(RENDER_MS));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    timer
}

async fn apply_batch(
    engine: &mut trading_core::engine::TradingEngine,
    events: &[NormalizedEvent],
    run: &mut OfflineRun,
    batch: usize,
    sweep: &SweepProfile,
) {
    for _ in 0..batch {
        let Some(event) = events.get(run.idx).cloned() else {
            run.finish();
            return;
        };
        run.bump_sim(event_ts(&event));
        engine.apply_event(event);
        engine.refresh_dirty_markets().await;
        run.idx += 1;
    }
    run.observe(crate::snapshot::build_snapshot(engine, sweep).equity);
    if run.idx >= events.len() {
        run.finish();
    }
}

fn make_snapshot(
    engine: &trading_core::engine::TradingEngine,
    mode: RunMode,
    run: &OfflineRun,
    sweep: &SweepProfile,
) -> WorldSnapshot {
    let mut snap = crate::snapshot::build_snapshot(engine, sweep);
    snap.mode = mode;
    run.attach(&mut snap, engine.state.portfolio.closed_trade_stats());
    snap
}

async fn wait_next(
    timer: &mut tokio::time::Interval,
    input_rx: &mut mpsc::Receiver<UiCmd>,
    run: &mut OfflineRun,
    ui: &mut tui::UiState,
    render: &tui::RenderState,
) {
    tokio::select! {
        maybe_cmd = input_rx.recv() => {
            if let Some(cmd) = maybe_cmd {
                apply_cmd(run, ui, render, cmd);
            }
        }
        _ = timer.tick() => {}
    }
}

fn drain_input(
    input_rx: &mut mpsc::Receiver<UiCmd>,
    run: &mut OfflineRun,
    ui: &mut tui::UiState,
    render: &tui::RenderState,
) {
    while let Ok(cmd) = input_rx.try_recv() {
        apply_cmd(run, ui, render, cmd);
    }
}

struct OfflineRun {
    idx: usize,
    speed_idx: usize,
    step_once: bool,
    quit: bool,
    phase: BacktestPhase,
    started_at: Instant,
    first_ts: Option<Instant>,
    last_ts: Option<Instant>,
    total_events: usize,
    total_sim_secs: f64,
    start_equity: f64,
    peak_equity: f64,
    max_dd_frac: f64,
    equity: VecDeque<(f64, f64)>,
    drawdown: VecDeque<(f64, f64)>,
}

impl OfflineRun {
    fn new(start_equity: f64, events: &[NormalizedEvent]) -> Self {
        let mut run = Self {
            idx: 0,
            speed_idx: 3,
            step_once: false,
            quit: false,
            phase: phase_for(events),
            started_at: Instant::now(),
            first_ts: events.first().map(event_ts),
            last_ts: None,
            total_events: events.len(),
            total_sim_secs: total_sim_secs(events),
            start_equity,
            peak_equity: start_equity,
            max_dd_frac: 0.0,
            equity: VecDeque::new(),
            drawdown: VecDeque::new(),
        };
        run.observe(start_equity);
        run
    }

    fn toggle_pause(&mut self) {
        self.phase = match self.phase {
            BacktestPhase::Running => BacktestPhase::Paused,
            BacktestPhase::Paused => BacktestPhase::Running,
            other => other,
        };
    }

    fn next_batch(&mut self) -> usize {
        if self.phase == BacktestPhase::Running {
            return SPEEDS[self.speed_idx];
        }
        if self.step_once {
            self.step_once = false;
            return 1;
        }
        0
    }

    fn bump_sim(&mut self, ts: Instant) {
        self.last_ts = Some(ts);
    }

    fn observe(&mut self, equity: f64) {
        self.peak_equity = self.peak_equity.max(equity);
        self.max_dd_frac = self.max_dd_frac.max(drawdown(self.peak_equity, equity));
        let progress = self.progress() * 100.0;
        push_trace(&mut self.equity, (progress, equity));
        push_trace(&mut self.drawdown, (progress, self.max_dd_frac * 100.0));
    }

    fn attach(&self, snap: &mut WorldSnapshot, trades: ClosedTradeStats) {
        snap.backtest = Some(BacktestSnapshot {
            phase: self.phase,
            processed_events: self.idx,
            total_events: self.total_events,
            progress: self.progress(),
            batch_size: SPEEDS[self.speed_idx],
            wall_secs: self.started_at.elapsed().as_secs_f64(),
            sim_secs: self.sim_secs(),
            total_sim_secs: self.total_sim_secs,
            event_rate: self.event_rate(),
            closed_trades: trades.count,
            wins: trades.wins,
            losses: trades.losses,
            win_rate: trades.win_rate(),
            profit_factor: trades.profit_factor(),
            best_trade: trades.best_pnl,
            worst_trade: trades.worst_pnl,
            max_drawdown_frac: self.max_dd_frac,
            peak_equity: self.peak_equity,
            total_return: ret_frac(self.start_equity, snap.equity),
            equity_series: self.equity.iter().copied().collect(),
            drawdown_series: self.drawdown.iter().copied().collect(),
        });
    }

    fn progress(&self) -> f64 {
        frac(self.idx, self.total_events)
    }

    fn sim_secs(&self) -> f64 {
        match (self.first_ts, self.last_ts) {
            (Some(a), Some(b)) => b.duration_since(a).as_secs_f64(),
            _ => 0.0,
        }
    }

    fn event_rate(&self) -> f64 {
        let wall = self.started_at.elapsed().as_secs_f64();
        if wall > 0.0 {
            self.idx as f64 / wall
        } else {
            0.0
        }
    }

    fn finish(&mut self) {
        self.phase = BacktestPhase::Completed;
    }
}

fn apply_cmd(run: &mut OfflineRun, ui: &mut tui::UiState, render: &tui::RenderState, cmd: UiCmd) {
    let Some(action) = ui.handle(cmd, render) else {
        return;
    };
    run.apply(action);
}

impl OfflineRun {
    fn apply(&mut self, action: tui::UiAction) {
        match action {
            tui::UiAction::Quit => self.quit = true,
            tui::UiAction::TogglePause => self.toggle_pause(),
            tui::UiAction::Faster => self.speed_idx = up_idx(self.speed_idx),
            tui::UiAction::Slower => self.speed_idx = down_idx(self.speed_idx),
            tui::UiAction::Step => {
                self.step_once = self.phase != BacktestPhase::Completed;
            }
            tui::UiAction::SetPage(_) | tui::UiAction::None => {}
        }
    }
}

fn event_ts(event: &NormalizedEvent) -> Instant {
    match event {
        NormalizedEvent::UnderlyingTick { ts, .. } => *ts,
        NormalizedEvent::BookSnapshot { ts, .. } => *ts,
        NormalizedEvent::BookDelta { ts, .. } => *ts,
        NormalizedEvent::TradePrint { ts, .. } => *ts,
        NormalizedEvent::TickSizeChange { ts, .. } => *ts,
        NormalizedEvent::MarketDiscovered { ts, .. } => *ts,
        NormalizedEvent::MarketExpired { ts, .. } => *ts,
        NormalizedEvent::LiveOrderUpdate { ts, .. } => *ts,
        NormalizedEvent::LiveTrade { ts, .. } => *ts,
        NormalizedEvent::TimerTick { ts, .. } => *ts,
    }
}

fn total_sim_secs(events: &[NormalizedEvent]) -> f64 {
    match (events.first(), events.last()) {
        (Some(a), Some(b)) => event_ts(b).duration_since(event_ts(a)).as_secs_f64(),
        _ => 0.0,
    }
}

fn phase_for(events: &[NormalizedEvent]) -> BacktestPhase {
    if events.is_empty() {
        BacktestPhase::Completed
    } else {
        BacktestPhase::Running
    }
}

fn push_trace(trace: &mut VecDeque<(f64, f64)>, pt: (f64, f64)) {
    trace.push_back(pt);
    while trace.len() > TRACE_CAP {
        trace.pop_front();
    }
}

fn drawdown(peak: f64, equity: f64) -> f64 {
    if peak > 0.0 {
        ((peak - equity) / peak).max(0.0)
    } else {
        0.0
    }
}

fn frac(num: usize, den: usize) -> f64 {
    if den > 0 {
        num as f64 / den as f64
    } else {
        1.0
    }
}

fn ret_frac(start: f64, equity: f64) -> f64 {
    if start > 0.0 {
        (equity - start) / start
    } else {
        0.0
    }
}

fn up_idx(idx: usize) -> usize {
    (idx + 1).min(SPEEDS.len() - 1)
}

fn down_idx(idx: usize) -> usize {
    idx.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawdown_tracks_peak() {
        let mut run = OfflineRun::new(100.0, &[]);
        run.observe(110.0);
        run.observe(95.0);
        assert!(run.max_dd_frac > 0.13);
    }

    #[test]
    fn speed_controls_stay_bounded() {
        assert_eq!(up_idx(SPEEDS.len() - 1), SPEEDS.len() - 1);
        assert_eq!(down_idx(0), 0);
    }
}
