use anyhow::{Context, Result, anyhow};
use image::DynamicImage;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::SetTitle;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui_image::picker::Picker;
use ratatui_image::picker::cap_parser::QueryStdioOptions;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{FontSize, StatefulImage};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::browse;
use crate::config::Config;
use crate::cover;
use crate::mount;
use crate::mpv::{self, Mpv, PlaylistEntry, Repeat, Status};
use crate::nts;
use crate::player;
use crate::queue;
use crate::visualizer::Visualizer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    NowPlaying,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Play,
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoverKey {
    Local(PathBuf),
    Url(String),
}

pub fn run(config: &Config) -> Result<()> {
    // Query the terminal before any other I/O (the auto-add picker, the
    // alternate screen, cover loading) so the response is not lost and the
    // first cover is encoded with the detected protocol.
    let picker = detect_picker();
    let mut app = App::new(config.clone(), picker);
    // Opening the panel with nothing queued should go straight to the add
    // picker, so the first thing you see is a library to choose from.
    if let Err(error) = auto_add_if_empty(config) {
        app.show_message(format!("{error}"));
    }
    app.refresh();

    let mut terminal = ratatui::try_init().context("initializing terminal")?;
    let _ = execute!(std::io::stdout(), SetTitle("lilt panel"));
    let _guard = TermGuard;

    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        old_hook(info);
    }));

    app.visualizer_ready = true;
    app.sync_visualizer();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    result
}

/// Detect the terminal's image protocol and cell size. Falls back to
/// halfblocks on terminals without graphics support.
fn detect_picker() -> Picker {
    let query = QueryStdioOptions {
        // Give slow terminals and multiplexers time to answer; a successful
        // query returns immediately, so this only affects fallback cases.
        timeout: Duration::from_millis(1000),
        ..QueryStdioOptions::default()
    };
    Picker::from_query_stdio_with_options(query).unwrap_or_else(|_| Picker::halfblocks())
}

fn auto_add_if_empty(config: &Config) -> Result<()> {
    let Ok(mut client) = Mpv::connect(&config.socket) else {
        return Ok(());
    };
    let Ok(status) = client.status() else {
        return Ok(());
    };
    if status.playlist_count > 0 {
        return Ok(());
    }
    if let Some(selection) = browse::pick(config, false)? {
        match selection {
            browse::Selection::Local(paths) => player::append_paths(config, &paths)?,
            browse::Selection::Nts(stations) => nts::add_many(config, &stations)?,
        }
    }
    Ok(())
}

struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

struct App {
    config: Config,
    mpv: Option<Mpv>,
    status: Status,
    focus: Focus,
    playlist: Vec<PlaylistEntry>,
    items: Vec<String>,
    list_state: ListState,
    mounted: bool,
    last_poll: Instant,
    message: Option<(String, Instant)>,
    pending: Option<Pending>,
    filter_query: String,
    filter_editing: bool,
    filtered: Vec<usize>,
    number_buffer: String,
    seek_editing: bool,
    seek_buffer: String,
    picker: Picker,
    cover_key: Option<CoverKey>,
    cover_pending: Option<(CoverKey, Receiver<Option<DynamicImage>>)>,
    cover_aspect: Option<f32>,
    cover_protocol: Option<StatefulProtocol>,
    visualizer: Option<Visualizer>,
    visualizer_enabled: bool,
    visualizer_ready: bool,
    visualizer_retry_at: Option<Instant>,
    saw_mpv: bool,
    mpv_lost_at: Option<Instant>,
    quit: bool,
}

