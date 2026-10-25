//! Bounded, immutable visible-cell selection; never buffers incoming PTY output.

use crate::{
    app::{App, Flash, FlashKind, HitTarget},
    terminal::is_copy_key,
};
use crossterm::event::{Event, KeyCode, MouseButton, MouseEventKind};
use sylvops_core::protocol::{MAX_TERMINAL_COLUMNS, MAX_TERMINAL_ROWS};

pub(crate) const MAX_SELECTION_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Position {
    row: u16,
    column: u16,
}

#[derive(Debug)]
pub(crate) struct TerminalSelection {
    cells: Vec<vt100::Cell>,
    wrapped: Vec<bool>,
    pub rows: u16,
    pub columns: u16,
    anchor: Position,
    focus: Position,
    pub dragging: bool,
}

impl TerminalSelection {
    pub fn has_range(&self) -> bool {
        self.anchor != self.focus
    }
    pub fn capture(
        screen: &vt100::Screen,
        rows: u16,
        columns: u16,
        row: u16,
        column: u16,
    ) -> Result<Self, &'static str> {
        let (screen_rows, screen_columns) = screen.size();
        let rows = rows.min(screen_rows).min(MAX_TERMINAL_ROWS);
        let columns = columns.min(screen_columns).min(MAX_TERMINAL_COLUMNS);
        if rows == 0 || columns == 0 {
            return Err("Terminal selection is unavailable in an empty viewport.");
        }
        let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(columns));
        let mut bytes = usize::from(rows); // Include every possible newline in the budget.
        let mut wrapped = Vec::with_capacity(usize::from(rows));
        for row in 0..rows {
            // A partially visible row cannot join text hidden beyond the viewport.
            wrapped.push(columns == screen_columns && screen.row_wrapped(row));
            for column in 0..columns {
                let cell = screen
                    .cell(row, column)
                    .ok_or("Terminal selection is unavailable.")?;
                bytes += cell.contents().len().max(1);
                if bytes > MAX_SELECTION_BYTES {
                    return Err("Terminal selection exceeds the 1 MiB limit.");
                }
                cells.push(cell.clone());
            }
        }
        let mut selection = Self {
            cells,
            wrapped,
            rows,
            columns,
            anchor: Position { row: 0, column: 0 },
            focus: Position { row: 0, column: 0 },
            dragging: true,
        };
        selection.anchor = selection.position(row, column);
        selection.focus = selection.anchor;
        Ok(selection)
    }

    pub fn cell(&self, row: u16, column: u16) -> Option<&vt100::Cell> {
        if row >= self.rows || column >= self.columns {
            return None;
        }
        self.cells
            .get(usize::from(row) * usize::from(self.columns) + usize::from(column))
    }

    fn position(&self, row: u16, column: u16) -> Position {
        let row = row.min(self.rows - 1);
        let mut column = column.min(self.columns - 1);
        if self
            .cell(row, column)
            .is_some_and(vt100::Cell::is_wide_continuation)
        {
            column = column.saturating_sub(1);
        }
        Position { row, column }
    }

    pub fn update(&mut self, row: u16, column: u16) {
        if self.dragging {
            self.focus = self.position(row, column);
        }
    }

    pub fn is_selected(&self, row: u16, column: u16) -> bool {
        if self.anchor == self.focus || row >= self.rows || column >= self.columns {
            return false;
        }
        let position = self.position(row, column);
        let (start, end) = self.range();
        position >= start && position <= end
    }

    fn range(&self) -> (Position, Position) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }

    pub fn selected_text(&self) -> Option<String> {
        if self.anchor == self.focus {
            return None;
        }
        let (start, end) = self.range();
        let mut text = String::new();
        for row in start.row..=end.row {
            let first = if row == start.row { start.column } else { 0 };
            let mut last = if row == end.row {
                end.column
            } else {
                self.columns - 1
            };
            // Remove unused terminal padding, retaining spaces actually present in cells.
            while last > first
                && self
                    .cell(row, last)
                    .is_some_and(|cell| !cell.has_contents())
            {
                last -= 1;
            }
            for column in first..=last {
                let cell = self.cell(row, column)?;
                if !cell.is_wide_continuation() {
                    if cell.has_contents() {
                        text.push_str(cell.contents());
                    } else if column < last {
                        text.push(' ');
                    }
                }
            }
            if row < end.row && !self.wrapped[usize::from(row)] {
                text.push('\n');
            }
        }
        (!text.is_empty()).then_some(text)
    }
}

pub(crate) enum SelectionAction {
    Unhandled,
    Handled,
    Copy(String),
}

