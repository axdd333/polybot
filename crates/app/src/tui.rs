use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, BorderType, Chart, Dataset, GraphType, List, ListItem, Paragraph, Wrap,
    },
    Frame,
};
use std::collections::BTreeMap;

use crate::UiCmd;
use trading_core::config::RunMode;
use trading_core::market::quote;
use trading_core::market::types::{OrderAction, Side};
use trading_core::snapshot::{BacktestPhase, BacktestSnapshot, MarketSnapshot, WorldSnapshot};

struct PairView<'a> {
    symbol: String,
    window_label: String,
    expiry_secs: u64,
    up: Option<&'a MarketSnapshot>,
    down: Option<&'a MarketSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiPage {
    #[default]
    Overview,
    Markets,
    Activity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    None,
    Quit,
    TogglePause,
    Faster,
    Slower,
    Step,
    SetPage(UiPage),
}

#[derive(Debug, Clone, Copy)]
struct HitTarget {
    area: Rect,
    action: UiAction,
}

#[derive(Debug, Clone, Default)]
pub struct RenderState {
    hits: Vec<HitTarget>,
}

#[derive(Debug, Clone, Default)]
pub struct UiState {
    page: UiPage,
    focus: usize,
}

impl UiState {
    pub(crate) fn handle(&mut self, cmd: UiCmd, render: &RenderState) -> Option<UiAction> {
        match cmd {
            UiCmd::Quit => Some(UiAction::Quit),
            UiCmd::TogglePause => Some(UiAction::TogglePause),
            UiCmd::Faster => Some(UiAction::Faster),
            UiCmd::Slower => Some(UiAction::Slower),
            UiCmd::Step => Some(UiAction::Step),
            UiCmd::Prev => {
                self.shift_focus(render, false);
                None
            }
            UiCmd::Next => {
                self.shift_focus(render, true);
                None
            }
            UiCmd::PagePrev => Some(self.page_action(false)),
            UiCmd::PageNext => Some(self.page_action(true)),
            UiCmd::Activate => self.focus_action(render),
            UiCmd::MousePress(x, y) => self.mouse_action(render, x, y),
        }
    }

    pub fn page(&self) -> UiPage {
        self.page
    }

    fn shift_focus(&mut self, render: &RenderState, forward: bool) {
        if render.hits.is_empty() {
            self.focus = 0;
            return;
        }
        let len = render.hits.len();
        self.focus = if forward {
            (self.focus + 1) % len
        } else {
            (self.focus + len - 1) % len
        };
    }

    fn page_action(&mut self, forward: bool) -> UiAction {
        let pages = [UiPage::Overview, UiPage::Markets, UiPage::Activity];
        let idx = pages
            .iter()
            .position(|page| *page == self.page)
            .unwrap_or(0);
        let next = if forward {
            (idx + 1) % pages.len()
        } else {
            (idx + pages.len() - 1) % pages.len()
        };
        self.page = pages[next];
        UiAction::SetPage(self.page)
    }

    fn focus_action(&mut self, render: &RenderState) -> Option<UiAction> {
        let action = render.hits.get(self.focus).map(|hit| hit.action)?;
        self.apply_action(action);
        Some(action)
    }

    fn mouse_action(&mut self, render: &RenderState, x: u16, y: u16) -> Option<UiAction> {
        let idx = render
            .hits
            .iter()
            .position(|hit| contains(hit.area, x, y))?;
        self.focus = idx;
        let action = render.hits[idx].action;
        self.apply_action(action);
        Some(action)
    }

    fn apply_action(&mut self, action: UiAction) {
        match action {
            UiAction::SetPage(page) => self.page = page,
            UiAction::None
            | UiAction::Quit
            | UiAction::TogglePause
            | UiAction::Faster
            | UiAction::Slower
            | UiAction::Step => {}
        }
    }
}

pub fn render(f: &mut Frame, snapshot: &WorldSnapshot, ui: &UiState) -> RenderState {
    let area = f.area();
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(18),
        Constraint::Length(12),
        Constraint::Length(3),
    ])
    .split(area);

    let pairs = grouped_markets(snapshot, snapshot.entry_threshold);
    let render = render_controls(f, snapshot, ui, rows[0]);

    render_header(f, snapshot, rows[1]);
    match ui.page() {
        UiPage::Overview => render_overview(f, snapshot, &pairs, rows[2], rows[3]),
        UiPage::Markets => render_markets_page(f, snapshot, &pairs, rows[2], rows[3]),
        UiPage::Activity => render_activity_page(f, snapshot, &pairs, rows[2], rows[3]),
    }
    render_footer(f, snapshot, &pairs, rows[4]);
    render
}

