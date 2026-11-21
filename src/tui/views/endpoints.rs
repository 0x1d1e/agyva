//! Custom LLM endpoints tab: protocol badges, active marker, default model,
//! model count, latency, rate-limit/credits summary, and cached errors.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use super::{line, push_fit};
use crate::core::app_integrations::EndpointView;
use crate::core::format::{self, ago, fit};
use crate::core::protocol::{EndpointProtocol, RateLimitInfo};
use crate::tui::app::AppState;
use crate::tui::theme;

const MARK_W: usize = 2;
const NUM_W: usize = 3;
const PROTO_W: usize = 11;
const MODEL_W: usize = 18;
const COUNT_W: usize = 7;
const LAT_W: usize = 7;
const RATE_W: usize = 18;
const ALIAS_MIN: usize = 6;

pub(crate) fn protocol_badge(ep: &EndpointView) -> &'static str {
    match ep.detected_protocol.unwrap_or(ep.protocol) {
        EndpointProtocol::OpenAi => "[OPENAI]",
        EndpointProtocol::OpenAiResponses => "[RESPONSES]",
        EndpointProtocol::Anthropic => "[ANTHROPIC]",
        EndpointProtocol::Gemini => "[GEMINI]",
        EndpointProtocol::AzureOpenAi => "[AZURE]",
        EndpointProtocol::Ollama => "[OLLAMA]",
        EndpointProtocol::Cohere => "[COHERE]",
        EndpointProtocol::Custom => "[CUSTOM]",
        EndpointProtocol::Auto => "[AUTO]",
    }
}

pub(crate) fn rate_summary(rl: &RateLimitInfo) -> Option<String> {
    if let (Some(rem), Some(lim)) = (rl.requests_remaining, rl.requests_limit) {
        return Some(format!("req {rem}/{lim}"));
    }
    if let (Some(rem), Some(lim)) = (rl.tokens_remaining, rl.tokens_limit) {
        return Some(format!("tok {rem}/{lim}"));
    }
    if let Some(credits) = rl.credits_remaining {
        if let Some(lim) = rl.credits_limit {
            return Some(format!("${credits:.2}/${lim:.2}"));
        }
        return Some(format!("${credits:.2} left"));
    }
    if let Some(hint) = &rl.reset_hint {
        return Some(format!("reset {hint}"));
    }
    None
}

#[derive(Debug, Clone, Copy)]
struct EpColumnPlan {
    proto: bool,
    model: bool,
    count: bool,
    latency: bool,
    status: bool,
    alias: usize,
}

fn ep_column_plan(width: usize) -> EpColumnPlan {
    let mut used = MARK_W + NUM_W + ALIAS_MIN;
    let mut take = |w: usize| {
        let ok = used + w < width;
        if ok {
            used += w + 1;
        }
        ok
    };
    let proto = take(PROTO_W);
    let latency = proto && take(LAT_W);
    let count = latency && take(COUNT_W);
    let model = count && take(MODEL_W);
    let status = model && take(RATE_W);
    EpColumnPlan {
        proto,
        model,
        count,
        latency,
        status,
        alias: width.saturating_sub(used - ALIAS_MIN).max(1),
    }
}

fn status_cell(ep: &EndpointView) -> (String, Style) {
    if !ep.enabled {
        return ("disabled".to_owned(), theme::dim());
    }
    if let Some(err) = &ep.last_error {
        return (format!("err: {}", format::sanitize(err)), theme::bad());
    }
    if let Some(rl) = &ep.rate_limits
        && let Some(summary) = rate_summary(rl)
    {
        return (summary, theme::info());
    }
    if ep.fetched_at.is_some() {
        return ("ready".to_owned(), theme::good());
    }
    ("unprobed".to_owned(), theme::dim())
}

