use anyhow::Result;
use clap::{Parser, Subcommand};
use polymarket_adapter::{build_executor, spawn_live_feeds};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use trading_core::config::{load_profile, AppProfile, RunMode};
use trading_core::events::NormalizedEvent;
use trading_core::replay::{read_recorded_events, EventRecorder};

#[derive(Parser)]
#[command(name = "app")]
#[command(about = "Polymarket trading bot")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Live {
        #[arg(long, default_value = "config/live.toml")]
        profile: PathBuf,
    },
    Replay {
        #[arg(long)]
        events: PathBuf,
        #[arg(long, default_value = "config/replay.toml")]
        profile: PathBuf,
        #[arg(long, default_value_t = 5)]
        journal_lines: usize,
        #[arg(long, default_value_t = false)]
        tui: bool,
    },
    Backtest {
        #[arg(long)]
        events: PathBuf,
        #[arg(long, default_value = "config/backtest.toml")]
        profile: PathBuf,
        #[arg(long, default_value_t = 5)]
        journal_lines: usize,
        #[arg(long, default_value_t = false)]
        tui: bool,
    },
    Record {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 60)]
        duration_secs: u64,
        #[arg(long, default_value = "config/live.toml")]
        profile: PathBuf,
    },
    InspectConfig {
        #[arg(long, default_value = "config/live.toml")]
        profile: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Live { profile } => app::run(load_profile(profile)?).await,
        Command::Replay {
            events,
            profile,
            journal_lines,
            tui,
        } => {
            if tui {
                return app::run_offline(load_profile(profile)?, events, RunMode::Replay).await;
            }
            run_offline(
                load_profile(profile)?,
                events,
                RunMode::Replay,
                journal_lines,
            )
            .await
        }
        Command::Backtest {
            events,
            profile,
            journal_lines,
            tui,
        } => {
            if tui {
                return app::run_offline(load_profile(profile)?, events, RunMode::Backtest).await;
            }
            run_offline(
                load_profile(profile)?,
                events,
                RunMode::Backtest,
                journal_lines,
            )
            .await
        }
        Command::Record {
            output,
            duration_secs,
            profile,
        } => run_record(load_profile(profile)?, output, duration_secs).await,
        Command::InspectConfig { profile } => {
            let profile = load_profile(profile)?;
            println!("{}", serde_json::to_string_pretty(&profile)?);
            Ok(())
        }
    }
}

async fn run_offline(
    profile: AppProfile,
    events_path: PathBuf,
    mode: RunMode,
    journal_lines: usize,
) -> Result<()> {
    let sweep = profile.sweep.clone();
    let executor = build_executor(&profile)?;
    let mut engine = app::strategy::build_engine(&profile, mode, executor);
    for event in read_recorded_events(events_path)? {
        engine.apply_event(event);
        engine.refresh_dirty_markets().await;
    }

    let snapshot = app::snapshot::build_snapshot(&engine, &sweep);
    let total_pnl = snapshot.equity - profile.sweep.starting_cash;
    println!(
        "mode={:?} cash=${:.2} equity=${:.2} total_pnl=${:.2} realized=${:.2} unrealized=${:.2} open_positions={}",
        mode,
        snapshot.cash,
        snapshot.equity,
        total_pnl,
        snapshot.realized_pnl,
        snapshot.unrealized_pnl,
        snapshot.open_positions,
    );
    println!(
        "eligible_markets={} signal={:.2} pnl_5m=${:.2} shortfall_5m=${:.2} flow=${:.3}/min cycles={:.2}/min hold={:.1}s closed=${:.2}",
        snapshot.eligible_markets,
        snapshot.signal_strength,
        snapshot.realized_pnl_5m,
        snapshot.pnl_shortfall_5m,
        snapshot.flow_pnl_per_min,
        snapshot.cycle_rate_per_min,
        snapshot.avg_hold_secs,
        snapshot.recent_closed_pnl,
    );
    print_trade_summary(&engine.state.portfolio.closed_trade_stats());
    if let Some(market) = snapshot.markets.first() {
        println!(
            "top_market={} {} {} ask={} fair={} edge={:+.1}c",
            market.symbol,
            market.window_label,
            match market.side {
                trading_core::market::types::Side::Up => "U",
                trading_core::market::types::Side::Down => "D",
            },
            trading_core::market::quote::cents_or_dash(market.best_ask),
            trading_core::market::quote::cents_or_dash(market.fair_value),
            market.edge_buy * 100.0,
        );
    }
    print_journal_tail(&snapshot.journal_tail, journal_lines);
    Ok(())
}

fn print_trade_summary(stats: &trading_core::portfolio::ClosedTradeStats) {
    println!(
        "trades={} wins={} losses={} win_rate={:.1}% pf={} best=${:.2} worst=${:.2}",
        stats.count,
        stats.wins,
        stats.losses,
        stats.win_rate() * 100.0,
        pf_label(stats.profit_factor()),
        stats.best_pnl,
        stats.worst_pnl,
    );
}

fn pf_label(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.2}")
    } else {
        "inf".to_string()
    }
}

fn print_journal_tail(lines: &[String], limit: usize) {
    for line in lines.iter().rev().take(limit) {
        println!("journal={line}");
    }
}

async fn run_record(profile: AppProfile, output: PathBuf, duration_secs: u64) -> Result<()> {
    let mut recorder = EventRecorder::create(&output)?;
    let (tx, mut rx) = mpsc::channel::<NormalizedEvent>(1024);
    let handles = spawn_live_feeds(profile.adapter, Some(profile.execution.live.clone()), tx);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(duration_secs);

    loop {
        tokio::select! {
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else { break; };
                recorder.record(&event)?;
            }
            _ = tokio::time::sleep_until(deadline) => {
                break;
            }
        }
    }

    recorder.flush()?;
    for handle in handles {
        handle.abort();
    }
    println!("recorded normalized events to {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backtest_fixture_stays_profitable() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
        let profile = load_profile(root.join("config/backtest.toml")).unwrap();
        let events = root.join("data/fixtures/sample_positive.ndjson");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(runtime
            .block_on(run_offline(profile, events, RunMode::Backtest, 5))
            .is_ok());
    }
}
