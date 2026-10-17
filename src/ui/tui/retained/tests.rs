//! Unit tests for the retained backend. Kept beside it rather than inline: the module was a
//! third of the file.

use super::*;

/// The two tests below read and write the process-wide transcript geometry slot (one through a
/// paint, one directly). Run in parallel they see each other's numbers; the lock keeps them
/// honest without changing what they assert.
static GEOM_SLOT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The thread currently holding [`GEOM_SLOT_TEST_LOCK`], so a re-acquire on that thread is a no-op.
static GEOM_OWNER: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);

/// Serialises every test that paints a frame or touches the transcript geometry slot (a
/// process-wide static): a painter on another thread would otherwise overwrite the slot between a
/// test's draw and its read — which is how two tests holding the lock still failed together while
/// four unlocked painters ran beside them. Re-entrant per thread, so a test may hold it across
/// several paints and `painted_rows` can take it unconditionally.
struct GeomLock(Option<std::sync::MutexGuard<'static, ()>>);

impl GeomLock {
    fn acquire() -> Self {
        let me = std::thread::current().id();
        let owner = || *GEOM_OWNER.lock().unwrap_or_else(|e| e.into_inner());
        if owner() == Some(me) {
            return GeomLock(None);
        }
        let guard = GEOM_SLOT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *GEOM_OWNER.lock().unwrap_or_else(|e| e.into_inner()) = Some(me);
        GeomLock(Some(guard))
    }
}

impl Drop for GeomLock {
    fn drop(&mut self) {
        if self.0.is_some() {
            *GEOM_OWNER.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
}

fn scratch_display_fixture() -> (
    crate::core::file_placement::tests::TestDir,
    crate::ui::links::DisplayTarget,
) {
    let temp = crate::core::file_placement::tests::TestDir::new();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let placement = crate::core::file_placement::PlacementContext::with_paths(
        root,
        temp.path().join("home"),
        temp.path().join("process"),
        "display-fixture",
    )
    .unwrap();
    let mut display = crate::ui::links::DisplayTarget::default();
    for name in ["báo cáo/結果 e\u{301} 👩‍💻.cjs", "nested/second file.txt"] {
        if !display.text.is_empty() {
            display.text.push_str(" → ");
        }
        let path = placement
            .scratch_display(&placement.scratch_root().join(name))
            .unwrap();
        let start = display.text.len();
        display.text.push_str(path.label());
        display.paths.push((start..display.text.len(), path));
    }
    (temp, display)
}

#[test]
fn scratch_display_wrap_copy_and_links_keep_typed_real_targets() {
    let (_temp, display) = scratch_display_fixture();
    let target = display
        .paths
        .iter()
        .map(|(_, p)| p.actual().display().to_string())
        .collect::<Vec<_>>()
        .join(" → ");
    let event = ToolEvent {
        seq: 190,
        icon: "◆".into(),
        name: "file_move".into(),
        target: target.clone(),
        display: display.clone(),
        digest: "moved".into(),
        state: ToolState::Ok,
        elapsed_ms: Some(20),
        body: format!("moved {target}"),
    };
    let block = UiBlock {
        id: 1,
        kind: BlockKind::Tool,
        payload: Payload::Tool(event),
        complete: true,
        rev: 0,
    };
    for width in [0, 1, 2, 3, 7, 16, 40, 100] {
        let rows = render_block_rows(&block, width);
        assert_eq!(rows.plain.len(), rows.paths.len());
        assert!(rows
            .plain
            .iter()
            .all(|r| crate::ui::links::display_width(r) <= usize::from(width)));
        if width <= 1 {
            assert!(rows.paths.iter().all(Vec::is_empty));
            continue;
        }
        for (index, (_, path)) in display.paths.iter().enumerate() {
            let fragments: Vec<_> = rows
                .paths
                .iter()
                .enumerate()
                .flat_map(|(row, paths)| {
                    paths
                        .iter()
                        .filter(move |p| p.id == (190, index))
                        .map(move |p| (row, p))
                })
                .collect();
            assert!(!fragments.is_empty(), "width {width}, alias {index}");
            assert_eq!(
                fragments
                    .iter()
                    .map(|(_, p)| p.span.text.as_str())
                    .collect::<String>(),
                path.label()
            );
            assert!(fragments.iter().all(|(_, p)| p.actual == path.actual()));
            assert!(fragments.iter().all(|(_, p)| {
                let linked = url::Url::parse(&p.span.url)
                    .unwrap()
                    .to_file_path()
                    .unwrap();
                crate::core::config::normalize_path_key(&linked)
                    == crate::core::config::normalize_path_key(path.actual())
            }));
            let (first_row, first) = fragments[0];
            let (last_row, last) = *fragments.last().unwrap();
            let selection = SelectionRange {
                anchor_line: first_row,
                anchor_col: first.span.col,
                cursor_line: last_row,
                cursor_col: last.span.col + last.span.width,
            };
            let copied = extract_from_path_rows(&rows.plain, &rows.paths, selection);
            assert_eq!(copied, path.actual().to_string_lossy(), "width {width}");
            assert_eq!(
                extract_from_path_rows(
                    &rows.plain,
                    &rows.paths,
                    SelectionRange {
                        anchor_line: selection.cursor_line,
                        anchor_col: selection.cursor_col,
                        cursor_line: selection.anchor_line,
                        cursor_col: selection.anchor_col
                    }
                ),
                copied
            );
            let partial = SelectionRange {
                cursor_line: first_row,
                cursor_col: first.span.col + 1,
                ..selection
            };
            assert_eq!(
                extract_from_path_rows(&rows.plain, &rows.paths, partial),
                "s"
            );
            if last_row > first_row {
                let clipped = extract_from_path_rows(
                    &rows.plain[first_row + 1..],
                    &rows.paths[first_row + 1..],
                    SelectionRange {
                        anchor_line: 0,
                        anchor_col: 0,
                        cursor_line: last_row - first_row - 1,
                        cursor_col: last.span.col + last.span.width,
                    },
                );
                assert!(
                    !clipped.contains(&path.actual().to_string_lossy().to_string()),
                    "an incomplete viewport cannot invent the missing alias"
                );
            }
        }
        let mut output = Vec::new();
        crate::ui::links::inject_hyperlinks_with_paths(
            &mut output,
            &rows.sgr,
            &rows.plain,
            &crate::ui::links::InjectCtx {
                area: Rect::new(4, 3, width + 1, rows.plain.len() as u16),
                visible: rows.plain.len(),
                ..Default::default()
            },
            &rows.paths,
        );
        let links = crate::ui::links::captured_links(&output);
        for (index, (_, path)) in display.paths.iter().enumerate() {
            let uri = url::Url::from_file_path(path.actual()).unwrap().to_string();
            assert_eq!(
                links
                    .iter()
                    .filter(|(_, _, u, _)| u == &uri)
                    .map(|(_, _, _, text)| plain(text))
                    .collect::<String>(),
                path.label(),
                "width {width}, alias {index}"
            );
        }
        assert!(links.iter().all(|(x, y, _, text)| *x >= 4
            && *y >= 3
            && *x + crate::ui::links::display_width(&plain(text)) <= usize::from(width) + 4));
    }
    let Payload::Tool(stored) = &block.payload else {
        unreachable!()
    };
    assert_eq!(stored.target, target);
    assert_eq!(stored.body, format!("moved {target}"));
}

#[test]
fn scratch_display_geometry_resize_and_revision_are_frame_aligned() {
    let _geom = GeomLock::acquire();
    let (_temp, display) = scratch_display_fixture();
    let mut state = AppState::new("intro", "status");
    let mut event = ToolEvent {
        seq: 191,
        icon: "◆".into(),
        name: "file_move".into(),
        target: "raw target".into(),
        display,
        digest: String::new(),
        state: ToolState::Running,
        elapsed_ms: None,
        body: String::new(),
    };
    apply_command(&mut state, Command::Tool(event.clone()));
    for width in [20, 124, 125, 140, 3, 2, 1] {
        let _ = painted_rows(&mut state, width, 80);
        let g = transcript_geom_slot().lock().unwrap().clone();
        assert_eq!(g.plain_rows.len(), g.paths.len());
        assert_eq!(g.plain_rows.len(), g.sgr_rows.len());
        assert_eq!(g.plain_rows.len(), g.row_tool_seq.len());
        for (i, paths) in g.paths.iter().enumerate() {
            for path in paths {
                assert_eq!(g.row_tool_seq[i], Some(path.id.0));
                assert!(g.plain_rows[i].contains(&path.span.text));
            }
        }
    }
    event.display = Default::default();
    event.state = ToolState::Ok;
    event.digest = "done".into();
    apply_command(&mut state, Command::Tool(event));
    let _ = painted_rows(&mut state, 124, 80);
    let g = transcript_geom_slot().lock().unwrap().clone();
    assert!(
        g.paths.iter().all(Vec::is_empty),
        "new revision must not retain stale aliases"
    );
}

#[test]
fn overlay_menu_hit_maps_rows_scroll_and_dead_zones() {
    let g = OverlayMenuGeom {
        inner: Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 6,
        },
        scroll: 2,
        rows: 7,
    };
    // Top visible row is index `scroll`; the mapping is row-relative plus scroll.
    assert_eq!(menu_hit_index(&g, 10, 5), Some(2), "top row, left edge");
    assert_eq!(
        menu_hit_index(&g, 49, 9),
        Some(6),
        "last pickable row, right edge"
    );
    // Inside the panel but past the pickable rows (the hint line) → dead, not a phantom pick.
    assert_eq!(menu_hit_index(&g, 20, 10), None, "hint row is not pickable");
    // Outside the panel on every side → None (the click belongs to the transcript handler).
    assert_eq!(menu_hit_index(&g, 9, 6), None, "left of the panel");
    assert_eq!(menu_hit_index(&g, 50, 6), None, "right of the panel");
    assert_eq!(menu_hit_index(&g, 20, 4), None, "above the panel");
    assert_eq!(menu_hit_index(&g, 20, 11), None, "below the panel");
    // The published slot round-trips, and clearing it kills the hit-test (stale-rect guard).
    set_overlay_menu(Some(g));
    assert_eq!(overlay_menu_hit(10, 5), Some(2));
    set_overlay_menu(None);
    assert_eq!(
        overlay_menu_hit(10, 5),
        None,
        "no overlay → no phantom picks"
    );
}

#[test]
fn sanitizes_terminal_control_sequences() {
    let got = sanitize_text("ok\x1b[2J\x1b[31mred\x1b[0m\nnext\r");
    assert_eq!(got, "okred\nnext");
}

#[test]
fn keep_sgr_drops_cursor_moves_but_keeps_colour() {
    // Erase (`\x1b[2J`) and a save-cursor (`\x1b7`) are stripped; the colour codes survive so
    // `ansi_spans` can turn them into styled spans.
    let got = sanitize_keep_sgr("\x1b7\x1b[2Jok\x1b[31mred\x1b[0m\nnext\r");
    assert_eq!(got, "ok\x1b[31mred\x1b[0m\nnext");
}

/// A draft of `text` with the caret at `cursor` and the box scrolled to wrapped row `row_scroll`.
fn input_state(text: &str, cursor: usize, row_scroll: usize) -> AppState {
    let mut state = AppState::new("intro", "status");
    state.input.draft = text.chars().collect();
    state.input.cursor = cursor;
    state.input_row_scroll = row_scroll;
    state
}

/// Full text of a laid-out box, rows joined by `|` — enough to assert where the breaks landed.
fn joined(layout: &InputLayout) -> String {
    layout
        .rows
        .iter()
        .map(|r| r.shown.clone())
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn a_long_draft_wraps_downward_instead_of_scrolling_away() {
    // THE BUG: the box was ONE row, so everything before the window slid off the left and could not
    // be read back or copied. A draft wider than the box must now occupy more rows, with every char
    // of it still on screen.
    let long: String = ('a'..='z').cycle().take(80).collect();
    // width 24 → text area 24 - 2 (`❯ `) - 1 (caret margin) = 21 cells per row.
    let layout = input_layout(&input_state(&long, 80, 0), 24, 10);
    assert!(layout.rows.len() > 1, "80 chars at 21 cells must wrap");
    assert_eq!(layout.total, 4, "80 chars / 21 cells = 4 rows");
    let painted: String = layout.rows.iter().map(|r| r.shown.clone()).collect();
    assert_eq!(painted, long, "every char stays on screen, in order");
    assert_eq!(layout.scroll, 0, "4 rows fit under a 10-row cap");
}

#[test]
fn wrapping_breaks_on_spaces_but_still_splits_a_long_word() {
    // Prose wraps at the last space, so a wrapped prompt reads as prose. Width 11 leaves 8 cells of
    // text (11 - 2 for `❯ ` - 1 for the caret margin), so each word lands on its own row.
    let layout = input_layout(&input_state("hello world again", 0, 0), 11, 10);
    assert_eq!(joined(&layout), "hello |world |again");
    // A single word wider than the box has no space to break on — it breaks hard rather than
    // producing a row that never ends.
    let layout = input_layout(&input_state("aaaaaaaaaaaaaaaa", 0, 0), 14, 10);
    assert_eq!(joined(&layout), "aaaaaaaaaaa|aaaaa");
}

#[test]
fn embedded_newlines_start_a_new_row() {
    // Shift+Enter and pasted blocks break rows on their own, whatever the width. The newline keeps a
    // cell of its own (painted blank) so a click at the end of a line lands BEFORE the break.
    let layout = input_layout(&input_state("a\nb\nc", 5, 0), 40, 10);
    assert_eq!(layout.rows.len(), 3);
    assert_eq!(joined(&layout), "a |b |c");
    assert_eq!(layout.rows[1].start, 2, "row 2 starts after the first \\n");
    // A draft ending in a newline gets the empty row the caret is sitting on.
    let layout = input_layout(&input_state("a\n", 2, 0), 40, 10);
    assert_eq!(layout.rows.len(), 2);
    assert_eq!(layout.caret, (1, 0), "caret on the fresh empty row");
}

#[test]
fn the_caret_lands_on_the_row_that_owns_it() {
    let layout = input_layout(&input_state("abc\ndef", 5, 0), 40, 10);
    assert_eq!(layout.caret, (1, 1), "second row, one cell in");
    // End of the draft: the last row, past its last char.
    let layout = input_layout(&input_state("abc\ndef", 7, 0), 40, 10);
    assert_eq!(layout.caret, (1, 3));
    // On the newline itself: still the END of the first row, not the start of the second.
    let layout = input_layout(&input_state("abc\ndef", 3, 0), 40, 10);
    assert_eq!(layout.caret, (0, 3));
}

#[test]
fn the_box_scrolls_by_rows_only_once_it_hits_its_ceiling() {
    let many: String = (0..8).map(|i| format!("line{i}\n")).collect();
    let end = many.chars().count();
    // 9 wrapped rows (8 newline-terminated + the trailing empty one), box capped at 3.
    let layout = input_layout(&input_state(&many, end, 0), 40, 3);
    assert_eq!(layout.total, 9);
    assert_eq!(layout.rows.len(), 3, "never taller than the cap");
    assert_eq!(layout.scroll, 6, "the caret's row is the bottom one");
    assert_eq!(layout.caret.0, 2);
    // Caret walked back above the window → the window follows it up.
    let layout = input_layout(&input_state(&many, 0, 6), 40, 3);
    assert_eq!(layout.scroll, 0);
    // Caret INSIDE the window → the window does not move, so reading back a long paste is stable.
    let layout = input_layout(&input_state(&many, end - 1, 6), 40, 3);
    assert_eq!(
        layout.scroll, 6,
        "a caret already on screen must not scroll"
    );
}

#[test]
fn max_input_rows_leaves_the_transcript_room() {
    assert_eq!(max_input_rows(40), 10, "capped by MAX_INPUT_TEXT_ROWS");
    assert_eq!(max_input_rows(10), 4, "10 - 3 chrome - 3 transcript");
    assert_eq!(max_input_rows(6), 1, "a tiny terminal still gets one row");
    assert_eq!(max_input_rows(1), 1, "never zero");
}

#[test]
fn an_empty_draft_is_one_placeholder_row() {
    let layout = input_layout(&AppState::new("intro", "status"), 60, 10);
    assert_eq!(layout.rows.len(), 1);
    assert!(layout.placeholder);
    assert!(layout.rows[0].shown.starts_with("Type a message"));
    assert_eq!(
        layout.rows[0].cols,
        vec![0],
        "the placeholder maps no chars, so a click on it parks the caret at 0"
    );
    assert_eq!(layout.hint, "↵ send · Tab complete");
}

#[test]
fn column_map_covers_every_char_on_the_row_and_one_past_the_end() {
    let layout = input_layout(&input_state("abc", 3, 0), 40, 10);
    assert_eq!(
        layout.rows[0].cols,
        vec![0, 1, 2, 3],
        "3 chars + the past-the-end cell"
    );
    // A wide char takes two cells, so the map is not a 1:1 index → column relation. `♥` is width 1;
    // use a CJK char, which every wcwidth table agrees is 2.
    let wide = input_layout(&input_state("a漢b", 3, 0), 40, 10);
    assert_eq!(wide.rows[0].cols, vec![0, 1, 3, 4]);
    // Both cells of the wide char resolve to it; past the end lands after the last char.
    let cols: Vec<u16> = wide.rows[0].cols.iter().map(|c| *c as u16).collect();
    assert_eq!(hit_index(&cols, 0, 1), 1, "left cell of 漢");
    assert_eq!(
        hit_index(&cols, 0, 2),
        1,
        "right cell of 漢 is the same char"
    );
    assert_eq!(hit_index(&cols, 0, 3), 2, "the char after it");
    assert_eq!(
        hit_index(&cols, 0, 9),
        3,
        "far right of the row → end of text"
    );
    assert_eq!(hit_index(&cols, 0, 0), 0, "first cell");
}

#[test]
fn a_selection_spanning_wrapped_rows_lights_every_row_it_covers() {
    // The highlight is one range of DRAFT indices but several rows of cells, so each row clips it to
    // its own slice. Without that, only the row the drag started on would light up.
    let layout = input_layout(&input_state("hello world again", 0, 0), 11, 10);
    // Rows: "hello " (0..6), "world " (6..12), "again" (12..17). Select "lo world ag".
    let sel = Some((3usize, 14usize));
    let off = layout.text_off;
    assert_eq!(
        sel_cells(sel, layout.rows[0].start, &layout.rows[0].cols, off),
        Some((off + 3, off + 6)),
        "from the anchor to the end of the first row"
    );
    assert_eq!(
        sel_cells(sel, layout.rows[1].start, &layout.rows[1].cols, off),
        Some((off, off + 6)),
        "the whole middle row"
    );
    assert_eq!(
        sel_cells(sel, layout.rows[2].start, &layout.rows[2].cols, off),
        Some((off, off + 2)),
        "up to the cursor on the last row"
    );
}

#[test]
fn hit_index_offsets_by_the_window_start() {
    // The map only covers what is visible, so the index it yields is relative to the window.
    let cols: Vec<u16> = vec![10, 11, 12, 13];
    assert_eq!(hit_index(&cols, 40, 10), 40);
    assert_eq!(hit_index(&cols, 40, 12), 42);
    // Left of the text (the `❯ ` prompt) parks the caret at the start of the window, not at 0 —
    // char 0 of the draft may be scrolled far off screen.
    assert_eq!(hit_index(&cols, 40, 0), 40);
}

#[test]
fn selection_cells_clip_to_the_visible_window() {
    // Window shows draft chars 10..14 at row cells 2..6 (text_off = 2 for `❯ `).
    let cols = vec![0, 1, 2, 3, 4];
    // Fully inside.
    assert_eq!(sel_cells(Some((11, 13)), 10, &cols, 2), Some((3, 5)));
    // Starts before the window → clipped to its left edge.
    assert_eq!(sel_cells(Some((0, 12)), 10, &cols, 2), Some((2, 4)));
    // Runs past the end → clipped to the last visible cell.
    assert_eq!(sel_cells(Some((12, 99)), 10, &cols, 2), Some((4, 6)));
    // Entirely off-window in either direction, and nothing selected at all → nothing painted.
    assert_eq!(sel_cells(Some((0, 9)), 10, &cols, 2), None);
    assert_eq!(sel_cells(Some((20, 30)), 10, &cols, 2), None);
    assert_eq!(sel_cells(None, 10, &cols, 2), None);
}

#[test]
fn ansi_spans_splits_on_colour_and_collapses_when_plain() {
    // A plain row (no SGR) → exactly one span in the base style, so uncoloured output is byte-
    // identical to before.
    let plain = ansi_spans("hello", Style::default().fg(Color::Gray));
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].content.as_ref(), "hello");

    // A coloured segment becomes its own span; a reset (`\x1b[0m`) returns to the base style.
    let spans = ansi_spans("a\x1b[31mred\x1b[0mb", Style::default().fg(Color::Gray));
    let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(texts, vec!["a", "red", "b"]);
    assert_eq!(
        spans[1].style.fg,
        Some(Color::Red),
        "the middle span is red"
    );
    assert_eq!(
        spans[2].style.fg,
        Some(Color::Gray),
        "reset returns to base"
    );
}

#[test]
fn ansi_spans_reads_256_colour() {
    // The app emits 256-colour SGR (`\x1b[38;5;Nm`) for its accent/ok/err palette — parse it.
    let spans = ansi_spans(
        "\x1b[38;5;71mgreen\x1b[0m",
        Style::default().fg(Color::Gray),
    );
    assert_eq!(spans[0].style.fg, Some(Color::Indexed(71)));
}

#[test]
fn assistant_cache_is_width_and_revision_keyed() {
    let mut cache = RenderCache::default();
    let mut block = UiBlock {
        id: 7,
        kind: BlockKind::Assistant,
        payload: Payload::Text("hello world".into()),
        complete: true,
        rev: 0,
    };
    let a = cache.get_or_render(&block, 40);
    let b = cache.get_or_render(&block, 40);
    assert!(
        std::rc::Rc::ptr_eq(&a, &b),
        "a second read of the same revision is the cached rows"
    );
    assert_eq!(cache.hits, 1);
    // The cache keys on the block's revision, not a payload hash: a mutation must bump `rev` —
    // every mutation site in `AppState` does — or it would paint stale rows.
    if let Payload::Text(s) = &mut block.payload {
        s.push('!');
    }
    block.rev += 1;
    let c = cache.get_or_render(&block, 40);
    assert!(!std::rc::Rc::ptr_eq(&a, &c), "a bumped revision re-renders");
    let _ = cache.get_or_render(&block, 20);
    assert_eq!(cache.misses, 3);
}

#[test]
fn a_streaming_block_reuses_its_last_render_inside_the_interval() {
    let mut cache = RenderCache::default();
    let mut block = UiBlock {
        id: 7,
        kind: BlockKind::Assistant,
        payload: Payload::Text("hello".into()),
        complete: false,
        rev: 0,
    };
    let a = cache.get_or_render(&block, 40);
    assert_eq!(cache.misses, 1);
    // A delta bumps the revision (a new cache key), but inside the adaptive interval the stream
    // slot's rows are reused instead of re-parsing the block's markdown.
    if let Payload::Text(s) = &mut block.payload {
        s.push_str(" world");
    }
    block.rev += 1;
    let b = cache.get_or_render(&block, 40);
    assert!(
        std::rc::Rc::ptr_eq(&a, &b),
        "throttled: the previous render is reused"
    );
    assert_eq!(cache.misses, 1, "no re-render inside the interval");
    // The completion flip bypasses the slot: the final state always renders, immediately.
    block.complete = true;
    block.rev += 1;
    let c = cache.get_or_render(&block, 40);
    assert!(
        !std::rc::Rc::ptr_eq(&a, &c),
        "the final render is not throttled"
    );
    assert_eq!(cache.misses, 2);
    assert!(
        c.plain.iter().any(|r| r.contains("hello world")),
        "the final render carries the whole text: {:?}",
        c.plain
    );
}

#[test]
fn payload_mutations_bump_the_revision_so_the_cache_cannot_go_stale() {
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::AssistantDelta("a".into()));
    let id = state.active_assistant.expect("a delta opens the block");
    let rev = |state: &AppState| state.blocks.iter().find(|b| b.id == id).unwrap().rev;
    let r0 = rev(&state);
    apply_command(&mut state, Command::AssistantDelta("b".into()));
    assert!(rev(&state) > r0, "a streamed delta bumps the revision");

    apply_command(
        &mut state,
        Command::Plan(vec![PlanRow {
            status: 0,
            text: "x".into(),
        }]),
    );
    let p0 = state
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Plan)
        .unwrap()
        .rev;
    apply_command(
        &mut state,
        Command::Plan(vec![PlanRow {
            status: 1,
            text: "x".into(),
        }]),
    );
    let p1 = state
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Plan)
        .unwrap()
        .rev;
    assert!(p1 > p0, "a plan refresh bumps the revision");

    let ev = |state_flag: ToolState| ToolEvent {
        seq: 3,
        icon: "⚙".into(),
        name: "file_read".into(),
        target: "x.rs".into(),
        display: Default::default(),
        digest: String::new(),
        state: state_flag,
        elapsed_ms: None,
        body: String::new(),
    };
    apply_command(&mut state, Command::Tool(ev(ToolState::Running)));
    let t0 = state
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Tool)
        .unwrap()
        .rev;
    apply_command(&mut state, Command::Tool(ev(ToolState::Ok)));
    let t1 = state
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Tool)
        .unwrap()
        .rev;
    assert!(t1 > t0, "a tool result bumps the revision");
}