fn row(ep: &EndpointView, num: usize, plan: EpColumnPlan, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mark = if ep.active { "\u{25cf}" } else { " " };
    let flag = if !ep.enabled {
        "-"
    } else if ep.last_error.is_some() {
        "!"
    } else {
        " "
    };
    let flag_style = if ep.last_error.is_some() {
        theme::bad()
    } else {
        theme::dim()
    };
    let mut room = width;
    room = push_fit(&mut spans, mark, theme::active_marker(), room);
    room = push_fit(&mut spans, flag, flag_style, room);
    room = push_fit(&mut spans, &format!("{num:>2} "), theme::dim(), room);
    let alias_style = if ep.active {
        theme::bold()
    } else if !ep.enabled {
        theme::dim()
    } else {
        theme::plain()
    };
    room = push_fit(&mut spans, &fit(&ep.alias, plan.alias), alias_style, room);

    if plan.proto {
        room = push_fit(
            &mut spans,
            &format!(" {}", fit(protocol_badge(ep), PROTO_W)),
            theme::badge_proto(),
            room,
        );
    }
    if plan.model {
        let model = ep.default_model.as_deref().unwrap_or("\u{2014}");
        let st = if ep.default_model.is_some() {
            theme::plain()
        } else {
            theme::dim()
        };
        room = push_fit(&mut spans, &format!(" {}", fit(model, MODEL_W)), st, room);
    }
    if plan.count {
        let cnt = if ep.model_count > 0 {
            format!("{}m", ep.model_count)
        } else {
            "\u{2014}".to_owned()
        };
        let st = if ep.model_count > 0 {
            theme::plain()
        } else {
            theme::dim()
        };
        room = push_fit(&mut spans, &format!(" {}", fit(&cnt, COUNT_W)), st, room);
    }
    if plan.latency {
        let lat = ep
            .latency_ms
            .map_or_else(|| "\u{2014}".to_owned(), |ms| format!("{ms}ms"));
        let st = match ep.latency_ms {
            Some(ms) if ms < 300 => theme::good(),
            Some(ms) if ms < 1000 => theme::warn(),
            Some(_) => theme::bad(),
            None => theme::dim(),
        };
        room = push_fit(&mut spans, &format!(" {}", fit(&lat, LAT_W)), st, room);
    }
    if plan.status {
        let (txt, st) = status_cell(ep);
        push_fit(&mut spans, &format!(" {}", fit(&txt, RATE_W)), st, room);
    }
    Line::from(spans)
}

fn header_row(plan: EpColumnPlan) -> Line<'static> {
    let mut t = format!("{}{}", fit("", MARK_W + NUM_W), fit("ENDPOINT", plan.alias));
    if plan.proto {
        t.push_str(&format!(" {}", fit("PROTOCOL", PROTO_W)));
    }
    if plan.model {
        t.push_str(&format!(" {}", fit("MODEL", MODEL_W)));
    }
    if plan.count {
        t.push_str(&format!(" {}", fit("MODELS", COUNT_W)));
    }
    if plan.latency {
        t.push_str(&format!(" {}", fit("LATENCY", LAT_W)));
    }
    if plan.status {
        t.push_str(&format!(" {}", fit("STATUS / LIMITS", RATE_W)));
    }
    line(t, theme::dim())
}

fn mini_card(f: &mut Frame<'_>, area: Rect, title: &str, value: String, value_style: Style) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(format!(" {title} "), theme::card_title()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width > 0 && inner.height > 0 {
        let mut spans = Vec::new();
        push_fit(&mut spans, &value, value_style, usize::from(inner.width));
        f.render_widget(Paragraph::new(Line::from(spans)), inner);
    }
}