impl App {
    fn new(config: Config, picker: Picker) -> Self {
        let mpv = Mpv::connect(&config.socket).ok();
        let saw_mpv = mpv.is_some();
        let mut app = Self {
            mounted: mount::is_mounted(&config.music_dir),
            config,
            mpv,
            status: Status::default(),
            focus: Focus::NowPlaying,
            playlist: Vec::new(),
            items: Vec::new(),
            list_state: ListState::default(),
            last_poll: Instant::now() - Duration::from_secs(1),
            message: None,
            pending: None,
            filter_query: String::new(),
            filter_editing: false,
            filtered: Vec::new(),
            number_buffer: String::new(),
            seek_editing: false,
            seek_buffer: String::new(),
            picker,
            cover_key: None,
            cover_pending: None,
            cover_aspect: None,
            cover_protocol: None,
            visualizer: None,
            visualizer_enabled: true,
            visualizer_ready: false,
            visualizer_retry_at: None,
            saw_mpv,
            mpv_lost_at: None,
            quit: false,
        };
        app.refresh();
        app
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            terminal.draw(|frame| self.draw(frame))?;

            let timeout = if self.visualizer_enabled && self.visualizer.is_some() {
                Duration::from_millis(33)
            } else {
                Duration::from_millis(100)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }

            if let Some(action) = self.pending.take() {
                self.run_pending(terminal, action)?;
            }

            if self.last_poll.elapsed() >= Duration::from_millis(100) {
                self.refresh();
            }

            if daemon_loss_expired(self.mpv_lost_at, Instant::now()) {
                self.quit = true;
            }

            if self.quit {
                return Ok(());
            }
        }
    }

    fn refresh(&mut self) {
        self.last_poll = Instant::now();
        self.mounted = mount::is_mounted(&self.config.music_dir);

        if self.mpv.is_none() {
            self.mpv = Mpv::connect(&self.config.socket).ok();
        }

        let previous_pos = self.status.playlist_pos;
        let previous_count = self.status.playlist_count;
        let mut playback_error = None;

        if let Some(client) = self.mpv.as_mut() {
            match client.status() {
                Ok(status) => {
                    self.status = status;
                    nts::apply_status(&self.config, &mut self.status);
                    self.saw_mpv = true;
                    self.mpv_lost_at = None;
                    for event in client.drain_events() {
                        if event.get("event").and_then(|value| value.as_str()) == Some("end-file")
                            && event.get("reason").and_then(|value| value.as_str()) == Some("error")
                        {
                            let detail = event
                                .get("file_error")
                                .and_then(|value| value.as_str())
                                .unwrap_or("playback failed");
                            playback_error = Some(detail.to_string());
                        }
                    }
                }
                Err(_) => {
                    self.mpv = None;
                    self.status = Status::default();
                    if self.saw_mpv && mpv::daemon_pid(&self.config).is_none() {
                        self.mpv_lost_at.get_or_insert_with(Instant::now);
                    }
                }
            }
        } else {
            self.status = Status::default();
            if self.saw_mpv && mpv::daemon_pid(&self.config).is_none() {
                self.mpv_lost_at.get_or_insert_with(Instant::now);
            }
        }

        if let Some(error) = playback_error {
            self.show_message(format!("mpv: {error}"));
        }

        if self.playlist.is_empty()
            || previous_count != self.status.playlist_count
            || previous_pos != self.status.playlist_pos
        {
            self.reload_playlist();
        }

        self.sync_cover();

        if self.visualizer_ready {
            self.sync_visualizer();
        }
    }

    fn sync_cover(&mut self) {
        if let Some((key, receiver)) = self.cover_pending.take() {
            match receiver.try_recv() {
                Ok(image) => {
                    if self.cover_key.as_ref() == Some(&key) {
                        self.cover_aspect = image.as_ref().and_then(|image| {
                            (image.height() > 0)
                                .then(|| image.width() as f32 / image.height() as f32)
                        });
                        self.cover_protocol =
                            image.map(|image| self.picker.new_resize_protocol(image));
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.cover_pending = Some((key, receiver));
                }
                Err(TryRecvError::Disconnected) => {}
            }
        }

        let path = self.status.path.clone();
        let (key, local) = if path.is_empty() {
            (None, None)
        } else if let Some(url) = queue::external_url(&path) {
            (Some(CoverKey::Url(url.to_string())), None)
        } else if queue::is_url(&path) {
            (Some(CoverKey::Url(path)), None)
        } else {
            let track = PathBuf::from(path);
            let dir = track.parent().map(Path::to_path_buf);
            (
                dir.clone().map(CoverKey::Local),
                dir.map(|dir| (dir, track)),
            )
        };

        if self.cover_key == key {
            return;
        }

        self.cover_key = key.clone();
        self.cover_pending = None;
        self.cover_aspect = None;
        self.cover_protocol = None;

        let Some(key) = key else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        let config = self.config.clone();
        match key.clone() {
            CoverKey::Url(url) => {
                std::thread::spawn(move || {
                    let image = nts::cover(&config, &url)
                        .or_else(|| cover::load_url(&config.cache_dir, &url));
                    let _ = sender.send(image);
                });
            }
            CoverKey::Local(_) => {
                let Some((dir, track)) = local else {
                    return;
                };
                if !track.is_file() {
                    return;
                }
                std::thread::spawn(move || {
                    let _ = sender.send(cover::load(&dir, &track));
                });
            }
        }
        self.cover_pending = Some((key, receiver));
    }

    fn cover_slot(&self) -> bool {
        self.cover_key.is_some() && !self.status.path.is_empty()
    }

    fn external_only_queue(&self) -> bool {
        !self.playlist.is_empty()
            && self
                .playlist
                .iter()
                .all(|entry| queue::is_url(&entry.filename))
    }

    fn cover_loading(&self) -> bool {
        self.cover_pending.is_some()
    }

    fn reload_playlist(&mut self) {
        let entries = match self.client() {
            Some(client) => client.playlist().ok(),
            None => None,
        };
        if let Some(entries) = entries {
            self.playlist = entries;
            self.rebuild_items();
            self.apply_filter();
            if self.focus == Focus::NowPlaying {
                self.select_current();
            }
        }
    }

    fn apply_filter(&mut self) {
        let previous_actual = self
            .list_state
            .selected()
            .and_then(|index| self.filtered.get(index).copied());
        let query = self.filter_query.to_lowercase();

        self.filtered = if query.is_empty() {
            (0..self.items.len()).collect()
        } else {
            self.items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.to_lowercase().contains(&query))
                .map(|(index, _)| index)
                .collect()
        };

        if self.filtered.is_empty() {
            self.list_state.select(None);
            return;
        }

        let target = previous_actual
            .and_then(|actual| self.filtered.iter().position(|&index| index == actual))
            .or_else(|| {
                if self.status.playlist_pos >= 0 {
                    let position = self.status.playlist_pos as usize;
                    self.filtered.iter().position(|&index| index == position)
                } else {
                    None
                }
            })
            .unwrap_or(0);
        self.list_state.select(Some(target));
    }

    fn select_current(&mut self) {
        if self.filtered.is_empty() {
            self.list_state.select(None);
            return;
        }
        let position = if self.status.playlist_pos >= 0 {
            let current = self.status.playlist_pos as usize;
            self.filtered.iter().position(|&index| index == current)
        } else {
            None
        };
        self.list_state.select(Some(position.unwrap_or(0)));
    }

    fn rebuild_items(&mut self) {
        let nts = nts::cached_stations(&self.config);
        self.items = self
            .playlist
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let current = index as i64 == self.status.playlist_pos;
                let marker = if current { "●" } else { " " };
                let name = if let Some(station) = nts::find_station(&nts, &entry.filename) {
                    station.title.clone()
                } else {
                    entry
                        .title
                        .clone()
                        .unwrap_or_else(|| player::display_name(&entry.filename))
                };
                format!("{marker} {:>4}  {name}", index + 1)
            })
            .collect();
    }

    fn client(&mut self) -> Option<&mut Mpv> {
        if self.mpv.is_none() {
            self.mpv = Mpv::connect(&self.config.socket).ok();
        }
        self.mpv.as_mut()
    }

    fn act<F>(&mut self, action: F) -> bool
    where
        F: FnOnce(&mut Mpv) -> Result<()>,
    {
        let result = match self.client() {
            Some(client) => action(client),
            None => Err(anyhow!("mpv is not running")),
        };
        match result {
            Ok(()) => true,
            Err(err) => {
                self.mpv = None;
                self.show_message(format!("{err}"));
                false
            }
        }
    }

    fn show_message(&mut self, message: String) {
        self.message = Some((message, Instant::now()));
    }

    fn handle_key(&mut self, mut key: KeyEvent) {
        // Terminals report uppercase letters as `Char('G') + SHIFT`; fold that
        // into a plain uppercase code so shortcuts such as `N` `G` and the
        // filter/number prompts do not depend on modifier bookkeeping.
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            if let KeyCode::Char(character) = key.code
                && character.is_ascii_lowercase()
            {
                key.code = KeyCode::Char(character.to_ascii_uppercase());
            }
            key.modifiers.remove(KeyModifiers::SHIFT);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.seek_editing {
                self.cancel_seek();
            } else if self.filter_editing {
                self.clear_filter();
            } else {
                self.quit = true;
            }
            return;
        }

        if self.seek_editing {
            self.handle_seek_key(key);
            return;
        }

        if self.filter_editing {
            self.handle_filter_key(key);
            return;
        }

        if key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char(character) if character.is_ascii_digit() => {
                    if self.number_buffer.len() < 9 {
                        self.number_buffer.push(character);
                    }
                    return;
                }
                KeyCode::Backspace if !self.number_buffer.is_empty() => {
                    self.number_buffer.pop();
                    return;
                }
                KeyCode::Esc if !self.number_buffer.is_empty() => {
                    self.number_buffer.clear();
                    return;
                }
                KeyCode::Enter if !self.number_buffer.is_empty() => {
                    self.jump_to_number();
                    return;
                }
                KeyCode::Char('G') if !self.number_buffer.is_empty() => {
                    self.jump_to_number();
                    return;
                }
                _ => {}
            }
            if !self.number_buffer.is_empty() {
                self.number_buffer.clear();
            }
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') if !ctrl => {
                self.quit = true;
                return;
            }
            KeyCode::Char('Q') if !ctrl => {
                self.quit_mpv();
                self.quit = true;
                return;
            }
            KeyCode::Tab => {
                self.toggle_focus();
                return;
            }
            KeyCode::Char('/') => {
                self.open_filter();
                return;
            }
            KeyCode::Char('o') | KeyCode::Char('b') if !ctrl => {
                self.pending = Some(Pending::Play);
                return;
            }
            KeyCode::Char('a') if !ctrl => {
                self.pending = Some(Pending::Add);
                return;
            }
            KeyCode::Char('v') if !ctrl => {
                self.toggle_visualizer();
                return;
            }
            KeyCode::Char(' ') => {
                self.toggle_pause();
                return;
            }
            KeyCode::Char('n') if !ctrl => {
                self.next_track();
                return;
            }
            KeyCode::Char('p') if !ctrl => {
                self.prev_track();
                return;
            }
            KeyCode::Char('t') if !ctrl => {
                self.open_seek();
                return;
            }
            KeyCode::Char('h') if !ctrl => {
                self.seek_relative(-10.0);
                return;
            }
            KeyCode::Char('l') if !ctrl => {
                self.seek_relative(10.0);
                return;
            }
            KeyCode::Char('H') if !ctrl => {
                self.seek_relative(-60.0);
                return;
            }
            KeyCode::Char('L') if !ctrl => {
                self.seek_relative(60.0);
                return;
            }
            KeyCode::Char('+') | KeyCode::Char('=') if !ctrl => {
                self.adjust_volume(5.0);
                return;
            }
            KeyCode::Char('-') | KeyCode::Char('_') if !ctrl => {
                self.adjust_volume(-5.0);
                return;
            }
            KeyCode::Char('s') if !ctrl => {
                self.cycle_shuffle();
                return;
            }
            KeyCode::Char('r') if !ctrl => {
                self.cycle_repeat();
                return;
            }
            KeyCode::Char('m') if !ctrl => {
                self.cycle_mute();
                return;
            }
            KeyCode::Char('x') if !ctrl => {
                self.stop_playback();
                return;
            }
            KeyCode::Char('f') if !ctrl => {
                if self.status.path.is_empty() {
                    self.show_message("nothing playing".to_string());
                } else if let Err(error) = player::open_path(&self.status.path) {
                    self.show_message(format!("{error}"));
                }
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::NowPlaying => self.handle_now_playing(key),
            Focus::Queue => self.handle_queue(key),
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.clear_filter(),
            KeyCode::Enter => self.filter_editing = false,
            KeyCode::Backspace => {
                self.filter_query.pop();
                self.apply_filter();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter_query.clear();
                self.apply_filter();
            }
            KeyCode::Tab => {
                self.filter_editing = false;
                self.toggle_focus();
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.filter_query.push(character);
                self.apply_filter();
            }
            _ => {}
        }
    }

    fn clear_filter(&mut self) {
        self.filter_query.clear();
        self.filter_editing = false;
        self.apply_filter();
    }

    fn open_filter(&mut self) {
        if self.playlist.is_empty() {
            self.reload_playlist();
        }
        self.focus = Focus::Queue;
        self.filter_query.clear();
        self.filter_editing = true;
        self.apply_filter();
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::NowPlaying => Focus::Queue,
            Focus::Queue => Focus::NowPlaying,
        };
        self.filter_editing = false;
    }

    fn quit_mpv(&mut self) {
        let _ = mpv::quit(&self.config, true);
        self.mpv = None;
        self.status = Status::default();
    }

    fn play_index(&mut self, index: i64) {
        self.act(move |client| {
            client.command_values(vec![json!("playlist-play-index"), json!(index)])?;
            Ok(())
        });
    }

    fn jump_to_number(&mut self) {
        let value = std::mem::take(&mut self.number_buffer);
        let Ok(number) = value.parse::<usize>() else {
            return;
        };
        if number == 0 || number > self.items.len() {
            self.show_message(format!("no track {number}"));
            return;
        }

        self.filter_query.clear();
        self.filter_editing = false;
        self.apply_filter();
        self.list_state.select(Some(number - 1));
        self.play_index((number - 1) as i64);
    }

    fn handle_now_playing(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') => self.next_track(),
            KeyCode::Char('k') => self.prev_track(),
            KeyCode::Left => self.seek_relative(-10.0),
            KeyCode::Right => self.seek_relative(10.0),
            KeyCode::Up => self.adjust_volume(5.0),
            KeyCode::Down => self.adjust_volume(-5.0),
            _ => {}
        }
    }

    fn toggle_pause(&mut self) {
        self.act(player::toggle_client);
    }

    fn next_track(&mut self) {
        self.act(|client| {
            if client.command(&["playlist-next"]).is_err() {
                let _ = client.command_values(vec![json!("playlist-play-index"), json!(0)]);
            }
            Ok(())
        });
    }

    fn prev_track(&mut self) {
        self.act(|client| {
            if client.command(&["playlist-prev"]).is_err() {
                let count = client
                    .get("playlist-count")
                    .ok()
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0);
                if count > 0 {
                    let _ =
                        client.command_values(vec![json!("playlist-play-index"), json!(count - 1)]);
                }
            }
            Ok(())
        });
    }

    fn open_seek(&mut self) {
        self.message = None;
        self.number_buffer.clear();
        self.seek_buffer.clear();
        self.seek_editing = true;
    }

    fn cancel_seek(&mut self) {
        self.seek_editing = false;
        self.seek_buffer.clear();
    }

    fn handle_seek_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.cancel_seek(),
            KeyCode::Enter => self.apply_seek(),
            KeyCode::Backspace => {
                self.seek_buffer.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.seek_buffer.clear();
            }
            KeyCode::Char(character)
                if (character.is_ascii_digit() || matches!(character, '+' | '-' | '.' | '%'))
                    && self.seek_buffer.len() < 16 =>
            {
                self.seek_buffer.push(character);
            }
            _ => {}
        }
    }

    fn apply_seek(&mut self) {
        let value = std::mem::take(&mut self.seek_buffer);
        self.seek_editing = false;
        let value = value.trim().to_string();
        if value.is_empty() {
            return;
        }
        if self.act(|client| player::seek_client(client, &value)) {
            self.show_message(format!("seek {value}"));
        }
    }

    fn seek_relative(&mut self, seconds: f64) {
        self.act(move |client| {
            client.command_values(vec![json!("seek"), json!(seconds), json!("relative")])?;
            Ok(())
        });
    }

    fn adjust_volume(&mut self, delta: f64) {
        self.act(move |client| {
            client.command_values(vec![json!("add"), json!("volume"), json!(delta)])?;
            Ok(())
        });
    }

    fn cycle_shuffle(&mut self) {
        self.act(player::toggle_shuffle_client);
        self.reload_playlist();
    }

    fn cycle_repeat(&mut self) {
        self.act(|client| {
            let current = client.status()?.repeat;
            let next = match current {
                Repeat::Off => Repeat::All,
                Repeat::All => Repeat::One,
                Repeat::One => Repeat::Off,
            };
            player::set_repeat(client, next)
        });
    }

    fn cycle_mute(&mut self) {
        self.act(|client| client.command(&["cycle", "mute"]));
    }

    fn stop_playback(&mut self) {
        self.act(|client| client.command(&["stop"]));
    }

    fn sync_visualizer(&mut self) {
        if !self.visualizer_enabled {
            self.visualizer = None;
            return;
        }

        // Both backends need a playing mpv stream: Linux records the mpv sink
        // input, while macOS taps the mpv process directly.
        let should_run = !self.status.idle && !self.status.path.is_empty();

        if !should_run {
            self.visualizer = None;
            self.visualizer_retry_at = None;
            return;
        }

        if self
            .visualizer
            .as_ref()
            .is_some_and(|visualizer| !visualizer.available())
        {
            self.visualizer = None;
        }

        if self.visualizer.is_none() {
            let now = Instant::now();
            if self.visualizer_retry_at.is_none_or(|retry| now >= retry) {
                self.visualizer = Some(Visualizer::start(&self.config));
                self.visualizer_retry_at = Some(now + Duration::from_secs(3));
            }
        }
    }

    fn toggle_visualizer(&mut self) {
        self.visualizer_enabled = !self.visualizer_enabled;
        self.visualizer = None;
        self.visualizer_retry_at = None;
        self.sync_visualizer();
    }

    fn handle_queue(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            if self.filter_query.is_empty() {
                self.focus = Focus::NowPlaying;
            } else {
                self.clear_filter();
            }
            return;
        }

        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        // The list widget clamps an out-of-range selection while drawing, but
        // the queue pane is not drawn on very small terminals.
        let selected = self.list_state.selected().unwrap_or(0).min(len - 1);
        let half_page = 10usize;
        let page = 20usize;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.list_state.select(Some(selected.saturating_sub(1)));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.list_state.select(Some((selected + 1).min(len - 1)));
            }
            KeyCode::PageUp => {
                self.list_state.select(Some(selected.saturating_sub(page)));
            }
            KeyCode::PageDown => {
                self.list_state.select(Some((selected + page).min(len - 1)));
            }
            KeyCode::Char('u') if ctrl => {
                self.list_state
                    .select(Some(selected.saturating_sub(half_page)));
            }
            KeyCode::Char('d') if ctrl => {
                self.list_state
                    .select(Some((selected + half_page).min(len - 1)));
            }
            KeyCode::Char('b') if ctrl => {
                self.list_state.select(Some(selected.saturating_sub(page)));
            }
            KeyCode::Char('f') if ctrl => {
                self.list_state.select(Some((selected + page).min(len - 1)));
            }
            KeyCode::Enter => {
                let actual = self.filtered[selected] as i64;
                self.play_index(actual);
            }
            _ => {}
        }
    }

    /// Suspend the TUI, run the fzf picker, then rebuild the terminal.
    fn run_pending(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        action: Pending,
    ) -> Result<()> {
        ratatui::restore();

        let result = (|| -> Result<Option<&'static str>> {
            match action {
                Pending::Play => match browse::pick(&self.config, true)? {
                    Some(browse::Selection::Local(paths)) => {
                        browse::play_paths(&self.config, &paths)?;
                        Ok(Some("playing new selection"))
                    }
                    Some(browse::Selection::Nts(stations)) => {
                        nts::play_many(&self.config, &stations)?;
                        Ok(Some("playing NTS"))
                    }
                    None => Ok(None),
                },
                Pending::Add => match browse::pick(&self.config, false)? {
                    Some(browse::Selection::Local(paths)) => {
                        player::append_paths(&self.config, &paths)?;
                        Ok(Some("added to queue"))
                    }
                    Some(browse::Selection::Nts(stations)) => {
                        nts::add_many(&self.config, &stations)?;
                        Ok(Some("added NTS"))
                    }
                    None => Ok(None),
                },
            }
        })();

        *terminal = ratatui::try_init().context("reinitializing terminal")?;
        terminal.clear()?;

        match result {
            Ok(Some(message)) => {
                self.show_message(message.to_string());
                self.refresh();
            }
            Ok(None) => {}
            Err(error) => self.show_message(format!("{error}")),
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 40 || area.height < 12 {
            let paragraph = Paragraph::new("terminal too small (need 40x12)")
                .alignment(Alignment::Center)
                .block(Block::bordered());
            frame.render_widget(paragraph, area);
            return;
        }

        let show_visualizer = self.visualizer_enabled && area.height >= 20;
        let show_cover = self.cover_slot() && area.width >= 64 && area.height >= 20;
        let now_height = if show_cover {
            if area.height >= 40 {
                16
            } else if area.height >= 30 {
                14
            } else if area.height >= 24 {
                10
            } else {
                8
            }
        } else {
            6
        };
        let mut constraints = vec![Constraint::Length(now_height)];
        if show_visualizer {
            constraints.push(Constraint::Length(if area.height >= 30 { 8 } else { 5 }));
        }
        constraints.push(Constraint::Min(3));
        constraints.push(Constraint::Length(2));
        constraints.push(Constraint::Length(1));
        let chunks = Layout::vertical(constraints).split(area);

        let now_index = 0;
        let viz_index = if show_visualizer { Some(1) } else { None };
        let queue_index = if show_visualizer { 2 } else { 1 };
        let help_index = queue_index + 1;
        let message_index = help_index + 1;

        self.draw_now_playing(frame, chunks[now_index]);
        if let Some(index) = viz_index {
            self.draw_spectrum_pane(frame, chunks[index]);
        }
        self.draw_queue(frame, chunks[queue_index]);
        self.draw_help(frame, chunks[help_index]);
        self.draw_message(frame, chunks[message_index]);
    }

    fn draw_now_playing(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::NowPlaying;
        let block = Block::bordered()
            .title(" now playing ")
            .border_style(border_style(focused))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let art_width = if self.cover_slot() {
            let font = self.picker.font_size();
            let aspect = self.cover_aspect.unwrap_or(1.0);
            let cell_aspect = aspect * (font.height.max(1) as f32 / font.width.max(1) as f32);
            let desired = (inner.height as f32 * cell_aspect).round() as u16;
            desired.min(30).min(inner.width.saturating_sub(31)).max(12)
        } else {
            0
        };
        let text_area = if art_width >= 12 {
            let chunks = Layout::horizontal([
                Constraint::Length(art_width),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(inner);
            let image_area = cover_area(chunks[0], self.cover_aspect, self.picker.font_size());
            if let Some(protocol) = self.cover_protocol.as_mut() {
                frame.render_stateful_widget(StatefulImage::new(), image_area, protocol);
            } else {
                self.draw_cover_placeholder(frame, chunks[0]);
            }
            chunks[2]
        } else {
            inner
        };

        let text_block = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(4),
            Constraint::Fill(1),
        ])
        .split(text_area)[1];
        let lines = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(text_block);

        let external = queue::is_url(&self.status.path);
        let path_fallback = !external && !self.status.has_title_tag && !self.status.path.is_empty();
        let context = if self.status.artist.is_empty() && self.status.album.is_empty() {
            if path_fallback {
                "file".to_string()
            } else if external {
                "stream".to_string()
            } else {
                "mpv".to_string()
            }
        } else if self.status.album.is_empty() {
            self.status.artist.clone()
        } else if self.status.artist.is_empty() {
            self.status.album.clone()
        } else {
            format!("{} — {}", self.status.artist, self.status.album)
        };

        let title_text = if path_fallback {
            self.status.path.as_str()
        } else if self.status.media_title.is_empty() {
            "Nothing playing"
        } else {
            self.status.media_title.as_str()
        };
        let title = if self.status.paused {
            format!("|| {title_text}")
        } else {
            title_text.to_string()
        };

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                context,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))),
            lines[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            ))),
            lines[1],
        );

        frame.render_widget(
            Paragraph::new(progress_line(
                lines[2].width,
                self.status.time_pos,
                self.status.duration,
                self.status.idle,
            )),
            lines[2],
        );

        let queue = if self.status.playlist_count > 0 && self.status.playlist_pos >= 0 {
            format!(
                "  ({}/{})",
                self.status.playlist_pos + 1,
                self.status.playlist_count
            )
        } else {
            String::new()
        };
        let buffering = if self.status.paused_for_cache {
            "  buffering…"
        } else {
            ""
        };
        // Keep the mute state at the start of the line. The now-playing pane
        // narrows for wide covers, and a trailing "muted" can be clipped.
        let volume = if self.status.mute {
            "muted".to_string()
        } else {
            format!("vol {:.0}%", self.status.volume.floor())
        };
        let state = format!(
            "{}   shuffle {}   repeat {}{}{}",
            volume,
            if self.status.shuffle { "on" } else { "off" },
            player::repeat_label(self.status.repeat),
            queue,
            buffering
        );
        frame.render_widget(
            Paragraph::new(state).style(Style::default().fg(Color::DarkGray)),
            lines[3],
        );
    }

    fn draw_cover_placeholder(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered().border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 2 {
            return;
        }

        let label = if self.cover_loading() {
            "loading…"
        } else {
            "no cover"
        };
        let centered = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(2),
            Constraint::Fill(1),
        ])
        .split(inner)[1];
        let lines = vec![
            Line::from(Span::styled(
                "♪",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(label, Style::default().fg(Color::DarkGray))),
        ];
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), centered);
    }

    fn draw_spectrum_pane(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(" spectrum ")
            .border_style(Style::default().fg(Color::DarkGray))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let (mut bands, available) = match &self.visualizer {
            Some(visualizer) => (visualizer.bands(), visualizer.available()),
            None => (Vec::new(), false),
        };
        // The audio tap has noticeable buffering latency. Reflect mute
        // immediately instead of waiting for the captured audio to drain.
        if self.status.mute {
            bands.fill(0.0);
        }
        render_spectrum(frame, inner, &bands, available);
    }

    fn draw_queue(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Queue;
        let title = if self.filter_query.is_empty() {
            format!(" queue — {} track(s) ", self.items.len())
        } else {
            format!(
                " queue — {}/{} track(s) ",
                self.filtered.len(),
                self.items.len()
            )
        };
        let block = Block::bordered()
            .title(title)
            .border_style(border_style(focused))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let filter_visible =
            self.filter_editing || !self.filter_query.is_empty() || !self.number_buffer.is_empty();
        let chunks = Layout::vertical([
            Constraint::Length(if filter_visible { 1 } else { 0 }),
            Constraint::Min(0),
        ])
        .split(inner);

        if filter_visible {
            let line = if !self.number_buffer.is_empty() {
                Paragraph::new(format!("goto: {}█", self.number_buffer))
                    .style(Style::default().fg(Color::Yellow))
            } else if self.filter_editing {
                Paragraph::new(format!("/{}█", self.filter_query))
                    .style(Style::default().fg(Color::Yellow))
            } else {
                Paragraph::new(format!(
                    "filter: {}   ({} match(es))   / edit   Esc clear",
                    self.filter_query,
                    self.filtered.len()
                ))
                .style(Style::default().fg(Color::DarkGray))
            };
            frame.render_widget(line, chunks[0]);
        }

        if self.filtered.is_empty() {
            let message = if self.items.is_empty() {
                "queue empty — press `a` to add"
            } else {
                "no matches"
            };
            frame.render_widget(
                Paragraph::new(message)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
        } else {
            let items: Vec<ListItem> = self
                .filtered
                .iter()
                .filter_map(|&index| self.items.get(index))
                .map(|item| ListItem::new(item.as_str()))
                .collect();
            let list = List::new(items)
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▸ ");
            frame.render_stateful_widget(list, chunks[1], &mut self.list_state);
        }
    }

    fn draw_help(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        frame.render_widget(
            Paragraph::new(help_lines(self.focus, area.width))
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn draw_message(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        if self.seek_editing {
            frame.render_widget(
                Paragraph::new(format!(
                    "seek: {}█   Enter apply · Esc cancel",
                    self.seek_buffer
                ))
                .style(Style::default().fg(Color::Yellow)),
                area,
            );
            return;
        }

        if let Some((message, at)) = &self.message {
            if at.elapsed() < Duration::from_secs(3) {
                frame.render_widget(
                    Paragraph::new(message.as_str()).style(Style::default().fg(Color::Yellow)),
                    area,
                );
                return;
            }
            self.message = None;
        }

        if !self.mounted && !self.external_only_queue() {
            frame.render_widget(
                Paragraph::new("library not mounted — run `lilt mount`")
                    .style(Style::default().fg(Color::Yellow)),
                area,
            );
            return;
        }

        if self.status.idle && self.status.media_title.is_empty() {
            let message = if self.status.playlist_count > 0 {
                "queue ended — press space to restart"
            } else {
                "queue empty — press `a` to add"
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(Color::DarkGray)),
                area,
            );
            return;
        }

        frame.render_widget(Paragraph::new(""), area);
    }
}

fn cover_area(area: Rect, aspect: Option<f32>, font: FontSize) -> Rect {
    let aspect = aspect
        .filter(|aspect| aspect.is_finite() && *aspect > 0.0)
        .unwrap_or(1.0);
    let font_width = font.width.max(1) as f32;
    let font_height = font.height.max(1) as f32;
    let cell_aspect = aspect * font_height / font_width;
    if !cell_aspect.is_finite() || cell_aspect <= 0.0 || area.is_empty() {
        return area;
    }

    let available_width = area.width as f32;
    let available_height = area.height as f32;
    let (width, height) = if available_width / available_height > cell_aspect {
        let height = available_height;
        let width = (height * cell_aspect).round().max(1.0);
        (width, height)
    } else {
        let width = available_width;
        let height = (width / cell_aspect).round().max(1.0);
        (width, height)
    };

    let width = (width as u16).clamp(1, area.width);
    let height = (height as u16).clamp(1, area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn help_lines(focus: Focus, width: u16) -> Vec<Line<'static>> {
    if width >= 70 {
        match focus {
            Focus::NowPlaying => vec![
                Line::from("space pause  j/k next/prev  h/l 10s  H/L 60s  t seek  -/+ vol"),
                Line::from("s/r/m  x  o/b play  a add  v viz  Tab queue  / filter  f open  q/Q"),
            ],
            Focus::Queue => vec![
                Line::from("j/k move  Ctrl-d/u half  Ctrl-f/b page  t seek  Enter play"),
                Line::from(
                    "N+Enter/G jump  space pause  x stop  / filter  o/a  v viz  f open  Tab",
                ),
            ],
        }
    } else {
        match focus {
            Focus::NowPlaying => vec![
                Line::from("space j/k h/l H/L -/+ t seek"),
                Line::from("s r m x f o/b a v Tab / q/Q"),
            ],
            Focus::Queue => vec![
                Line::from("j/k ^d/^u ^f/^b Enter t seek"),
                Line::from("N+↵/G jump  space x / o a v Tab q/Q"),
            ],
        }
    }
}

fn progress_line(
    width: u16,
    position: Option<f64>,
    duration: Option<f64>,
    idle: bool,
) -> Line<'static> {
    if idle {
        return Line::from(Span::styled("idle", Style::default().fg(Color::DarkGray)));
    }
    if width == 0 {
        return Line::default();
    }

    let elapsed = player::format_time(position);
    let duration_text = player::format_time(duration);
    let ratio = match (position, duration) {
        (Some(position), Some(duration))
            if position.is_finite() && duration.is_finite() && duration > 0.0 =>
        {
            (position / duration).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };

    let bar_width = usize::from(width).saturating_sub(elapsed.len() + duration_text.len() + 2);
    if bar_width < 3 {
        return Line::from(Span::styled(
            format!("{elapsed} / {duration_text}"),
            Style::default().fg(Color::Gray),
        ));
    }

    let mut filled = (ratio * bar_width as f64).round() as usize;
    if ratio > 0.0 && filled == 0 {
        filled = 1;
    }
    let filled = filled.min(bar_width);
    let empty = bar_width - filled;

    let mut spans = Vec::with_capacity(5);
    spans.push(Span::styled(
        elapsed,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(" "));
    if filled > 0 {
        spans.push(Span::styled(
            "━".repeat(filled),
            Style::default().fg(Color::Cyan),
        ));
    }
    if empty > 0 {
        spans.push(Span::styled(
            "─".repeat(empty),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        duration_text,
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

fn daemon_loss_expired(lost_at: Option<Instant>, now: Instant) -> bool {
    lost_at.is_some_and(|lost| now.saturating_duration_since(lost) >= Duration::from_secs(1))
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_spectrum(frame: &mut Frame, area: Rect, bands: &[f32], available: bool) {
    if area.is_empty() {
        return;
    }
    if !available || bands.is_empty() {
        let message = if cfg!(target_os = "macos") {
            "visualizer unavailable (audia-tap)"
        } else {
            "visualizer unavailable"
        };
        frame.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let width = area.width as usize;
    if width == 0 {
        return;
    }
    let (count, bar_width) = spectrum_columns(width, bands.len());

    let bars: Vec<Bar> = (0..count)
        .map(|index| {
            let start = index * bands.len() / count;
            let end = ((index + 1) * bands.len() / count)
                .max(start + 1)
                .min(bands.len());
            let peak = bands[start..end]
                .iter()
                .copied()
                .filter(|value| value.is_finite())
                .fold(0.0f32, f32::max);
            let value = (peak.clamp(0.0, 1.0) * 100.0).round() as u64;
            Bar::new(value)
                .style(band_style(index, count))
                .text_value("")
        })
        .collect();

    let chart = BarChart::default()
        .data(BarGroup::default().bars(&bars))
        .bar_width(bar_width)
        .bar_gap(1)
        .max(100);
    frame.render_widget(chart, area);
}

/// Pick the bar count and bar width that fill the available width as closely
/// as possible, preferring more, thinner bars.
fn spectrum_columns(width: usize, max_bars: usize) -> (usize, u16) {
    let max_count = max_bars.clamp(1, 64);
    if width == 0 {
        return (1, 1);
    }

    let gap = 1usize;
    let mut best_total = 0usize;
    let mut best = (1usize, 1u16);

    for bar_width in 1..=32u16 {
        let bar = bar_width as usize;
        let count = ((width + gap) / (bar + gap)).min(max_count).max(1);
        let total = count * bar + (count.saturating_sub(1)) * gap;
        if total <= width && total > best_total {
            best_total = total;
            best = (count, bar_width);
        }
    }

    best
}

fn band_style(index: usize, count: usize) -> Style {
    let t = if count <= 1 {
        0.0
    } else {
        index as f32 / (count - 1) as f32
    };
    let (from, to, amount) = if t < 0.5 {
        ((0, 200, 255), (0, 255, 120), t * 2.0)
    } else {
        ((0, 255, 120), (255, 0, 200), (t - 0.5) * 2.0)
    };
    let (red, green, blue) = lerp_rgb(from, to, amount);
    Style::default().fg(Color::Rgb(red, green, blue))
}

fn lerp_rgb(from: (u8, u8, u8), to: (u8, u8, u8), amount: f32) -> (u8, u8, u8) {
    let mix = |start: u8, end: u8| {
        (start as f32 + (end as f32 - start as f32) * amount.clamp(0.0, 1.0)) as u8
    };
    (mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn text(line: Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn progress_line_fills_available_width() {
        for width in [20, 40, 80] {
            let rendered = text(progress_line(width, Some(11.0), Some(303.0), false));
            assert_eq!(rendered.chars().count(), usize::from(width));
            assert!(rendered.starts_with("0:11 "));
            assert!(rendered.ends_with(" 5:03"));
            assert!(rendered.contains('━'));
            assert!(rendered.contains('─'));
        }
    }

    #[test]
    fn progress_line_handles_extremes_and_idle() {
        let empty = text(progress_line(40, Some(0.0), Some(100.0), false));
        assert!(!empty.contains('━'));
        assert!(empty.contains('─'));

        let full = text(progress_line(40, Some(100.0), Some(100.0), false));
        assert!(!full.contains('─'));
        assert!(full.contains('━'));

        assert_eq!(text(progress_line(40, None, None, true)), "idle");
    }

    #[test]
    fn narrow_progress_line_falls_back_to_times() {
        assert_eq!(
            text(progress_line(10, Some(11.0), Some(303.0), false)),
            "0:11 / 5:03"
        );
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;

    fn line_width(line: &Line<'_>) -> usize {
        line.spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum()
    }

    #[test]
    fn hints_fit_the_available_width() {
        for focus in [Focus::NowPlaying, Focus::Queue] {
            for width in [40u16, 41, 69, 70, 100, 160] {
                for line in help_lines(focus, width) {
                    assert!(
                        line_width(&line) <= usize::from(width),
                        "hint too wide for {width}: {:?}",
                        line
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod daemon_tests {
    use super::*;

    #[test]
    fn daemon_loss_waits_for_the_grace_period() {
        let now = Instant::now();
        assert!(!daemon_loss_expired(None, now));
        assert!(!daemon_loss_expired(Some(now), now));
        assert!(!daemon_loss_expired(
            Some(now - Duration::from_millis(500)),
            now
        ));
        assert!(daemon_loss_expired(Some(now - Duration::from_secs(1)), now));
    }
}

#[cfg(test)]
mod fuzz_tests {
    use super::*;
    use crate::test_support::{Rng, cases};

    #[test]
    fn fuzz_display_name_never_panics() {
        let mut rng = Rng::new(7);
        for _ in 0..cases(5_000) {
            let input = rng.string(100);
            let _ = player::display_name(&input);
        }
    }
}

#[cfg(test)]
mod layout_fuzz_tests {
    use super::*;
    use crate::test_support::{Rng, cases, test_config, unique_base};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn fuzz_tui_rendering_never_panics() {
        let mut rng = Rng::new(0xBEEF);
        let iterations = cases(500).min(2_000);
        let base = unique_base("tui-layout");
        let config = test_config(base.join("Music"), &base);

        for _ in 0..iterations {
            let width = (rng.below(120) + 1) as u16;
            let height = (rng.below(60) + 1) as u16;
            let count = rng.below(30);
            let items: Vec<String> = (0..count)
                .map(|index| format!("● {:>4}  {}", index + 1, rng.string(60)))
                .collect();
            let playlist: Vec<PlaylistEntry> = (0..count)
                .map(|_| PlaylistEntry {
                    filename: rng.string(30),
                    title: Some(rng.string(30)),
                })
                .collect();
            let filtered: Vec<usize> = (0..count).filter(|_| rng.boolean()).collect();
            let selected = if filtered.is_empty() {
                None
            } else {
                Some(rng.below(filtered.len()))
            };

            let mut app = App {
                config: config.clone(),
                mpv: None,
                status: Status {
                    idle: rng.boolean(),
                    media_title: rng.string(80),
                    artist: rng.string(40),
                    album: rng.string(40),
                    path: rng.string(80),
                    has_title_tag: rng.boolean(),
                    time_pos: if rng.boolean() {
                        Some(rng.f64_bits())
                    } else {
                        None
                    },
                    duration: if rng.boolean() {
                        Some(rng.f64_bits())
                    } else {
                        None
                    },
                    paused: rng.boolean(),
                    volume: rng.f64_bits(),
                    shuffle: rng.boolean(),
                    repeat: match rng.below(3) {
                        0 => Repeat::Off,
                        1 => Repeat::All,
                        _ => Repeat::One,
                    },
                    playlist_pos: rng.below(35) as i64 - 1,
                    playlist_count: rng.below(35) as i64,
                    paused_for_cache: rng.boolean(),
                    mute: rng.boolean(),
                },
                focus: if rng.boolean() {
                    Focus::NowPlaying
                } else {
                    Focus::Queue
                },
                playlist,
                items,
                list_state: ListState::default().with_selected(selected),
                mounted: rng.boolean(),
                last_poll: Instant::now(),
                message: if rng.boolean() {
                    Some((rng.string(50), Instant::now()))
                } else {
                    None
                },
                pending: None,
                filter_query: rng.string(20),
                filter_editing: rng.boolean(),
                filtered,
                number_buffer: rng.string(6),
                seek_editing: rng.boolean(),
                seek_buffer: rng.string(12),
                picker: Picker::halfblocks(),
                cover_key: None,
                cover_pending: None,
                cover_aspect: None,
                cover_protocol: None,
                visualizer: None,
                visualizer_enabled: rng.boolean(),
                visualizer_ready: false,
                visualizer_retry_at: None,
                saw_mpv: false,
                mpv_lost_at: None,
                quit: false,
            };

            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
        }

        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod cover_placeholder_tests {
    use super::*;
    use crate::test_support::{test_config, unique_base};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_text(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn shows_loading_and_missing_cover_placeholders() {
        let base = unique_base("tui-cover");
        let music = base.join("Music");
        std::fs::create_dir_all(&music).unwrap();
        let config = test_config(music.clone(), &base);
        let mut app = App::new(config, Picker::halfblocks());
        let path = music.join("song.flac").display().to_string();
        app.status.path = path.clone();
        app.status.media_title = "song.flac".to_string();
        app.cover_key = Some(CoverKey::Local(music.clone()));

        let (sender, receiver) = mpsc::channel();
        app.cover_pending = Some((CoverKey::Local(music), receiver));
        let text = rendered_text(&mut app);
        assert!(text.contains("♪"));
        assert!(text.contains("loading…"));
        assert!(text.contains(&path));
        assert!(text.contains("file"));

        drop(sender);
        app.cover_pending = None;
        let text = rendered_text(&mut app);
        assert!(text.contains("♪"));
        assert!(text.contains("no cover"));
        assert!(text.contains(&path));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cover_area_matches_aspect_and_is_centered() {
        let area = Rect::new(0, 0, 40, 14);
        let font = FontSize::new(10, 20);
        assert_eq!(cover_area(area, Some(1.0), font), Rect::new(6, 0, 28, 14));
        assert_eq!(cover_area(area, Some(2.0), font), Rect::new(0, 2, 40, 10));
        assert_eq!(cover_area(area, None, font), Rect::new(6, 0, 28, 14));
    }

    #[test]
    fn keeps_the_detected_image_picker() {
        let base = unique_base("tui-picker");
        let music = base.join("Music");
        std::fs::create_dir_all(&music).unwrap();
        let config = test_config(music, &base);
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Kitty);
        let app = App::new(config, picker);
        assert_eq!(
            app.picker.protocol_type(),
            ratatui_image::picker::ProtocolType::Kitty
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn external_url_uses_media_title_instead_of_path() {
        let base = unique_base("tui-external");
        let music = base.join("Music");
        std::fs::create_dir_all(&music).unwrap();
        let config = test_config(music, &base);
        let mut app = App::new(config, Picker::halfblocks());
        app.status.path = "https://www.youtube.com/watch?v=abc".to_string();
        app.status.media_title = "Video Title".to_string();
        app.status.has_title_tag = false;

        let text = rendered_text(&mut app);
        assert!(text.contains("Video Title"));
        assert!(!text.contains("https://www.youtube.com/watch?v=abc"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tagged_title_wins_over_path_fallback() {
        let base = unique_base("tui-title");
        let music = base.join("Music");
        std::fs::create_dir_all(&music).unwrap();
        let config = test_config(music.clone(), &base);
        let mut app = App::new(config, Picker::halfblocks());
        let path = music.join("song.flac").display().to_string();
        app.status.path = path.clone();
        app.status.has_title_tag = true;
        app.status.media_title = "Real Title".to_string();
        app.status.artist = "Artist".to_string();
        app.status.album = "Album".to_string();

        let text = rendered_text(&mut app);
        assert!(text.contains("Real Title"));
        assert!(text.contains("Artist — Album"));
        assert!(!text.contains(&path));

        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use crate::test_support::{test_config, unique_base};
    use std::path::PathBuf;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app_with_items(items: &[&str]) -> (App, PathBuf) {
        let base = unique_base("tui-keys");
        let config = test_config(base.join("Music"), &base);
        let mut app = App::new(config, Picker::halfblocks());
        app.items = items.iter().map(|item| item.to_string()).collect();
        app.playlist = items
            .iter()
            .map(|item| PlaylistEntry {
                filename: item.to_string(),
                title: Some(item.to_string()),
            })
            .collect();
        app.filtered = (0..items.len()).collect();
        app.focus = Focus::Queue;
        app.list_state.select(Some(0));
        (app, base)
    }

    #[test]
    fn filter_edits_clears_and_maps_selection() {
        let (mut app, base) = app_with_items(&["alpha", "beta", "gamma"]);

        app.handle_key(key(KeyCode::Char('/')));
        assert!(app.filter_editing);

        app.handle_key(key(KeyCode::Char('e')));
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.filter_query, "et");
        assert_eq!(app.filtered, vec![1]);

        app.handle_key(key(KeyCode::Enter));
        assert!(!app.filter_editing);
        assert_eq!(app.filter_query, "et");

        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.filter_query, "");
        assert_eq!(app.filtered, vec![0, 1, 2]);
        assert_eq!(app.focus, Focus::Queue);

        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::NowPlaying);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn seek_prompt_edits_applies_and_cancels() {
        let (mut app, base) = app_with_items(&["alpha"]);

        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Char('t')));
        assert!(app.seek_editing);
        assert_eq!(app.number_buffer, "");

        for character in ['-', '3', '0', '%'] {
            app.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.seek_buffer, "-30%");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.seek_buffer, "-30");
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.seek_editing);
        assert_eq!(app.seek_buffer, "");

        app.handle_key(key(KeyCode::Char('t')));
        for character in ['5', '0', '%'] {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.seek_editing);
        assert_eq!(app.seek_buffer, "");
        assert!(app.message.is_some());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn multi_digit_number_jumps_to_queue_item() {
        let items: Vec<String> = (0..200).map(|index| format!("track {index}")).collect();
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        let (mut app, base) = app_with_items(&refs);

        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.number_buffer, "123");
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.number_buffer, "");
        assert_eq!(app.list_state.selected(), Some(122));
        assert_eq!(app.focus, Focus::Queue);

        app.handle_key(key(KeyCode::Char('4')));
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.list_state.selected(), Some(41));
        assert_eq!(app.focus, Focus::Queue);

        app.focus = Focus::NowPlaying;
        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Char('0')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.list_state.selected(), Some(9));
        assert_eq!(app.focus, Focus::NowPlaying);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn filtering_keeps_the_selected_track() {
        let (mut app, base) =
            app_with_items(&["zero", "one", "two", "three", "four", "five", "six"]);
        app.list_state.select(Some(5));

        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('f')));
        assert_eq!(app.filtered, vec![4, 5]);
        assert_eq!(
            app.list_state.selected(),
            Some(1),
            "selection should follow the track, not its absolute queue index"
        );

        // Enter must not index past the filtered list.
        app.handle_key(key(KeyCode::Enter));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shifted_g_jumps_to_queue_item() {
        let items: Vec<String> = (0..20).map(|index| format!("track {index}")).collect();
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        let (mut app, base) = app_with_items(&refs);

        app.handle_key(key(KeyCode::Char('3')));
        // Terminals report uppercase letters with the SHIFT modifier.
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));

        assert_eq!(app.number_buffer, "");
        assert_eq!(app.list_state.selected(), Some(2));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn out_of_range_number_is_rejected() {
        let items: Vec<String> = (0..20).map(|index| format!("track {index}")).collect();
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        let (mut app, base) = app_with_items(&refs);

        for digit in "999".chars() {
            app.handle_key(key(KeyCode::Char(digit)));
        }
        app.handle_key(key(KeyCode::Enter));
        let message = app.message.as_ref().map(|(message, _)| message.as_str());
        assert_eq!(message, Some("no track 999"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tab_switches_focus_and_enter_keeps_queue_focus() {
        let (mut app, base) = app_with_items(&["a", "b"]);

        assert_eq!(app.focus, Focus::Queue);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::NowPlaying);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Queue);

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Queue);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn open_key_without_playback_shows_message() {
        let (mut app, base) = app_with_items(&["a"]);
        app.status.path.clear();

        app.handle_key(key(KeyCode::Char('f')));
        let message = app.message.as_ref().map(|(message, _)| message.as_str());
        assert_eq!(message, Some("nothing playing"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn picker_keys_set_pending_action() {
        let (mut app, base) = app_with_items(&["a"]);

        app.handle_key(key(KeyCode::Char('o')));
        assert_eq!(app.pending, Some(Pending::Play));
        app.pending = None;

        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.pending, Some(Pending::Add));

        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod page_key_tests {
    use super::*;
    use crate::test_support::{test_config, unique_base};
    use std::path::PathBuf;

    fn app_with_items(items: &[String]) -> (App, PathBuf) {
        let base = unique_base("tui-pages");
        let config = test_config(base.join("Music"), &base);
        let mut app = App::new(config, Picker::halfblocks());
        app.items = items.to_vec();
        app.playlist = items
            .iter()
            .map(|item| PlaylistEntry {
                filename: item.clone(),
                title: Some(item.clone()),
            })
            .collect();
        app.filtered = (0..items.len()).collect();
        app.focus = Focus::Queue;
        app.list_state.select(Some(0));
        (app, base)
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_page_keys_move_selection() {
        let items: Vec<String> = (0..30).map(|index| format!("track {index}")).collect();
        let (mut app, base) = app_with_items(&items);

        app.handle_key(ctrl('f'));
        assert_eq!(app.list_state.selected(), Some(20));
        app.handle_key(ctrl('b'));
        assert_eq!(app.list_state.selected(), Some(0));
        app.handle_key(ctrl('d'));
        assert_eq!(app.list_state.selected(), Some(10));
        app.handle_key(ctrl('u'));
        assert_eq!(app.list_state.selected(), Some(0));

        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod spectrum_fuzz_tests {
    use super::*;
    use crate::test_support::{Rng, cases};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn spectrum_columns_fill_width() {
        for width in [20, 40, 60, 96, 145, 200, 300, 500, 1_000] {
            let (count, bar_width) = spectrum_columns(width, 64);
            let total = count * bar_width as usize + count.saturating_sub(1);
            assert!(count >= 1 && bar_width >= 1);
            assert!(total <= width, "width {width}, total {total}");
            assert!(
                width - total <= bar_width as usize,
                "width {width}, total {total}, bar_width {bar_width}"
            );
        }
    }

    #[test]
    fn full_scale_bars_fill_near_the_right_edge() {
        let width = 145u16;
        let height = 10u16;
        let bands = vec![1.0f32; 64];
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_spectrum(frame, frame.area(), &bands, true))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let last = (0..width)
            .rev()
            .find(|&x| {
                (0..height).any(|y| {
                    buffer
                        .cell(ratatui::layout::Position::new(x, y))
                        .map(|cell| cell.symbol() != " ")
                        .unwrap_or(false)
                })
            })
            .unwrap_or(0);

        let (count, bar_width) = spectrum_columns(width as usize, 64);
        let total = count * bar_width as usize + count.saturating_sub(1);
        assert!(
            last as usize + 1 >= total.saturating_sub(1),
            "last {last}, width {width}, total {total}"
        );

        let has_digit = buffer.content.iter().any(|cell| {
            cell.symbol()
                .chars()
                .any(|character| character.is_ascii_digit())
        });
        assert!(!has_digit, "spectrum should not render values");
    }

    #[test]
    fn fuzz_spectrum_rendering_never_panics() {
        let mut rng = Rng::new(0xABC);
        let iterations = cases(500).min(1_000);
        for _ in 0..iterations {
            let width = (rng.below(120) + 1) as u16;
            let height = (rng.below(20) + 1) as u16;
            let bands: Vec<f32> = (0..64).map(|_| rng.f64_bits() as f32).collect();
            let available = rng.boolean();

            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_spectrum(frame, frame.area(), &bands, available))
                .unwrap();
        }
    }
}