#[test]
fn scroll_and_selection_ride_the_frame_cap_but_typing_does_not() {
    // Coalesced: the output stream AND high-rate view changes — wheel scroll, scrollbar drags,
    // mouse selection. Their intermediate states are superseded by the next event, so painting
    // each one is pure jitter.
    for cmd in [
        Command::AssistantDelta("x".into()),
        Command::Scroll(3),
        Command::Scroll(-1),
        Command::ScrollTo(0),
        Command::ScrollEnd,
        Command::ClearSelection,
        Command::SetSelection(SelectionRange::default()),
    ] {
        assert!(is_coalesced_command(&cmd));
    }
    // Interactive edges still paint on arrival: keystroke latency is the responsiveness contract.
    for cmd in [
        Command::Input(InputSnapshot::default()),
        Command::Focus(true),
    ] {
        assert!(!is_coalesced_command(&cmd));
    }
}

#[test]
fn the_composer_layout_is_cached_until_an_input_it_reads_moves() {
    let mut state = input_state("hello", 5, 0);
    let mut cache = None;
    let a = cached_input_layout(&mut cache, &state, 40, 10);
    let b = cached_input_layout(&mut cache, &state, 40, 10);
    assert!(
        std::rc::Rc::ptr_eq(&a, &b),
        "unchanged input ⇒ the cached layout"
    );
    state.input.draft.push('!');
    let c = cached_input_layout(&mut cache, &state, 40, 10);
    assert!(!std::rc::Rc::ptr_eq(&a, &c), "a keystroke re-lays out");

    // The sticky window offset is one of the layout's inputs: scrolling the draft re-lays out too.
    let many = "x\n".repeat(10);
    let mut state = input_state(&many, 13, 4); // caret on wrapped row 6, window at 4
    let mut cache = None;
    let d = cached_input_layout(&mut cache, &state, 40, 5);
    assert_eq!(d.scroll, 4);
    state.input_row_scroll = 2;
    let e = cached_input_layout(&mut cache, &state, 40, 5);
    assert_eq!(e.scroll, 2, "a draft-window scroll re-lays out");
}

