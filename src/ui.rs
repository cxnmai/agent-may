use std::io;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::auth::UserProfile;
use crate::openai::{ChatClient, ChatTurn};
use crate::storage::{ChatStore, ChatSummary, StoredChat, refresh_chat_metadata};

const PICKER_HELP: &str = "Enter open | n new chat | r refresh | q quit";
const CHAT_HELP: &str =
    "Enter send | Esc back to chats | Up/Down messages | Ctrl+Up/Down input | Ctrl+C quit";

#[derive(Clone)]
struct UiMessage {
    role: String,
    content: String,
}

enum WorkerEvent {
    Response(String),
    Error(String),
}

enum Screen {
    Picker(PickerState),
    Chat(ChatState),
}

struct PickerState {
    chats: Vec<ChatSummary>,
    selected: usize,
    status: String,
}

struct ChatState {
    chat: StoredChat,
    messages: Vec<UiMessage>,
    input: String,
    pasted_segments: Vec<PastedSegment>,
    input_scroll: usize,
    selected: usize,
    status: String,
    pending: bool,
}

struct PastedSegment {
    start: usize,
    end: usize,
    preview: String,
}

struct App {
    model: String,
    profile: UserProfile,
    store: ChatStore,
    screen: Screen,
}

impl App {
    fn new(model: String, profile: UserProfile, store: ChatStore) -> Result<Self> {
        let chats = store.list_chats()?;
        let picker = PickerState {
            chats,
            selected: 0,
            status: "Select a chat or press n to create a new one.".to_string(),
        };

        Ok(Self {
            model,
            profile,
            store,
            screen: Screen::Picker(picker),
        })
    }
}

impl PickerState {
    fn selected_chat(&self) -> Option<&ChatSummary> {
        self.chats.get(self.selected)
    }
}

impl ChatState {
    fn from_chat(chat: StoredChat) -> Self {
        let mut messages = turns_to_messages(&chat.turns);
        if messages.is_empty() {
            messages.push(UiMessage {
                role: "system".to_string(),
                content: "New chat. Type a prompt and press Enter.".to_string(),
            });
        }

        let selected = messages.len().saturating_sub(1);
        Self {
            chat,
            messages,
            input: String::new(),
            pasted_segments: Vec::new(),
            input_scroll: 0,
            selected,
            status: "Idle".to_string(),
            pending: false,
        }
    }

    fn push_message(&mut self, role: impl Into<String>, content: impl Into<String>) {
        self.messages.push(UiMessage {
            role: role.into(),
            content: content.into(),
        });
        self.selected = self.messages.len().saturating_sub(1);
    }
}

pub fn run_chat_ui(
    client: ChatClient,
    model: String,
    profile: UserProfile,
    store: ChatStore,
) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create the terminal")?;

    let result = run_app(&mut terminal, client, model, profile, store);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableBracketedPaste).ok();
    terminal.show_cursor().ok();

    result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: ChatClient,
    model: String,
    profile: UserProfile,
    store: ChatStore,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    let mut app = App::new(model, profile, store)?;

    loop {
        while let Ok(event) = rx.try_recv() {
            apply_worker_event(&mut app, event)?;
        }

        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(Duration::from_millis(100)).context("failed to poll terminal events")? {
            match event::read().context("failed to read a terminal event")? {
                Event::Key(key) => {
                    if handle_key(key, &mut app, &client, &tx)? {
                        break;
                    }
                }
                Event::Paste(text) => {
                    handle_paste(text, &mut app)?;
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }

    Ok(())
}

fn apply_worker_event(app: &mut App, event: WorkerEvent) -> Result<()> {
    let Screen::Chat(chat_state) = &mut app.screen else {
        return Ok(());
    };

    match event {
        WorkerEvent::Response(text) => {
            chat_state.pending = false;
            chat_state.status = "Idle".to_string();
            chat_state.chat.turns.push(ChatTurn {
                role: "assistant".to_string(),
                content: text.clone(),
            });
            refresh_chat_metadata(&mut chat_state.chat);
            app.store.save_chat(&chat_state.chat)?;
            chat_state.push_message("assistant", text);
        }
        WorkerEvent::Error(message) => {
            chat_state.pending = false;
            chat_state.status = "Request failed".to_string();
            chat_state.push_message("error", message);
        }
    }

    Ok(())
}

fn handle_key(
    key: KeyEvent,
    app: &mut App,
    client: &ChatClient,
    tx: &mpsc::Sender<WorkerEvent>,
) -> Result<bool> {
    if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(true);
    }

    match app.screen {
        Screen::Picker(_) => handle_picker_key(key, app),
        Screen::Chat(_) => handle_chat_key(key, app, client, tx),
    }
}

fn handle_picker_key(key: KeyEvent, app: &mut App) -> Result<bool> {
    let Screen::Picker(picker) = &mut app.screen else {
        return Ok(false);
    };

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('r') => {
            picker.chats = app.store.list_chats()?;
            picker.selected = picker
                .selected
                .min(picker.chats.len().saturating_sub(1));
            picker.status = format!("Loaded {} chats.", picker.chats.len());
        }
        KeyCode::Char('n') => {
            let chat = app.store.create_chat(&app.model)?;
            app.screen = Screen::Chat(ChatState::from_chat(chat));
        }
        KeyCode::Up => {
            picker.selected = picker.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            picker.selected = picker
                .selected
                .saturating_add(1)
                .min(picker.chats.len().saturating_sub(1));
        }
        KeyCode::Enter => {
            if let Some(summary) = picker.selected_chat() {
                let chat = app.store.load_chat(&summary.id)?;
                app.screen = Screen::Chat(ChatState::from_chat(chat));
            } else {
                picker.status = "No chats yet. Press n to create one.".to_string();
            }
        }
        _ => {}
    }

    Ok(false)
}

