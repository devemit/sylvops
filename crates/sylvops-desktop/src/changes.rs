// UI pixels and bounded row indexes are converted only within the review viewport.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
use super::{
    Border, ClientRequest, Color, ContentSurface, DaemonResponse, DesktopApp, Duration, Element,
    Fill, Font, Instant, MainTab, Message, Operation, Task, bounded_redacted_diagnostic, button,
    centered_message, clipboard, container, diff_line_style, mouse, mouse_area, responsive,
    rich_text, row, scrollable, sensor, space, span, text, text_input, widget, workspace_surface,
};
use iced::widget::{column, slider};
use sylvops_core::changes::{ChangeGroup, ChangeKey, ReviewState, display_path};

const LINE_HEIGHT: f32 = 24.0;

#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // Independent viewport and request-coalescing controls.
pub(super) struct Review {
    pub(super) state: ReviewState,
    pending: Option<Operation>,
    last_refresh: Option<Instant>,
    updated_at: Option<Instant>,
    queued: bool,
    list_scroll: usize,
    list_width: f32,
    rendered_split: bool,
    selection: Option<(usize, usize)>,
    selecting: bool,
    scroll_to: Option<usize>,
    reset_horizontal: bool,
    reset_list_scroll: bool,
    discard_pending: bool,
    prepared_syntax: Option<Vec<Vec<sylvops_core::changes::SyntaxToken>>>,
    columns: usize,
    resizing: bool,
}

#[derive(Clone, Debug)]
pub(super) enum Action {
    Select(ChangeKey),
    Filter(String),
    Search(String),
    Group(ChangeGroup),
    Refresh,
    Pause,
    Split,
    Back,
    Jump(bool, bool),
    CopyPath,
    CopyDiff,
    Scroll(usize),
    Layout(iced::Size),
    SelectStart(usize),
    SelectExtend(usize),
    SelectEnd,
    ListScroll(usize),
    Width(f32),
    ResizeStart,
    ResizeMove(f32),
    ResizeEnd,
    Horizontal(f32),
}

impl DesktopApp {
    pub(super) fn review_prepared(
        &mut self,
        operation: Operation,
        prepared: sylvops_core::changes::PreparedFile,
    ) {
        self.review.prepared_syntax = Some(prepared.syntax);
        self.review_response(operation, DaemonResponse::FileChange(prepared.file));
        self.review.prepared_syntax = None;
    }
    pub(super) fn review_scroll_task(&mut self) -> Option<Task<Message>> {
        let mut tasks = Vec::new();
        if let Some(line) = self.review.scroll_to.take() {
            tasks.push(widget::operation::scroll_to(
                "changes-diff",
                scrollable::AbsoluteOffset {
                    x: std::mem::take(&mut self.review.reset_horizontal).then_some(0.0),
                    y: Some(line as f32 * LINE_HEIGHT),
                },
            ));
        }
        if std::mem::take(&mut self.review.reset_list_scroll) {
            tasks.push(widget::operation::scroll_to(
                "changes-files",
                scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
            ));
        }
        if tasks.is_empty() {
            None
        } else {
            Some(Task::batch(tasks))
        }
    }

