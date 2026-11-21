//! Routes & Project Profiles tab: active project profile summary, saved
//! profiles, `routing.toml` groups, ranked candidate rows with share
//! percentages and exclusion reasons.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use super::{line, push_fit};
use crate::core::dashboard::RouteGroupSnapshot;
use crate::core::format::{self, fit, percent_text};
use crate::core::profile::{ProfileSource, ProfileSummary};
use crate::core::route::RouteRow;
use crate::core::route_config::Ranking;
use crate::tui::app::{AppState, RouteItem, RouteItemKind};
use crate::tui::theme;

fn source_label(src: ProfileSource) -> &'static str {
    match src {
        ProfileSource::Local => "local",
        ProfileSource::Global => "global",
        ProfileSource::Explicit => "explicit",
    }
}

fn ranking_label(r: Ranking) -> &'static str {
    match r {
        Ranking::Smart => "smart",
        Ranking::Priority => "priority",
        Ranking::LeastUsed => "least-used",
    }
}

fn render_active_profile_banner(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(
            " Active Project Profile ",
            theme::card_title(),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(d) = &state.data else {
        return;
    };
    let mut spans = Vec::new();
    let w = usize::from(inner.width);
    if let Some(prof) = &d.active_profile {
        let name = prof
            .name
            .as_deref()
            .unwrap_or_else(|| source_label(prof.source));
        let mut parts = vec![format!("◈ {name} ({})", source_label(prof.source))];
        if let Some(a) = &prof.spec.account {
            parts.push(format!("acct:{a}"));
        }
        if let Some(ep) = &prof.spec.endpoint {
            parts.push(format!("ep:{ep}"));
        }
        if let Some(g) = &prof.spec.group {
            parts.push(format!("group:{g}"));
        }
        if let Some(m) = &prof.spec.model {
            parts.push(format!("model:{m}"));
        }
        push_fit(&mut spans, &parts.join("  ·  "), theme::badge_profile(), w);
    } else {
        push_fit(
            &mut spans,
            "No active profile (.agyva.toml or default). Select a profile below and press Enter.",
            theme::dim(),
            w,
        );
    }
    f.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn profile_row(
    p: &ProfileSummary,
    num: usize,
    active_name: Option<&str>,
    width: usize,
) -> Line<'static> {
    let is_active = active_name == Some(p.name.as_str());
    let mut spans = Vec::new();
    let mut room = width;
    let mark = if is_active { "\u{25cf}" } else { " " };
    room = push_fit(&mut spans, mark, theme::active_marker(), room);
    room = push_fit(&mut spans, " ", theme::plain(), room);
    room = push_fit(&mut spans, &format!("{num:>2} "), theme::dim(), room);
    room = push_fit(&mut spans, "[PROF] ", theme::badge_profile(), room);

    let name_w = width.saturating_sub(28).clamp(8, 20);
    let name_style = if is_active {
        theme::bold()
    } else {
        theme::plain()
    };
    room = push_fit(&mut spans, &fit(&p.name, name_w), name_style, room);

    let mut meta = vec![source_label(p.source).to_owned()];
    if p.is_default {
        meta.push("default".to_owned());
    }
    if let Some(a) = &p.account {
        meta.push(format!("acct:{a}"));
    }
    if let Some(ep) = &p.endpoint {
        meta.push(format!("ep:{ep}"));
    }
    if let Some(g) = &p.group {
        meta.push(format!("grp:{g}"));
    }
    push_fit(
        &mut spans,
        &format!(" {}", meta.join(" · ")),
        theme::dim(),
        room,
    );
    Line::from(spans)
}

fn group_row(g: &RouteGroupSnapshot, num: usize, width: usize) -> Line<'static> {
    let rec_alias = g.plan.recommendation.as_deref().and_then(|id| {
        g.plan
            .accounts
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.alias.as_str())
    });
    let mut spans = Vec::new();
    let mut room = width;
    let mark = if rec_alias.is_some() { "\u{25b8}" } else { "!" };
    let mark_style = if rec_alias.is_some() {
        theme::good()
    } else {
        theme::warn()
    };
    room = push_fit(&mut spans, mark, mark_style, room);
    room = push_fit(&mut spans, " ", theme::plain(), room);
    room = push_fit(&mut spans, &format!("{num:>2} "), theme::dim(), room);
    room = push_fit(&mut spans, "[ROUT] ", theme::badge_proto(), room);

    let name_w = width.saturating_sub(28).clamp(8, 20);
    room = push_fit(&mut spans, &fit(&g.name, name_w), theme::bold(), room);

    let rec_str = match rec_alias {
        Some(a) => format!("-> {a}"),
        None => "no eligible".to_owned(),
    };
    let auto = if g.auto_switch { " · auto" } else { "" };
    let info = format!(" [{}]{auto} {rec_str}", ranking_label(g.ranking));
    let info_style = if rec_alias.is_some() {
        theme::dim()
    } else {
        theme::warn()
    };
    push_fit(&mut spans, &info, info_style, room);
    Line::from(spans)
}