/// Consume local selection gestures before any event can become provider input.
pub(crate) fn handle_input(app: &mut App, input: &Event) -> SelectionAction {
    let viewport = app
        .hit_map
        .0
        .iter()
        .rev()
        .find(|region| region.target == HitTarget::TerminalViewport)
        .map(|region| region.area);
    let Some(terminal) = app.attached.as_mut() else {
        return SelectionAction::Unhandled;
    };
    let copy = matches!(input, Event::Key(key) if is_copy_key(*key))
        || matches!(input, Event::Mouse(mouse)
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && app.hit_map.target_at(mouse.column, mouse.row) == Some(HitTarget::TerminalCopy));
    if copy {
        return terminal.selected_text().map_or_else(
            || {
                app.flash = Some(Flash::transient(
                    FlashKind::Info,
                    "Select terminal text first.",
                ));
                SelectionAction::Handled
            },
            SelectionAction::Copy,
        );
    }
    match input {
        Event::Key(key) if key.code == KeyCode::Esc && terminal.selection.is_some() => {
            terminal.scroll_to_latest();
            SelectionAction::Handled
        }
        Event::Mouse(mouse) => {
            let Some(viewport) = viewport else {
                return SelectionAction::Unhandled;
            };
            let row = mouse.row.saturating_sub(viewport.y);
            let column = mouse.column.saturating_sub(viewport.x);
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if app.hit_map.target_at(mouse.column, mouse.row)
                        == Some(HitTarget::TerminalViewport) =>
                {
                    if let Err(error) =
                        terminal.begin_selection(row, column, viewport.height, viewport.width)
                    {
                        app.flash = Some(Flash::error(error));
                    }
                    SelectionAction::Handled
                }
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                    if terminal
                        .selection
                        .as_ref()
                        .is_some_and(|selection| selection.dragging) =>
                {
                    terminal.update_selection(row, column);
                    if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                        terminal.finish_selection();
                    }
                    SelectionAction::Handled
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    terminal.clear_selection();
                    SelectionAction::Unhandled
                }
                _ => SelectionAction::Unhandled,
            }
        }
        Event::Key(_) | Event::Paste(_) | Event::Resize(_, _) => {
            terminal.clear_selection();
            SelectionAction::Unhandled
        }
        _ => SelectionAction::Unhandled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{FocusZone, Mode},
        render,
        terminal::AttachedTerminal,
    };
    use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent};
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use sylvops_core::{
        domain::{AttachmentRole, DaemonSnapshot},
        ids::SessionId,
    };

    fn fixture(role: AttachmentRole) -> (App, Terminal<TestBackend>, Rect) {
        let mut app = App::new(DaemonSnapshot::default(), Vec::new(), None);
        app.focus = FocusZone::Main;
        app.mode = Mode::TerminalAttached;
        let mut parser = vt100::Parser::new(6, 40, 100);
        parser.process(b"hello\r\nworld");
        app.attached = Some(AttachedTerminal {
            terminal_revision: 0,
            scrollback_capacity_bytes: 4 * 1024 * 1024,
            selection: None,
            session_id: SessionId::new(),
            role,
            parser,
            last_sequence: 0,
            columns: 40,
            rows: 6,
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let viewport = app
            .hit_map
            .0
            .iter()
            .find(|region| region.target == HitTarget::TerminalViewport)
            .unwrap()
            .area;
        (app, terminal, viewport)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn select(app: &mut App, viewport: Rect) {
        for (kind, column, row) in [
            (
                MouseEventKind::Down(MouseButton::Left),
                viewport.x,
                viewport.y,
            ),
            (
                MouseEventKind::Drag(MouseButton::Left),
                viewport.x + 4,
                viewport.y + 1,
            ),
            (
                MouseEventKind::Up(MouseButton::Left),
                viewport.x + 4,
                viewport.y + 1,
            ),
        ] {
            assert!(matches!(
                handle_input(app, &mouse(kind, column, row)),
                SelectionAction::Handled
            ));
        }
    }

    #[test]
    fn controller_and_observer_copy_gestures_are_local_and_keep_selection() {
        for role in [AttachmentRole::Controller, AttachmentRole::Observer] {
            let (mut app, mut terminal, viewport) = fixture(role);
            select(&mut app, viewport);
            let copy = Event::Key(KeyEvent::new(
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ));
            let SelectionAction::Copy(text) = handle_input(&mut app, &copy) else {
                panic!("copy must be consumed locally");
            };
            assert_eq!(text, "hello\nworld");
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let button = app
                .hit_map
                .0
                .iter()
                .find(|region| region.target == HitTarget::TerminalCopy)
                .unwrap()
                .area;
            assert!(matches!(
                handle_input(
                    &mut app,
                    &mouse(MouseEventKind::Down(MouseButton::Left), button.x, button.y)
                ),
                SelectionAction::Copy(_)
            ));
            assert!(app.attached.as_ref().unwrap().selection.is_some());
            let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert!(matches!(
                handle_input(&mut app, &escape),
                SelectionAction::Handled
            ));
            assert!(matches!(
                handle_input(&mut app, &escape),
                SelectionAction::Unhandled
            ));
            assert!(matches!(
                handle_input(&mut app, &copy),
                SelectionAction::Handled
            ));
        }
    }

    #[test]
    fn input_resize_and_scroll_clear_selection_before_the_existing_handler() {
        let (mut app, _, viewport) = fixture(AttachmentRole::Controller);
        for event in [
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Event::Paste("pasted".into()),
            Event::Resize(90, 30),
            mouse(MouseEventKind::ScrollUp, viewport.x, viewport.y),
        ] {
            select(&mut app, viewport);
            assert!(matches!(
                handle_input(&mut app, &event),
                SelectionAction::Unhandled
            ));
            assert!(app.attached.as_ref().unwrap().selection.is_none());
        }
    }

    #[test]
    fn escape_returns_to_latest_output_after_selecting_scrollback() {
        let (mut app, _, viewport) = fixture(AttachmentRole::Observer);
        let attached = app.attached.as_mut().unwrap();
        attached
            .parser
            .process(b"\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven");
        attached.scroll_lines(1);
        assert!(attached.is_scrolled_back());
        select(&mut app, viewport);
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            handle_input(&mut app, &escape),
            SelectionAction::Handled
        ));
        assert!(!app.attached.as_ref().unwrap().is_scrolled_back());
        assert!(app.attached.as_ref().unwrap().selection.is_none());
    }

    #[test]
    fn dragging_outside_clamps_to_snapshot_and_clicks_outside_do_not_start_selection() {
        let (mut app, _, viewport) = fixture(AttachmentRole::Observer);
        assert!(matches!(
            handle_input(
                &mut app,
                &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0)
            ),
            SelectionAction::Unhandled
        ));
        assert!(app.attached.as_ref().unwrap().selection.is_none());
        handle_input(
            &mut app,
            &mouse(
                MouseEventKind::Down(MouseButton::Left),
                viewport.x,
                viewport.y,
            ),
        );
        handle_input(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), u16::MAX, u16::MAX),
        );
        handle_input(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), u16::MAX, u16::MAX),
        );
        let selection = app.attached.as_ref().unwrap().selection.as_ref().unwrap();
        assert!(!selection.dragging);
        assert!(selection.is_selected(5, 39));
    }

    #[test]
    fn selection_highlights_frozen_cells_without_changing_geometry_in_every_theme() {
        for theme in sylvops_core::ui::TuiTheme::ALL {
            let (mut app, mut terminal, viewport) = fixture(AttachmentRole::Controller);
            app.theme = theme;
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let original = terminal
                .backend()
                .buffer()
                .cell((viewport.x, viewport.y))
                .unwrap()
                .clone();
            select(&mut app, viewport);
            app.attached
                .as_mut()
                .unwrap()
                .parser
                .process(b"\x1b[2J\x1b[Hchanged");
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let highlighted = terminal
                .backend()
                .buffer()
                .cell((viewport.x, viewport.y))
                .unwrap();
            assert_eq!(highlighted.symbol(), "h");
            assert_ne!(highlighted.style(), original.style());
            assert_eq!(
                app.hit_map
                    .0
                    .iter()
                    .find(|region| region.target == HitTarget::TerminalViewport)
                    .unwrap()
                    .area,
                viewport
            );
            app.attached.as_mut().unwrap().clear_selection();
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            assert_eq!(
                terminal
                    .backend()
                    .buffer()
                    .cell((viewport.x, viewport.y))
                    .unwrap()
                    .symbol(),
                "c"
            );
        }
    }

    #[test]
    fn copy_stays_in_the_border_in_narrow_and_short_layouts() {
        for (width, height) in [(80, 24), (50, 18), (30, 12)] {
            let (mut app, _, _) = fixture(AttachmentRole::Observer);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let viewport = app
                .hit_map
                .0
                .iter()
                .find(|region| region.target == HitTarget::TerminalViewport)
                .unwrap()
                .area;
            assert!(
                !app.hit_map
                    .0
                    .iter()
                    .any(|region| region.target == HitTarget::TerminalCopy)
            );
            select(&mut app, viewport);
            terminal
                .draw(|frame| render::draw(frame, &mut app))
                .unwrap();
            let button = app
                .hit_map
                .0
                .iter()
                .find(|region| region.target == HitTarget::TerminalCopy)
                .unwrap()
                .area;
            assert!(button.y < viewport.y);
            assert!(button.right() <= width);
            assert_eq!(
                app.hit_map
                    .0
                    .iter()
                    .find(|region| region.target == HitTarget::TerminalViewport)
                    .unwrap()
                    .area,
                viewport
            );
            assert!(matches!(
                handle_input(
                    &mut app,
                    &mouse(MouseEventKind::Down(MouseButton::Left), button.x, button.y)
                ),
                SelectionAction::Copy(_)
            ));
        }
    }
}