    fn review_row(&self) -> usize {
        if self.review.rendered_split {
            self.review
                .state
                .file
                .as_ref()
                .and_then(|f| {
                    f.split_rows().iter().position(|(a, b)| {
                        *a == Some(self.review.state.scroll) || *b == Some(self.review.state.scroll)
                    })
                })
                .unwrap_or(0)
        } else {
            self.review.state.scroll
        }
    }
    pub(super) fn poll_review(&mut self) {
        if self.main_tab != MainTab::Changes || self.content_surface != ContentSurface::Session {
            return;
        }
        if self.selected_worktree_id != self.review.state.checkout
            || (!self.review.state.paused
                && self
                    .review
                    .last_refresh
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(2)))
        {
            self.refresh_review();
        }
    }

    pub(super) fn refresh_review(&mut self) {
        let Some(id) = self.selected_worktree_id else {
            return;
        };
        if self.review.state.checkout != Some(id) {
            self.review.discard_pending = false;
        }
        self.review.state.enter(id);
        if self.review.pending.is_some() {
            self.review.queued = true;
            return;
        }
        self.review.last_refresh = Some(Instant::now());
        self.review.queued = false;
        self.send_review(
            Operation::ReviewList(id, self.review.state.generation),
            ClientRequest::ListCheckoutChanges { worktree_id: id },
        );
    }

    fn send_review(&mut self, operation: Operation, request: ClientRequest) {
        if self.bridge.request(operation, request) {
            self.review.pending = Some(operation);
        } else {
            self.review.state.error = Some("Background service busy. Refresh to retry.".into());
        }
    }

    fn load_review_file(&mut self) {
        if self.review.pending.is_some() {
            self.review.queued = true;
            return;
        }
        if let (Some(id), Some(key)) = (
            self.review.state.checkout,
            self.review.state.selected.clone(),
        ) {
            self.send_review(
                Operation::ReviewFile(id, self.review.state.generation),
                ClientRequest::LoadFileChange {
                    worktree_id: id,
                    key,
                },
            );
        }
    }

    pub(super) fn review_failure(&mut self, operation: Option<Operation>, message: &str) -> bool {
        if !matches!(
            operation,
            Some(Operation::ReviewList(_, _) | Operation::ReviewFile(_, _))
        ) {
            return false;
        }
        if self.review.pending == operation {
            self.review.pending = None;
            if let Some(
                Operation::ReviewList(id, generation) | Operation::ReviewFile(id, generation),
            ) = operation
                && (self.selected_worktree_id != Some(id)
                    || self.review.state.checkout != Some(id)
                    || self.review.state.generation != generation)
            {
                self.review.queued = false;
                if self.review.state.checkout == self.selected_worktree_id {
                    self.load_review_file();
                } else {
                    self.refresh_review();
                }
                return true;
            }
            self.review.state.error = Some(message.to_owned());
            self.review.last_refresh = Some(Instant::now());
        }
        true
    }

    pub(super) fn review_response(&mut self, operation: Operation, response: DaemonResponse) {
        if self.review.pending != Some(operation) {
            return;
        }
        self.review.pending = None;
        if self.review.discard_pending {
            self.review.discard_pending = false;
            self.review.queued = false;
            return;
        }
        let (Operation::ReviewList(id, generation) | Operation::ReviewFile(id, generation)) =
            operation
        else {
            return;
        };
        if self.selected_worktree_id != Some(id) || self.review.state.checkout != Some(id) {
            self.refresh_review();
            return;
        }
        if self.review.state.generation != generation {
            self.review.queued = false;
            self.load_review_file();
            return;
        }
        match response {
            DaemonResponse::CheckoutChanges(list) if list.worktree_id == id => {
                self.review.state.apply_list(list);
                self.load_review_file();
            }
            DaemonResponse::FileChange(file) => {
                let previous_row = self.review_row();
                self.review.columns = file
                    .lines
                    .iter()
                    .map(|line| {
                        line.text.chars().fold(0_usize, |column, c| {
                            column
                                + if c == '\t' {
                                    8 - column % 8
                                } else if c.is_ascii() {
                                    1
                                } else {
                                    2
                                }
                        })
                    })
                    .max()
                    .unwrap_or(0);
                if let Some(syntax) = self.review.prepared_syntax.take() {
                    self.review
                        .state
                        .apply_prepared(sylvops_core::changes::PreparedFile { file, syntax });
                } else {
                    self.review.state.apply_file(file);
                }
                let next_row = self.review_row();
                if next_row != previous_row {
                    self.review.scroll_to = Some(next_row);
                }
                if self.review.state.updated {
                    self.review.updated_at = Some(Instant::now());
                }
                if self.review.queued {
                    self.refresh_review();
                }
            }
            DaemonResponse::Error(error) => {
                self.review.state.error = Some(bounded_redacted_diagnostic(&error.message));
            }
            _ => {
                self.review.state.error =
                    Some("Unexpected Changes response. Refresh to retry.".into());
            }
        }
    }

    #[allow(clippy::too_many_lines)] // One UI action reducer; daemon requests remain delegated.
    pub(super) fn update_review(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Select(key) => {
                self.review.discard_pending = false;
                self.review.state.select(Some(key));
                self.review.selection = None;
                self.review.scroll_to = Some(0);
                self.review.reset_horizontal = true;
                self.review.state.detail = true;
                self.load_review_file();
            }
            Action::Filter(value) => {
                self.review.state.filter = value.chars().take(256).collect();
                self.review.list_scroll = 0;
                self.review.reset_list_scroll = true;
                if !self
                    .review
                    .state
                    .visible()
                    .iter()
                    .any(|f| Some(&f.key) == self.review.state.selected.as_ref())
                {
                    let key = self.review.state.visible().first().map(|f| f.key.clone());
                    self.review.state.select(key);
                    self.review.discard_pending = false;
                    self.review.scroll_to = Some(0);
                    self.review.reset_horizontal = true;
                    self.load_review_file();
                }
            }
            Action::Search(value) => self.review.state.search = value.chars().take(256).collect(),
            Action::Group(group) => {
                self.review.state.collapsed[group as usize] =
                    !self.review.state.collapsed[group as usize];
            }
            Action::Refresh => {
                self.review.discard_pending = false;
                self.refresh_review();
            }
            Action::Pause => {
                self.review.state.paused = !self.review.state.paused;
                if self.review.state.paused {
                    self.review.discard_pending = self.review.pending.is_some();
                    self.review.queued = false;
                }
                if !self.review.state.paused {
                    self.review.discard_pending = false;
                    self.refresh_review();
                }
            }
            Action::Split => {
                self.review.state.split = !self.review.state.split;
                self.review.state.scroll = 0;
                self.review.scroll_to = Some(0);
                self.review.reset_horizontal = true;
            }
            Action::Back => self.review.state.detail = false,
            Action::Jump(forward, search) => {
                self.review.state.jump(forward, search);
                self.review.scroll_to = Some(self.review_row());
            }
            Action::CopyPath => {
                if let Some(key) = &self.review.state.selected {
                    return clipboard::write(key.display_path());
                }
            }
            Action::CopyDiff => {
                if let Some(file) = &self.review.state.file {
                    let range = self.review.selection.map(|(a, b)| a.min(b)..=a.max(b));
                    return clipboard::write(
                        file.lines
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| range.as_ref().is_none_or(|r| r.contains(i)))
                            .map(|(_, l)| l.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
            }
            Action::Scroll(line) => {
                self.review.state.scroll = if self.review.rendered_split {
                    self.review
                        .state
                        .file
                        .as_ref()
                        .and_then(|f| f.split_rows().get(line).copied())
                        .and_then(|(a, b)| a.or(b))
                        .unwrap_or(0)
                } else {
                    line
                };
            }
            Action::Layout(size) => {
                let split = self.review.state.split && size.width >= 1_000.0;
                if self.review.rendered_split != split {
                    self.review.rendered_split = split;
                    self.review.scroll_to = Some(self.review_row());
                }
            }
            Action::SelectStart(line) => {
                self.review.selecting = true;
                self.review.selection = Some((line, line));
            }
            Action::SelectExtend(line) => {
                if self.review.selecting
                    && let Some((start, _)) = self.review.selection
                {
                    self.review.selection = Some((start, line));
                }
            }
            Action::SelectEnd => self.review.selecting = false,
            Action::ListScroll(line) => self.review.list_scroll = line,
            Action::Width(width) => self.review.list_width = width.clamp(180.0, 480.0),
            Action::ResizeStart => self.review.resizing = true,
            Action::ResizeMove(width) => {
                if self.review.resizing {
                    self.review.list_width = width.clamp(180.0, 480.0);
                }
            }
            Action::ResizeEnd => self.review.resizing = false,
            Action::Horizontal(position) => {
                self.review.state.horizontal = (position as usize).min(self.review.columns);
            }
        }
        self.review_scroll_task().unwrap_or_else(Task::none)
    }

    pub(super) fn review_view(&self) -> Element<'_, Message> {
        if self.selected_worktree_id.is_none() {
            return centered_message("Select a checkout to review its changes.");
        }
        if self.review.state.checkout != self.selected_worktree_id {
            return centered_message("Loading changes…");
        }
        let review = &self.review.state;
        let name = self
            .snapshot
            .worktrees
            .iter()
            .find(|w| Some(w.id) == self.selected_worktree_id)
            .map_or_else(String::new, |w| {
                format!("{} · {}", w.name, w.branch.as_deref().unwrap_or("detached"))
            });
        let toolbar = row![
            text(name).size(14),
            space::horizontal(),
            button(if review.paused { "Resume" } else { "Pause" })
                .on_press(Message::Review(Action::Pause)),
            button("Refresh now").on_press(Message::Review(Action::Refresh))
        ]
        .spacing(8);
        let mut body = column![toolbar].spacing(8);
        if let Some(error) = &review.error {
            body = body.push(text(format!("Stale · {error}")).size(12));
        }
        if review.truncated {
            body = body.push(text("File list incomplete: safety limit reached. Narrow the checkout’s changes to review all files.").size(12));
        }
        if self
            .review
            .updated_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
        {
            body = body.push(text("Updated").size(12));
        }
        body = body
            .push(responsive(move |size| {
                let narrow = size.width < 800.0;
                let list = self.review_file_list(size.height, !narrow);
                if narrow {
                    if review.detail {
                        self.review_detail(size.width, size.height)
                    } else {
                        list
                    }
                } else {
                    let width = self.review_file_width();
                    mouse_area(
                        row![
                            container(list).width(width),
                            mouse_area(container(space::horizontal()).width(6).height(Fill).style(
                                |theme: &iced::Theme| {
                                    container::Style {
                                        background: Some(
                                            Color {
                                                a: 0.15,
                                                ..theme.palette().primary
                                            }
                                            .into(),
                                        ),
                                        ..container::Style::default()
                                    }
                                }
                            ))
                            .on_press(Message::Review(Action::ResizeStart))
                            .interaction(mouse::Interaction::ResizingHorizontally),
                            self.review_detail(size.width - width - 22.0, size.height)
                        ]
                        .spacing(8)
                        .height(Fill),
                    )
                    .on_move(|point| Message::Review(Action::ResizeMove(point.x)))
                    .on_release(Message::Review(Action::ResizeEnd))
                    .on_exit(Message::Review(Action::ResizeEnd))
                    .into()
                }
            }))
            .height(Fill);
        container(body)
            .padding(14)
            .height(Fill)
            .width(Fill)
            .style(|theme| {
                let mut style = workspace_surface(theme);
                style.text_color = Some(theme.palette().text);
                style
            })
            .into()
    }

    fn review_file_list(&self, height: f32, resizable: bool) -> Element<'_, Message> {
        let state = &self.review.state;
        let mut list = column![].spacing(0);
        let query = state.filter.to_lowercase();
        let mut rows = Vec::new();
        for group in ChangeGroup::ALL {
            let count = state.files.iter().filter(|f| f.key.group == group).count();
            rows.push((None, group, count));
            if state.collapsed[group as usize] {
                continue;
            }
            for file in state.files.iter().filter(|f| {
                f.key.group == group && f.key.display_path().to_lowercase().contains(&query)
            }) {
                rows.push((Some(file), group, count));
            }
        }
        let total = rows.len();
        let start = self.review.list_scroll.min(total.saturating_sub(1));
        let visible = ((height / LINE_HEIGHT) as usize + 3).min(total.saturating_sub(start));
        list = list.push(space::vertical().height(start as f32 * LINE_HEIGHT));
        for (file, group, count) in rows.into_iter().skip(start).take(visible) {
            let item = if let Some(file) = file {
                let path = file.old_path.as_ref().map_or_else(
                    || file.key.display_path(),
                    |old| format!("{} → {}", display_path(old), file.key.display_path()),
                );
                let selected = Some(&file.key) == state.selected.as_ref();
                button(
                    text(format!(
                        "{}{} · {path}",
                        if selected { "› " } else { "" },
                        file.status
                    ))
                    .size(12)
                    .wrapping(text::Wrapping::None),
                )
                .on_press(Message::Review(Action::Select(file.key.clone())))
                .style(move |theme, status| review_file_style(theme, status, selected))
            } else {
                button(
                    text(format!(
                        "{} {} ({count})",
                        if state.collapsed[group as usize] {
                            ">"
                        } else {
                            "v"
                        },
                        group.label()
                    ))
                    .size(12),
                )
                .on_press(Message::Review(Action::Group(group)))
            };
            list = list.push(item.width(Fill).height(LINE_HEIGHT));
        }
        list = list.push(
            space::vertical().height((total.saturating_sub(start + visible)) as f32 * LINE_HEIGHT),
        );
        let empty = if state.files.is_empty() {
            if self.review.pending.is_some() {
                "Loading changes…"
            } else if state.error.is_some() {
                "Changes unavailable. Refresh to retry."
            } else if state.truncated {
                "No files retained: list incomplete."
            } else {
                "Checkout is clean."
            }
        } else if state.visible().is_empty() {
            "No files match, or groups are collapsed."
        } else {
            ""
        };
        let mut browser = column![
            text_input("Filter file paths", &state.filter)
                .on_input(|v| Message::Review(Action::Filter(v))),
            text(empty).size(12),
            scrollable(list)
                .id("changes-files")
                .on_scroll(|v| Message::Review(Action::ListScroll(
                    (v.absolute_offset().y / LINE_HEIGHT) as usize
                )))
                .height(Fill)
        ]
        .spacing(6)
        .height(Fill);
        if resizable {
            let width = self.review_file_width();
            browser = browser.push(
                row![
                    button("Narrower").on_press(Message::Review(Action::Width(width - 20.0))),
                    button("Wider").on_press(Message::Review(Action::Width(width + 20.0)))
                ]
                .spacing(4),
            );
        }
        browser.into()
    }

    fn review_file_width(&self) -> f32 {
        if self.review.list_width < 180.0 {
            260.0
        } else {
            self.review.list_width
        }
    }

    #[allow(clippy::too_many_lines)] // Header and virtualized diff belong to one responsive view.
    fn review_detail(&self, width: f32, height: f32) -> Element<'_, Message> {
        let state = &self.review.state;
        let Some(key) = &state.selected else {
            return centered_message("Choose a changed file.");
        };
        let mut header = column![
            row![
                button("Files").on_press(Message::Review(Action::Back)),
                text(key.display_path()).size(13),
                space::horizontal(),
                button("Copy path").on_press(Message::Review(Action::CopyPath))
            ]
            .spacing(8),
            row![
                text(format!(
                    "{} · {}",
                    key.group.label(),
                    state
                        .files
                        .iter()
                        .find(|f| &f.key == key)
                        .map_or_else(String::new, |f| f.status.to_string())
                ))
                .size(12),
                button(if state.split { "Split" } else { "Unified" })
                    .on_press(Message::Review(Action::Split)),
                button(if self.review.selection.is_some() {
                    "Copy selection"
                } else {
                    "Copy diff"
                })
                .on_press(Message::Review(Action::CopyDiff))
            ]
            .spacing(8)
        ]
        .spacing(6);
        let Some(file) = &state.file else {
            return column![header, text("Loading file…")].into();
        };
        header = header.push(
            row![
                text(format!("+{} −{}", file.additions, file.deletions)).size(12),
                button("Previous hunk").on_press(Message::Review(Action::Jump(false, false))),
                button("Next hunk").on_press(Message::Review(Action::Jump(true, false)))
            ]
            .spacing(8),
        );
        header = header.push(
            row![
                text_input("Find in this diff", &state.search)
                    .on_input(|v| Message::Review(Action::Search(v))),
                text(format!(
                    "{} matches{}",
                    state.matches().len(),
                    if file.truncated {
                        " (partial diff)"
                    } else {
                        ""
                    }
                ))
                .size(12),
                button("Previous").on_press(Message::Review(Action::Jump(false, true))),
                button("Next").on_press(Message::Review(Action::Jump(true, true)))
            ]
            .spacing(6),
        );
        if let Some(notice) = &file.notice {
            return column![header, text(notice)].spacing(8).into();
        }
        if file.truncated {
            header = header.push(text("Diff truncated at the review safety limit.").size(12));
        }
        let split = state.split && width >= 1_000.0;
        if split {
            header = header.push(row![
                text("Before").size(12).width(Fill),
                text("After").size(12).width(Fill)
            ]);
        }
        if state.split && !split {
            header = header.push(text("Unified view · widen the diff pane for Split").size(12));
        }
        let rows = if split {
            file.split_rows()
        } else {
            (0..file.lines.len()).map(|i| (Some(i), None)).collect()
        };
        let start = if split {
            rows.iter()
                .position(|(a, b)| *a == Some(state.scroll) || *b == Some(state.scroll))
                .unwrap_or(0)
        } else {
            state.scroll
        }
        .min(rows.len().saturating_sub(1));
        let count = ((height / LINE_HEIGHT) as usize + 3).min(rows.len().saturating_sub(start));
        // Horizontal scroll children receive an unbounded width. Give Fill rows a finite
        // canvas, including a conservative monospace extent for tabs and wide glyphs.
        let columns = self.review.columns;
        let canvas_width = if split {
            (width - 16.0).max(1.0)
        } else {
            (width - 16.0).max(columns as f32 * 9.0 + 144.0)
        };
        let mut content = column![space::vertical().height(start as f32 * LINE_HEIGHT)]
            .spacing(0)
            .width(canvas_width);
        for (left, right) in rows.iter().skip(start).take(count) {
            if split {
                content = content.push(
                    row![
                        self.review_line(*left, true, true),
                        self.review_line(*right, false, true)
                    ]
                    .spacing(1),
                );
            } else {
                content = content.push(self.review_line(*left, false, false));
            }
        }
        content = content.push(
            space::vertical().height(rows.len().saturating_sub(start + count) as f32 * LINE_HEIGHT),
        );
        let mut detail = column![
            header,
            sensor(
                scrollable(content)
                    .width(Fill)
                    .id("changes-diff")
                    .direction(scrollable::Direction::Both {
                        vertical: scrollable::Scrollbar::default(),
                        horizontal: scrollable::Scrollbar::default()
                    })
                    .on_scroll(|v| Message::Review(Action::Scroll(
                        (v.absolute_offset().y / LINE_HEIGHT) as usize
                    )))
                    .height(Fill)
            )
            .on_resize(|size| Message::Review(Action::Layout(size)))
        ]
        .spacing(6)
        .height(Fill)
        .width(Fill);
        if split {
            detail = detail.push(
                row![
                    text("Horizontal").size(12),
                    slider(
                        0.0..=columns.max(1) as f32,
                        state.horizontal.min(columns) as f32,
                        |value| Message::Review(Action::Horizontal(value))
                    )
                ]
                .spacing(8),
            );
        }
        detail.into()
    }

    fn review_line(&self, index: Option<usize>, old: bool, split: bool) -> Element<'_, Message> {
        let Some(index) = index else {
            return container(text(" ")).height(LINE_HEIGHT).width(Fill).into();
        };
        let line = &self.review.state.file.as_ref().expect("loaded file").lines[index];
        let old_number = line.old_line.map_or_else(String::new, |n| n.to_string());
        let new_number = line.new_line.map_or_else(String::new, |n| n.to_string());
        let number = if split {
            if old { old_number } else { new_number }
        } else {
            format!("{old_number:>5} {new_number:>5}")
        };
        let matched = !self.review.state.search.is_empty()
            && line
                .text
                .to_lowercase()
                .contains(&self.review.state.search.to_lowercase());
        let palette = self.theme().palette();
        let light = palette.background.r + palette.background.g + palette.background.b > 1.5;
        let mut spans: Vec<iced::advanced::text::Span<'_, ()>> = Vec::new();
        let prefix = if split {
            usize::from(line.text.starts_with(['+', '-', ' ']))
        } else {
            0
        };
        if prefix > 0 {
            spans.push(span(&line.text[..prefix]));
        }
        let start = if split {
            line.text[prefix..]
                .char_indices()
                .nth(self.review.state.horizontal)
                .map_or(line.text.len(), |(i, _)| prefix + i)
        } else {
            0
        };
        let mut offset = start;
        if let Some(tokens) = self.review.state.syntax.get(index) {
            for token in tokens.iter().filter(|token| token.end > start) {
                let begin = token.start.max(start);
                if begin > offset {
                    spans.push(span(&line.text[offset..begin]));
                }
                let [r, g, b] = if light { token.light } else { token.dark };
                spans.push(span(&line.text[begin..token.end]).color(Color::from_rgb8(r, g, b)));
                offset = token.end;
            }
        }
        if offset < line.text.len() {
            spans.push(span(&line.text[offset..]));
        }
        let content = row![
            text(if matched { "›" } else { " " }).size(12),
            text(number).font(Font::MONOSPACE).size(12),
            rich_text(spans)
                .font(Font::MONOSPACE)
                .size(13)
                .wrapping(text::Wrapping::None)
        ]
        .spacing(8);
        let kind = line.kind;
        let selected = self
            .review
            .selection
            .is_some_and(|(a, b)| (a.min(b)..=a.max(b)).contains(&index));
        mouse_area(
            container(content)
                .id(format!("changes-line-{index}-{old}"))
                .padding([2, 4])
                .height(LINE_HEIGHT)
                .width(Fill)
                .clip(true)
                .style(move |theme| {
                    let mut style = diff_line_style(theme, kind);
                    if selected || matched {
                        style.border = Border {
                            color: theme.palette().primary,
                            width: 1.0,
                            radius: 0.0.into(),
                        };
                    }
                    style
                }),
        )
        .on_press(Message::Review(Action::SelectStart(index)))
        .on_enter(Message::Review(Action::SelectExtend(index)))
        .on_release(Message::Review(Action::SelectEnd))
        .into()
    }
}

fn review_file_style(
    theme: &iced::Theme,
    status: iced::widget::button::Status,
    selected: bool,
) -> button::Style {
    let mut style = button::text(theme, status);
    if selected {
        style.background = Some(
            Color {
                a: 0.12,
                ..theme.palette().primary
            }
            .into(),
        );
        style.border = Border {
            color: theme.palette().primary,
            width: 1.0,
            radius: 0.0.into(),
        };
    }
    style
}