fn render_header(f: &mut Frame, snapshot: &WorldSnapshot, area: Rect) {
    if let Some(backtest) = snapshot.backtest.as_ref() {
        render_backtest_header(f, snapshot, backtest, area);
        return;
    }

    let header = Line::from(vec![
        Span::styled(
            "LIVE SWEEP",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}  {} ${:.0}  mkts {}  elig {}  ticket ${:.2}  buy <= {}  exit cap {}  net {:.1}%  cash ${:.2}  eq ${:.2}  rpnl ${:.2}  upnl ${:.2}  flow ${:.3}/m  cyc {:.2}/m  hold {:.0}s  open {}  sig {:.2}\u{03c3}",
            if snapshot.paper_real_mode { "paper-real" } else { "wallet-live" },
            snapshot.ref_symbol,
            snapshot.ref_spot,
            snapshot.markets.len() / 2,
            snapshot.eligible_markets,
            snapshot.ticket_dollars,
            quote::cents_label(snapshot.entry_threshold),
            quote::cents_label(snapshot.exit_threshold),
            snapshot.min_exit_roi * 100.0,
            snapshot.cash,
            snapshot.equity,
            snapshot.realized_pnl,
            snapshot.unrealized_pnl,
            snapshot.flow_pnl_per_min,
            snapshot.cycle_rate_per_min,
            snapshot.avg_hold_secs,
            snapshot.open_positions,
            snapshot.signal_strength,
        )),
    ]);

    f.render_widget(
        Paragraph::new(header).block(Block::bordered().border_type(BorderType::Rounded)),
        area,
    );
}

fn render_backtest_header(
    f: &mut Frame,
    snapshot: &WorldSnapshot,
    backtest: &BacktestSnapshot,
    area: Rect,
) {
    let header = Line::from(vec![
        Span::styled(
            mode_title(snapshot.mode),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}  ev {} / {}  {:>5.1}%  batch {}  rate {:.0}/s  sim {} / {}  eq ${:.2}  pnl ${:+.2}  dd {:.1}%  trades {}  win {:.1}%  pf {}",
            phase_label(backtest.phase),
            backtest.processed_events,
            backtest.total_events,
            backtest.progress * 100.0,
            backtest.batch_size,
            backtest.event_rate,
            secs_label(backtest.sim_secs),
            secs_label(backtest.total_sim_secs),
            snapshot.equity,
            snapshot.realized_pnl + snapshot.unrealized_pnl,
            backtest.max_drawdown_frac * 100.0,
            backtest.closed_trades,
            backtest.win_rate * 100.0,
            pf_label(backtest.profit_factor),
        )),
    ]);

    f.render_widget(
        Paragraph::new(header).block(Block::bordered().border_type(BorderType::Rounded)),
        area,
    );
}

fn render_controls(
    f: &mut Frame,
    snapshot: &WorldSnapshot,
    ui: &UiState,
    area: Rect,
) -> RenderState {
    let mut controls = control_specs(snapshot, ui.page());
    let lens = controls
        .iter()
        .map(|(label, _, _)| Constraint::Length(button_width(label)))
        .collect::<Vec<_>>();
    let cells = Layout::horizontal(lens).split(area);
    let mut hits = Vec::new();

    for (idx, ((label, action, tone), cell)) in
        controls.drain(..).zip(cells.iter().copied()).enumerate()
    {
        let selected = idx == ui.focus;
        let style = control_style(tone, selected);
        f.render_widget(
            Paragraph::new(label).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .style(style),
            ),
            cell,
        );
        hits.push(HitTarget { area: cell, action });
    }

    RenderState { hits }
}

fn render_overview(
    f: &mut Frame,
    snapshot: &WorldSnapshot,
    pairs: &[PairView<'_>],
    body_area: Rect,
    lower_area: Rect,
) {
    let body = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(body_area);
    render_markets(f, pairs, body[0]);
    render_focus(f, pairs, body[1]);
    render_lower(f, snapshot, pairs, lower_area);
}

fn render_markets_page(
    f: &mut Frame,
    snapshot: &WorldSnapshot,
    pairs: &[PairView<'_>],
    body_area: Rect,
    lower_area: Rect,
) {
    let body = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(body_area);
    render_markets(f, pairs, body[0]);
    render_focus(f, pairs, body[1]);
    let lower = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(lower_area);
    if let Some(backtest) = snapshot.backtest.as_ref() {
        render_backtest_panel(f, snapshot, backtest, lower[0]);
    } else {
        render_best_setups(f, snapshot, pairs, lower[0]);
    }
    render_activity(f, snapshot, lower[1]);
}

fn render_activity_page(
    f: &mut Frame,
    snapshot: &WorldSnapshot,
    pairs: &[PairView<'_>],
    body_area: Rect,
    lower_area: Rect,
) {
    let body = Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(body_area);
    render_activity(f, snapshot, body[0]);
    if let Some(backtest) = snapshot.backtest.as_ref() {
        render_backtest_panel(f, snapshot, backtest, body[1]);
    } else {
        render_focus(f, pairs, body[1]);
    }
    let lower = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(lower_area);
    if let Some(backtest) = snapshot.backtest.as_ref() {
        render_backtest_charts(f, backtest, lower[0]);
    } else {
        render_best_setups(f, snapshot, pairs, lower[0]);
    }
    render_markets(f, pairs, lower[1]);
}

fn render_markets(f: &mut Frame, pairs: &[PairView<'_>], area: Rect) {
    let items = if pairs.is_empty() {
        vec![ListItem::new(Line::from(" waiting for live markets..."))]
    } else {
        pairs
            .iter()
            .map(|pair| {
                let best = best_side(pair);
                let spread = best
                    .map(|m| spread_label(m.best_bid, m.best_ask))
                    .unwrap_or_else(|| "n/a".to_string());
                let flow = best
                    .map(|m| activity_label(m.trade_intensity))
                    .unwrap_or("n/a");
                let position = pair
                    .up
                    .into_iter()
                    .chain(pair.down)
                    .find(|m| m.position_qty > 0.0)
                    .map(position_brief)
                    .unwrap_or_else(|| "pos -".to_string());

                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            format!(" {} {} ", pair.symbol, pair.window_label),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(
                            " {}  spr {}  flow {}  {}",
                            mins_secs(pair.expiry_secs),
                            spread,
                            flow,
                            position,
                        )),
                    ]),
                    side_compare_line(pair),
                ])
            })
            .collect::<Vec<_>>()
    };

    f.render_widget(
        List::new(items).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Markets  [side  mkt/fair  edge] "),
        ),
        area,
    );
}