#[test]
fn assistant_rows_consume_inline_code_backticks() {
    // Regression guard for the live/replay parity fix: the live retained path now renders the
    // active assistant block through the SAME `MarkdownStream` the replay path uses, instead of
    // the old pulldown-cmark `render_retained`. The old renderer left inline `` `code` `` as
    // literal backticks (`line.push('`')`); `MarkdownStream`'s `inline()` consumes them and tints
    // the span. Backtick-stripping is the one divergence observable WITHOUT a colour terminal —
    // under `console`'s test default (no TTY ⇒ SGR suppressed) both renderers drop `**` markers,
    // so bold is not distinguishable here, but the literal backtick is. If the two renderers ever
    // split again, this breaks.
    let code = render_assistant_rows("call `foo()` now", 40).join("\n");
    assert!(
        !code.contains('`'),
        "inline code backticks must be consumed, not shown literally: {code:?}"
    );
    assert!(
        code.contains("foo()"),
        "the code text itself must survive: {code:?}"
    );
}

/// Codepoints in the Dingbats block (U+2700..U+27BF) carrying `Emoji=Yes` in Unicode's
/// `emoji-data.txt`. They default to TEXT presentation, so `unicode-width` — which reads
/// East_Asian_Width, a different table — reports them as 1 cell and waves them through. But a
/// terminal that resolves them through its emoji font (Windows Terminal with Segoe UI Emoji in
/// the fallback chain does exactly this) paints them 2 cells wide. That is the failure mode a
/// width assertion cannot see, so the spinner alphabet is checked against this list instead.
const DINGBAT_EMOJI: &[char] = &[
    '\u{2702}', '\u{2705}', '\u{2708}', '\u{2709}', '\u{270A}', '\u{270B}', '\u{270C}', '\u{270D}',
    '\u{270F}', '\u{2712}', '\u{2714}', '\u{2716}', '\u{271D}', '\u{2721}', '\u{2728}', '\u{2733}',
    '\u{2734}', '\u{2744}', '\u{2747}', '\u{274C}', '\u{274E}', '\u{2753}', '\u{2754}', '\u{2755}',
    '\u{2757}', '\u{2763}', '\u{2764}', '\u{2795}', '\u{2796}', '\u{2797}', '\u{27A1}', '\u{27B0}',
    '\u{27BF}',
];

#[test]
fn bloom_frames_never_render_two_cells_wide() {
    // THE constraint on the spinner alphabet. The caption is drawn immediately right of the
    // glyph, so a frame that measures 2 cells shoves the whole line sideways once per cycle —
    // a visible horizontal twitch, not a subtle one.
    //
    // The obvious picks for a "star bloom" are exactly the trap: ✳ (U+2733) and ✴ (U+2734) sit
    // mid-sequence by shape, and BOTH are `Emoji=Yes`. Note the ordering of the two assertions
    // below — the width check ALONE is not enough and would pass on those two, because their
    // East_Asian_Width is Narrow. The deny-list is what actually keeps them out.
    for f in BLOOM {
        assert_eq!(
            f.chars().count(),
            1,
            "frame {f:?} must be a single char — a variation selector or ZWJ sequence flips \
             renderers into emoji presentation, and thus double-width"
        );
        let c = f.chars().next().unwrap();
        assert!(
            !DINGBAT_EMOJI.contains(&c),
            "frame {f:?} (U+{:04X}) is Emoji=Yes: a terminal resolving it through an emoji \
             font draws it 2 cells and the caption jumps sideways once per cycle",
            c as u32
        );
        assert_eq!(
            console::measure_text_width(f),
            1,
            "spinner frame {f:?} must be exactly one cell wide"
        );
    }
}

#[test]
fn bloom_opens_on_the_brand_mark_and_ping_pongs() {
    // Two things the sequence must do, both user-visible:
    //  1. `Working(false)` zeroes `frame`, so frame 0 is what every turn OPENS on — it has to be
    //     the ✦ brand mark, not an arbitrary mid-bloom star.
    //  2. The tail mirrors the head (ping-pong), so the cycle CLOSES back toward ✦ instead of
    //     snapping from the widest star straight to the mark.
    assert_eq!(BLOOM[0], "✦", "a turn must open on the brand mark");
    assert_eq!(
        BLOOM.len() % 2,
        0,
        "an odd length can't mirror cleanly around its peak"
    );
    let peak = BLOOM.len() / 2;
    for i in 1..peak {
        assert_eq!(
            BLOOM[peak - i],
            BLOOM[peak + i],
            "frame {} and {} must mirror for the bloom to close symmetrically",
            peak - i,
            peak + i
        );
    }
    // No braille anywhere: U+2800..=U+28FF is the generic-CLI spinner block, and wearing it
    // would trade the ✦ brand (shared with the `✦ ultimate` chip) for anonymity.
    for f in BLOOM {
        let c = f.chars().next().unwrap();
        assert!(
            !('\u{2800}'..='\u{28FF}').contains(&c),
            "frame {f:?} is braille — that's the look this spinner exists to avoid"
        );
    }
}

#[test]
fn working_line_advances_glyph_with_the_frame_counter() {
    // The spinner shares the typewriter's tick (no second clock), so a `Tick` must move BOTH.
    // Guards against a future "let's give the spinner its own timer" refactor silently pinning
    // the glyph to frame 0 while the caption keeps typing.
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    state.set_work_caption("Reading retained.rs".to_string(), None);

    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..BLOOM.len() {
        let (plain, _) = working_line(&state).remove(0);
        let glyph = plain.chars().next().unwrap();
        seen.insert(glyph);
        apply_command(&mut state, Command::Tick);
    }
    // A full cycle shows every DISTINCT frame; the ping-pong reuses ✶/✷/✹, so 8 frames render 5.
    let distinct: std::collections::BTreeSet<char> =
        BLOOM.iter().map(|f| f.chars().next().unwrap()).collect();
    assert_eq!(
        seen, distinct,
        "one full cycle must render every distinct bloom frame"
    );
    assert!(seen.len() > 2, "a 2-glyph blink is what we replaced");
}

#[test]
fn working_line_draws_no_caret_of_its_own() {
    // The working line must NOT imitate a text cursor. The terminal's real cursor is already on
    // screen — ratatui's `apply_buffer_with_cursor` calls `show_cursor()` for any frame that sets
    // a position, so `draw_footer`'s `set_cursor_position` re-shows it every frame and the `Hide`
    // in `TerminalSession::enter` survives exactly one. A second, drawn caret reads as a stray
    // blinking cursor, and worst of all when the caption is empty between tool steps: it collapsed
    // onto the glyph as `✦ ▏`, looking like a cursor stuck to the spinner.
    //
    // Checked in three states, because the old code only drew the caret while `!done` — testing
    // just the settled state would have passed against the very bug this guards.
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    state.set_work_caption("Reading retained.rs".to_string(), None);

    // 1. mid-typewriter
    apply_command(&mut state, Command::Tick);
    apply_command(&mut state, Command::Tick);
    let (typing, _) = working_line(&state).remove(0);
    assert!(
        !typing.contains('▏'),
        "no drawn caret while typing: {typing:?}"
    );

    // 2. fully revealed
    for _ in 0..state.work_caption.chars().count() {
        apply_command(&mut state, Command::Tick);
    }
    let (settled, _) = working_line(&state).remove(0);
    assert!(
        !settled.contains('▏'),
        "no drawn caret once settled: {settled:?}"
    );

    // 3. empty caption — the case that produced `✦ ▏`.
    state.work_caption.clear();
    state.work_reveal = 0;
    let (empty, _) = working_line(&state).remove(0);
    assert!(
        !empty.contains('▏'),
        "an empty caption must not collapse a caret onto the spinner: {empty:?}"
    );
    // The spinner and clock still render — this removed a caret, not the line. (Frame-agnostic:
    // by now the bloom has advanced well past frame 0.)
    let head = empty.chars().next().unwrap().to_string();
    assert!(
        BLOOM.contains(&head.as_str()),
        "a bloom frame still leads the line: {empty:?}"
    );
    assert!(empty.contains("Esc to stop"), "clock survives: {empty:?}");
}

#[test]
fn working_caption_types_out_then_holds() {
    // The typewriter: `Working(true)` seeds a whimsical verb and starts the reveal at zero, each
    // tick exposes one more char, and the reveal CLAMPS at the caption length (it must not run past
    // the end and start slicing nothing, nor keep incrementing forever).
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    assert!(!state.work_caption.is_empty(), "a turn seeds a verb");
    assert_eq!(state.work_reveal, 0, "reveal starts from scratch");

    let len = state.work_caption.chars().count();
    for _ in 0..len {
        apply_command(&mut state, Command::Tick);
    }
    assert_eq!(state.work_reveal, len, "one char per tick, fully revealed");
    // Extra ticks must not overshoot.
    for _ in 0..5 {
        apply_command(&mut state, Command::Tick);
    }
    assert_eq!(
        state.work_reveal, len,
        "reveal clamps at the caption length"
    );

    // The rendered line shows the whole caption plus the elapsed clock + stop hint.
    let (plain, _) = working_line(&state).remove(0);
    assert!(
        plain.contains(&state.work_caption),
        "the revealed caption must appear: {plain:?}"
    );
    assert!(
        plain.contains("Esc to stop"),
        "the stop hint rides the working line now: {plain:?}"
    );
}

#[test]
fn working_line_carries_the_sent_token_chip_and_resets_per_turn() {
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    // No call has gone out yet — the chip stays off the line.
    let (before, _) = working_line(&state).remove(0);
    assert!(!before.contains('↑'), "no chip before a send: {before:?}");

    apply_command(&mut state, Command::SentTokens(6_200));
    let (with, _) = working_line(&state).remove(0);
    assert!(
        with.contains("↑6.2K tok"),
        "the request size rides the working line: {with:?}"
    );

    // A NEW turn starts blank — the previous request size says nothing about it.
    apply_command(&mut state, Command::Working(false));
    apply_command(&mut state, Command::Working(true));
    let (fresh, _) = working_line(&state).remove(0);
    assert!(
        !fresh.contains('↑'),
        "a fresh turn resets the chip: {fresh:?}"
    );
}

#[test]
fn working_clock_rolls_into_minutes_past_sixty_seconds() {
    // A long turn used to read `754s` — the clock must roll into `12m34s` units instead.
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    let Some(t) = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90)) else {
        return; // very early process uptime can't back-date an Instant — nothing to assert
    };
    state.working_since = Some(t);
    let (plain, _) = working_line(&state).remove(0);
    assert!(
        plain.contains("1m3"),
        "90s must read as minutes (1m30s): {plain:?}"
    );
}

#[test]
fn tool_caption_replaces_verb_then_falls_back() {
    // The hybrid caption: a tool call re-points it at a concrete action (restarting the reveal so
    // the new text types out), and an empty `WorkCaption` — what `emit_tool_result` sends when the
    // call ends — falls back to the whimsical verb rather than freezing on the finished action.
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    let verb = state.work_verb.clone();
    apply_command(&mut state, Command::Tick); // reveal 1 char of the verb

    apply_command(
        &mut state,
        Command::WorkCaption(
            "Reading retained.rs".into(),
            Some(crate::ui::theme::LANE_READ),
        ),
    );
    assert_eq!(state.work_caption, "Reading retained.rs");
    assert_eq!(state.work_reveal, 0, "a new caption retypes from scratch");
    assert_eq!(
        state.work_tint,
        Some(crate::ui::theme::LANE_READ),
        "a tool caption carries its lane hue"
    );

    // Re-asserting the SAME caption must not stutter the reveal back to zero.
    apply_command(&mut state, Command::Tick);
    apply_command(
        &mut state,
        Command::WorkCaption(
            "Reading retained.rs".into(),
            Some(crate::ui::theme::LANE_READ),
        ),
    );
    assert_eq!(state.work_reveal, 1, "same text ⇒ reveal is preserved");

    apply_command(&mut state, Command::WorkCaption(String::new(), None));
    assert_eq!(state.work_caption, verb, "empty falls back to the verb");
    assert_eq!(state.work_tint, None, "the verb never keeps a tool's hue");
}

