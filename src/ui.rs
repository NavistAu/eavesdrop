//! The interactive view: a scrollback of messages that re-wraps on resize.
//!
//! The plain stream cannot reflow. Once a line is written to a terminal it
//! belongs to the terminal, so narrowing the window re-wraps every paragraph
//! to column 0 and the indent that separated body from header is gone. Holding
//! the messages and redrawing is the only way to keep the layout true to the
//! current width, and redrawing means owning the screen.
//!
//! Mouse capture is deliberately **not** enabled: it would take the wheel but
//! also take click-drag selection, and being able to copy what an agent said
//! matters more than wheel scrolling. Scrolling is on the keyboard.

use crate::feed::{Feed, Message};
use crate::render::{self, message_rows, Groups, Row, Seg};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use std::io::Stdout;
use std::time::Duration;

/// How long a redraw waits for a keystroke before going back to poll the
/// transcripts. Short enough that typing feels immediate, long enough that an
/// idle feed is not a spin loop.
const TICK: Duration = Duration::from_millis(100);

pub struct App {
    pub feed: Feed,
    pub groups: Groups,
    /// Every message accepted so far, oldest first. Held rather than printed,
    /// because reflow means rendering them again at a new width.
    pub messages: Vec<Message>,
    /// First visible line of the laid-out scrollback.
    scroll: usize,
    /// Stick to the newest message as it arrives. Cleared by scrolling up,
    /// restored by scrolling back to the bottom.
    follow: bool,
    /// The status line's left half: what is being watched.
    pub scope: String,
    /// Shown in place of the feed while there is nothing in it.
    pub empty_hint: Vec<String>,
    /// Set when the layout must be recomputed — new message, resize, scroll.
    dirty: bool,
}

impl App {
    pub fn new(feed: Feed, scope: String, empty_hint: Vec<String>) -> Self {
        Self {
            feed,
            groups: Groups::new(),
            messages: Vec::new(),
            scroll: 0,
            follow: true,
            scope,
            empty_hint,
            dirty: true,
        }
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
        self.dirty = true;
    }

    /// Lay every message out at `width`, newest last.
    ///
    /// Recomputed from scratch on each draw rather than cached, so a resize
    /// needs no invalidation logic. `Groups::colour_of` is idempotent for keys
    /// it has already seen, so re-running it does not disturb the colours —
    /// and unlike the plain stream, a component that merges after the fact
    /// repaints its earlier messages to match.
    fn layout(&mut self, width: usize) -> Vec<Row> {
        // An empty feed is a normal outcome, not a failure — most tmux windows
        // hold agents that never message each other. A blank screen cannot be
        // told apart from a broken filter, so say what was searched instead.
        if self.messages.is_empty() {
            return self
                .empty_hint
                .iter()
                .map(|line| {
                    if line.is_empty() {
                        Row::new()
                    } else {
                        vec![render::hint(format!("    {line}"))]
                    }
                })
                .collect();
        }
        let mut rows = Vec::new();
        // Split the borrow: `message_rows` needs `&self.feed` and
        // `&mut self.groups` at once.
        let (feed, groups) = (&self.feed, &mut self.groups);
        for message in &self.messages {
            rows.extend(message_rows(feed, groups, message, width));
        }
        rows
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [body_area, status_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

        let rows = self.layout(body_area.width as usize);
        let viewport = body_area.height as usize;
        self.settle(rows.len(), viewport);

        let visible: Vec<Line> = rows
            .iter()
            .skip(self.scroll)
            .take(viewport)
            .map(to_line)
            .collect();
        frame.render_widget(Paragraph::new(visible), body_area);

        let position = if self.follow {
            "following".to_string()
        } else {
            format!(
                "{} of {} lines",
                self.scroll + viewport.min(rows.len()),
                rows.len()
            )
        };
        let status = Line::from(vec![
            Span::styled(
                format!(
                    " {} · {} messages · {position} ",
                    self.scope,
                    self.messages.len()
                ),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(
                "· ↑↓ PgUp/PgDn g/G scroll · q quit ",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]);
        frame.render_widget(Paragraph::new(status), status_area);
        self.dirty = false;
    }

    /// Apply a key. Returns false when the app should exit.
    fn on_key(&mut self, key: KeyEvent, viewport: usize) -> bool {
        let page = viewport.saturating_sub(2).max(1);
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return false,
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return false,
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => self.scroll_by(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => self.scroll_by(1),
            (KeyCode::PageUp, _) => self.scroll_by(-(page as isize)),
            (KeyCode::PageDown, _) => self.scroll_by(page as isize),
            (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
                self.scroll = 0;
                self.follow = false;
                self.dirty = true;
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                self.follow = true;
                self.dirty = true;
            }
            _ => {}
        }
        true
    }

    /// Pin the scroll position against the real line count, and decide whether
    /// the view is still attached to the tail.
    ///
    /// Split out of `draw` because it is the whole of the follow behaviour and
    /// a `Frame` cannot be constructed in a test. Where the bottom sits depends
    /// on how many lines the messages wrapped to at the current width, which is
    /// known only here — so scrolling down could not reattach on its own, and a
    /// single stray keypress used to strand the view above the newest message
    /// with `G` as the only way back.
    fn settle(&mut self, total: usize, viewport: usize) {
        let max_scroll = total.saturating_sub(viewport);
        if self.follow {
            self.scroll = max_scroll;
        }
        self.scroll = self.scroll.min(max_scroll);
        if self.scroll == max_scroll {
            self.follow = true;
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        // Scrolling up detaches from the tail. Scrolling back down reattaches,
        // but that decision belongs to `draw`, which is the only place that
        // knows where the bottom currently is.
        if delta < 0 {
            self.follow = false;
        }
        self.dirty = true;
    }
}

fn to_line(row: &Row) -> Line<'static> {
    Line::from(row.iter().map(to_span).collect::<Vec<_>>())
}

fn to_span(seg: &Seg) -> Span<'static> {
    let mut style = Style::default();
    if let Some(code) = seg.colour {
        style = style.fg(Color::Indexed(code));
    }
    if seg.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if seg.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    Span::styled(seg.text.clone(), style)
}

/// Put the terminal into raw alternate-screen mode, and guarantee it is
/// restored — including on panic, which would otherwise leave the user with a
/// dead shell showing no echo.
pub struct Screen {
    terminal: Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
}

impl Screen {
    pub fn open() -> std::io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, EnterAlternateScreen)?;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore();
            previous(info);
        }));
        Ok(Self {
            terminal: Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?,
        })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = restore();
    }
}