fn handle_chat_key(
    key: KeyEvent,
    app: &mut App,
    client: &ChatClient,
    tx: &mpsc::Sender<WorkerEvent>,
) -> Result<bool> {
    let Screen::Chat(chat) = &mut app.screen else {
        return Ok(false);
    };

    match key.code {
        KeyCode::Esc => {
            if chat.pending {
                chat.status = "Wait for the current response before leaving this chat.".to_string();
            } else {
                let chats = app.store.list_chats()?;
                app.screen = Screen::Picker(PickerState {
                    chats,
                    selected: 0,
                    status: "Select a chat or press n to create a new one.".to_string(),
                });
            }
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
            chat.input_scroll = chat.input_scroll.saturating_sub(1);
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let input_width = current_input_width();
            let max_scroll = max_input_scroll(
                &render_input_preview(&chat.input, &chat.pasted_segments),
                input_width,
            );
            chat.input_scroll = chat.input_scroll.saturating_add(1).min(max_scroll);
        }
        KeyCode::Up => {
            chat.selected = chat.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            chat.selected = chat
                .selected
                .saturating_add(1)
                .min(chat.messages.len().saturating_sub(1));
        }
        KeyCode::Backspace => {
            backspace_input(chat);
            scroll_input_to_bottom(chat);
        }
        KeyCode::Enter => {
            if chat.pending {
                return Ok(false);
            }

            let prompt = chat.input.clone();
            if prompt.trim().is_empty() {
                return Ok(false);
            }

            chat.input.clear();
            chat.pasted_segments.clear();
            chat.input_scroll = 0;
            chat.status = format!("Waiting for {}...", app.model);
            chat.pending = true;
            chat.chat.turns.push(ChatTurn {
                role: "user".to_string(),
                content: prompt.clone(),
            });
            refresh_chat_metadata(&mut chat.chat);
            app.store.save_chat(&chat.chat)?;
            chat.push_message("user", prompt);

            let turns = chat.chat.turns.clone();
            let client = client.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let event = match client.send(&turns) {
                    Ok(text) => WorkerEvent::Response(text),
                    Err(err) => WorkerEvent::Error(format!("{err:#}")),
                };
                let _ = tx.send(event);
            });
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            chat.input.push(ch);
            scroll_input_to_bottom(chat);
        }
        _ => {}
    }

    Ok(false)
}

fn handle_paste(text: String, app: &mut App) -> Result<()> {
    let Screen::Chat(chat) = &mut app.screen else {
        return Ok(());
    };

    if chat.pending || text.is_empty() {
        return Ok(());
    }

    let start = chat.input.len();
    chat.input.push_str(&text);
    let end = chat.input.len();
    chat.pasted_segments.push(PastedSegment {
        start,
        end,
        preview: make_paste_preview(&text),
    });
    scroll_input_to_bottom(chat);
    Ok(())
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    match &app.screen {
        Screen::Picker(picker) => draw_picker(frame, app, picker),
        Screen::Chat(chat) => draw_chat(frame, app, chat),
    }
}