fn render_focus(f: &mut Frame, pairs: &[PairView<'_>], area: Rect) {
    let Some(pair) = pairs.first() else {
        f.render_widget(
            Paragraph::new("focus waits for a live market").block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Focus "),
            ),
            area,
        );
        return;
    };
    let Some(best) = best_side(pair) else {
        f.render_widget(
            Paragraph::new("focus waits for a valid side").block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Focus "),
            ),
            area,
        );
        return;
    };

    let other = other_side(pair, best.side);
    let chunks = Layout::vertical([
        Constraint::Length(8),
        Constraint::Min(10),
        Constraint::Length(5),
    ])
    .split(area);

    let mut lines = vec![
        Line::from(Span::styled(
            format!("\u{1f3af} {} ", best.title),
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            format!(" {} {} ", pair.symbol, pair.window_label),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(" mkt = live ask   fair = model price   edge = fair - ask"),
        focus_side_line(best),
    ];

    if let Some(other) = other {
        lines.push(focus_side_line(other));
    }

    let position = if best.position_qty > 0.0 {
        format!(
            " pos {:.1} @ {}  uPnL ${:+.2}",
            best.position_qty,
            quote::cents_label(best.avg_entry),
            best.unrealized_pnl
        )
    } else {
        " pos -".to_string()
    };
    lines.push(Line::from(format!(
        " bid {} x {:.0}  ask {} x {:.0}  spr {}  flow {}  {}",
        quote_cents(best.best_bid),
        best.bid_size,
        quote_cents(best.best_ask),
        best.ask_size,
        spread_label(best.best_bid, best.best_ask),
        activity_label(best.trade_intensity),
        position,
    )));

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Focus "),
            )
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    render_charts(f, best, chunks[1]);

    let orders = if best.intents.is_empty() {
        vec![Line::from(" no buy orders")]
    } else {
        best.intents
            .iter()
            .take(4)
            .map(|intent| {
                Line::from(format!(
                    " {} {} {} x {:.1}",
                    match (intent.action, intent.aggressive) {
                        (OrderAction::Buy, true) => "buy now",
                        (OrderAction::Buy, false) => "buy rest",
                        (OrderAction::Sell, true) => "sell now",
                        (OrderAction::Sell, false) => "sell rest",
                    },
                    match intent.action {
                        OrderAction::Buy => "at",
                        OrderAction::Sell => "at",
                    },
                    quote::cents_label(intent.price),
                    intent.qty,
                ))
            })
            .collect::<Vec<_>>()
    };
    f.render_widget(
        Paragraph::new(orders)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Planned Orders "),
            )
            .wrap(Wrap { trim: false }),
        chunks[2],
    );
}