fn restore() -> std::io::Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)
}

/// Draw, then wait up to `TICK` for input. Returns false when the app should
/// exit, so the caller can go back to polling transcripts in between.
pub fn tick(screen: &mut Screen, app: &mut App) -> std::io::Result<bool> {
    if app.dirty {
        screen.terminal.draw(|frame| app.draw(frame))?;
    }
    if !event::poll(TICK)? {
        return Ok(true);
    }
    let viewport = screen.terminal.size()?.height.saturating_sub(1) as usize;
    match event::read()? {
        Event::Key(key) if key.is_press() => Ok(app.on_key(key, viewport)),
        // A resize invalidates every wrap, which is the entire point of
        // holding the messages instead of printing them.
        Event::Resize(_, _) => {
            app.dirty = true;
            Ok(true)
        }
        _ => Ok(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;

    fn app() -> App {
        App::new(
            Feed::new(Registry::empty()),
            "all sessions".into(),
            vec!["No messages.".into()],
        )
    }

    fn message(body: &str) -> Message {
        Message {
            ts: crate::feed::parse_ts("2026-08-12T14:30:00.000Z").unwrap(),
            from_session: Some("aaaa1111".into()),
            from_hint: None,
            to_session: Some("bbbb2222".into()),
            to_target: Some("bbbb2222".into()),
            summary: None,
            body: body.into(),
            sent_seen: true,
            recv_seen: true,
        }
    }

    #[test]
    fn narrowing_the_width_produces_more_lines_from_the_same_message() {
        let mut a = app();
        a.push(message(
            "a fairly long sentence that has to fold differently at different widths",
        ));
        let wide = a.layout(200).len();
        let narrow = a.layout(50).len();
        assert!(narrow > wide, "reflow should add lines: {narrow} vs {wide}");
    }

    #[test]
    fn relaying_out_does_not_change_the_colours() {
        let mut a = app();
        a.push(message("one"));
        let first: Vec<_> = a.layout(80).iter().map(colours).collect();
        let again: Vec<_> = a.layout(40).iter().map(colours).collect();
        // Widths differ so the row counts may differ, but the header's colour
        // must not move when the layout is recomputed.
        assert_eq!(first[1], again[1]);
    }

    fn colours(row: &Row) -> Vec<Option<u8>> {
        row.iter().map(|s| s.colour).collect()
    }

    #[test]
    fn scrolling_up_detaches_from_the_tail() {
        let mut a = app();
        a.push(message("one"));
        assert!(a.follow);
        a.on_key(KeyEvent::from(KeyCode::Up), 10);
        assert!(!a.follow, "scrolling up must stop auto-scrolling");
        a.on_key(KeyEvent::from(KeyCode::End), 10);
        assert!(a.follow, "End must reattach to the tail");
    }

    #[test]
    fn an_empty_feed_explains_itself_instead_of_going_blank() {
        let mut a = App::new(
            Feed::new(Registry::empty()),
            "this tmux window".into(),
            vec![
                "No messages in the last 24h.".into(),
                String::new(),
                "Watching 4 agents in this tmux window:".into(),
            ],
        );
        let rows = a.layout(80);
        let text: String = rows
            .iter()
            .flat_map(|r| r.iter().map(|s| s.text.clone()))
            .collect();
        assert!(text.contains("No messages in the last 24h."), "{text:?}");
        assert!(text.contains("Watching 4 agents"), "{text:?}");
        // Dimmed, so guidance never reads as something an agent said.
        assert!(rows.iter().flatten().all(|s| s.dim), "{rows:?}");

        // And it gives way the moment there is anything to show.
        a.push(message("real traffic"));
        let rows = a.layout(80);
        let text: String = rows
            .iter()
            .flat_map(|r| r.iter().map(|s| s.text.clone()))
            .collect();
        assert!(text.contains("real traffic"), "{text:?}");
        assert!(!text.contains("No messages"), "{text:?}");
    }

    #[test]
    fn following_pins_the_view_to_the_newest_line() {
        let mut a = app();
        a.settle(100, 10);
        assert_eq!(a.scroll, 90, "a following view sits at the bottom");
        // More arrives: the view moves with it rather than staying put.
        a.settle(130, 10);
        assert_eq!(a.scroll, 120);
    }

    #[test]
    fn scrolling_back_to_the_bottom_resumes_following() {
        let mut a = app();
        a.settle(100, 10);
        a.on_key(KeyEvent::from(KeyCode::Up), 10);
        a.settle(100, 10);
        assert!(!a.follow, "still detached one line up");
        assert_eq!(a.scroll, 89);

        a.on_key(KeyEvent::from(KeyCode::Down), 10);
        a.settle(100, 10);
        assert!(a.follow, "arriving at the bottom must reattach");

        // And it now tracks new arrivals again without another keypress.
        a.settle(140, 10);
        assert_eq!(a.scroll, 130);
    }

    #[test]
    fn a_view_scrolled_up_stays_put_while_messages_arrive() {
        let mut a = app();
        a.settle(100, 10);
        a.on_key(KeyEvent::from(KeyCode::PageUp), 10);
        a.settle(100, 10);
        let parked = a.scroll;
        assert!(!a.follow);
        a.settle(200, 10);
        assert_eq!(a.scroll, parked, "reading must not be yanked to the tail");
    }

    #[test]
    fn a_feed_shorter_than_the_screen_is_always_following() {
        let mut a = app();
        a.on_key(KeyEvent::from(KeyCode::Up), 10);
        a.settle(3, 40);
        assert_eq!(a.scroll, 0);
        assert!(a.follow, "nothing to scroll means nothing to detach from");
    }

    #[test]
    fn home_jumps_to_the_top_and_stops_following() {
        let mut a = app();
        a.push(message("one"));
        a.on_key(KeyEvent::from(KeyCode::Home), 10);
        assert_eq!(a.scroll, 0);
        assert!(!a.follow);
    }

    #[test]
    fn q_and_ctrl_c_both_exit() {
        let mut a = app();
        assert!(!a.on_key(KeyEvent::from(KeyCode::Char('q')), 10));
        assert!(!a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 10));
        assert!(a.on_key(KeyEvent::from(KeyCode::Char('x')), 10));
    }

    #[test]
    fn scrolling_up_from_the_top_stops_rather_than_underflowing() {
        let mut a = app();
        a.push(message("one"));
        for _ in 0..5 {
            a.on_key(KeyEvent::from(KeyCode::PageUp), 10);
        }
        assert_eq!(a.scroll, 0);
    }
}