fn candidate_lines(
    rank_idx: usize,
    r: &RouteRow,
    state: &AppState,
    width: usize,
) -> Vec<Line<'static>> {
    let mut spans = Vec::new();
    let mut room = width;
    let act = if r.active { "\u{25cf}" } else { " " };
    room = push_fit(&mut spans, act, theme::active_marker(), room);
    room = push_fit(&mut spans, " ", theme::plain(), room);
    room = push_fit(
        &mut spans,
        &format!("#{:<2} ", rank_idx + 1),
        theme::dim(),
        room,
    );

    let (badge, b_style) = if r.eligible {
        ("[OK] ", theme::good())
    } else {
        ("[NO] ", theme::bad())
    };
    room = push_fit(&mut spans, badge, b_style, room);

    let alias_w = width.saturating_sub(34).clamp(8, 18);
    room = push_fit(
        &mut spans,
        &fit(&r.alias, alias_w),
        if r.active {
            theme::bold()
        } else {
            theme::plain()
        },
        room,
    );

    let share = r
        .remaining_percent
        .map_or_else(|| "\u{2014}".to_owned(), percent_text);
    let share_style = if !r.eligible {
        theme::dim()
    } else if r.remaining_percent.is_some_and(|v| v < 20.0) {
        theme::bad()
    } else if r.remaining_percent.is_some_and(|v| v < 50.0) {
        theme::warn()
    } else {
        theme::good()
    };
    room = push_fit(&mut spans, &format!(" {:>4}", share), share_style, room);

    if let Some(rate) = r.rate_remaining_percent {
        room = push_fit(
            &mut spans,
            &format!(" 5h:{}", percent_text(rate)),
            theme::dim(),
            room,
        );
    }
    if let Some(wk_reset) = r.renewals[0] {
        push_fit(
            &mut spans,
            &format!(" rst {}", format::until(state.now, wk_reset)),
            theme::dim(),
            room,
        );
    }

    let reason_line = Line::from(vec![
        Span::styled("       ", theme::plain()),
        Span::styled(
            format::truncate(&r.why, width.saturating_sub(7)),
            if r.eligible {
                theme::dim()
            } else {
                theme::warn()
            },
        ),
    ]);
    vec![Line::from(spans), reason_line]
}