fn render_lower(f: &mut Frame, snapshot: &WorldSnapshot, pairs: &[PairView<'_>], area: Rect) {
    let cols =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(area);
    if let Some(backtest) = snapshot.backtest.as_ref() {
        render_backtest_panel(f, snapshot, backtest, cols[0]);
    } else {
        render_best_setups(f, snapshot, pairs, cols[0]);
    }
    render_activity(f, snapshot, cols[1]);
}

fn render_best_setups(f: &mut Frame, snapshot: &WorldSnapshot, pairs: &[PairView<'_>], area: Rect) {
    let actionable = pairs
        .iter()
        .filter_map(|pair| {
            let best = best_side(pair)?;
            (best.position_qty <= 0.0
                && quote::valid_live_quote(best.best_bid, best.best_ask)
                && best.expiry_secs > snapshot.no_new_entry_expiry_secs
                && best.best_ask >= snapshot.min_entry_threshold
                && best.best_ask <= snapshot.entry_threshold
                && best.min_tick_size / best.best_ask.max(0.01) <= snapshot.max_tick_frac
                && best.fair_value
                    >= crate::planner::min_profitable_exit(
                        crate::planner::entry_cost_basis(best.best_ask, best.taker_fee_bps, 0.0),
                        best.maker_fee_bps,
                        snapshot.min_exit_roi,
                    ) + best.min_tick_size
                && quote::has_tight_spread(best.best_bid, best.best_ask, snapshot.max_spread))
            .then_some((pair, best))
        })
        .take(8)
        .collect::<Vec<_>>();

    let items = if actionable.is_empty() {
        vec![ListItem::new(
            " no sweep entries pass spread + price gates right now",
        )]
    } else {
        actionable
            .into_iter()
            .map(|(pair, best)| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:>4} {:<11} ", pair.symbol, pair.window_label),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<2} ", side_tag(best.side)),
                        Style::default().fg(side_color(best.side)),
                    ),
                    Span::raw(format!(
                        "{} ask {}  exit {}",
                        prob_meter(best.best_ask),
                        quote_cents(best.best_ask),
                        quote::cents_label(candidate_exit_target(
                            best.best_ask,
                            best.fair_value,
                            snapshot.exit_threshold,
                            best.maker_fee_bps,
                            snapshot.min_exit_roi,
                        )),
                    )),
                ]))
            })
            .collect::<Vec<_>>()
    };

    f.render_widget(
        List::new(items).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Sweep Candidates "),
        ),
        area,
    );
}

fn render_backtest_panel(
    f: &mut Frame,
    snapshot: &WorldSnapshot,
    backtest: &BacktestSnapshot,
    area: Rect,
) {
    let rows = Layout::vertical([Constraint::Length(6), Constraint::Min(6)]).split(area);
    let lines = vec![
        Line::from(format!(
            " return {:+.2}%  peak ${:.2}  dd {:.2}%",
            backtest.total_return * 100.0,
            backtest.peak_equity,
            backtest.max_drawdown_frac * 100.0,
        )),
        Line::from(format!(
            " trades {}  wins {}  losses {}  win {:.1}%  pf {}",
            backtest.closed_trades,
            backtest.wins,
            backtest.losses,
            backtest.win_rate * 100.0,
            pf_label(backtest.profit_factor),
        )),
        Line::from(format!(
            " best ${:+.2}  worst ${:+.2}  cash ${:.2}  eq ${:.2}",
            backtest.best_trade, backtest.worst_trade, snapshot.cash, snapshot.equity,
        )),
        Line::from(format!(
            " rpnl ${:+.2}  upnl ${:+.2}  open {}  sig {:.2}σ",
            snapshot.realized_pnl,
            snapshot.unrealized_pnl,
            snapshot.open_positions,
            snapshot.signal_strength,
        )),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Backtest Summary "),
            )
            .wrap(Wrap { trim: false }),
        rows[0],
    );
    render_backtest_charts(f, backtest, rows[1]);
}

fn render_backtest_charts(f: &mut Frame, backtest: &BacktestSnapshot, area: Rect) {
    let cols =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(area);
    let equity = smooth_series(&backtest.equity_series, 0.20);
    let drawdown = smooth_series(&backtest.drawdown_series, 0.18);
    render_trace_chart(f, &equity, " Equity Curve ", cols[0], Color::Green, "$");
    render_trace_chart(f, &drawdown, " Drawdown %", cols[1], Color::Red, "%");
}

fn render_trace_chart(
    f: &mut Frame,
    series: &[(f64, f64)],
    title: &str,
    area: Rect,
    color: Color,
    suffix: &str,
) {
    let zero = zero_line(series);
    let y = bounds(series, &zero, 0.0, 1.0, 0.0, 1.0);
    let chart = Chart::new(vec![
        Dataset::default()
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::DarkGray))
            .data(&zero),
        Dataset::default()
            .marker(symbols::Marker::HalfBlock)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(color))
            .data(series),
    ])
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(title),
    )
    .x_axis(
        Axis::default()
            .style(Style::default().fg(Color::DarkGray))
            .bounds([0.0, 100.0])
            .labels(vec![Line::from("0"), Line::from("50"), Line::from("100")]),
    )
    .y_axis(
        Axis::default()
            .style(Style::default().fg(Color::DarkGray))
            .bounds(y)
            .labels(vec![
                Line::from(axis_label(y[0], suffix)),
                Line::from(axis_label((y[0] + y[1]) * 0.5, suffix)),
                Line::from(axis_label(y[1], suffix)),
            ]),
    );
    f.render_widget(chart, area);
}