#[test]
fn working_line_only_rides_the_transcript_while_working() {
    // The working indicator moved OUT of the HUD and onto the transcript tail. It must appear only
    // while a turn is in flight — and `Working(false)` must clear the caption so no stale "Reading
    // …" row lingers under the finished answer.
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    apply_command(
        &mut state,
        Command::WorkCaption("Run cargo test".into(), Some(crate::ui::theme::LANE_EXEC)),
    );
    assert!(state.working, "turn in flight");

    apply_command(&mut state, Command::Working(false));
    assert!(!state.working);
    assert!(
        state.work_caption.is_empty() && state.work_reveal == 0,
        "finishing a turn clears the caption, leaving no stale row"
    );
}

#[test]
fn ultimate_recolours_the_input_box_to_gold() {
    // `/ultimate` recolours the input box framing to the reserved gold so the heightened mode is
    // unmistakable, and toggling back returns it to moonlight. The state is PUSHED (never read from
    // disk in the draw path, which runs at ~9fps).
    let mut state = AppState::new("intro", "status");
    assert!(!state.ultimate, "moonlight by default");

    apply_command(&mut state, Command::Ultimate(true));
    assert!(state.ultimate);
    apply_command(&mut state, Command::Ultimate(false));
    assert!(!state.ultimate, "toggling off returns to moonlight");
    // Guard the palette choice itself: gold must stay distinct from the default silver, or the
    // recolour would be invisible.
    assert_ne!(crate::ui::theme::WARN, crate::ui::theme::ACCENT_DIM);
}

#[test]
fn the_row_cache_is_an_lru_not_a_flush() {
    let mut cache = RenderCache::default();
    let block = |id: u64| UiBlock {
        id,
        kind: BlockKind::Generic,
        payload: Payload::Text(format!("row {id}")),
        complete: true,
        rev: 0,
    };
    let hot = block(0);
    for id in 1..(CACHE_LIMIT as u64 + 64) {
        let _ = cache.get_or_render(&block(id), 40);
        if id % 32 == 0 {
            let _ = cache.get_or_render(&hot, 40);
        }
    }
    assert!(cache.rows.len() <= CACHE_LIMIT, "{}", cache.rows.len());
    let hits = cache.hits;
    let _ = cache.get_or_render(&hot, 40);
    assert_eq!(
        cache.hits,
        hits + 1,
        "the block touched all along survives eviction"
    );
    let misses = cache.misses;
    let _ = cache.get_or_render(&block(1), 40);
    assert_eq!(cache.misses, misses + 1, "the coldest block was evicted");
    // Heights outlive row eviction: placing the viewport never needs a render.
    assert_eq!(cache.height(&block(2), 40), 1);
    assert_eq!(cache.misses, misses + 1);
    cache.forget_before(500);
    assert!(cache.heights.keys().all(|k| k.id >= 500));
}

#[test]
fn a_frame_renders_the_viewport_not_the_session() {
    let _geom = GeomLock::acquire();
    let mut state = AppState::new("intro", "status");
    for i in 0..2_000 {
        state.push_text(BlockKind::Generic, format!("line-{i}"), true);
    }
    let _ = painted_rows(&mut state, 80, 24);
    let after_first = state.cache.misses;
    assert!(
        after_first >= 2_000,
        "every block is measured once: {after_first}"
    );
    let _ = painted_rows(&mut state, 80, 24);
    assert_eq!(
        state.cache.misses, after_first,
        "a second frame at the tail renders nothing new"
    );
    state.scroll_from_tail = 1_000;
    let _ = painted_rows(&mut state, 80, 24);
    let window = 24 + 2 * (24 + paint::RENDER_MARGIN_ROWS) + 4;
    let rendered = (state.cache.misses - after_first) as usize;
    assert!(
        rendered <= window,
        "a scroll renders only the new window: {rendered} blocks (cap {window})"
    );
    assert!(state.cache.rows.len() <= CACHE_LIMIT);
    let g = transcript_geom_slot().lock().unwrap();
    assert!(
        g.rows_offset > 0 && g.rows_offset <= g.start,
        "{} / {}",
        g.rows_offset,
        g.start
    );
    assert!(g.plain_rows.len() <= window);
}

#[test]
fn a_selection_is_rebased_onto_the_rendered_window() {
    let sel = |a: usize, b: usize| SelectionRange {
        anchor_line: a,
        anchor_col: 2,
        cursor_line: b,
        cursor_col: 5,
    };
    // Entirely inside: shifted by the offset.
    let s = shift_selection(sel(110, 112), 100, 50).unwrap();
    assert_eq!(
        (s.anchor_line, s.anchor_col, s.cursor_line, s.cursor_col),
        (10, 2, 12, 5)
    );
    // Starting above the window: clamped to its first row.
    let s = shift_selection(sel(90, 105), 100, 50).unwrap();
    assert_eq!((s.anchor_line, s.anchor_col), (0, 0));
    assert_eq!((s.cursor_line, s.cursor_col), (5, 5));
    // Ending below it: clamped to its last row, whole row.
    let s = shift_selection(sel(140, 900), 100, 50).unwrap();
    assert_eq!((s.cursor_line, s.cursor_col), (49, usize::MAX));
    // Outside on either side, or an empty window: nothing to highlight.
    assert!(shift_selection(sel(10, 20), 100, 50).is_none());
    assert!(shift_selection(sel(200, 210), 100, 50).is_none());
    assert!(shift_selection(sel(110, 112), 100, 0).is_none());
    // Reversed anchors are ordered first.
    let s = shift_selection(sel(112, 110), 100, 50).unwrap();
    assert!(s.anchor_line <= s.cursor_line);
}

