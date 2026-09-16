//! The discrete pieces a frame is assembled from: the overlay pane, assistant rows, the tool row,
//! the plan box, the diff box and the verify line.
//!
//! Each takes its payload and returns rendered lines, so `paint` decides placement and these decide
//! appearance.

use super::*;

/// Draw the informational overlay, applying `scroll` (rows hidden above the top) clamped so the last
/// page is the furthest you can go. Returns the CLAMPED scroll so the caller can write it back — a
/// PageDown past the end then reads as "at the bottom" rather than drifting into empty space.
pub(super) fn draw_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    overlay: &OverlaySnapshot,
    scroll: usize,
) -> (usize, Rect) {
    let width = area.width.saturating_sub(4).min(84).max(20);
    let height = (overlay.lines.len() as u16 + 4)
        .min(area.height.saturating_sub(2))
        .max(5);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = FrameBlock::default()
        .borders(Borders::ALL)
        .title(overlay.title.clone())
        .border_style(Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT_DIM)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let mut lines: Vec<Line<'static>> = overlay
        .lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let selected = overlay.selected == Some(i);
            Line::styled(
                if selected {
                    format!("› {line}")
                } else {
                    format!("  {line}")
                },
                if selected {
                    Style::default()
                        .fg(Color::Indexed(crate::ui::theme::ACCENT))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                },
            )
        })
        .collect();
    if !overlay.hint.is_empty() {
        lines.push(Line::styled(
            overlay.hint.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // Clamp scroll so the final page is the furthest reachable position (never scroll past the end).
    let visible = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    let clamped = scroll.min(max_scroll);
    // A SELECTABLE overlay is a menu: its rows must stay one screen row each so a mouse click can be
    // mapped back to a row index (`overlay_menu_hit`). Long rows are clipped at the panel edge, not
    // wrapped — wrapping would break the row↔line mapping the hit-test depends on. Informational
    // overlays (no selection) keep wrapping and publish no geometry.
    let selectable = overlay.selected.is_some();
    let paragraph = if selectable {
        Paragraph::new(lines).scroll((clamped.min(u16::MAX as usize) as u16, 0))
    } else {
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((clamped.min(u16::MAX as usize) as u16, 0))
    };
    frame.render_widget(paragraph, inner);
    set_overlay_menu(selectable.then_some(OverlayMenuGeom {
        inner,
        scroll: clamped,
        rows: overlay.lines.len(),
    }));
    (clamped, rect)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

pub(super) fn render_assistant_rows(raw: &str, width: usize) -> Vec<String> {
    // ONE renderer for both surfaces. The live turn (streaming) and the replayed transcript
    // (`agent::replay_transcript` → `MarkdownStream`) must produce byte-identical output, or the same
    // message looks different when re-opened than when first shown. So feed the whole raw block through
    // `MarkdownStream` here too — the block is cached by `content_hash`, so re-parsing the full body on
    // each width/content change is the same trip `replay_transcript` makes, with none of the
    // incremental-splice risk an in-place streaming parser would carry.
    //
    // Keep SGR: the renderer emits the moonlight gutter, code-box borders, and syntax highlight as
    // colour codes — `sanitize_keep_sgr` preserves them (dropping only cursor moves) and `ansi_spans`
    // turns them into styled spans at draw time.
    let mut md = crate::ui::markdown::MarkdownStream::new(true, width.max(24));
    let mut rendered = md.push(&format!("{raw}\n"));
    rendered.push_str(&md.finish());
    sanitize_keep_sgr(&rendered)
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// Format a run time for the result line: `· 940ms` under a second, `· 1.2s` under a minute, then
/// `· 7m03s` / `· 2h05m`. Sub-second times keep millisecond resolution; the minute/hour tiers exist
/// because a sub-agent dispatch is a single tool call that can run for hours, and `· 7203.4s` makes
/// the reader do the division. Empty for `None` (unknown — restored transcripts / eager-adopted).
pub(crate) fn fmt_elapsed(ms: Option<u64>) -> String {
    match ms {
        None => String::new(),
        Some(ms) if ms < 1000 => format!(" · {ms}ms"),
        Some(ms) if ms < 60_000 => format!(" · {:.1}s", ms as f64 / 1000.0),
        Some(ms) => format!(" · {}", crate::agent::orchestration::fmt_secs(ms / 1000)),
    }
}

/// Lay out one tool-call line under the transcript, mockup-style but result-below: the call
/// `<icon> <name>   <target>` on its own line (icon + name in the tool's WORK-LANE colour under
/// the `lanes` theme — blue read, gold edit, mauve shell, cyan web, violet memory, pink
/// delegation/asking, teal plan — so a glance down the transcript shows what kind of work
/// happened; the default `moonlight` theme paints them silver; target dim silver), then an
/// indented `└ <digest> · <time>` line beneath it — the result digest tinted by state (running =
/// faint, ok = green, err = salmon) and carrying the wall-clock run time. A still-running call
/// (empty digest) is just the call line; the result line is added when the digest lands.
pub(crate) fn render_tool_row(t: &ToolEvent, width: usize) -> String {
    use crate::ui::theme;
    let icon = if t.icon.is_empty() {
        String::new()
    } else {
        format!("{} ", t.icon)
    };
    let name_styled = theme::lane(&t.name, &t.name).to_string();
    let call_line = if t.target.is_empty() {
        format!("{}{}", theme::lane(&t.name, &icon), name_styled)
    } else {
        format!(
            "{}{}   {}",
            theme::lane(&t.name, &icon),
            name_styled,
            theme::accent_dim(&t.target)
        )
    };
    if t.digest.is_empty() {
        return call_line;
    }
    // Result on the line below: `└ <digest> · <time>`, digest tinted by outcome, time dimmed.
    let digest_styled = match t.state {
        ToolState::Running => theme::faint(&t.digest).to_string(),
        ToolState::Ok => theme::ok(&t.digest).to_string(),
        ToolState::Err => theme::err(&t.digest).to_string(),
    };
    let time = fmt_elapsed(t.elapsed_ms);
    let time_styled = if time.is_empty() {
        String::new()
    } else {
        theme::faint(&time).to_string()
    };
    let mut out = format!(
        "{call_line}\n{} {digest_styled}{time_styled}",
        theme::faint("└")
    );
    // A failed call auto-expands: its last lines ride under the digest, so `exit 101` is never
    // the whole story on screen. `Ctrl-E` opens the full tail; the hint says so only when
    // there is more than what is shown.
    if t.state == ToolState::Err && !t.body.is_empty() {
        let lines: Vec<&str> = t.body.lines().filter(|l| !l.trim().is_empty()).collect();
        let shown = lines.len().min(AUTO_EXPAND_LINES);
        let max_w = width.saturating_sub(4).max(16);
        for l in &lines[lines.len() - shown..] {
            let clipped: String = l.chars().take(max_w).collect();
            out.push_str(&format!(
                "\n  {} {}",
                theme::faint("│"),
                theme::faint(&clipped)
            ));
        }
        if lines.len() > shown {
            out.push_str(&format!(
                "\n  {} {}",
                theme::faint("│"),
                theme::faint(&format!(
                    "… {} more line(s) — Ctrl-E expands",
                    lines.len() - shown
                ))
            ));
        }
    }
    out
}

/// Lines of a failed tool's tail painted under its row.
pub(crate) const AUTO_EXPAND_LINES: usize = 6;

/// Render the in-place plan panel as a boxed checklist: a `☑ done/total · plan` header row, then one
/// `✓ / ▸ / ○` row per item, framed with the same rounded box the markdown renderer uses — but in
/// the PLAN lane's teal, so the panel reads as "progress" from across the room the same way a tool
/// row's hue reads as its kind of work. Done rows are green + dim-struck, the in-progress row is
/// bright moonlight, pending rows are faint.
pub(crate) fn render_plan_box(rows: &[PlanRow], width: usize) -> Vec<String> {
    use crate::ui::theme;
    // The frame tint: the plan/checkpoint lane's teal under the `lanes` theme, the quiet dim
    // silver every box wears under `moonlight`.
    let frame_color = if theme::lanes_enabled() {
        theme::LANE_PLAN
    } else {
        theme::ACCENT_DIM
    };
    let frame = |s: String| console::style(s).color256(frame_color).to_string();
    let done = rows.iter().filter(|r| r.status == 2).count();
    let header = format!("☑ {done}/{} · plan", rows.len());
    // Inner width: cap so the box doesn't sprawl on a very wide pane; leave room for `│ ` + ` │`.
    let inner = width.saturating_sub(2).min(72).max(12);
    let bar = "─".repeat(inner);
    let mut out = Vec::new();
    out.push(frame(format!(
        "╭─ {} ─╮",
        pad_to(&header, inner.saturating_sub(4))
    )));
    for r in rows {
        let glyph = match r.status {
            2 => "✓",
            1 => "▸",
            _ => "○",
        };
        let g = match r.status {
            2 => theme::ok(glyph).to_string(),
            1 => theme::accent(glyph).bold().to_string(),
            _ => theme::faint(glyph).to_string(),
        };
        // Body row `│ <glyph> <text><pad> │`: the span BETWEEN the two rules must be exactly `inner`
        // cells so the right `│` lines up with the border corners. That span is
        // ` `+glyph(1)+` `+text+pad+` ` = 4 + text + pad, so pad = inner − 4 − text.
        let text_budget = inner.saturating_sub(4);
        let clipped = clip_to(&r.text, text_budget);
        let styled = match r.status {
            2 => theme::muted(&clipped).to_string(),
            1 => theme::accent(&clipped).bold().to_string(),
            _ => theme::faint(&clipped).to_string(),
        };
        let pad = inner.saturating_sub(4 + console::measure_text_width(&clipped));
        out.push(format!(
            "{} {} {}{} {}",
            frame("│".to_string()),
            g,
            styled,
            " ".repeat(pad),
            frame("│".to_string())
        ));
    }
    out.push(frame(format!("╰{bar}╯")));
    out
}

/// One display cell of the diff panes: `(line number, kind, text)` — kind 0 = context, 1 = added,
/// 2 = removed; number 0 = unknown (headerless hunk → blank gutter).
type DiffCell = (usize, u8, String);

/// Pair one hunk's unified rows into side-by-side rows: context appears on BOTH sides, a run of
/// removed lines pairs positionally with the run of added lines that follows it (the GitHub /
/// OpenCode alignment), and the unpaired remainder gets a blank cell opposite.
fn pair_diff_rows(h: &crate::ui::tui::DiffHunk) -> Vec<(Option<DiffCell>, Option<DiffCell>)> {
    let mut out: Vec<(Option<DiffCell>, Option<DiffCell>)> = Vec::new();
    let (mut o, mut n) = (h.start_old, h.start_new);
    // Removed cells waiting for an added partner; flushed unpaired at a context row / hunk end.
    let mut pending: std::collections::VecDeque<DiffCell> = std::collections::VecDeque::new();
    for (kind, text) in &h.rows {
        match kind {
            2 => {
                pending.push_back((o, 2, text.clone()));
                o = o.saturating_add(1);
            }
            1 => {
                let left = pending.pop_front();
                out.push((left, Some((n, 1, text.clone()))));
                n = n.saturating_add(1);
            }
            _ => {
                while let Some(c) = pending.pop_front() {
                    out.push((Some(c), None));
                }
                out.push(((o, 0, text.clone()).into(), (n, 0, text.clone()).into()));
                o = o.saturating_add(1);
                n = n.saturating_add(1);
            }
        }
    }
    while let Some(c) = pending.pop_front() {
        out.push((Some(c), None));
    }
    out
}

/// Render one pane cell to exactly `w` display columns: `NNN ± text` with the whole padded run
/// background-tinted for a changed row, quiet faint/muted for context, plain spaces for the blank
/// side of an unpaired row. `numw` = gutter digits (0 = no gutter).
fn diff_cell(cell: Option<&DiffCell>, numw: usize, w: usize) -> String {
    use crate::ui::theme;
    let Some((num, kind, text)) = cell else {
        return " ".repeat(w);
    };
    let gut = if numw > 0 {
        if *num > 0 {
            format!("{num:>numw$} ")
        } else {
            " ".repeat(numw + 1)
        }
    } else {
        String::new()
    };
    let mark = match kind {
        1 => '+',
        2 => '−',
        _ => ' ',
    };
    let lead = format!("{gut}{mark} ");
    let clipped = clip_to(text, w.saturating_sub(console::measure_text_width(&lead)));
    let pad = w
        .saturating_sub(console::measure_text_width(&lead) + console::measure_text_width(&clipped));
    match kind {
        1 => theme::diff_add(format!("{lead}{clipped}{}", " ".repeat(pad))).to_string(),
        2 => theme::diff_del(format!("{lead}{clipped}{}", " ".repeat(pad))).to_string(),
        _ => format!(
            "{}{}{}",
            theme::faint(lead),
            theme::muted(clipped),
            " ".repeat(pad)
        ),
    }
}

/// Render a boxed diff preview. Wide enough, it becomes the side-by-side panes of the OpenCode /
/// GitHub review look — old on the left, new on the right, real file line numbers in the gutters,
/// removed rows on a deep-red tint and added rows on a deep-green one, context quiet between
/// them. Narrow, the same rows stack as a single unified column. The header keeps the
/// `diff · <path>  +A −D` shape with the counts in their semantic colours.
pub(crate) fn render_diff_box(d: &DiffPayload, width: usize) -> Vec<String> {
    use crate::ui::theme;
    // The box takes the full width it is handed (the transcript pane): a wide terminal buys the
    // panes real code columns. The old 100-column clamp left half the pane empty while both sides
    // clipped code at "…".
    let inner = width.saturating_sub(2).max(12);
    let mut out = Vec::new();

    // ── header ──────────────────────────────────────────────────────────────
    let counts_plain = format!("+{} −{}", d.adds, d.dels);
    let counts_w = console::measure_text_width(&counts_plain);
    // The header's fixed columns sum to 8: "╭─ " (3) + "  " before the counts (2) + the TWO
    // spaces before the dash run (1 in the format string + 1 leading the run itself) + "╮" (1).
    // With the row budgeted at `inner + 2` like every other row, that leaves label+counts+fill =
    // inner − 6. This subtracted 5, painting every header one column wider than the box — the old
    // 100-column clamp kept the overhang inside the pane, so nobody saw it.
    let label = clip_to(
        &format!("diff · {}", d.path),
        inner.saturating_sub(6 + counts_w).max(8),
    );
    let fill = inner.saturating_sub(6 + console::measure_text_width(&label) + counts_w);
    out.push(format!(
        "{}{}  {} {}{}",
        theme::accent_dim("╭─ "),
        // Under the `lanes` theme the label rides the EDIT lane's gold — the diff box is what an
        // edit-lane tool just did, so its title matches the `file_edit` row that produced it. The
        // frame stays quiet silver either way: a gold outline around a whole code box would shout;
        // one gold word ties them together. Moonlight keeps the label bright silver as it was.
        console::style(&label).color256(if theme::lanes_enabled() {
            theme::LANE_EDIT
        } else {
            theme::ACCENT
        }),
        format!(
            "{} {}",
            theme::ok(format!("+{}", d.adds)),
            theme::err(format!("−{}", d.dels))
        ),
        theme::accent_dim(format!(" {}", "─".repeat(fill))),
        theme::accent_dim("╮"),
    ));

    // ── gutter width: digits of the last line any hunk reaches (0 = no numbers anywhere) ──
    let mut max_line = 0usize;
    for h in &d.hunks {
        if h.start_old == 0 {
            continue;
        }
        let dels = h.rows.iter().filter(|(k, _)| *k == 2).count();
        let adds = h.rows.iter().filter(|(k, _)| *k == 1).count();
        let ctx = h.rows.len() - dels - adds;
        max_line = max_line
            .max(h.start_old + ctx + dels)
            .max(h.start_new + ctx + adds);
    }
    let numw = if max_line == 0 {
        0
    } else {
        max_line.to_string().len().max(3)
    };

    // Side-by-side needs room for two readable panes; below that the rows stack unified.
    let side_by_side = inner >= 58;
    for (i, h) in d.hunks.iter().enumerate() {
        if i > 0 {
            // Between hunks: a quiet broken rule, so far-apart windows don't read as adjacent.
            out.push(format!(
                "{} {} {}",
                theme::accent_dim("│"),
                theme::faint("┄".repeat(inner.saturating_sub(2))),
                theme::accent_dim("│")
            ));
        }
        if side_by_side {
            let pane_l = inner.saturating_sub(5) / 2;
            let pane_r = inner.saturating_sub(5) - pane_l;
            for (l, r) in pair_diff_rows(h) {
                out.push(format!(
                    "{} {} {} {} {}",
                    theme::accent_dim("│"),
                    diff_cell(l.as_ref(), numw, pane_l),
                    theme::faint("│"),
                    diff_cell(r.as_ref(), numw, pane_r),
                    theme::accent_dim("│"),
                ));
            }
        } else {
            let (mut o, mut n) = (h.start_old, h.start_new);
            for (kind, text) in &h.rows {
                let num = match kind {
                    1 => {
                        let v = n;
                        n = n.saturating_add(1);
                        v
                    }
                    2 => {
                        let v = o;
                        o = o.saturating_add(1);
                        v
                    }
                    _ => {
                        let v = o;
                        o = o.saturating_add(1);
                        n = n.saturating_add(1);
                        v
                    }
                };
                let cell = (num, *kind, text.clone());
                out.push(format!(
                    "{} {} {}",
                    theme::accent_dim("│"),
                    diff_cell(Some(&cell), numw, inner.saturating_sub(2)),
                    theme::accent_dim("│"),
                ));
            }
        }
    }
    out.push(theme::accent_dim(format!("╰{}╯", "─".repeat(inner))).to_string());
    out
}

/// Render the verify-gate success line: a green `✓ <cmd> — <detail>`, clipped to width.
pub(crate) fn render_verify_line(v: &VerifyPayload, width: usize) -> String {
    use crate::ui::theme;
    let text = if v.detail.is_empty() {
        format!("✓ {}", v.cmd)
    } else {
        format!("✓ {} — {}", v.cmd, v.detail)
    };
    theme::ok(clip_to(&text, width)).to_string()
}