fn render_activity(f: &mut Frame, snapshot: &WorldSnapshot, area: Rect) {
    let items = if snapshot.journal_tail.is_empty() {
        vec![ListItem::new(" no planner notes, orders, or fills yet")]
    } else {
        snapshot
            .journal_tail
            .iter()
            .map(|line| ListItem::new(Line::from(compact_activity(line))))
            .collect::<Vec<_>>()
    };

    f.render_widget(
        List::new(items).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(activity_title(snapshot.mode)),
        ),
        area,
    );
}

fn render_footer(f: &mut Frame, snapshot: &WorldSnapshot, pairs: &[PairView<'_>], area: Rect) {
    let footer = match snapshot.backtest.as_ref() {
        Some(backtest) => backtest_footer(backtest),
        None => pairs
            .first()
            .and_then(best_side)
            .map(live_focus_footer)
            .unwrap_or_else(|| "[q] quit  click or tab through controls".to_string()),
    };
    f.render_widget(
        Paragraph::new(footer).block(Block::bordered().border_type(BorderType::Rounded)),
        area,
    );
}

fn render_charts(f: &mut Frame, market: &MarketSnapshot, area: Rect) {
    let chunks =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);

    let edge_series = smooth_series(&resample_series(&market.edge_series, 90, -30.0, 0.0), 0.35);
    let zero_edge = zero_line(&edge_series);
    let edge_bounds = bounds(&edge_series, &zero_edge, -1.0, 1.0, -1.0, 1.0);
    let edge = Dataset::default()
        .name("edge")
        .marker(symbols::Marker::HalfBlock)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(&edge_series);
    let zero = Dataset::default()
        .name("zero")
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::DarkGray))
        .data(&zero_edge);
    let edge_chart = Chart::new(vec![zero, edge])
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Gap To Buy 30s "),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([-30.0, 0.0])
                .labels(vec![Line::from("-30"), Line::from("-15"), Line::from("0")]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds(edge_bounds)
                .labels(vec![
                    Line::from(signed_cents(edge_bounds[0])),
                    Line::from("0"),
                    Line::from(signed_cents(edge_bounds[1])),
                ]),
        );
    f.render_widget(edge_chart, chunks[0]);

    let pressure_series =
        smooth_series(&resample_series(&market.micro_series, 90, -30.0, 0.0), 0.30);
    let pressure = Dataset::default()
        .name("pressure")
        .marker(symbols::Marker::HalfBlock)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Yellow))
        .data(&pressure_series);
    let flow_scaled = smooth_series(
        &scale_series(&resample_series(&market.flow_series, 90, -30.0, 0.0)),
        0.25,
    );
    let flow = Dataset::default()
        .name("flow")
        .marker(symbols::Marker::HalfBlock)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Green))
        .data(&flow_scaled);
    let zero_pressure = zero_line(&pressure_series);
    let lower_bounds = bounds(&pressure_series, &zero_pressure, -1.0, 1.0, -1.0, 1.0);
    let pressure_chart = Chart::new(vec![
        Dataset::default()
            .name("zero")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::DarkGray))
            .data(&zero_pressure),
        pressure,
        flow,
    ])
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Pressure + Flow 30s "),
    )
    .x_axis(
        Axis::default()
            .style(Style::default().fg(Color::DarkGray))
            .bounds([-30.0, 0.0])
            .labels(vec![Line::from("-30"), Line::from("-15"), Line::from("0")]),
    )
    .y_axis(
        Axis::default()
            .style(Style::default().fg(Color::DarkGray))
            .bounds(lower_bounds)
            .labels(vec![Line::from("-"), Line::from("0"), Line::from("+")]),
    );
    f.render_widget(pressure_chart, chunks[1]);
}

// ---------------------------------------------------------------------------
// Grouping & sorting helpers
// ---------------------------------------------------------------------------

