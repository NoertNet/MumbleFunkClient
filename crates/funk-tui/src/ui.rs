//! Drawing. Uses only the 8 named ANSI colours so it also works on the Linux text console, and has
//! an ASCII fallback for fonts without block characters.

use std::time::Instant;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, BANDS, Mode};

pub const MIN_WIDTH: u16 = 44;
pub const MIN_HEIGHT: u16 = 20;

pub fn mode_color(mode: Mode) -> Color {
    match mode {
        Mode::Idle | Mode::Lost => Color::Yellow,
        Mode::Tx => Color::Green,
        Mode::Rx | Mode::Busy => Color::Red,
    }
}

fn mode_text(mode: Mode) -> &'static str {
    match mode {
        Mode::Idle => "BEREIT",
        Mode::Tx => "SENDET",
        Mode::Rx => "EMPFANG",
        Mode::Busy => "BELEGT",
        Mode::Lost => "VERBINDE NEU",
    }
}

/// Letter-spaced text ("E I N S A T Z") if it still fits, otherwise as is.
fn spaced(text: &str, width: u16) -> String {
    let upper = text.to_uppercase();
    let wide: String = upper.chars().flat_map(|c| [c, ' ']).collect::<String>().trim_end().to_string();
    if wide.chars().count() as u16 + 2 <= width { wide } else { upper }
}

pub fn draw(f: &mut Frame, app: &App, now: Instant, ascii: bool) {
    let area = f.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = format!("Terminal zu klein ({}x{}). Mindestens {MIN_WIDTH}x{MIN_HEIGHT} nötig.", area.width, area.height);
        f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
        return;
    }
    let mode = app.mode();
    let color = mode_color(mode);
    let [display, keys, ptt] = Layout::vertical([Constraint::Min(14), Constraint::Length(1), Constraint::Length(5)]).areas(area);

    draw_display(f, display, app, mode, color, now, ascii);
    draw_keys(f, keys, app, now);
    draw_ptt(f, ptt, app, mode);
    if app.list_open {
        draw_channel_list(f, area, app);
    }
}

fn lamp(on: bool, color: Color, label: &str, ascii: bool) -> Vec<Span<'static>> {
    let dot = match (on, ascii) {
        (true, false) => "●",
        (false, false) => "○",
        (true, true) => "*",
        (false, true) => "o",
    };
    let style = if on { Style::new().fg(color).add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::DarkGray) };
    vec![Span::styled(format!("{dot} {label}"), style), Span::raw(" ")]
}