#[test]
fn a_failed_tool_row_stays_one_line_and_points_at_ctrl_e() {
    let mut ev = ToolEvent {
        seq: 5,
        icon: "⚙".into(),
        name: "shell_run".into(),
        target: "cargo test".into(),
        display: Default::default(),
        digest: "exit 101".into(),
        state: ToolState::Err,
        elapsed_ms: Some(900),
        body: (1..=9)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let rows = render_tool_row(&ev, 80);
    let plain = console::strip_ansi_codes(&rows).to_string();
    // The failure digest is the whole error line; the body must NOT spill into the transcript,
    // or a run with several failed calls buries everything under indented log rows.
    assert!(plain.contains("exit 101"), "{plain}");
    assert!(
        !plain.contains("│ line 9") && !plain.contains("│ line 4"),
        "a failed call no longer auto-expands its tail: {plain}"
    );
    assert!(plain.contains("9 more line(s) — Ctrl-E expands"), "{plain}");
    assert_eq!(
        plain.lines().count(),
        3,
        "call line + digest line + one hint: {plain}"
    );
    ev.state = ToolState::Ok;
    let ok = console::strip_ansi_codes(&render_tool_row(&ev, 80)).to_string();
    assert!(
        !ok.contains("│ line") && !ok.contains("Ctrl-E"),
        "a successful call stays two rows: {ok}"
    );
    ev.state = ToolState::Err;
    ev.body.clear();
    let bare = console::strip_ansi_codes(&render_tool_row(&ev, 80)).to_string();
    assert_eq!(bare.lines().count(), 2, "no body, no tail: {bare}");
}

#[test]
fn failed_tool_row_never_echoes_its_body_and_counts_what_ctrl_e_holds() {
    let message = "error: provide exactly one of path, evidence_id or ref_id";
    let mut ev = ToolEvent {
        seq: 6,
        icon: String::new(),
        name: "image_read".into(),
        target: String::new(),
        display: Default::default(),
        digest: message.into(),
        state: ToolState::Err,
        elapsed_ms: None,
        body: format!(" \x1b[31m{message}\x1b[0m \r\n\n"),
    };
    let original = ev.body.clone();
    let plain = sanitize_text(&render_tool_row(&ev, 120));
    assert_eq!(plain.matches(message).count(), 1, "{plain}");
    assert!(!plain.contains('│'), "{plain}");
    assert_eq!(ev.body, original, "full details remain untouched");

    // A body line that merely EXTENDS the digest is not a repeat of it, so it counts as detail
    // held for Ctrl-E — but it is still not printed.
    ev.body = format!("{message}: received both path and ref_id");
    let plain = sanitize_text(&render_tool_row(&ev, 120));
    assert!(!plain.contains("received both path and ref_id"), "{plain}");
    assert!(plain.contains("1 more line(s) — Ctrl-E expands"), "{plain}");
    ev.digest = "error: provide exactly one…".into();
    let plain = sanitize_text(&render_tool_row(&ev, 120));
    assert!(plain.contains("1 more line(s) — Ctrl-E expands"), "{plain}");

    // Nine body lines, one of which is an exact repeat of the digest: the repeat is not counted
    // as extra detail, and none of the rest reach the transcript.
    ev.digest = message.into();
    ev.body = format!(
        "{message}\n{}\n{message}",
        (1..=7)
            .map(|i| format!("diagnostic {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let plain = sanitize_text(&render_tool_row(&ev, 120));
    assert_eq!(plain.matches(message).count(), 1, "{plain}");
    assert!(!plain.contains("diagnostic"), "{plain}");
    assert!(plain.contains("7 more line(s) — Ctrl-E expands"), "{plain}");
    assert_eq!(plain.lines().count(), 3, "{plain}");

    ev.body = message.into();
    let narrow = sanitize_text(&render_tool_row(&ev, 24));
    assert!(!narrow.contains('│'), "{narrow}");
    assert!(narrow
        .lines()
        .all(|line| console::measure_text_width(line) <= 24));
    assert_eq!(ev.body, message);
}

#[test]
fn tool_seq_is_found_by_transcript_row_through_the_window() {
    let _geom = GeomLock::acquire();
    {
        let mut g = geom::transcript_geom_slot().lock().unwrap();
        g.rows_offset = 100;
        g.row_tool_seq = vec![None, Some(7), Some(7), None];
    }
    assert_eq!(geom::tool_seq_at_row(101), Some(7));
    assert_eq!(geom::tool_seq_at_row(103), None);
    assert_eq!(geom::tool_seq_at_row(50), None, "above the window");
    assert_eq!(geom::tool_seq_at_row(900), None, "past the window");
}

#[test]
fn pruning_keeps_whole_blocks() {
    let mut state = AppState::new("intro", "status");
    for i in 0..BLOCK_LIMIT + 20 {
        state.push_text(BlockKind::Generic, format!("line-{i}"), true);
    }
    assert_eq!(state.blocks.len(), BLOCK_LIMIT);
    assert!(state
        .blocks
        .iter()
        .all(|b| !matches!(&b.payload, Payload::Text(s) if s.is_empty())));
}

#[test]
fn scroll_routes_to_open_overlay_not_the_transcript() {
    let mut state = AppState::new("intro", "status");
    // No overlay → scroll moves the transcript.
    apply_command(&mut state, Command::Scroll(-3));
    assert_eq!(state.scroll_from_tail, 3);
    assert_eq!(state.overlay_scroll, 0);

    // Open an overlay → scroll now moves ITS content, leaving the transcript offset untouched.
    apply_command(
        &mut state,
        Command::OpenOverlay(OverlaySnapshot {
            title: "info".into(),
            lines: (0..40).map(|i| format!("row {i}")).collect(),
            selected: None,
            hint: String::new(),
        }),
    );
    assert_eq!(state.overlay_scroll, 0, "a fresh overlay starts at the top");
    apply_command(&mut state, Command::Scroll(-5));
    assert_eq!(state.overlay_scroll, 5, "overlay scrolled");
    assert_eq!(
        state.scroll_from_tail, 3,
        "transcript offset is left alone while the overlay is up"
    );

    // Home/End resets the overlay while it's open.
    apply_command(&mut state, Command::ScrollEnd);
    assert_eq!(state.overlay_scroll, 0);
    assert_eq!(state.scroll_from_tail, 3);

    // Closing the overlay hands scroll back to the transcript.
    apply_command(&mut state, Command::CloseOverlay);
    apply_command(&mut state, Command::Scroll(-2));
    assert_eq!(state.scroll_from_tail, 5);
}

/// `/workflows` republishes itself about once a second so its elapsed times tick. If a refresh
/// re-opened the overlay it would yank the reader back to the top every second — unusable while
/// paging through the history section. `UpdateOverlay` swaps the body and leaves scroll alone.
#[test]
fn a_live_overlay_refresh_keeps_the_readers_scroll_position() {
    let mut state = AppState::new("intro", "status");
    apply_command(
        &mut state,
        Command::OpenOverlay(OverlaySnapshot {
            title: "Activity".into(),
            lines: (0..40).map(|i| format!("row {i} · 3s")).collect(),
            selected: None,
            hint: "Esc/q close".into(),
        }),
    );
    apply_command(&mut state, Command::Scroll(-12));
    assert_eq!(state.overlay_scroll, 12);

    apply_command(
        &mut state,
        Command::UpdateOverlay((0..40).map(|i| format!("row {i} · 4s")).collect()),
    );

    assert_eq!(state.overlay_scroll, 12, "the reader's position survives");
    let overlay = state.input.overlay.as_ref().expect("still open");
    assert!(overlay.lines[0].ends_with("4s"), "body was refreshed");
    assert_eq!(overlay.title, "Activity", "title/hint are not disturbed");
    assert_eq!(overlay.hint, "Esc/q close");
}

/// A refresh that races the Esc which closed the panel must not resurrect it: the refresher thread
/// sleeps between publishes, so one in-flight update after the close is entirely ordinary.
#[test]
fn a_refresh_arriving_after_close_does_not_reopen_the_overlay() {
    let mut state = AppState::new("intro", "status");
    apply_command(
        &mut state,
        Command::OpenOverlay(OverlaySnapshot {
            title: "Activity".into(),
            lines: vec!["row".into()],
            selected: None,
            hint: String::new(),
        }),
    );
    apply_command(&mut state, Command::CloseOverlay);
    apply_command(&mut state, Command::UpdateOverlay(vec!["late".into()]));
    assert!(state.input.overlay.is_none(), "stays closed");
}

/// The per-tool result line carries a sub-agent dispatch's whole runtime, and that can be hours.
/// `· 7203.4s` is correct and unreadable; the minute/hour tiers exist for exactly that row.
#[test]
fn tool_row_time_gains_minute_and_hour_tiers() {
    assert_eq!(fmt_elapsed(None), "");
    assert_eq!(fmt_elapsed(Some(940)), " · 940ms");
    assert_eq!(fmt_elapsed(Some(1_250)), " · 1.2s");
    assert_eq!(fmt_elapsed(Some(59_900)), " · 59.9s");
    assert_eq!(fmt_elapsed(Some(60_000)), " · 1m00s");
    assert_eq!(fmt_elapsed(Some(7_203_400)), " · 2h00m");
}

/// The highlight is the ONLY selection surface now — there is no floating Copy button to keep in
/// sync with it, because Ctrl-C copies whatever is highlighted. So the two state transitions that
/// remain have to be exact: setting a selection paints it, clearing it takes it away completely.
#[test]
fn selection_state_tracks_set_and_clear() {
    let mut state = AppState::new("intro", "status");
    let sel = SelectionRange {
        anchor_line: 1,
        anchor_col: 0,
        cursor_line: 1,
        cursor_col: 4,
    };
    apply_command(&mut state, Command::SetSelection(sel));
    assert_eq!(state.selection, Some(sel));

    apply_command(&mut state, Command::ClearSelection);
    assert!(state.selection.is_none());
}

#[test]
fn transcript_scroll_follows_tail_anchors_and_clamps() {
    // At the bottom (offset 0): stay pinned to the tail as content streams in.
    let (start, offset, last) = resolve_transcript_scroll(0, 100, 120, 10);
    assert_eq!(offset, 0, "offset stays 0 at the bottom");
    assert_eq!(start, 110, "view follows the new tail");
    assert_eq!(last, 120);

    // Scrolled up (offset 5) while 20 lines are appended: the offset grows by the delta so the
    // content under the reader's eyes does not move.
    let (start, offset, _) = resolve_transcript_scroll(5, 100, 120, 10);
    assert_eq!(offset, 25, "offset bumped by the 20 appended lines");
    // tail_start = 120 - 10 = 110; start = 110 - 25 = 85 (same absolute rows as 105 - 5 before).
    assert_eq!(start, 85);

    // PageUp past the top must not inflate the offset: it clamps to tail_start and writes back, so
    // a single PageDown moves the view immediately (no dead zone).
    let (start, offset, _) = resolve_transcript_scroll(9999, 50, 50, 10);
    assert_eq!(
        offset, 40,
        "clamped to tail_start (50 - 10), not left at 9999"
    );
    assert_eq!(start, 0, "pinned at the very top");

    // Fewer lines than the viewport: nothing to scroll, offset collapses to 0.
    let (start, offset, _) = resolve_transcript_scroll(3, 4, 4, 10);
    assert_eq!(offset, 0);
    assert_eq!(start, 0);
}

// ── the mockup redesign: structured transcript blocks ────────────────────
fn plain(s: &str) -> String {
    console::strip_ansi_codes(s).into_owned()
}

#[test]
fn tool_row_puts_digest_and_time_on_the_line_below() {
    // The call `⚙ file_read   src/auth.rs` sits on top; the result digest drops to an indented
    // `└ 142 lines · <time>` line beneath it (not right-aligned on the same row).
    let ev = ToolEvent {
        seq: 1,
        icon: "⚙".into(),
        name: "file_read".into(),
        target: "src/auth.rs".into(),
        display: Default::default(),
        digest: "142 lines".into(),
        state: ToolState::Ok,
        elapsed_ms: Some(1200),
        body: String::new(),
    };
    let row = plain(&render_tool_row(&ev, 60));
    let lines: Vec<&str> = row.split('\n').collect();
    assert_eq!(lines.len(), 2, "call line + result line: {row:?}");
    assert_eq!(
        lines[0], "⚙ file_read   src/auth.rs",
        "call on top: {row:?}"
    );
    assert_eq!(
        lines[1], "└ 142 lines · 1.2s",
        "digest + time below: {row:?}"
    );
}

#[test]
fn tool_row_shows_ms_for_subsecond_runs() {
    let ev = ToolEvent {
        seq: 1,
        icon: "⚙".into(),
        name: "search_files".into(),
        target: "foo".into(),
        display: Default::default(),
        digest: "3 match(es)".into(),
        state: ToolState::Ok,
        elapsed_ms: Some(940),
        body: String::new(),
    };
    let row = plain(&render_tool_row(&ev, 60));
    assert!(
        row.ends_with("└ 3 match(es) · 940ms"),
        "sub-second → ms: {row:?}"
    );
}

#[test]
fn tool_row_omits_time_when_unknown() {
    // A restored / eager-adopted call has no timing → the result line carries the digest only.
    let ev = ToolEvent {
        seq: 1,
        icon: "⚙".into(),
        name: "file_read".into(),
        target: "x.rs".into(),
        display: Default::default(),
        digest: "10 lines".into(),
        state: ToolState::Ok,
        elapsed_ms: None,
        body: String::new(),
    };
    let row = plain(&render_tool_row(&ev, 60));
    assert!(row.ends_with("└ 10 lines"), "no time appended: {row:?}");
    assert!(!row.contains('·'), "no time separator: {row:?}");
}

#[test]
fn tool_row_omits_result_line_when_no_digest() {
    // A still-running call (empty digest) is just the call line, no `└` result line yet.
    let ev = ToolEvent {
        seq: 1,
        icon: "⚙".into(),
        name: "shell_run".into(),
        target: "cargo check".into(),
        display: Default::default(),
        digest: String::new(),
        state: ToolState::Running,
        elapsed_ms: None,
        body: String::new(),
    };
    let row = plain(&render_tool_row(&ev, 60));
    assert_eq!(row, "⚙ shell_run   cargo check");
    assert!(!row.contains('\n'), "no result line while running: {row:?}");
}

#[test]
fn plan_box_frames_and_counts() {
    let rows = vec![
        PlanRow {
            status: 2,
            text: "design".into(),
        },
        PlanRow {
            status: 1,
            text: "implement".into(),
        },
        PlanRow {
            status: 0,
            text: "verify".into(),
        },
    ];
    let out: Vec<String> = render_plan_box(&rows, 40)
        .iter()
        .map(|s| plain(s))
        .collect();
    assert!(
        out[0].contains("☑ 1/3 · plan"),
        "header counts done/total: {:?}",
        out[0]
    );
    assert!(
        out.iter().any(|l| l.contains("✓ design")),
        "done row: {out:?}"
    );
    assert!(
        out.iter().any(|l| l.contains("▸ implement")),
        "in-progress row: {out:?}"
    );
    assert!(
        out.iter().any(|l| l.contains("○ verify")),
        "pending row: {out:?}"
    );
    // Top border, three rows, bottom border.
    assert_eq!(out.len(), 5);
    // Every framed line is the same display width (a true rectangle).
    let w0 = console::measure_text_width(&out[0]);
    assert!(
        out.iter().all(|l| console::measure_text_width(l) == w0),
        "uniform width: {out:?}"
    );
}

#[test]
fn plan_update_is_in_place_not_appended() {
    // A second todo_write REPLACES the panel rather than stacking a fresh box (only one Plan block
    // ever exists), and it keeps its original position among other blocks.
    let mut state = AppState::new("intro", "status");
    state.push_text(BlockKind::Generic, "before".into(), true);
    apply_command(
        &mut state,
        Command::Plan(vec![PlanRow {
            status: 0,
            text: "a".into(),
        }]),
    );
    state.push_text(BlockKind::Generic, "after".into(), true);
    let plan_pos = state
        .blocks
        .iter()
        .position(|b| b.kind == BlockKind::Plan)
        .unwrap();
    apply_command(
        &mut state,
        Command::Plan(vec![
            PlanRow {
                status: 2,
                text: "a".into(),
            },
            PlanRow {
                status: 1,
                text: "b".into(),
            },
        ]),
    );
    assert_eq!(
        state
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Plan)
            .count(),
        1,
        "exactly one plan block"
    );
    assert_eq!(
        state.blocks.iter().position(|b| b.kind == BlockKind::Plan),
        Some(plan_pos),
        "the panel stays where it first appeared"
    );
    // An empty list removes the panel entirely.
    apply_command(&mut state, Command::Plan(vec![]));
    assert!(
        state.blocks.iter().all(|b| b.kind != BlockKind::Plan),
        "cleared plan leaves no box"
    );
}

#[test]
fn tool_event_updates_the_same_line_by_seq() {
    // A Running event opens a line; the Ok event with the same seq updates it in place (no second
    // Tool block appended), and flips it to complete.
    let mut state = AppState::new("intro", "status");
    apply_command(
        &mut state,
        Command::Tool(ToolEvent {
            seq: 7,
            icon: "⚙".into(),
            name: "file_read".into(),
            target: "x.rs".into(),
            display: Default::default(),
            digest: String::new(),
            state: ToolState::Running,
            elapsed_ms: None,
            body: String::new(),
        }),
    );
    assert_eq!(
        state
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Tool)
            .count(),
        1
    );
    apply_command(
        &mut state,
        Command::Tool(ToolEvent {
            seq: 7,
            icon: "⚙".into(),
            name: "file_read".into(),
            target: "x.rs".into(),
            display: Default::default(),
            digest: "10 lines".into(),
            state: ToolState::Ok,
            elapsed_ms: Some(42),
            body: String::new(),
        }),
    );
    let tools: Vec<&UiBlock> = state
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Tool)
        .collect();
    assert_eq!(tools.len(), 1, "same line updated in place, not appended");
    assert!(tools[0].complete, "result flips it complete");
    match &tools[0].payload {
        Payload::Tool(t) => assert_eq!(t.digest, "10 lines"),
        _ => panic!("expected a Tool payload"),
    }
}

#[test]
fn a_long_boot_note_wraps_to_the_pane_instead_of_clipping() {
    // The reported bug: `sandbox:` / `[dense]` boot notes are one long line; the Generic render
    // used to keep them as ONE row, and the paint clipped it at the pane's right edge.
    let note = "sandbox: no kernel sandbox backend on this platform — commands run with software guards only (guarded)";
    let rows = wrap_keep_sgr(note, 40);
    assert!(rows.len() > 1, "a long note must fold: {rows:?}");
    assert!(
        rows.iter().all(|r| console::measure_text_width(r) <= 40),
        "every folded row fits the pane: {rows:?}"
    );
    assert_eq!(
        rows.concat().split_whitespace().collect::<Vec<_>>(),
        note.split_whitespace().collect::<Vec<_>>(),
        "no words are lost or reordered by the fold"
    );
    // Word-boundary preference: no row starts mid-word with a space left dangling on it.
    assert!(
        rows.iter().skip(1).all(|r| !r.starts_with(' ')),
        "soft breaks land AFTER the space: {rows:?}"
    );
}

#[test]
fn a_styled_note_keeps_its_colour_past_the_fold() {
    let sgr = "\x1b[38;5;179m";
    let note =
        format!("{sgr}mcp: server 'design' skipped because its launcher exited early\x1b[0m");
    let rows = wrap_keep_sgr(&note, 30);
    assert!(rows.len() > 1, "must fold: {rows:?}");
    for r in &rows[1..] {
        assert!(
            r.starts_with(sgr),
            "continuation rows re-open the active colour: {r:?}"
        );
    }
}

#[test]
fn generic_blocks_render_folded_rows_through_the_cache() {
    let mut cache = RenderCache::default();
    let block = UiBlock {
        id: 9,
        kind: BlockKind::Generic,
        payload: Payload::Text(
            "[dense] loaded model2vec-potion-multilingual-128M (dim 256) — auto-detected".into(),
        ),
        complete: true,
        rev: 0,
    };
    let rows = cache.get_or_render(&block, 32);
    assert!(
        rows.sgr.len() > 1,
        "the Generic path folds long rows: {:?}",
        rows.sgr
    );
    assert!(rows
        .sgr
        .iter()
        .all(|r| console::measure_text_width(r) <= 32));
}

fn sample_diff() -> DiffPayload {
    DiffPayload {
        path: "src/auth.rs".into(),
        adds: 1,
        dels: 1,
        hunks: vec![crate::ui::tui::DiffHunk {
            start_old: 96,
            start_new: 96,
            rows: vec![
                (0, "required".into()),
                (2, "tabIndex={4}".into()),
                (1, "tabIndex={5}".into()),
                (0, "autoComplete".into()),
            ],
        }],
    }
}

#[test]
fn diff_box_narrow_stacks_unified_with_line_numbers() {
    let out: Vec<String> = render_diff_box(&sample_diff(), 48)
        .iter()
        .map(|s| plain(s))
        .collect();
    assert!(
        !out[0].contains("src/auth.rs"),
        "header drops the path (the tool row above already names it): {:?}",
        out[0]
    );
    assert!(
        out[0].contains("+1 −1"),
        "header carries the counts: {:?}",
        out[0]
    );
    // Unified: one column, gutter numbered from the hunk header — the removed line keeps the OLD
    // file's number, the added line the NEW file's.
    assert!(
        out.iter().any(|l| l.contains(" 96   required")),
        "leading context with its number: {out:?}"
    );
    assert!(
        out.iter().any(|l| l.contains(" 97 − tabIndex={4}")),
        "removed line: {out:?}"
    );
    assert!(
        out.iter().any(|l| l.contains(" 97 + tabIndex={5}")),
        "added line: {out:?}"
    );
}

#[test]
fn diff_box_wide_pairs_old_and_new_panes() {
    let out: Vec<String> = render_diff_box(&sample_diff(), 100)
        .iter()
        .map(|s| plain(s))
        .collect();
    // Side-by-side: the removed/added pair share ONE row — old pane left, new pane right.
    let paired = out
        .iter()
        .find(|l| l.contains("− tabIndex={4}") && l.contains("+ tabIndex={5}"))
        .expect("one row carries both sides of the change");
    assert!(
        paired.find("− tabIndex={4}").unwrap() < paired.find("+ tabIndex={5}").unwrap(),
        "old pane sits left of the new pane: {paired:?}"
    );
    // Context appears on BOTH panes of its row, numbered on each side.
    assert!(
        out.iter()
            .any(|l| l.match_indices("96   required").count() == 2),
        "context is mirrored across the panes: {out:?}"
    );
}

#[test]
fn diff_box_spans_the_full_width_it_is_given() {
    // The box used to clamp itself to 100 columns, so on a wide terminal both panes clipped code
    // at "…" while the right half of the pane sat empty. Every row — header, pane rows, footer —
    // must measure exactly the width handed in, in both the side-by-side (160) and unified (48)
    // layouts. The header once ran one column past the box (its fill under-counted the fixed
    // columns), which the clamp kept hidden inside the pane.
    for width in [48usize, 160] {
        for row in &render_diff_box(&sample_diff(), width) {
            assert_eq!(
                console::measure_text_width(&plain(row)),
                width,
                "row painted off-width at {width}: {:?}",
                plain(row)
            );
        }
    }
}

#[test]
fn verify_line_reads_green_success() {
    let v = VerifyPayload {
        cmd: "cargo check".into(),
        detail: "0 errors · verify gate passed".into(),
    };
    let row = plain(&render_verify_line(&v, 80));
    assert_eq!(row, "✓ cargo check — 0 errors · verify gate passed");
}

/// Paint one whole frame into an in-memory terminal and read the rows back as plain text. Wraps the
/// only end-to-end check there is that the footer's height, its rules and its text rows agree — the
/// layout arithmetic being right is no use if the widgets are handed the wrong rects.
fn painted_rows(state: &mut AppState, w: u16, h: u16) -> Vec<String> {
    let _geom = GeomLock::acquire(); // painting writes the geometry slot
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, state)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..h)
        .map(|y| {
            let row: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
            row.trim_end().to_string()
        })
        .collect()
}

