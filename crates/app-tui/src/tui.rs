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

use trading_core::market::quote;
use trading_core::market::types::Side;
use trading_core::snapshot::{MarketSnapshot, WorldSnapshot};

struct PairView<'a> {
    symbol: String,
    window_label: String,
    expiry_secs: u64,
    up: Option<&'a MarketSnapshot>,
    down: Option<&'a MarketSnapshot>,
}

pub fn render(f: &mut Frame, snapshot: &WorldSnapshot) {
    let area = f.area();
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(18),
        Constraint::Length(12),
        Constraint::Length(3),
    ])
    .split(area);

    let pairs = grouped_markets(snapshot, snapshot.entry_threshold);

    render_header(f, snapshot, rows[0]);

    let body =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(rows[1]);
    render_markets(f, &pairs, body[0]);
    render_focus(f, &pairs, body[1]);
    render_lower(f, snapshot, &pairs, rows[2]);
    render_footer(f, &pairs, rows[3]);
}

fn render_header(f: &mut Frame, snapshot: &WorldSnapshot, area: Rect) {
    let header = Line::from(vec![
        Span::styled(
            "LIVE SWEEP",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}  BTC ${:.0}  mkts {}  elig {}  ticket ${:.2}  buy <= {}  exit floor {}  cash ${:.2}  eq ${:.2}  rpnl ${:.2}  upnl ${:.2}  open {}  sig {:.2}\u{03c3}",
            if snapshot.paper_real_mode { "paper-real" } else { "wallet-live" },
            snapshot.btc_spot,
            snapshot.markets.len() / 2,
            snapshot.eligible_markets,
            snapshot.ticket_dollars,
            quote::cents_label(snapshot.entry_threshold),
            quote::cents_label(snapshot.exit_threshold),
            snapshot.cash,
            snapshot.equity,
            snapshot.realized_pnl,
            snapshot.unrealized_pnl,
            snapshot.open_positions,
            snapshot.signal_strength,
        )),
    ]);

    f.render_widget(
        Paragraph::new(header).block(Block::bordered().border_type(BorderType::Rounded)),
        area,
    );
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
                    " {} {} x {:.1}",
                    if intent.aggressive {
                        "buy now"
                    } else {
                        "resting"
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
    render_best_setups(f, snapshot, pairs, cols[0]);
    render_activity(f, snapshot, cols[1]);
}

fn render_best_setups(f: &mut Frame, snapshot: &WorldSnapshot, pairs: &[PairView<'_>], area: Rect) {
    let actionable = pairs
        .iter()
        .filter_map(|pair| {
            let best = best_side(pair)?;
            (best.position_qty <= 0.0
                && quote::valid_live_quote(best.best_bid, best.best_ask)
                && best.best_ask <= snapshot.entry_threshold
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
                            snapshot.exit_threshold
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
                .title(" Planner + Activity "),
        ),
        area,
    );
}

fn render_footer(f: &mut Frame, pairs: &[PairView<'_>], area: Rect) {
    let footer = pairs
        .first()
        .and_then(best_side)
        .map(|best| {
            format!(
                "[q] quit  * {} {} {} {} / {} {}",
                best.symbol,
                best.window_label,
                side_tag(best.side),
                quote::cents_label(best.best_ask),
                quote::cents_label(best.fair_value),
                signed_cents(best.edge_buy),
            )
        })
        .unwrap_or_else(|| "[q] quit  bootstrapping...".to_string());
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

fn spread_label(best_bid: f64, best_ask: f64) -> String {
    quote::spread_cents_label(best_bid, best_ask)
}

fn candidate_exit_target(best_ask: f64, exit_floor: f64) -> f64 {
    if best_ask > 0.0 {
        (best_ask + 0.01).clamp(exit_floor, 0.99)
    } else {
        exit_floor
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