fn draw_display(f: &mut Frame, area: Rect, app: &App, mode: Mode, color: Color, now: Instant, ascii: bool) {
    let mut lamps: Vec<Span> = Vec::new();
    lamps.extend(lamp(mode != Mode::Lost, Color::Yellow, "NET", ascii));
    lamps.extend(lamp(matches!(mode, Mode::Rx | Mode::Busy), Color::Red, "RX", ascii));
    lamps.extend(lamp(mode == Mode::Tx, Color::Green, "TX", ascii));
    let block = Block::bordered()
        .border_style(Style::new().fg(color))
        .title(Line::from(" MF-1 · SIMPLEX ").left_aligned())
        .title(Line::from(lamps).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [head, _gap, name, status, who, users, sep, eq] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(inner);

    // top row: signal bars + ms | server | channel position
    let [sig_area, server_area, pos_area] =
        Layout::horizontal([Constraint::Length(16), Constraint::Min(1), Constraint::Length(12)]).areas(head);
    let (level, ms, stale) = app.signal(now);
    let glyphs: [&str; 4] = if ascii { ["_", "-", "=", "#"] } else { ["▂", "▄", "▆", "█"] };
    let mut sig: Vec<Span> = glyphs
        .iter()
        .enumerate()
        .map(|(i, g)| Span::styled(*g, Style::new().fg(if (i as u8) < level { color } else { Color::DarkGray })))
        .collect();
    let ms_text = match (ms, stale) {
        (None, _) => " — ms".to_string(),
        (Some(_), true) => " kein Signal".to_string(),
        (Some(v), false) => format!(" {v} ms"),
    };
    sig.push(Span::styled(ms_text, Style::new().fg(color)));
    f.render_widget(Paragraph::new(Line::from(sig)), sig_area);
    f.render_widget(
        Paragraph::new(app.server.to_uppercase()).alignment(Alignment::Center).style(Style::new().fg(color).add_modifier(Modifier::DIM)),
        server_area,
    );
    let pos = app.channel_position().map_or("KANAL".to_string(), |(i, n)| format!("KANAL {i}/{n}"));
    f.render_widget(Paragraph::new(pos).alignment(Alignment::Right).style(Style::new().fg(color)), pos_area);

    f.render_widget(
        Paragraph::new(spaced(&app.own_channel_name(), name.width))
            .alignment(Alignment::Center)
            .style(Style::new().fg(color).add_modifier(Modifier::BOLD)),
        name,
    );
    f.render_widget(
        Paragraph::new(spaced(mode_text(mode), status.width))
            .alignment(Alignment::Center)
            .style(Style::new().fg(color).add_modifier(Modifier::BOLD)),
        status,
    );
    let others = app.others_speaking().join(", ");
    let who_text = match mode {
        Mode::Idle | Mode::Tx => format!("ICH: {}", app.username),
        Mode::Rx => others.to_uppercase(),
        Mode::Busy => format!("{} SENDET", others.to_uppercase()),
        Mode::Lost => "VERBINDUNG VERLOREN".to_string(),
    };
    f.render_widget(Paragraph::new(who_text).alignment(Alignment::Center).style(Style::new().fg(color)), who);

    // users in the channel; the ones speaking right now are highlighted
    let own = app.st.own_channel();
    let mut spans: Vec<Span> = Vec::new();
    if let Some(ch) = own {
        for u in app.st.users_in(ch) {
            let mut text = u.name.to_uppercase();
            if Some(u.session) == app.st.own_session {
                text.push_str(" (ICH)");
            }
            let style = if app.speaking.contains(&u.session) {
                Style::new().fg(Color::Black).bg(color)
            } else {
                Style::new().fg(color)
            };
            spans.push(Span::styled(format!(" {text} "), style));
            spans.push(Span::raw(" "));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }), users);

    let head_line = if app.devices.is_empty() { "SPEKTRUM".to_string() } else { format!("SPEKTRUM · {}", app.devices) };
    f.render_widget(
        Paragraph::new(head_line).style(Style::new().fg(color).add_modifier(Modifier::DIM)),
        sep,
    );
    draw_eq(f.buffer_mut(), eq, &app.eq_cur, color, ascii);
}

/// 16 columns, bottom-up fill in eighths of a cell (block characters) or `#`/`.` in ASCII mode.
pub fn draw_eq(buf: &mut Buffer, area: Rect, vals: &[f32; BANDS], color: Color, ascii: bool) {
    if area.width < BANDS as u16 || area.height == 0 {
        return;
    }
    let gap = if area.width >= BANDS as u16 * 3 { 1 } else { 0 };
    let col_w = ((area.width - gap * (BANDS as u16 - 1)) / BANDS as u16).max(1);
    let used = BANDS as u16 * col_w + gap * (BANDS as u16 - 1);
    let left = area.x + (area.width - used) / 2;
    const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    for (i, &v) in vals.iter().enumerate() {
        let x0 = left + i as u16 * (col_w + gap);
        let total = (v.clamp(0.0, 1.0) * area.height as f32 * 8.0).round() as i32;
        for row in 0..area.height {
            let y = area.y + area.height - 1 - row;
            let fill = (total - row as i32 * 8).clamp(0, 8) as usize;
            let (sym, style) = match (fill, ascii) {
                (0, _) if row == 0 => (if ascii { "_" } else { "▁" }, Style::new().fg(Color::DarkGray)),
                (0, _) => continue,
                (f, false) => (EIGHTHS[f], Style::new().fg(color)),
                (f, true) => (if f >= 4 { "#" } else { "." }, Style::new().fg(color)),
            };
            for dx in 0..col_w {
                if let Some(cell) = buf.cell_mut((x0 + dx, y)) {
                    cell.set_symbol(sym).set_style(style);
                }
            }
        }
    }
}