#[test]
fn a_long_prompt_is_painted_down_the_screen_not_off_the_side() {
    // THE BUG this box exists to fix: a prompt longer than one row scrolled sideways, so most of
    // what you typed was hidden and there was no way to read it back — or select it to copy.
    let mut state = AppState::new("intro", "status");
    let prompt = "hay giup toi viet mot doan van rat dai de kiem tra xem hop nhap co xuong dong";
    state.input.draft = prompt.chars().collect();
    state.input.cursor = state.input.draft.len();
    let rows = painted_rows(&mut state, 40, 12);
    assert_eq!(rows[8], "❯ hay giup toi viet mot doan van rat");
    assert_eq!(rows[9], "  dai de kiem tra xem hop nhap co");
    assert_eq!(rows[10], "  xuong dong");
    assert_eq!(rows[7], "─".repeat(40), "top rule sits above the text rows");
    assert_eq!(rows[11], "─".repeat(40), "bottom rule closes the box");
    // Nothing is dropped: the painted rows re-join into the prompt exactly.
    let joined = rows[8..=10]
        .iter()
        .map(|r| r.trim_start_matches("❯ ").trim_start())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(joined, prompt);
}

#[test]
fn a_draft_taller_than_the_box_stops_growing_and_says_what_is_hidden() {
    // The box grows, but not without limit: the transcript keeps its floor, and the rule reports the
    // rows behind it so a capped composer never reads as a truncated one.
    let mut state = AppState::new("intro", "status");
    let many: String = (0..14).map(|i| format!("dong so {i}\n")).collect();
    state.input.draft = many.chars().collect();
    state.input.cursor = 30;
    let rows = painted_rows(&mut state, 40, 16);
    assert_eq!(rows[5], "❯ dong so 0");
    assert_eq!(rows[14], "  dong so 9", "10 text rows is the ceiling");
    assert!(
        rows[15].ends_with("↓5 ──"),
        "the bottom rule counts the hidden rows: {:?}",
        rows[15]
    );
    assert_eq!(rows[0], "intro", "the transcript keeps its floor");
}

#[test]
fn a_wide_terminal_docks_the_sidebar_and_a_narrow_one_does_not() {
    let mut state = AppState::new("intro", "status");
    state.ctx_permille = 420;
    state.facts = SessionFacts {
        tokens: "~6.6K/128.0K tok".to_string(),
        turns: 3,
        session: "phien-thu-nghiem-0822".to_string(),
        provider: "openrouter".to_string(),
        mcp_servers: 1,
        lsp: "rust ready".to_string(),
        memory_facts: 12,
        memory_recalled: 4,
    };
    state.apply_plan(vec![
        PlanRow {
            status: 2,
            text: "read the code".to_string(),
        },
        PlanRow {
            status: 1,
            text: "write the fix".to_string(),
        },
        PlanRow {
            status: 0,
            text: "run the tests".to_string(),
        },
    ]);

    // Wide: the right column carries the header and the live plan. The context gauge stays OUT —
    // the HUD row already draws that same permille, so the sidebar must not repeat it.
    let rows = painted_rows(&mut state, SIDEBAR_MIN_TERM_W, 24);
    let joined = rows.join("\n");
    assert!(
        joined.contains("Aizen v"),
        "sidebar header missing:\n{joined}"
    );
    assert!(joined.contains("Todo 1/3"), "plan tally missing:\n{joined}");
    assert!(joined.contains("write the fix"), "plan rows missing");
    assert!(
        !joined.contains("Context"),
        "context gauge must not be painted in the sidebar:\n{joined}"
    );
    assert!(
        joined.contains("~6.6K/128.0K tok"),
        "token line missing under Session"
    );
    assert!(
        joined.contains("phien-thu-nghiem-0822"),
        "session name missing"
    );
    assert!(
        joined.contains("Provider openrouter"),
        "provider row missing:\n{joined}"
    );
    assert!(joined.contains("MCP 1 server"), "mcp row missing");
    assert!(joined.contains("LSP rust ready"), "lsp row missing");
    assert!(joined.contains("Memory 12 facts"), "memory row missing");
    assert!(
        joined.contains("4 recalled this session"),
        "recall tally missing"
    );
    // The divider sits at one constant column on every row.
    let side_x = (SIDEBAR_MIN_TERM_W - SIDEBAR_W) as usize;
    assert!(
        rows.iter()
            .all(|r| r.chars().nth(side_x).is_some_and(|c| c == '│')),
        "divider column broken"
    );
    // The transcript still paints, narrowed, on the left.
    assert!(rows[0].starts_with("intro"));

    // One column short of the threshold: the conversation keeps the full width.
    let rows = painted_rows(&mut state, SIDEBAR_MIN_TERM_W - 1, 24);
    let joined = rows.join("\n");
    assert!(
        !joined.contains("Todo 1/3"),
        "sidebar must not appear below the width threshold"
    );
}

#[test]
fn sidebar_shows_jobs_and_the_narrow_strip_replaces_it() {
    use crate::ui::tui::{JobRow, JobsStatus};
    let _geom = GeomLock::acquire();
    let mut state = AppState::new("intro", "status");
    state.jobs = JobsStatus {
        processes: vec![
            JobRow {
                id: "proc_1".to_string(),
                running: true,
                elapsed: "7m03s".to_string(),
                detail: "npm run dev".to_string(),
            },
            JobRow {
                id: "proc_2".to_string(),
                running: false,
                elapsed: "3s".to_string(),
                detail: "cargo build".to_string(),
            },
        ],
        monitor: vec![JobRow {
            id: "#4".to_string(),
            running: true,
            elapsed: "12s".to_string(),
            detail: "reviewer · t1".to_string(),
        }],
    };

    // Wide: the sidebar carries a Jobs section — running process named, monitor counted.
    let rows = painted_rows(&mut state, SIDEBAR_MIN_TERM_W, 24);
    let joined = rows.join("\n");
    assert!(joined.contains("Jobs "), "jobs header missing:\n{joined}");
    assert!(
        joined.contains("npm run dev"),
        "running cmd missing:\n{joined}"
    );
    assert!(
        joined.contains("monitor"),
        "monitor count missing:\n{joined}"
    );

    // Narrow: no sidebar, so the one-row strip under the composer carries the summary instead.
    let rows = painted_rows(&mut state, SIDEBAR_MIN_TERM_W - 1, 24);
    let joined = rows.join("\n");
    assert!(
        joined.contains("running") && joined.contains("/jobs"),
        "narrow strip missing:\n{joined}"
    );
    // A click on the strip lands in the panel; its rect is published only while it is painted.
    let strip = jobs_strip_rect().expect("the strip publishes its rect while painted");
    assert!(strip.height == 1 && strip.width > 0);

    // With no jobs, neither surface appears and the strip rect is cleared.
    state.jobs = JobsStatus::default();
    let rows = painted_rows(&mut state, SIDEBAR_MIN_TERM_W - 1, 24);
    assert!(!rows.join("\n").contains("/jobs"));
    assert!(
        jobs_strip_rect().is_none(),
        "stale strip rect must be cleared"
    );
}

#[test]
fn sidebar_jobs_show_only_the_living() {
    use crate::ui::tui::{JobRow, JobsStatus};
    let _geom = GeomLock::acquire();
    let proc_row = |id: &str, running: bool, detail: &str| JobRow {
        id: id.to_string(),
        running,
        elapsed: "7m03s".to_string(),
        detail: detail.to_string(),
    };
    let monitor_row = |id: &str, detail: &str| JobRow {
        id: id.to_string(),
        running: true,
        elapsed: "12s".to_string(),
        detail: detail.to_string(),
    };

    // Completed processes belong to /jobs, not an idle sidebar section.
    let mut state = AppState::new("intro", "status");
    state.jobs = JobsStatus {
        processes: vec![
            proc_row("proc_1", false, "old build"),
            proc_row("proc_2", false, "old serve"),
            proc_row("proc_3", false, "old node"),
        ],
        monitor: vec![],
    };
    let joined = painted_rows(&mut state, SIDEBAR_MIN_TERM_W, 24).join("\n");
    assert!(
        !joined.contains("Jobs "),
        "done-only header shown:\n{joined}"
    );
    assert!(!joined.contains("0 running"), "{joined}");
    assert!(!joined.contains("old "), "done rows shown:\n{joined}");

    // A monitor-only snapshot still shows its live work, without the completed process lane.
    state.jobs = JobsStatus {
        processes: vec![
            proc_row("proc_1", false, "old build"),
            proc_row("proc_2", false, "old serve"),
            proc_row("proc_3", false, "old node"),
            proc_row("proc_4", false, "old test"),
        ],
        monitor: vec![monitor_row("#4", "reviewer · t1")],
    };
    let joined = painted_rows(&mut state, SIDEBAR_MIN_TERM_W, 24).join("\n");
    assert!(joined.contains("Jobs monitor 1"), "{joined}");
    assert!(
        joined.contains("reviewer · t1"),
        "live monitor missing:\n{joined}"
    );
    assert!(!joined.contains("done"), "done count shown:\n{joined}");
    assert!(!joined.contains("old "), "done rows shown:\n{joined}");

    // A running process still outranks the live monitor lane.
    state.jobs = JobsStatus {
        processes: vec![
            proc_row("proc_1", false, "old build"),
            proc_row("proc_9", true, "npm run dev"),
        ],
        monitor: vec![monitor_row("#4", "reviewer · t1")],
    };
    let joined = painted_rows(&mut state, SIDEBAR_MIN_TERM_W, 24).join("\n");
    assert!(joined.contains("Jobs 1 proc · 1 monitor"), "{joined}");
    let r = joined.find("npm run dev").unwrap();
    let m = joined.find("reviewer · t1").unwrap();
    assert!(r < m, "running process → monitor:\n{joined}");
    assert!(!joined.contains("done"), "done count shown:\n{joined}");
    assert!(!joined.contains("old build"), "done row shown:\n{joined}");
}