fn grouped_markets<'a>(snapshot: &'a WorldSnapshot, entry_threshold: f64) -> Vec<PairView<'a>> {
    let mut map: BTreeMap<(String, String), PairView<'a>> = BTreeMap::new();

    for market in &snapshot.markets {
        let key = (market.symbol.clone(), market.window_label.clone());
        let entry = map.entry(key).or_insert_with(|| PairView {
            symbol: market.symbol.clone(),
            window_label: market.window_label.clone(),
            expiry_secs: market.expiry_secs,
            up: None,
            down: None,
        });
        entry.expiry_secs = entry.expiry_secs.min(market.expiry_secs);
        match market.side {
            Side::Up => entry.up = Some(market),
            Side::Down => entry.down = Some(market),
        }
    }

    let mut pairs = map.into_values().collect::<Vec<_>>();
    pairs.sort_by(|a, b| {
        pair_priority(b, entry_threshold)
            .partial_cmp(&pair_priority(a, entry_threshold))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pairs
}

fn pair_priority(pair: &PairView<'_>, entry_threshold: f64) -> f64 {
    best_side(pair)
        .map(|m| {
            if m.position_qty > 0.0 {
                10_000.0 - m.avg_entry
            } else if quote::valid_live_quote(m.best_bid, m.best_ask)
                && m.best_ask <= entry_threshold
            {
                1_000.0 - m.best_ask
            } else if quote::valid_live_quote(m.best_bid, m.best_ask) {
                100.0 - m.best_ask
            } else {
                -999.0
            }
        })
        .unwrap_or(-999.0)
}

fn best_side<'a>(pair: &'a PairView<'a>) -> Option<&'a MarketSnapshot> {
    let mut sides: Vec<&MarketSnapshot> = [pair.up, pair.down].into_iter().flatten().collect();
    sides.sort_by(|a, b| {
        side_priority(a)
            .partial_cmp(&side_priority(b))
            .unwrap_or(std::cmp::Ordering::Equal)
            .reverse()
    });
    sides.into_iter().next()
}

fn other_side<'a>(pair: &'a PairView<'a>, side: Side) -> Option<&'a MarketSnapshot> {
    match side {
        Side::Up => pair.down,
        Side::Down => pair.up,
    }
}

fn side_compare_line(pair: &PairView<'_>) -> Line<'static> {
    let mut spans = side_compact(pair.up, Side::Up);
    spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
    spans.extend(side_compact(pair.down, Side::Down));
    Line::from(spans)
}

fn side_compact(market: Option<&MarketSnapshot>, side: Side) -> Vec<Span<'static>> {
    match market {
        Some(m) => vec![
            Span::styled(
                format!("{} ", side_tag(side)),
                Style::default().fg(side_color(side)),
            ),
            Span::raw(format!(
                "{} / {} ",
                quote_cents(m.best_ask),
                quote::cents_label(m.fair_value),
            )),
            Span::styled(
                format!("{} ", signed_cents(m.edge_buy)),
                edge_style(m.edge_buy),
            ),
            Span::styled(edge_meter(m.edge_buy), edge_style(m.edge_buy)),
        ],
        None => vec![
            Span::styled(
                format!("{} ", side_tag(side)),
                Style::default().fg(side_color(side)),
            ),
            Span::raw("-- / --  --"),
        ],
    }
}

fn focus_side_line(market: &MarketSnapshot) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {} ", side_tag(market.side)),
            Style::default()
                .fg(side_color(market.side))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "M {} {}  F {} {}  E {}",
            prob_meter(market.best_ask),
            quote_cents(market.best_ask),
            prob_meter(market.fair_value),
            quote::cents_label(market.fair_value),
            signed_cents(market.edge_buy),
        )),
    ])
}

fn position_brief(market: &MarketSnapshot) -> String {
    format!(
        "pos {:.1}@{}",
        market.position_qty,
        quote::cents_label(market.avg_entry)
    )
}

fn side_priority(market: &MarketSnapshot) -> f64 {
    if market.position_qty > 0.0 {
        10_000.0 - market.avg_entry
    } else if quote::valid_live_quote(market.best_bid, market.best_ask) {
        100.0 - market.best_ask
    } else {
        -999.0
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ControlTone {
    Neutral,
    Accent,
    Warm,
    Danger,
}

fn control_specs(snapshot: &WorldSnapshot, page: UiPage) -> Vec<(String, UiAction, ControlTone)> {
    let phase = snapshot
        .backtest
        .as_ref()
        .map(|bt| phase_label(bt.phase))
        .unwrap_or("live");
    let speed = snapshot
        .backtest
        .as_ref()
        .map(|bt| format!("x{}", bt.batch_size))
        .unwrap_or_else(|| "real".to_string());
    vec![
        page_button(UiPage::Overview, "Overview", page),
        page_button(UiPage::Markets, "Markets", page),
        page_button(UiPage::Activity, "Activity", page),
        (
            format!("Mode {}", mode_title(snapshot.mode)),
            UiAction::None,
            ControlTone::Neutral,
        ),
        (
            "Strategy Sweep".to_string(),
            UiAction::None,
            ControlTone::Neutral,
        ),
        (
            format!("Phase {phase}"),
            UiAction::TogglePause,
            ControlTone::Accent,
        ),
        ("Step".to_string(), UiAction::Step, ControlTone::Warm),
        ("-".to_string(), UiAction::Slower, ControlTone::Neutral),
        (
            format!("Speed {speed}"),
            UiAction::Faster,
            ControlTone::Accent,
        ),
        ("Quit".to_string(), UiAction::Quit, ControlTone::Danger),
    ]
}

fn page_button(page: UiPage, label: &str, active: UiPage) -> (String, UiAction, ControlTone) {
    let tone = if page == active {
        ControlTone::Warm
    } else {
        ControlTone::Accent
    };
    (label.to_string(), UiAction::SetPage(page), tone)
}

fn control_style(tone: ControlTone, selected: bool) -> Style {
    let style = match tone {
        ControlTone::Neutral => Style::default().fg(Color::White),
        ControlTone::Accent => Style::default().fg(Color::Cyan),
        ControlTone::Warm => Style::default().fg(Color::Yellow),
        ControlTone::Danger => Style::default().fg(Color::Red),
    };
    if selected {
        style.add_modifier(Modifier::BOLD).bg(Color::DarkGray)
    } else {
        style
    }
}

fn button_width(label: &str) -> u16 {
    (label.chars().count() as u16).saturating_add(4)
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn mode_title(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Live => "LIVE SWEEP",
        RunMode::Replay => "REPLAY LAB",
        RunMode::Backtest => "BACKTEST LAB",
    }
}

fn phase_label(phase: BacktestPhase) -> &'static str {
    match phase {
        BacktestPhase::Idle => "idle",
        BacktestPhase::Running => "running",
        BacktestPhase::Paused => "paused",
        BacktestPhase::Completed => "done",
    }
}

fn activity_title(mode: RunMode) -> &'static str {
    if mode == RunMode::Live {
        " Planner + Activity "
    } else {
        " Backtest Journal "
    }
}