fn draw_keys(f: &mut Frame, area: Rect, app: &App, now: Instant) {
    if let Some(n) = app.notice(now) {
        f.render_widget(
            Paragraph::new(format!(" {n}")).style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            area,
        );
        return;
    }
    f.render_widget(
        Paragraph::new(" ←/→ Kanal   l Liste   Leertaste PTT   q Ende").style(Style::new().fg(Color::DarkGray)),
        area,
    );
}

fn draw_ptt(f: &mut Frame, area: Rect, app: &App, mode: Mode) {
    let border = match mode {
        Mode::Tx => Color::Green,
        Mode::Busy => Color::Red,
        _ => Color::DarkGray,
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(border))
        .title(Line::from(format!(" PTT: {} ", app.ptt_info)).left_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let (text, style) = match mode {
        Mode::Tx => ("S E N D E T", Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Mode::Busy => ("BELEGT", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Mode::Lost => ("PTT GESPERRT", Style::new().fg(Color::DarkGray)),
        _ => ("P T T", Style::new().add_modifier(Modifier::BOLD)),
    };
    let [_, mid, _] = Layout::vertical([Constraint::Min(0), Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    f.render_widget(Paragraph::new(text).alignment(Alignment::Center).style(style), mid);
}

fn draw_channel_list(f: &mut Frame, area: Rect, app: &App) {
    let channels = app.channels_sorted();
    let h = (channels.len() as u16 + 2).clamp(5, area.height.saturating_sub(4));
    let w = 38.min(area.width.saturating_sub(4));
    let rect = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    let items: Vec<ListItem> = channels
        .iter()
        .map(|c| {
            let name = if c.name.is_empty() { "Root" } else { c.name.as_str() };
            let n = app.st.users_in(c.id).len();
            let here = if Some(c.id) == app.st.own_channel() { "▶ " } else { "  " };
            ListItem::new(format!("{here}{name} ({n})"))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.list_sel.min(channels.len().saturating_sub(1))));
    f.render_widget(Clear, rect);
    f.render_stateful_widget(
        List::new(items)
            .block(Block::bordered().title(" KANÄLE · Enter wählt · Esc schließt ").border_style(Style::new().fg(Color::Yellow)))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        rect,
        &mut state,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Msg, sample_app};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App, w: u16, h: u16, ascii: bool) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app, Instant::now(), ascii)).unwrap();
        let buf = t.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fg_of(app: &App, w: u16, h: u16, needle_row: u16, x: u16) -> Color {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app, Instant::now(), false)).unwrap();
        t.backend().buffer()[(x, needle_row)].fg
    }

    #[test]
    fn idle_screen_shows_channel_users_and_status() {
        let mut a = sample_app();
        a.apply(Msg::Latency(23), Instant::now());
        let s = render(&a, 70, 26, false);
        assert!(s.contains("E I N S A T Z"), "{s}");
        assert!(s.contains("B E R E I T"), "{s}");
        assert!(s.contains("ICH: Alpha1"), "{s}");
        assert!(s.contains("23 ms"), "{s}");
        assert!(s.contains("KANAL 2/3"), "{s}");
        assert!(s.contains("ALPHA1 (ICH)") && s.contains("BRAVO2"), "{s}");
        assert!(s.contains("PTT: HALTEN (Leertaste)"), "{s}");
        assert!(s.contains("NET") && s.contains("RX") && s.contains("TX"), "{s}");
    }

    #[test]
    fn receiving_is_red_and_names_the_speaker() {
        let mut a = sample_app();
        a.speaking = vec![2];
        let s = render(&a, 70, 26, false);
        assert!(s.contains("E M P F A N G"), "{s}");
        assert!(s.contains("BRAVO2"), "{s}");
        // top-left border corner carries the mode colour
        assert_eq!(fg_of(&a, 70, 26, 0, 0), Color::Red);
    }

    #[test]
    fn lost_connection_is_shown_in_amber_with_the_net_lamp_off() {
        let mut a = sample_app();
        a.reconnecting = true;
        let s = render(&a, 70, 26, false);
        assert!(s.contains("V E R B I N D E"), "{s}");
        assert!(s.contains("VERBINDUNG VERLOREN"), "{s}");
        assert!(s.contains("○ NET") && s.contains("PTT GESPERRT"), "{s}");
        assert_eq!(fg_of(&a, 70, 26, 0, 0), Color::Yellow);
    }

    #[test]
    fn transmitting_is_green() {
        let mut a = sample_app();
        a.tx = true;
        let s = render(&a, 70, 26, false);
        assert!(s.contains("S E N D E T"), "{s}");
        assert_eq!(fg_of(&a, 70, 26, 0, 0), Color::Green);
    }

    #[test]
    fn busy_when_pressing_while_someone_speaks() {
        let mut a = sample_app();
        a.tx = true;
        a.speaking = vec![2];
        let s = render(&a, 70, 26, false);
        assert!(s.contains("B E L E G T"), "{s}");
        assert!(s.contains("BRAVO2 SENDET"), "{s}");
        assert_eq!(fg_of(&a, 70, 26, 0, 0), Color::Red);
    }

    #[test]
    fn no_signal_and_unknown_latency() {
        let mut a = sample_app();
        assert!(render(&a, 70, 26, false).contains("— ms"));
        a.latency = Some((20, Instant::now() - std::time::Duration::from_secs(20)));
        assert!(render(&a, 70, 26, false).contains("kein Signal"));
    }

    #[test]
    fn equalizer_draws_bars_and_baseline() {
        let mut a = sample_app();
        let flat = render(&a, 70, 26, false);
        assert!(flat.contains('▁'), "idle baseline missing");
        // the 4th signal bar is a '█' too, so compare counts instead of presence
        let idle_blocks = flat.matches('█').count();
        a.eq_cur = [1.0; BANDS];
        let full_blocks = render(&a, 70, 26, false).matches('█').count();
        assert!(full_blocks > idle_blocks + 16, "idle {idle_blocks}, full {full_blocks}");
    }

    #[test]
    fn ascii_mode_uses_no_unicode_blocks() {
        let mut a = sample_app();
        a.eq_cur = [0.8; BANDS];
        a.speaking = vec![2];
        let s = render(&a, 70, 26, true);
        for c in ['▁', '▂', '▄', '▆', '█', '●', '○'] {
            assert!(!s.contains(c), "unexpected {c} in ASCII mode");
        }
        assert!(s.contains('#'));
    }

    #[test]
    fn too_small_terminal_gets_a_message() {
        let s = render(&sample_app(), 30, 10, false);
        assert!(s.contains("Terminal zu klein"), "{s}");
    }

    #[test]
    fn channel_list_popup() {
        let mut a = sample_app();
        a.list_open = true;
        a.list_sel = 1;
        let s = render(&a, 70, 26, false);
        assert!(s.contains("KANÄLE") && s.contains("Einsatz Nord (2)") && s.contains("Kanal 7 (1)"), "{s}");
    }

    #[test]
    fn fits_the_smallest_supported_size() {
        let mut a = sample_app();
        a.eq_cur = [0.5; BANDS];
        let s = render(&a, MIN_WIDTH, MIN_HEIGHT, false);
        assert!(s.contains("PTT") && s.contains("BEREIT") || s.contains("B E R E I T"), "{s}");
    }
}