#[test]
fn jobs_surfaces_clear_on_mixed_to_done_only_to_empty_at_sidebar_boundary() {
    use crate::ui::tui::{JobRow, JobsStatus};
    let _geom = GeomLock::acquire();
    let read_rows = |term: &Terminal<ratatui::backend::TestBackend>| {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
    };
    for width in [125, 124] {
        let mut state = AppState::new("intro", "status");
        state.jobs = JobsStatus {
            processes: vec![
                JobRow {
                    id: "proc_old".into(),
                    running: false,
                    elapsed: "3s".into(),
                    detail: "finished build".into(),
                },
                JobRow {
                    id: "proc_live".into(),
                    running: true,
                    elapsed: "7m03s".into(),
                    detail: "npm run dev".into(),
                },
            ],
            monitor: vec![JobRow {
                id: "#4".into(),
                running: true,
                elapsed: "12s".into(),
                detail: "reviewer · t1".into(),
            }],
        };
        // Reuse the backend through all three frames: stale text must be erased, not just omitted.
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
        term.draw(|frame| draw(frame, &mut state)).unwrap();
        let mixed = read_rows(&term);
        let joined = mixed.join("\n");
        assert!(joined.contains("npm run dev"), "width {width}:\n{joined}");
        assert!(joined.contains("monitor"), "width {width}:\n{joined}");
        assert!(
            !joined.contains("finished build"),
            "width {width}:\n{joined}"
        );
        assert!(!joined.contains("done"), "width {width}:\n{joined}");
        let strip = jobs_strip_rect();
        let live_height = transcript_geom_slot().lock().unwrap().area.height;
        if width == 125 {
            assert!(joined.contains("Jobs 1 proc · 1 monitor"), "{joined}");
            assert!(joined.contains("reviewer · t1"), "{joined}");
            assert!(strip.is_none(), "docked sidebar must retire any strip rect");
        } else {
            let strip = strip.expect("live narrow strip must publish click geometry");
            assert_eq!(strip, Rect::new(0, 23, width, 1));
            assert!(mixed[strip.y as usize].contains("/jobs"), "{joined}");
        }

        for row in &mut state.jobs.processes {
            row.running = false;
        }
        state.jobs.monitor.clear();
        term.draw(|frame| draw(frame, &mut state)).unwrap();
        let done_only = read_rows(&term);
        let joined = done_only.join("\n");
        for hidden in [
            "Jobs ",
            "/jobs",
            "running",
            "monitor",
            "done",
            "npm run dev",
            "finished build",
            "reviewer · t1",
        ] {
            assert!(
                !joined.contains(hidden),
                "done-only width {width}: {hidden} leaked:\n{joined}"
            );
        }
        assert!(
            jobs_strip_rect().is_none(),
            "done-only must clear strip hit rect"
        );
        let done_height = transcript_geom_slot().lock().unwrap().area.height;
        assert_eq!(done_height, live_height + u16::from(width == 124));

        state.jobs = JobsStatus::default();
        term.draw(|frame| draw(frame, &mut state)).unwrap();
        let empty = read_rows(&term);
        assert_eq!(
            done_only, empty,
            "done-only paints exactly like empty at width {width}"
        );
        assert!(
            jobs_strip_rect().is_none(),
            "empty must keep strip hit rect cleared"
        );
    }
}

#[test]
fn jobs_live_monitors_show_without_running_processes() {
    use crate::ui::tui::{JobRow, JobsStatus};
    let _geom = GeomLock::acquire();
    let mut state = AppState::new("intro", "status");
    state.jobs = JobsStatus {
        processes: vec![JobRow {
            id: "proc_old".into(),
            running: false,
            elapsed: "3s".into(),
            detail: "finished build".into(),
        }],
        // Membership in the monitor snapshot means live, even while the run is awaiting input.
        monitor: vec![JobRow {
            id: "#4".into(),
            running: false,
            elapsed: "12s".into(),
            detail: "reviewer · t1".into(),
        }],
    };
    for width in [124, 125] {
        let joined = painted_rows(&mut state, width, 24).join("\n");
        assert!(joined.contains("monitor 1"), "width {width}:\n{joined}");
        assert!(
            joined.contains("reviewer · t1"),
            "live monitor missing at width {width}:\n{joined}"
        );
        assert!(!joined.contains("finished build"), "{joined}");
        assert!(!joined.contains("done"), "{joined}");
        assert!(!joined.contains("0 running"), "{joined}");
        if width == 124 {
            assert!(joined.contains("/jobs"), "{joined}");
            assert!(jobs_strip_rect().is_some());
        } else {
            assert!(joined.contains("Jobs monitor 1"), "{joined}");
            assert!(
                jobs_strip_rect().is_none(),
                "docking must clear the live strip hit rect"
            );
        }
    }
}

#[test]
fn completed_processes_do_not_consume_sidebar_jobs_six_row_cap() {
    use crate::ui::tui::{JobRow, JobsStatus};
    let _geom = GeomLock::acquire();
    let mut processes: Vec<_> = (0..8)
        .map(|i| JobRow {
            id: format!("proc_old_{i}"),
            running: false,
            elapsed: "3s".into(),
            detail: format!("finished build {i}"),
        })
        .collect();
    processes.push(JobRow {
        id: "proc_live".into(),
        running: true,
        elapsed: "7s".into(),
        detail: "npm run dev".into(),
    });
    let mut state = AppState::new("intro", "status");
    state.jobs = JobsStatus {
        processes,
        monitor: (1..=6)
            .map(|i| JobRow {
                id: format!("#{i}"),
                running: true,
                elapsed: "12s".into(),
                detail: format!("live monitor {i}"),
            })
            .collect(),
    };
    let joined = painted_rows(&mut state, 125, 24).join("\n");
    assert!(joined.contains("Jobs 1 proc · 6 monitor"), "{joined}");
    let mut last = joined.find("npm run dev").expect("running process leads");
    for i in 1..=5 {
        let detail = format!("live monitor {i}");
        let at = joined
            .find(&detail)
            .expect("six-row cap must contain only live rows");
        assert!(last < at, "live row order changed:\n{joined}");
        last = at;
    }
    assert!(
        !joined.contains("live monitor 6"),
        "seventh live row exceeds the cap:\n{joined}"
    );
    assert!(
        !joined.contains("finished build"),
        "completed rows consumed the cap:\n{joined}"
    );
    assert!(!joined.contains("done"), "{joined}");
}

#[test]
fn sidebar_wrap_breaks_words_and_marks_overflow() {
    assert_eq!(
        wrap_plain("hello world again", 11, 3),
        vec!["hello world", "again"]
    );
    assert_eq!(
        wrap_plain("aaaaaaaaaaaa", 5, 3),
        vec!["aaaaa", "aaaaa", "aa"]
    );
    let clipped = wrap_plain("one two three four five six seven", 8, 2);
    assert_eq!(clipped.len(), 2);
    assert!(
        clipped[1].ends_with('…'),
        "overflow must be marked: {clipped:?}"
    );
}

#[test]
fn content_width_subtracts_the_docked_sidebar() {
    // Pre-wrap consumers (markdown, stream boxes) must measure the PANE, not the grid — text
    // wrapped to the full grid paints its line tails under the sidebar, where they are clipped.
    COLS.store(140, Ordering::Relaxed);
    assert_eq!(content_width(), 140 - SIDEBAR_W);
    COLS.store(100, Ordering::Relaxed);
    assert_eq!(
        content_width(),
        100,
        "no sidebar → the full grid is the pane"
    );
}

fn subagents_payload(expanded: bool) -> SubAgentsPayload {
    SubAgentsPayload {
        rows: vec![
            SubAgentRow {
                mark: "⋯".into(),
                label: "coder · fix parser".into(),
                detail: "step 3 · file_edit".into(),
                elapsed: "42s".into(),
                running: true,
            },
            SubAgentRow {
                mark: "✓".into(),
                label: "reviewer · audit".into(),
                detail: String::new(),
                elapsed: "7m03s".into(),
                running: false,
            },
        ],
        summary: "coder · fix parser, reviewer · audit — 1 done · 1 running".into(),
        expanded,
    }
}

#[test]
fn subagents_block_collapsed_shows_header_and_summary() {
    let out: Vec<String> = render_subagents_block(&subagents_payload(false), 60)
        .iter()
        .map(|s| plain(s))
        .collect();
    assert!(
        out[0].contains("▸ sub-agents (2) · 1 running"),
        "collapsed header: {:?}",
        out[0]
    );
    // Collapsed: border, one summary line, border — the per-row labels stay hidden.
    assert_eq!(out.len(), 3, "collapsed frame: {out:?}");
    assert!(out[1].contains("reviewer · audit"), "summary row: {out:?}");
    assert!(
        !out.join("\n").contains("step 3 · file_edit"),
        "collapsed hides row detail: {out:?}"
    );
    let w0 = console::measure_text_width(&out[0]);
    assert!(
        out.iter().all(|l| console::measure_text_width(l) == w0),
        "uniform width: {out:?}"
    );
}

#[test]
fn subagents_block_expanded_lists_one_row_per_agent() {
    let out: Vec<String> = render_subagents_block(&subagents_payload(true), 60)
        .iter()
        .map(|s| plain(s))
        .collect();
    assert!(
        out[0].contains("▾ sub-agents (2) · 1 running"),
        "expanded header: {:?}",
        out[0]
    );
    // Border, two agent rows, border — the summary is replaced, not kept.
    assert_eq!(out.len(), 4, "expanded frame: {out:?}");
    assert!(
        out.iter()
            .any(|l| l.contains("⋯ coder · fix parser — step 3 · file_edit · 42s")),
        "running row: {out:?}"
    );
    assert!(
        out.iter().any(|l| l.contains("✓ reviewer · audit — 7m03s")),
        "done row: {out:?}"
    );
    assert!(
        !out.join("\n").contains("1 done · 1 running"),
        "expanded drops the summary line: {out:?}"
    );
    let w0 = console::measure_text_width(&out[0]);
    assert!(
        out.iter().all(|l| console::measure_text_width(l) == w0),
        "uniform width: {out:?}"
    );
}

#[test]
fn subagents_update_is_in_place_and_toggle_is_ui_owned() {
    // The panel mirrors the plan pattern: one stable block, snapshots replace the payload in place,
    // and a snapshot never closes what the user opened with Ctrl-E.
    let mut state = AppState::new("intro", "status");
    state.push_text(BlockKind::Generic, "before".into(), true);
    apply_command(&mut state, Command::SubAgents(subagents_payload(false)));
    state.push_text(BlockKind::Generic, "after".into(), true);
    let panel_pos = state
        .blocks
        .iter()
        .position(|b| b.kind == BlockKind::SubAgents)
        .unwrap();

    apply_command(&mut state, Command::ToggleSubAgents);
    let expanded_flag = state
        .blocks
        .iter()
        .find_map(|b| match &b.payload {
            Payload::SubAgents(p) => Some(p.expanded),
            _ => None,
        })
        .unwrap();
    assert!(expanded_flag, "Ctrl-E opened the panel");

    // A fresh registry snapshot lands in the SAME block — position kept, no stack, still open.
    apply_command(&mut state, Command::SubAgents(subagents_payload(false)));
    assert_eq!(
        state
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::SubAgents)
            .count(),
        1,
        "exactly one sub-agents block"
    );
    assert_eq!(
        state
            .blocks
            .iter()
            .position(|b| b.kind == BlockKind::SubAgents),
        Some(panel_pos),
        "the panel stays where it first appeared"
    );
    let still_open = state
        .blocks
        .iter()
        .find_map(|b| match &b.payload {
            Payload::SubAgents(p) => Some(p.expanded),
            _ => None,
        })
        .unwrap();
    assert!(still_open, "a snapshot never resets the user's toggle");
}