fn render_summary_strip(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let eps = state.endpoints();
    let active = eps.iter().find(|e| e.active);
    let active_val = match active {
        Some(ep) => (format!("\u{25cf} {}", ep.alias), theme::active_marker()),
        None => ("none (OAuth)".to_owned(), theme::dim()),
    };
    let enabled_count = eps.iter().filter(|e| e.enabled).count();
    let ready_count = eps
        .iter()
        .filter(|e| e.enabled && e.last_error.is_none())
        .count();
    let ready_val = (
        format!("{ready_count}/{} ready ({enabled_count} on)", eps.len()),
        if ready_count > 0 {
            theme::good()
        } else {
            theme::warn()
        },
    );

    if area.width >= 90 {
        let cols = Layout::horizontal([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .areas::<3>(area);
        let sel_val = match state.selected_endpoint() {
            Some(ep) if ep.last_error.is_some() => ("cached error".to_owned(), theme::bad()),
            Some(ep) => match ep.latency_ms {
                Some(ms) => (format!("{ms}ms · {} models", ep.model_count), theme::good()),
                None => ("not probed".to_owned(), theme::dim()),
            },
            None => ("\u{2014}".to_owned(), theme::dim()),
        };
        mini_card(f, cols[0], "Active Endpoint", active_val.0, active_val.1);
        mini_card(f, cols[1], "Health", ready_val.0, ready_val.1);
        mini_card(f, cols[2], "Selected Probe", sel_val.0, sel_val.1);
    } else {
        let cols =
            Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).areas::<2>(area);
        mini_card(f, cols[0], "Active Endpoint", active_val.0, active_val.1);
        mini_card(f, cols[1], "Health", ready_val.0, ready_val.1);
    }
}

fn render_selected_preview(ep: &EndpointView, state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(
            format!(" Selected: {} {} ", ep.alias, protocol_badge(ep)),
            theme::card_title(),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut preview = Vec::new();
    let probed = ep
        .fetched_at
        .map_or_else(|| "never probed".to_owned(), |t| ago(state.now, t));
    preview.push(line(
        format!(
            "URL: {}  ·  Default: {}  ·  Probed: {probed}",
            ep.base_url,
            ep.default_model.as_deref().unwrap_or("none")
        ),
        theme::plain(),
    ));
    if let Some(err) = &ep.last_error {
        preview.push(line(format!("Error: {err}"), theme::bad()));
    } else if let Some(rl) = &ep.rate_limits
        && !rl.is_empty()
    {
        let mut parts = Vec::new();
        if let (Some(r), Some(l)) = (rl.requests_remaining, rl.requests_limit) {
            parts.push(format!("requests {r}/{l}"));
        }
        if let (Some(r), Some(l)) = (rl.tokens_remaining, rl.tokens_limit) {
            parts.push(format!("tokens {r}/{l}"));
        }
        if let Some(c) = rl.credits_remaining {
            parts.push(format!("credits ${c:.2}"));
        }
        if let Some(h) = &rl.reset_hint {
            parts.push(format!("reset {h}"));
        }
        preview.push(line(
            format!("Rate limits: {}", parts.join(" · ")),
            theme::info(),
        ));
    } else if !ep.models.is_empty() {
        let sample: Vec<&str> = ep.models.iter().take(4).map(|m| m.id.as_str()).collect();
        let more = if ep.models.len() > 4 {
            format!(" (+{} more)", ep.models.len() - 4)
        } else {
            String::new()
        };
        preview.push(line(
            format!("Models: {}{more}", sample.join(", ")),
            theme::dim(),
        ));
    } else {
        preview.push(line(
            "Press t to test connection, f to fetch models, d for full details.",
            theme::dim(),
        ));
    }
    f.render_widget(Paragraph::new(preview).wrap(Wrap { trim: true }), inner);
}

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let visible = state.visible_endpoints();
    if visible.is_empty() {
        let text = if state.endpoints().is_empty() {
            if state.data.is_none() {
                "Loading endpoints..."
            } else {
                "No custom endpoints configured. Run `agyva endpoint add` to register one."
            }
        } else {
            "No endpoint matches the filter."
        };
        f.render_widget(
            Paragraph::new(line(text, theme::dim())).wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let rest = if area.width >= 60 && area.height >= 12 {
        let [strip, rest] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        render_summary_strip(state, f, strip);
        rest
    } else {
        area
    };

    let (table_area, preview_area) = if rest.width >= 60 && rest.height >= 14 {
        let [top, bottom] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(5)]).areas(rest);
        (top, Some(bottom))
    } else {
        (rest, None)
    };

    let bordered = table_area.width >= 60 && table_area.height >= 6;
    let inner = if bordered {
        let title = format!(
            " Custom Endpoints ({}/{}) ",
            visible.len(),
            state.endpoints().len()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(title, theme::card_title()));
        let inner = block.inner(table_area);
        f.render_widget(block, table_area);
        inner
    } else {
        table_area
    };

    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    if width > 0 && height > 0 {
        let plan = ep_column_plan(width);
        let with_header = height >= 4;
        let rows = height.saturating_sub(if with_header { 2 } else { 0 });
        let sel = state.selected_endpoint_index().unwrap_or(0);
        let offset = sel
            .saturating_sub(rows / 2)
            .min(visible.len().saturating_sub(rows));
        let mut lines = Vec::new();
        if with_header {
            lines.push(header_row(plan));
            lines.push(line(
                "Enter activate  t test  f fetch models  d details",
                theme::dim(),
            ));
        }
        let all_eps = state.endpoints();
        for (i, ep) in visible.iter().enumerate().skip(offset).take(rows) {
            let num = all_eps
                .iter()
                .position(|x| x.id == ep.id)
                .map_or(i + 1, |p| p + 1);
            let mut l = row(ep, num, plan, width);
            if i == sel {
                l = l.style(theme::selected());
            }
            lines.push(l);
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    if let (Some(p_area), Some(ep)) = (preview_area, state.selected_endpoint()) {
        render_selected_preview(ep, state, f, p_area);
    }
}