fn draw_picker(frame: &mut Frame<'_>, app: &App, picker: &PickerState) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let header = Paragraph::new(format!(
        "agent-may | model={} | chats={} | root={}",
        app.model,
        picker.chats.len(),
        app.store.root().display()
    ))
    .block(Block::default().borders(Borders::ALL).title("Chats"));
    frame.render_widget(header, areas[0]);

    let items = if picker.chats.is_empty() {
        vec![ListItem::new("No chats yet. Press n to create one.")]
    } else {
        picker
            .chats
            .iter()
            .map(|chat| {
                ListItem::new(vec![
                    Line::from(Span::styled(
                        chat.title.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(format!(
                        "updated {} | created {} | model {}",
                        chat.updated_at.format("%Y-%m-%d %H:%M"),
                        chat.created_at.format("%Y-%m-%d %H:%M"),
                        chat.model
                    )),
                ])
            })
            .collect::<Vec<_>>()
    };

    let mut state = ListState::default();
    if !picker.chats.is_empty() {
        state.select(Some(picker.selected));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Recent First"))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol(">> ");
    frame.render_stateful_widget(list, areas[1], &mut state);

    let footer = Paragraph::new(format!("{} | {}", picker.status, PICKER_HELP))
        .block(Block::default().borders(Borders::ALL).title("Status"))
        .wrap(Wrap { trim: true });
    frame.render_widget(footer, areas[2]);
}

fn draw_chat(frame: &mut Frame<'_>, app: &App, chat: &ChatState) {
    let input_preview = render_input_preview(&chat.input, &chat.pasted_segments);
    let input_inner_width = inner_input_width(frame.area());
    let input_lines = wrap_text_preserving_lines(&input_preview, input_inner_width);
    let input_height = input_box_height(input_lines.len());
    let input_visible_lines = input_height.saturating_sub(2) as usize;
    let input_scroll = chat
        .input_scroll
        .min(input_lines.len().saturating_sub(input_visible_lines.max(1)));

    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(input_height),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let title = format!(
        "{} | model={} | account={} | plan={}",
        chat.chat.title,
        app.model,
        app.profile.email.as_deref().unwrap_or("unknown"),
        app.profile.plan_type.as_deref().unwrap_or("unknown")
    );
    let header = Paragraph::new(title)
        .block(Block::default().borders(Borders::ALL).title("Session"));
    frame.render_widget(header, areas[0]);

    let items = chat
        .messages
        .iter()
        .map(|message| {
            let color = match message.role.as_str() {
                "assistant" => Color::Green,
                "error" => Color::Red,
                "system" => Color::Yellow,
                _ => Color::Cyan,
            };

            ListItem::new(wrap_message_lines(
                &message.role.to_uppercase(),
                &message.content,
                inner_message_width(areas[1]),
                color,
            ))
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    if !chat.messages.is_empty() {
        state.select(Some(chat.selected));
    }
    let messages = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Messages"))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol(">> ");
    frame.render_stateful_widget(messages, areas[1], &mut state);

    let input = Paragraph::new(input_preview)
        .block(Block::default().borders(Borders::ALL).title("Input"))
        .scroll((input_scroll as u16, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, areas[2]);

    let status = Paragraph::new(format!(
        "{} | last updated {} | {}",
        chat.status,
        chat.chat.updated_at.format("%Y-%m-%d %H:%M"),
        CHAT_HELP
    ))
    .block(Block::default().borders(Borders::ALL).title("Status"))
    .wrap(Wrap { trim: true });
    frame.render_widget(status, areas[3]);

    let (cursor_x, cursor_y) = input_cursor_position(areas[2], &input_lines, input_scroll);
    let input_x = areas[2].x + 1 + cursor_x;
    let input_y = areas[2].y + 1 + cursor_y;
    frame.set_cursor_position((input_x, input_y));
}

fn turns_to_messages(turns: &[ChatTurn]) -> Vec<UiMessage> {
    turns
        .iter()
        .map(|turn| UiMessage {
            role: turn.role.clone(),
            content: turn.content.clone(),
        })
        .collect()
}

fn inner_message_width(area: Rect) -> usize {
    area.width.saturating_sub(6).max(8) as usize
}

fn inner_input_width(area: Rect) -> usize {
    area.width.saturating_sub(3).max(8) as usize
}

fn current_input_width() -> usize {
    let (width, _) = crossterm::terminal::size().unwrap_or((80, 24));
    width.saturating_sub(3).max(8) as usize
}

fn wrap_message_lines(role: &str, content: &str, width: usize, color: Color) -> Vec<Line<'static>> {
    let prefix = format!("{role}: ");
    let indent = " ".repeat(prefix.chars().count());
    let normalized = if content.trim().is_empty() {
        vec![String::new()]
    } else {
        content.lines().map(|line| line.to_string()).collect::<Vec<_>>()
    };

    let mut lines = Vec::new();
    for (line_index, raw_line) in normalized.iter().enumerate() {
        let wrapped = wrap_text(raw_line, width.saturating_sub(prefix.chars().count()));
        for (segment_index, segment) in wrapped.iter().enumerate() {
            let label = if line_index == 0 && segment_index == 0 {
                prefix.clone()
            } else {
                indent.clone()
            };

            lines.push(Line::from(vec![
                Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(segment.clone()),
            ]));
        }
    }

    lines
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }

    let mut wrapped = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        if current.is_empty() {
            if word.chars().count() <= width {
                current.push_str(word);
            } else {
                push_long_word(word, width, &mut wrapped);
            }
            continue;
        }

        let candidate_len = current.chars().count() + 1 + word.chars().count();
        if candidate_len <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            wrapped.push(current);
            current = String::new();
            if word.chars().count() <= width {
                current.push_str(word);
            } else {
                push_long_word(word, width, &mut wrapped);
            }
        }
    }

    if !current.is_empty() {
        wrapped.push(current);
    }

    if wrapped.is_empty() {
        wrapped.push(String::new());
    }

    wrapped
}

fn push_long_word(word: &str, width: usize, wrapped: &mut Vec<String>) {
    let mut chunk = String::new();
    for ch in word.chars() {
        chunk.push(ch);
        if chunk.chars().count() >= width {
            wrapped.push(std::mem::take(&mut chunk));
        }
    }
    if !chunk.is_empty() {
        wrapped.push(chunk);
    }
}

fn trim_pasted_segments(segments: &mut Vec<PastedSegment>, input_len: usize) {
    segments.retain(|segment| segment.end <= input_len);
}

fn backspace_input(chat: &mut ChatState) {
    let input_len = chat.input.len();
    if input_len == 0 {
        return;
    }

    if let Some(segment_index) = chat
        .pasted_segments
        .iter()
        .rposition(|segment| segment.end == input_len)
    {
        let segment = chat.pasted_segments.remove(segment_index);
        chat.input.replace_range(segment.start..segment.end, "");
        shift_pasted_segments(&mut chat.pasted_segments, segment_index, segment.end - segment.start);
        return;
    }

    chat.input.pop();
    trim_pasted_segments(&mut chat.pasted_segments, chat.input.len());
}

fn shift_pasted_segments(segments: &mut [PastedSegment], start_index: usize, deleted_bytes: usize) {
    for segment in segments.iter_mut().skip(start_index) {
        segment.start = segment.start.saturating_sub(deleted_bytes);
        segment.end = segment.end.saturating_sub(deleted_bytes);
    }
}

fn make_paste_preview(text: &str) -> String {
    let count = text.chars().count();
    let words = text.split_whitespace().collect::<Vec<_>>();
    match (words.first(), words.last()) {
        (Some(first), Some(last)) if first != last => {
            format!("[{} <pasted text {} chars> {}]", first, count, last)
        }
        (Some(first), _) => format!("[{} <pasted text {} chars>]", first, count),
        _ => format!("[<pasted text {} chars>]", count),
    }
}

fn render_input_preview(input: &str, segments: &[PastedSegment]) -> String {
    if segments.is_empty() {
        return input.to_string();
    }

    let mut preview = String::new();
    let mut cursor = 0;

    for segment in segments {
        if segment.start > input.len() || segment.end > input.len() || segment.start < cursor {
            continue;
        }

        preview.push_str(&input[cursor..segment.start]);
        preview.push_str(&segment.preview);
        cursor = segment.end;
    }

    preview.push_str(&input[cursor..]);
    preview
}

fn wrap_text_preserving_lines(text: &str, width: usize) -> Vec<String> {
    let source_lines = if text.is_empty() {
        vec![String::new()]
    } else {
        text.lines().map(|line| line.to_string()).collect::<Vec<_>>()
    };

    let mut wrapped = Vec::new();
    for line in source_lines {
        wrapped.extend(wrap_text(&line, width));
    }

    if wrapped.is_empty() {
        wrapped.push(String::new());
    }

    wrapped
}

fn input_box_height(line_count: usize) -> u16 {
    let clamped = line_count.clamp(1, 8) as u16;
    clamped + 2
}

fn max_input_scroll(input_preview: &str, input_width: usize) -> usize {
    let input_lines = wrap_text_preserving_lines(input_preview, input_width);
    let visible_lines = input_lines.len().clamp(1, 8);
    input_lines.len().saturating_sub(visible_lines)
}

fn scroll_input_to_bottom(chat: &mut ChatState) {
    let input_width = current_input_width();
    let input_preview = render_input_preview(&chat.input, &chat.pasted_segments);
    chat.input_scroll = max_input_scroll(&input_preview, input_width);
}

fn input_cursor_position(area: Rect, input_lines: &[String], input_scroll: usize) -> (u16, u16) {
    let visible_capacity = area.height.saturating_sub(2) as usize;
    let visible_lines = visible_capacity.max(1);
    let start = input_scroll.min(input_lines.len().saturating_sub(visible_lines));
    let end = (start + visible_lines).min(input_lines.len());
    let cursor_index = input_lines.len().saturating_sub(1);
    let visible_index = cursor_index.clamp(start, end.saturating_sub(1));
    let cursor_line = visible_index.saturating_sub(start) as u16;
    let cursor_col = input_lines
        .get(visible_index)
        .map(|line| line.chars().count() as u16)
        .unwrap_or(0);
    (cursor_col, cursor_line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn test_chat_state(input: &str, pasted_segments: Vec<PastedSegment>) -> ChatState {
        ChatState {
            chat: StoredChat {
                id: "test".to_string(),
                title: "test".to_string(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                model: "gpt-5.4".to_string(),
                turns: Vec::new(),
            },
            messages: Vec::new(),
            input: input.to_string(),
            pasted_segments,
            input_scroll: 0,
            selected: 0,
            status: String::new(),
            pending: false,
        }
    }

    #[test]
    fn paste_preview_keeps_first_and_last_words() {
        let preview = make_paste_preview("this text is pasted into the chat");
        assert_eq!(preview, "[this <pasted text 33 chars> chat]");
    }

    #[test]
    fn input_preview_replaces_pasted_segment_visually_only() {
        let input = "start this text is pasted into the chat end";
        let preview = render_input_preview(
            input,
            &[PastedSegment {
                start: 6,
                end: 39,
                preview: "[this <pasted text 33 chars> chat]".to_string(),
            }],
        );

        assert_eq!(preview, "start [this <pasted text 33 chars> chat] end");
    }

    #[test]
    fn input_scroll_only_appears_after_visible_limit() {
        let preview = "one two three four five six seven eight nine ten eleven twelve";
        assert_eq!(max_input_scroll(preview, 8), 1);
    }

    #[test]
    fn backspace_removes_whole_pasted_segment_at_end() {
        let mut chat = test_chat_state(
            "before pasted block",
            vec![PastedSegment {
                start: 7,
                end: 19,
                preview: "[pasted]".to_string(),
            }],
        );

        backspace_input(&mut chat);

        assert_eq!(chat.input, "before ");
        assert!(chat.pasted_segments.is_empty());
    }

    #[test]
    fn backspace_keeps_pasted_segment_until_suffix_is_deleted() {
        let mut chat = test_chat_state(
            "before pasted block tail",
            vec![PastedSegment {
                start: 7,
                end: 19,
                preview: "[pasted]".to_string(),
            }],
        );

        backspace_input(&mut chat);

        assert_eq!(chat.input, "before pasted block tai");
        assert_eq!(chat.pasted_segments.len(), 1);
        assert_eq!(chat.pasted_segments[0].start, 7);
        assert_eq!(chat.pasted_segments[0].end, 19);
    }
}