#[test]
fn tool_wrap_preserves_full_paths_ids_and_failure_preview_in_every_state() {
    let source = format!("src/{}/source.rs", "nested_directory/".repeat(18));
    let dest = format!("src/{}/destination.rs", "another_directory/".repeat(18));
    let id = "session-0123456789abcdef".repeat(8);
    for state in [ToolState::Running, ToolState::Ok, ToolState::Err] {
        let event = ToolEvent {
            seq: 77,
            icon: "◆".into(),
            name: "file_move".into(),
            target: format!("{source} → {dest}"),
            display: Default::default(),
            digest: format!("moved {source} → {dest} · {id}"),
            state,
            elapsed_ms: Some(420),
            body: format!("failure details 界e\u{301}👩‍💻 {}", "payload/".repeat(50)),
        };
        let full = plain(&render_tool_row(&event, 10000)).replace('\n', "");
        for width in [2, 3, 18, 90, 105, 123, 124, 125, 139, 140] {
            let out = render_tool_row(&event, width);
            let rows: Vec<_> = out.split('\n').map(plain).collect();
            assert!(
                rows.iter()
                    .all(|r| crate::ui::links::display_width(r) <= width),
                "width {width}: {rows:?}"
            );
            assert_eq!(
                rows.concat(),
                full,
                "wrapping loses no paths, IDs or preview text"
            );
        }
        if state == ToolState::Running {
            let mut running = event.clone();
            running.digest.clear();
            assert_eq!(
                plain(&render_tool_row(&running, 21)).replace('\n', ""),
                format!("◆ file_move   {source} → {dest}")
            );
        }
        for width in 0..=1 {
            assert!(render_tool_row(&event, width)
                .split('\n')
                .all(|r| console::measure_text_width(r) <= width));
        }
    }
}

#[test]
fn sgr_wrap_sanitizes_control_strings_and_keeps_graphemes_and_style() {
    use unicode_segmentation::UnicodeSegmentation;
    let raw = "\x1b[31me\x1b[1m\u{301}界👩‍💻ab\x1b[0m";
    let wanted = "e\u{301}界👩‍💻ab";
    for width in 2..=8 {
        let rows = wrap_keep_sgr(raw, width);
        assert_eq!(rows.iter().map(|r| plain(r)).collect::<String>(), wanted);
        for row in &rows {
            assert!(
                console::measure_text_width(row) <= width,
                "{width}: {row:?}"
            );
            assert!(row.ends_with("\x1b[0m"));
            let spans = ansi_spans(row, Style::default());
            let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
            assert_eq!(text, plain(row));
            for glyph in text.graphemes(true) {
                assert!(wanted.contains(glyph));
            }
        }
        for row in &rows[1..] {
            assert!(row.starts_with("\x1b[31m"));
        }
    }
    assert!(wrap_keep_sgr(raw, 0).iter().all(String::is_empty));
    assert!(wrap_keep_sgr(raw, 1)
        .iter()
        .all(|r| console::measure_text_width(r) <= 1));
    for dirty in [
        "ok\x1b]0;hidden title\x07!",
        "ok\x1b]8;;https://evil.invalid\x1b\\!",
        "ok\x1b[?25l!",
        "ok\x1bPsecret payload\x1b\\!",
        "ok\u{009d}hidden\u{009c}!",
    ] {
        assert_eq!(sanitize_text(dirty), "ok!", "{dirty:?}");
        assert_eq!(plain(&wrap_keep_sgr(dirty, 2).join("")), "ok!");
    }
    let spans = ansi_spans(raw, Style::default());
    assert_eq!(
        spans[0].content, "e\u{301}",
        "SGR inside a grapheme cannot split its base"
    );
    assert_eq!(spans[0].style.fg, Some(Color::Red));
}

#[test]
fn tiny_footer_and_jobs_draw_with_zero_truncation_budgets() {
    let _geom = GeomLock::acquire();
    let mut state = AppState::new("intro", "a status wider than the available HUD cells");
    for width in 0..=3 {
        let layout = input_layout(&state, width, 1);
        assert!(
            layout.rows[0].shown.is_empty(),
            "no placeholder cells at width {width}"
        );
    }
    state.jobs.processes.push(crate::ui::tui::JobRow {
        id: "tiny-fixture".into(),
        running: true,
        elapsed: "1s".into(),
        detail: "a command wider than the jobs sample budget".into(),
    });
    // These widths give zero budgets for the composer placeholder, HUD status and jobs sample.
    // console::truncate_str(..., 0, "…") panics before ratatui has a chance to clip anything.
    for width in 1..=20 {
        let rows = painted_rows(&mut state, width, 10);
        assert_eq!(rows.len(), 10);
    }
}

#[test]
fn wrapped_tool_cache_resize_seq_selection_and_painted_sidebar_stay_aligned() {
    let _geom = GeomLock::acquire();
    let mut state = AppState::new("intro", "status");
    let path = format!("src/{}/file.rs", "directory/".repeat(32));
    apply_command(
        &mut state,
        Command::Tool(ToolEvent {
            seq: 81,
            icon: "◆".into(),
            name: "file_move".into(),
            target: format!("{path} → {path}"),
            display: Default::default(),
            digest: path.clone(),
            state: ToolState::Ok,
            elapsed_ms: Some(30),
            body: String::new(),
        }),
    );
    for width in [124, 125, 140, 3, 2, 1] {
        let _ = painted_rows(&mut state, width, 24);
        let g = transcript_geom_slot().lock().unwrap().clone();
        let budget = crate::ui::links::transcript_content_width(g.area.width);
        assert!(g
            .plain_rows
            .iter()
            .all(|row| crate::ui::links::display_width(row) <= budget as usize));
        assert_eq!(g.plain_rows.len(), g.sgr_rows.len());
        assert_eq!(g.plain_rows.len(), g.row_tool_seq.len());
        let block = state
            .blocks
            .iter()
            .find(|b| matches!(&b.payload, Payload::Tool(_)))
            .unwrap()
            .clone();
        let rendered = state.cache.get_or_render(&block, budget);
        assert_eq!(state.cache.height(&block, budget), rendered.plain.len());
        for (i, seq) in g.row_tool_seq.iter().enumerate() {
            if seq == &Some(81) {
                assert_eq!(tool_seq_at_row(g.rows_offset + i), Some(81));
            }
        }
        if width >= SIDEBAR_MIN_TERM_W {
            let rows = painted_rows(&mut state, width, 24);
            let side_x = (width - SIDEBAR_W) as usize;
            assert!(rows.iter().all(|row| row.chars().nth(side_x) == Some('│')));
        }
    }
    let _ = painted_rows(&mut state, 124, 24);
    state.selection = Some(SelectionRange {
        anchor_line: 1,
        anchor_col: 0,
        cursor_line: 2,
        cursor_col: 4,
    });
    state.scroll_from_tail = 1;
    let _ = painted_rows(&mut state, 125, 24);
    assert!(
        state.selection.is_none(),
        "resize invalidates flat wrapped-row coordinates"
    );
    assert!(state.scroll_from_tail <= state.last_total);
}

#[test]
fn file_move_raw_capture_keeps_full_dest_and_never_links_a_missing_source() {
    let _geom = GeomLock::acquire();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // A relative root has no spaces on any host and keeps both source/dest scanner-shaped.
    let root = std::env::current_dir()
        .unwrap()
        .join(format!(".aizen-link-test-{}-{nonce}", std::process::id()));
    struct TempRoot(std::path::PathBuf);
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = TempRoot(root.clone());
    let relative = format!(
        ".aizen-link-test-{}-{nonce}/{}",
        std::process::id(),
        "nested_directory/".repeat(7)
    );
    let source = format!("{relative}source.rs");
    let dest = format!("{relative}destination.rs");
    std::fs::create_dir_all(std::path::Path::new(&dest).parent().unwrap()).unwrap();
    std::fs::write(&source, "fixture").unwrap();
    std::fs::rename(&source, &dest).unwrap();
    let dest_uri = url::Url::from_file_path(std::env::current_dir().unwrap().join(&dest))
        .unwrap()
        .to_string();
    let mut state = AppState::new("intro", "status");
    apply_command(
        &mut state,
        Command::Tool(ToolEvent {
            seq: 93,
            icon: "◆".into(),
            name: "file_move".into(),
            target: format!("{source} → {dest}"),
            display: Default::default(),
            digest: format!("moved {source} → {dest}"),
            state: ToolState::Ok,
            elapsed_ms: Some(40),
            body: String::new(),
        }),
    );
    for width in [124, 125, 140] {
        let _ = painted_rows(&mut state, width, 40);
        let g = transcript_geom_slot().lock().unwrap().clone();
        assert!(
            g.plain_rows.concat().contains(&source),
            "generation remains lossless after move"
        );
        let mut out = Vec::new();
        crate::ui::links::inject_hyperlinks(
            &mut out,
            &g.sgr_rows,
            &g.plain_rows,
            &crate::ui::links::InjectCtx {
                start: g.start,
                visible: g.visible,
                area: g.area,
                rows_offset: g.rows_offset,
                ..Default::default()
            },
        );
        let links = crate::ui::links::captured_links(&out);
        assert!(!links.is_empty(), "the real destination must link");
        assert!(links.iter().all(|(_, _, uri, _)| uri == &dest_uri));
        assert_eq!(
            links
                .iter()
                .map(|(_, _, _, text)| plain(text))
                .collect::<String>(),
            format!("{dest}{dest}")
        );
        let right = g.area.x as usize + g.area.width as usize - 1;
        assert!(links
            .iter()
            .all(|(x, _, _, text)| *x + crate::ui::links::display_width(&plain(text)) <= right));
    }
}

#[test]
fn transcript_tool_paint_and_raw_capture_respect_nonzero_origin_sentinels() {
    let _geom = GeomLock::acquire();
    let area = Rect::new(7, 3, 27, 6);
    let uri = format!("https://example.com/{}", "x/".repeat(50));
    let mut state = AppState::new("", "status");
    apply_command(
        &mut state,
        Command::Tool(ToolEvent {
            seq: 94,
            icon: "◆".into(),
            name: "http_fetch".into(),
            target: uri.clone(),
            display: Default::default(),
            digest: String::new(),
            state: ToolState::Running,
            elapsed_ms: None,
            body: String::new(),
        }),
    );
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(48, 14)).unwrap();
    term.draw(|frame| {
        frame.render_widget(
            Paragraph::new(vec![Line::from("S".repeat(48)); 14]),
            frame.area(),
        );
        draw_transcript(frame, area, &mut state);
    })
    .unwrap();
    let buf = term.backend().buffer();
    for y in 0..14 {
        for x in 0..48 {
            if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
                assert_eq!(buf[(x, y)].symbol(), "S", "off-pane sentinel ({x},{y})");
            }
        }
    }
    let g = transcript_geom_slot().lock().unwrap().clone();
    let mut out = Vec::new();
    crate::ui::links::inject_hyperlinks(
        &mut out,
        &g.sgr_rows,
        &g.plain_rows,
        &crate::ui::links::InjectCtx {
            start: g.start,
            visible: g.visible,
            area,
            rows_offset: g.rows_offset,
            occluders: g.jump_button.into_iter().collect(),
            ..Default::default()
        },
    );
    let links = crate::ui::links::captured_links(&out);
    assert!(!links.is_empty());
    for (x, y, target, text) in links {
        assert_eq!(target, uri);
        assert!(x >= area.x as usize && y >= area.y as usize);
        assert!(
            x + crate::ui::links::display_width(&plain(&text)) < (area.x + area.width) as usize
        );
        assert!(y < (area.y + area.height) as usize);
    }
}

#[test]
fn painted_tool_rows_and_raw_links_share_sidebar_scrollbar_geometry() {
    let _geom = GeomLock::acquire();
    let uri = format!("https://example.com/{}", "界e\u{301}👩‍💻long/".repeat(45));
    let mut state = AppState::new("intro", "status");
    apply_command(
        &mut state,
        Command::Tool(ToolEvent {
            seq: 92,
            icon: "◆".into(),
            name: "http_fetch".into(),
            target: uri.clone(),
            display: Default::default(),
            digest: format!("downloaded {uri}"),
            state: ToolState::Ok,
            elapsed_ms: Some(50),
            body: String::new(),
        }),
    );
    for width in [124, 125, 140] {
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(width, 16)).unwrap();
        term.draw(|frame| draw(frame, &mut state)).unwrap();
        state.scroll_from_tail = 3;
        term.draw(|frame| draw(frame, &mut state)).unwrap();
        let g = transcript_geom_slot().lock().unwrap().clone();
        let caret = *caret_slot().lock().unwrap();
        let mut out = Vec::new();
        crate::ui::links::inject_hyperlinks(
            &mut out,
            &g.sgr_rows,
            &g.plain_rows,
            &crate::ui::links::InjectCtx {
                start: g.start,
                visible: g.visible,
                area: g.area,
                rows_offset: g.rows_offset,
                occluders: g.jump_button.into_iter().collect(),
                caret,
            },
        );
        let links = crate::ui::links::captured_links(&out);
        assert!(!links.is_empty(), "raw capture must exercise the injector");
        let right = (g.area.x + g.area.width - 1) as usize;
        for (x, y, target, raw) in links {
            assert_eq!(target, uri, "all fragments carry a FULL URI");
            assert!(x + crate::ui::links::display_width(&plain(&raw)) <= right);
            assert!(y < g.area.y as usize + g.area.height as usize);
            assert!(raw.ends_with("\x1b[0m"));
        }
        // The ratatui buffer is asserted separately: it cannot see these raw post-draw writes.
        for y in 0..g.area.height {
            let scrollbar_cell = term.backend().buffer()[(g.area.x + g.area.width - 1, y)].symbol();
            assert!(matches!(scrollbar_cell, " " | "█"));
            if width >= SIDEBAR_MIN_TERM_W {
                assert_eq!(
                    term.backend().buffer()[(width - SIDEBAR_W, y)].symbol(),
                    "│"
                );
            }
        }
    }
}