fn live_focus_footer(best: &MarketSnapshot) -> String {
    format!(
        "[q] quit  * {} {} {} {} / {} {}",
        best.symbol,
        best.window_label,
        side_tag(best.side),
        quote::cents_label(best.best_ask),
        quote::cents_label(best.fair_value),
        signed_cents(best.edge_buy),
    )
}

fn backtest_footer(backtest: &BacktestSnapshot) -> String {
    format!(
        "[q] quit  click or tab through controls  {}  ev {} / {}  rate {:.0}/s",
        phase_label(backtest.phase),
        backtest.processed_events,
        backtest.total_events,
        backtest.event_rate,
    )
}

fn quote_cents(value: f64) -> String {
    if value > 0.0 {
        quote::cents_label(value)
    } else {
        "--".to_string()
    }
}

fn signed_cents(value: f64) -> String {
    format!("{:+.1}c", value * 100.0)
}

fn mins_secs(total_secs: u64) -> String {
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

fn secs_label(total_secs: f64) -> String {
    mins_secs(total_secs.max(0.0).round() as u64)
}

fn spread_label(best_bid: f64, best_ask: f64) -> String {
    quote::spread_cents_label(best_bid, best_ask)
}

fn candidate_exit_target(
    best_ask: f64,
    fair: f64,
    exit_ceil: f64,
    maker_fee_bps: f64,
    min_exit_roi: f64,
) -> f64 {
    if best_ask > 0.0 {
        crate::planner::maker_exit_target(best_ask, exit_ceil, fair, maker_fee_bps, min_exit_roi)
    } else {
        exit_ceil
    }
}

fn activity_label(flow: f64) -> &'static str {
    if flow < 2.0 {
        "low"
    } else if flow < 8.0 {
        "medium"
    } else {
        "high"
    }
}

fn side_tag(side: Side) -> &'static str {
    match side {
        Side::Up => "U",
        Side::Down => "D",
    }
}

fn side_color(side: Side) -> Color {
    match side {
        Side::Up => Color::Green,
        Side::Down => Color::Red,
    }
}

fn prob_meter(value: f64) -> String {
    let width = 12usize;
    let filled = ((value.clamp(0.0, 1.0)) * width as f64).round() as usize;
    let mut out = String::from("[");
    for idx in 0..width {
        out.push(if idx < filled { '=' } else { '.' });
    }
    out.push(']');
    out
}

fn edge_meter(value: f64) -> String {
    let width = 5usize;
    let filled = ((value.abs() * 100.0) / 20.0)
        .clamp(0.0, width as f64)
        .round() as usize;
    let mut out = String::from("[");
    if value < 0.0 {
        for idx in 0..width {
            out.push(if idx >= width.saturating_sub(filled) {
                '<'
            } else {
                '.'
            });
        }
        out.push('|');
        for _ in 0..width {
            out.push('.');
        }
    } else {
        for _ in 0..width {
            out.push('.');
        }
        out.push('|');
        for idx in 0..width {
            out.push(if idx < filled { '>' } else { '.' });
        }
    }
    out.push(']');
    out
}

fn edge_style(value: f64) -> Style {
    if value > 0.0 {
        Style::default().fg(Color::Cyan)
    } else if value < 0.0 {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    }
}

fn compact_activity(line: &str) -> String {
    line.replace(" | ", "  ")
        .replace("queued", "q")
        .replace("fills", "fill")
        .replace("fair", "f")
        .replace("ask", "a")
        .replace("edge", "e")
}

fn pf_label(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.2}")
    } else {
        "inf".to_string()
    }
}

fn axis_label(value: f64, suffix: &str) -> String {
    if suffix == "$" {
        format!("${value:.0}")
    } else {
        format!("{value:.0}{suffix}")
    }
}

// ---------------------------------------------------------------------------
// Chart helpers
// ---------------------------------------------------------------------------