fn render_detail_pane(item: &RouteItem<'_>, state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let (title, body): (String, Vec<Line<'static>>) = match item.kind {
        RouteItemKind::Profile(p) => {
            let mut out = vec![line(
                format!(
                    "Profile: {} ({}{})",
                    p.name,
                    source_label(p.source),
                    if p.is_default { ", default" } else { "" }
                ),
                theme::bold(),
            )];
            if let Some(desc) = &p.description {
                out.push(line(desc.clone(), theme::plain()));
            }
            if let Some(path) = &p.path {
                out.push(line(format!("Path:     {}", path.display()), theme::dim()));
            }
            out.push(line(
                format!("Account:  {}", p.account.as_deref().unwrap_or("—")),
                theme::plain(),
            ));
            out.push(line(
                format!("Endpoint: {}", p.endpoint.as_deref().unwrap_or("—")),
                theme::plain(),
            ));
            out.push(line(
                format!("Group:    {}", p.group.as_deref().unwrap_or("—")),
                theme::plain(),
            ));
            out.push(line(
                format!("Model:    {}", p.model.as_deref().unwrap_or("—")),
                theme::plain(),
            ));
            if p.warn_percent.is_some() || p.critical_percent.is_some() {
                out.push(line(
                    format!(
                        "Alerts:   warn {}  crit {}",
                        p.warn_percent
                            .map_or_else(|| "—".to_owned(), |v| format!("{v:.0}%")),
                        p.critical_percent
                            .map_or_else(|| "—".to_owned(), |v| format!("{v:.0}%")),
                    ),
                    theme::dim(),
                ));
            }
            if !p.env.is_empty() {
                let keys: Vec<&str> = p.env.keys().map(String::as_str).collect();
                out.push(line(format!("Env keys: {}", keys.join(", ")), theme::dim()));
            }
            (format!(" Profile: {} ", p.name), out)
        }
        RouteItemKind::Group(g) => {
            let mut out = vec![line(
                format!(
                    "Ranking: {}  ·  Auto-switch: {}",
                    ranking_label(g.ranking),
                    if g.auto_switch { "on" } else { "off" }
                ),
                theme::dim(),
            )];
            let rec_name = g
                .plan
                .recommendation
                .as_deref()
                .and_then(|id| g.plan.accounts.iter().find(|r| r.id == id))
                .map(|r| r.alias.as_str());
            match rec_name {
                Some(name) => out.push(line(format!("Recommended: {name}"), theme::good())),
                None => out.push(line("Recommended: none eligible", theme::warn())),
            }
            out.push(Line::default());
            let inner_w = usize::from(area.width.saturating_sub(2));
            if g.plan.accounts.is_empty() {
                out.push(line("No candidate accounts in this group.", theme::dim()));
            } else {
                for (idx, row) in g.plan.accounts.iter().enumerate() {
                    out.extend(candidate_lines(idx, row, state, inner_w));
                }
            }
            (format!(" Candidates: {} ", g.name), out)
        }
    };

    let bordered = area.width >= 30 && area.height >= 6;
    if bordered {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(title, theme::card_title()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(body).wrap(Wrap { trim: true }), inner);
    } else {
        f.render_widget(Paragraph::new(body).wrap(Wrap { trim: true }), area);
    }
}

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let visible = state.visible_route_items();
    if visible.is_empty() {
        let text = if state.route_items().is_empty() {
            if state.data.is_none() {
                "Loading profiles and routes..."
            } else {
                "No profiles or routing groups configured. Run `agyva profile init` or `agyva route init`."
            }
        } else {
            "No profile or route group matches the filter."
        };
        f.render_widget(
            Paragraph::new(line(text, theme::dim())).wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let rest = if area.width >= 60 && area.height >= 12 {
        let [top, rest] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        render_active_profile_banner(state, f, top);
        rest
    } else {
        area
    };

    let (list_area, detail_area) = if rest.width >= 80 && rest.height >= 8 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)])
                .areas(rest);
        (left, Some(right))
    } else if rest.width >= 60 && rest.height >= 14 {
        let [top, bottom] =
            Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(rest);
        (top, Some(bottom))
    } else {
        (rest, None)
    };

    let bordered = list_area.width >= 40 && list_area.height >= 6;
    let inner = if bordered {
        let title = format!(
            " Profiles & Routes ({}/{}) ",
            visible.len(),
            state.route_items().len()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(title, theme::card_title()));
        let inner = block.inner(list_area);
        f.render_widget(block, list_area);
        inner
    } else {
        list_area
    };

    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    if width > 0 && height > 0 {
        let sel = state.selected_route_index().unwrap_or(0);
        let active_name = state.active_profile_name();
        let rows = height;
        let offset = sel
            .saturating_sub(rows / 2)
            .min(visible.len().saturating_sub(rows));
        let mut lines = Vec::new();
        for (i, item) in visible.iter().enumerate().skip(offset).take(rows) {
            let mut l = match item.kind {
                RouteItemKind::Profile(p) => {
                    profile_row(p, item.number, active_name.as_deref(), width)
                }
                RouteItemKind::Group(g) => group_row(g, item.number, width),
            };
            if i == sel {
                l = l.style(theme::selected());
            }
            lines.push(l);
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    if let (Some(d_area), Some(item)) = (detail_area, state.selected_route_item()) {
        render_detail_pane(&item, state, f, d_area);
    }
}