fn bounds(
    a: &[(f64, f64)],
    b: &[(f64, f64)],
    default_min: f64,
    default_max: f64,
    floor: f64,
    ceil: f64,
) -> [f64; 2] {
    let values: Vec<f64> = a.iter().chain(b.iter()).map(|(_, y)| *y).collect();
    if values.is_empty() {
        return [default_min, default_max];
    }
    let min = values
        .iter()
        .fold(f64::INFINITY, |acc, v| acc.min(*v))
        .min(floor);
    let max = values
        .iter()
        .fold(f64::NEG_INFINITY, |acc, v| acc.max(*v))
        .max(ceil);
    if (max - min).abs() < 1e-9 {
        [min - 1.0, max + 1.0]
    } else {
        [min, max]
    }
}

fn zero_line(series: &[(f64, f64)]) -> Vec<(f64, f64)> {
    series.iter().map(|(x, _)| (*x, 0.0)).collect()
}

fn scale_series(series: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let max = series.iter().map(|(_, y)| y.abs()).fold(0.0_f64, f64::max);
    if max <= 1e-9 {
        return series.iter().map(|(x, _)| (*x, 0.0)).collect();
    }
    series
        .iter()
        .map(|(x, y)| (*x, (y / max).clamp(-1.0, 1.0)))
        .collect()
}

fn resample_series(
    series: &[(f64, f64)],
    points: usize,
    start_x: f64,
    end_x: f64,
) -> Vec<(f64, f64)> {
    if series.is_empty() || points < 2 {
        return series.to_vec();
    }

    let mut sorted = series.to_vec();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let step = (end_x - start_x) / (points.saturating_sub(1) as f64);
    let mut out = Vec::with_capacity(points);
    let mut idx = 0usize;

    for point_idx in 0..points {
        let x = start_x + (point_idx as f64 * step);

        while idx + 1 < sorted.len() && sorted[idx + 1].0 < x {
            idx += 1;
        }

        let y = if x <= sorted[0].0 {
            sorted[0].1
        } else if x >= sorted[sorted.len() - 1].0 {
            sorted[sorted.len() - 1].1
        } else {
            let left = sorted[idx];
            let right = sorted[(idx + 1).min(sorted.len() - 1)];
            if (right.0 - left.0).abs() < 1e-9 {
                right.1
            } else {
                let t = (x - left.0) / (right.0 - left.0);
                left.1 + (right.1 - left.1) * t
            }
        };

        out.push((x, y));
    }

    out
}

fn smooth_series(series: &[(f64, f64)], alpha: f64) -> Vec<(f64, f64)> {
    if series.is_empty() {
        return Vec::new();
    }

    let alpha = alpha.clamp(0.01, 1.0);
    let mut smoothed = Vec::with_capacity(series.len());
    let mut last = series[0].1;

    for (x, y) in series.iter().copied() {
        last = alpha * y + (1.0 - alpha) * last;
        smoothed.push((x, last));
    }

    smoothed
}

#[cfg(test)]
mod tests {
    use super::{candidate_exit_target, HitTarget, RenderState, UiAction, UiPage, UiState};
    use crate::UiCmd;
    use ratatui::layout::Rect;

    #[test]
    fn candidate_exit_target_uses_cap_not_floor() {
        assert_eq!(candidate_exit_target(0.50, 0.50, 0.96, 0.0, 0.03), 0.515);
        let px = candidate_exit_target(0.75, 0.75, 0.96, 20.0, 0.03);
        assert!(px >= 0.774);
        assert!(px < 0.775);
        assert_eq!(candidate_exit_target(0.98, 0.99, 0.96, 0.0, 0.03), 0.96);
    }

    #[test]
    fn mouse_press_hits_control() {
        let mut ui = UiState::default();
        let render = RenderState {
            hits: vec![HitTarget {
                area: Rect::new(10, 2, 8, 3),
                action: UiAction::SetPage(UiPage::Activity),
            }],
        };
        let action = ui.handle(UiCmd::MousePress(11, 3), &render);
        assert_eq!(action, Some(UiAction::SetPage(UiPage::Activity)));
        assert_eq!(ui.page(), UiPage::Activity);
    }

    #[test]
    fn tab_focus_activates_selected_button() {
        let mut ui = UiState::default();
        let render = RenderState {
            hits: vec![
                HitTarget {
                    area: Rect::new(0, 0, 5, 3),
                    action: UiAction::SetPage(UiPage::Overview),
                },
                HitTarget {
                    area: Rect::new(6, 0, 5, 3),
                    action: UiAction::SetPage(UiPage::Markets),
                },
            ],
        };
        assert_eq!(ui.handle(UiCmd::Next, &render), None);
        assert_eq!(
            ui.handle(UiCmd::Activate, &render),
            Some(UiAction::SetPage(UiPage::Markets))
        );
        assert_eq!(ui.page(), UiPage::Markets);
    }
}
