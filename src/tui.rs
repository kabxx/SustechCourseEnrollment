use crate::{
    account::{Account, AccountStore, Credentials},
    cache::{self, CacheFile, Course, CourseTime, Semester},
    client::{SelectionEvent, SelectionEventHandler, SelectionResult, TisClient},
    error::{AppError, Result},
    settings::{ProxyMode, Settings},
};
use crossterm::{
    cursor,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    io::{self, Stdout},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::AbortHandle,
    time::sleep,
};

const BG: Color = Color::Rgb(17, 19, 23);
const PANEL: Color = Color::Rgb(25, 28, 34);
const PANEL_ALT: Color = Color::Rgb(36, 40, 47);
const BORDER: Color = Color::Rgb(61, 66, 75);
const TEXT: Color = Color::Rgb(235, 233, 226);
const MUTED: Color = Color::Rgb(157, 161, 169);
const CURSOR: Color = Color::Rgb(220, 220, 220);
const INPUT_BG: Color = Color::Rgb(45, 49, 57);
const INPUT_FG: Color = Color::White;
const CYAN: Color = Color::Rgb(238, 180, 83);
const TEAL: Color = Color::Rgb(92, 201, 170);
const GREEN: Color = Color::Rgb(100, 207, 148);
const YELLOW: Color = Color::Rgb(235, 190, 91);
const RED: Color = Color::Rgb(235, 111, 105);
const FOCUS_BORDER: Color = Color::Rgb(190, 194, 202);
const ACCOUNT_ITEM_HEIGHT: u16 = 1;
const LOG_CAPACITY: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Courses,
    Logs,
    Accounts,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Courses => Self::Logs,
            Self::Logs => Self::Accounts,
            Self::Accounts => Self::Courses,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollTarget {
    Accounts,
    Courses,
    Logs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollDrag {
    target: ScrollTarget,
    grab_offset: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CourseTab {
    Selected,
    Targets,
    All,
}

impl CourseTab {
    fn toggle(&mut self) {
        *self = match self {
            Self::Targets => Self::All,
            Self::All => Self::Selected,
            Self::Selected => Self::Targets,
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeKind {
    Info,
    Success,
    Warning,
    Failure,
    Error,
}

#[derive(Debug, Clone)]
struct Notice {
    kind: NoticeKind,
    text: String,
}

impl Notice {
    fn info(text: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Info,
            text: text.into(),
        }
    }

    fn success(text: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Success,
            text: text.into(),
        }
    }

    fn warning(text: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Warning,
            text: text.into(),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Error,
            text: text.into(),
        }
    }

    fn color(&self) -> Color {
        match self.kind {
            NoticeKind::Info => CYAN,
            NoticeKind::Success => GREEN,
            NoticeKind::Warning => YELLOW,
            NoticeKind::Failure => RED,
            NoticeKind::Error => RED,
        }
    }

    fn prefix(&self) -> &'static str {
        match self.kind {
            NoticeKind::Info => "[提示]",
            NoticeKind::Success => "[成功]",
            NoticeKind::Warning => "[警告]",
            NoticeKind::Failure => "[失败]",
            NoticeKind::Error => "[错误]",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheState {
    Empty,
    Loading,
    Cached,
    Refreshed,
}

impl CacheState {
    fn label(self) -> &'static str {
        match self {
            Self::Empty => "未加载",
            Self::Loading => "加载中",
            Self::Cached => "可用",
            Self::Refreshed => "已更新",
        }
    }
}

#[derive(Debug)]
enum Modal {
    None,
    ExitConfirm,
    AddAccount {
        id: String,
        password: String,
        field: AddField,
    },
    Search {
        value: String,
    },
    Settings {
        proxy_mode: ProxyMode,
        request_interval: String,
        field: SettingsField,
        error: Option<String>,
    },
    DeleteAccount {
        index: usize,
    },
    ConflictConfirm {
        course: Course,
        conflicts: Vec<String>,
    },
    CourseDetail {
        course: Course,
        conflicts: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalButton {
    Confirm,
    Cancel,
}

impl ModalButton {
    fn toggle(&mut self) {
        *self = match self {
            Self::Confirm => Self::Cancel,
            Self::Cancel => Self::Confirm,
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddField {
    Id,
    Password,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    Proxy,
    Interval,
}

impl SettingsField {
    fn next(self) -> Self {
        match self {
            Self::Proxy => Self::Interval,
            Self::Interval => Self::Proxy,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Proxy => Self::Interval,
            Self::Interval => Self::Proxy,
        }
    }
}

#[derive(Debug)]
enum ConfirmAction {
    QuickSelect { courses: Vec<(String, Course)> },
}

#[derive(Debug)]
struct CourseRow {
    key: String,
    name: String,
    course: Option<Course>,
    target: bool,
}

#[derive(Debug)]
struct ActionMessage {
    kind: NoticeKind,
    text: String,
}

struct LoadedAccount {
    client: TisClient,
    semester: Semester,
    course_kinds: Vec<String>,
    courses: BTreeMap<String, Course>,
    selected_courses: Option<BTreeMap<String, Course>>,
    cache_state: CacheState,
    display_name: Option<String>,
}

struct RefreshedContent {
    selected_courses: std::result::Result<BTreeMap<String, Course>, String>,
    courses: std::result::Result<BTreeMap<String, Course>, String>,
    course_kinds: Vec<String>,
}

enum UiMessage {
    AccountValidated {
        id: String,
        password: String,
        request_id: u64,
        result: std::result::Result<(TisClient, Option<String>), String>,
    },
    AccountLoaded {
        account_id: String,
        request_id: u64,
        result: std::result::Result<LoadedAccount, String>,
    },
    ContentRefreshed {
        account_id: String,
        request_id: u64,
        content: RefreshedContent,
    },
    ActionFinished {
        account_id: String,
        stopped: bool,
        result: std::result::Result<(), String>,
    },
    ActionProgress {
        account_id: String,
        succeeded: Option<String>,
        message: ActionMessage,
    },
}

#[derive(Debug, Clone, Copy)]
struct UiLayout {
    accounts: Rect,
    account_list: Rect,
    courses: Rect,
    course_rows: Rect,
    info: Rect,
    footer: Rect,
    compact: bool,
    tiny: bool,
}

impl UiLayout {
    fn new_with_help(area: Rect, help: &[&str]) -> Self {
        let tiny = area.width < 42 || area.height < 14;
        if tiny {
            return Self {
                accounts: Rect::default(),
                account_list: Rect::default(),
                courses: Rect::default(),
                course_rows: Rect::default(),
                info: Rect::default(),
                footer: Rect::default(),
                compact: false,
                tiny: true,
            };
        }

        let footer_height = footer_height_for(area.width, area.height, help);
        let usable_height = area.height.saturating_sub(footer_height);
        let main_min_height = 8;
        let info_height = (usable_height / 4)
            .max(5)
            .min(usable_height.saturating_sub(main_min_height));
        let footer_y = area
            .y
            .saturating_add(area.height.saturating_sub(footer_height));
        let footer = Rect {
            x: area.x,
            y: footer_y,
            width: area.width,
            height: footer_height,
        };
        let info = Rect {
            x: area.x,
            y: footer_y.saturating_sub(info_height),
            width: area.width,
            height: info_height,
        };
        let main = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: area
                .height
                .saturating_sub(footer.height)
                .saturating_sub(info.height),
        };
        let compact = area.width < 72 || main.height < 14;

        if compact {
            let panel_height = (main.height / 3).clamp(4, 7);
            let accounts = Rect {
                x: main.x,
                y: main.y,
                width: main.width,
                height: panel_height,
            };
            let account_inner = Rect {
                x: accounts.x.saturating_add(1),
                y: accounts.y.saturating_add(1),
                width: accounts.width.saturating_sub(2),
                height: accounts.height.saturating_sub(2),
            };
            let courses = Rect {
                x: main.x,
                y: main.y.saturating_add(panel_height),
                width: main.width,
                height: main.height.saturating_sub(panel_height),
            };
            let account_list = account_inner;
            let course_rows = Self::course_regions(courses);
            return Self {
                accounts,
                account_list,
                courses,
                course_rows,
                info,
                footer,
                compact,
                tiny,
            };
        }

        let sidebar_width = if area.width >= 96 { 24 } else { 20 };
        let accounts = Rect {
            x: main.x,
            y: main.y,
            width: sidebar_width.min(main.width.saturating_sub(37)),
            height: main.height,
        };
        let pane_gap = 1.min(main.width.saturating_sub(accounts.width));
        let courses = Rect {
            x: accounts
                .x
                .saturating_add(accounts.width)
                .saturating_add(pane_gap),
            y: main.y,
            width: main
                .width
                .saturating_sub(accounts.width)
                .saturating_sub(pane_gap),
            height: main.height,
        };
        let account_inner = Rect {
            x: accounts.x.saturating_add(1),
            y: accounts.y.saturating_add(1),
            width: accounts.width.saturating_sub(2),
            height: accounts.height.saturating_sub(2),
        };
        let account_list = account_inner;
        let course_rows = Self::course_regions(courses);
        Self {
            accounts,
            account_list,
            courses,
            course_rows,
            info,
            footer,
            compact,
            tiny,
        }
    }

    fn course_regions(courses: Rect) -> Rect {
        if courses.width < 3 || courses.height < 3 {
            return Rect::default();
        }
        Rect {
            x: courses.x.saturating_add(1),
            y: courses.y.saturating_add(1),
            width: courses.width.saturating_sub(2),
            height: courses.height.saturating_sub(2),
        }
    }
}

pub async fn run() -> Result<()> {
    let mut app = App::new()?;
    let (tx, rx) = mpsc::unbounded_channel();
    let mut terminal = setup_terminal()?;
    if app.account_index.is_some() {
        app.start_load(&tx);
    }
    let result = run_loop(&mut terminal, &mut app, tx, rx).await;
    restore_terminal(&mut terminal)?;
    result
}

pub fn run_scrollbar_preview() -> Result<()> {
    let mut app = preview_app();
    let mut terminal = setup_terminal()?;
    let result = run_preview_loop(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    result
}

fn preview_app() -> App {
    let accounts = (1..=48)
        .map(|number| Account {
            id: format!("2026{:04}", number),
            target_courses: Vec::new(),
            name: None,
        })
        .collect();
    App {
        store: AccountStore { accounts },
        settings: Settings::default(),
        account_index: Some(0),
        account_cursor: 0,
        account_scroll: 0,
        focus: Focus::Accounts,
        tab: CourseTab::Targets,
        row: 0,
        course_scroll: 0,
        search: String::new(),
        courses: BTreeMap::new(),
        selected_courses: None,
        course_kinds: Vec::new(),
        semester: None,
        client: None,
        cache_state: CacheState::Empty,
        loading: false,
        login_abort: None,
        login_request_id: 0,
        selection_stop: None,
        load_abort: None,
        load_request_id: 0,
        refresh_abort: None,
        refresh_request_id: 0,
        notice: Notice::info(""),
        logs: VecDeque::new(),
        log_scroll: 0,
        modal: Modal::None,
        modal_button: ModalButton::Confirm,
        scroll_drag: None,
        course_mouse_down: None,
        course_mouse_dragged: false,
        last_course_click: None,
        last_area: Rect::default(),
        should_quit: false,
    }
}

fn run_preview_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| app.render(frame))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                KeyCode::Up => move_preview_cursor(app, -1),
                KeyCode::Down => move_preview_cursor(app, 1),
                KeyCode::PageUp => move_preview_cursor(
                    app,
                    -(account_capacity(app.ui_layout(app.last_area).account_list.height) as i32),
                ),
                KeyCode::PageDown => move_preview_cursor(
                    app,
                    account_capacity(app.ui_layout(app.last_area).account_list.height) as i32,
                ),
                KeyCode::Home => {
                    app.account_cursor = 0;
                    app.account_index = Some(0);
                }
                KeyCode::End => {
                    let last = app.store.accounts.len().saturating_sub(1);
                    app.account_cursor = last;
                    app.account_index = Some(last);
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.should_quit = true;
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

fn move_preview_cursor(app: &mut App, delta: i32) {
    let count = app.store.accounts.len();
    if count == 0 {
        return;
    }
    let current = app.account_cursor.min(count.saturating_sub(1));
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        current.saturating_add(delta as usize).min(count - 1)
    };
    app.account_cursor = next;
    app.account_index = Some(next);
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, cursor::Hide) {
        let _ = terminal::disable_raw_mode();
        return Err(AppError::Io(error));
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    )?;
    terminal.show_cursor()?;
    Ok(())
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    tx: UnboundedSender<UiMessage>,
    mut rx: UnboundedReceiver<UiMessage>,
) -> Result<()> {
    execute!(terminal.backend_mut(), EnableMouseCapture)?;
    loop {
        terminal.draw(|frame| app.render(frame))?;
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            Some(message) = rx.recv() => app.apply_message(message, &tx),
            _ = sleep(Duration::from_millis(50)) => {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) if key.kind != KeyEventKind::Release => {
                            app.handle_key(key, &tx);
                        }
                        Event::Mouse(mouse) => {
                            if matches!(app.modal, Modal::None) {
                                app.handle_main_mouse(mouse, &tx);
                            } else {
                                app.handle_modal_mouse(mouse, &tx);
                            }
                        }
                        _ => {}
                    }
                    if app.should_quit {
                        return Ok(());
                    }
                }
            }
        }
    }
}

struct App {
    store: AccountStore,
    settings: Settings,
    account_index: Option<usize>,
    account_cursor: usize,
    account_scroll: usize,
    focus: Focus,
    tab: CourseTab,
    row: usize,
    course_scroll: usize,
    search: String,
    courses: BTreeMap<String, Course>,
    selected_courses: Option<BTreeMap<String, Course>>,
    course_kinds: Vec<String>,
    semester: Option<Semester>,
    client: Option<TisClient>,
    cache_state: CacheState,
    loading: bool,
    login_abort: Option<AbortHandle>,
    login_request_id: u64,
    selection_stop: Option<Arc<AtomicBool>>,
    load_abort: Option<AbortHandle>,
    load_request_id: u64,
    refresh_abort: Option<AbortHandle>,
    refresh_request_id: u64,
    notice: Notice,
    logs: VecDeque<Notice>,
    log_scroll: usize,
    modal: Modal,
    modal_button: ModalButton,
    scroll_drag: Option<ScrollDrag>,
    course_mouse_down: Option<(CourseTab, String)>,
    course_mouse_dragged: bool,
    last_course_click: Option<(String, Instant)>,
    last_area: Rect,
    should_quit: bool,
}

impl App {
    fn ui_layout(&self, area: Rect) -> UiLayout {
        let help = self.global_help_items();
        UiLayout::new_with_help(area, &help)
    }

    fn new() -> Result<Self> {
        let store = AccountStore::load()?;
        let settings = Settings::load()?;
        let account_index = (!store.accounts.is_empty()).then_some(0);
        Ok(Self {
            store,
            settings,
            account_index,
            account_cursor: account_index.unwrap_or(0),
            account_scroll: account_index.unwrap_or(0),
            focus: Focus::Courses,
            tab: CourseTab::Targets,
            row: 0,
            course_scroll: 0,
            search: String::new(),
            courses: BTreeMap::new(),
            selected_courses: None,
            course_kinds: Vec::new(),
            semester: None,
            client: None,
            cache_state: CacheState::Empty,
            loading: false,
            login_abort: None,
            login_request_id: 0,
            selection_stop: None,
            load_abort: None,
            load_request_id: 0,
            refresh_abort: None,
            refresh_request_id: 0,
            notice: if account_index.is_some() {
                Notice::info("正在检查账户会话和课程缓存")
            } else {
                Notice::info("")
            },
            logs: VecDeque::new(),
            log_scroll: 0,
            modal: Modal::None,
            modal_button: ModalButton::Confirm,
            scroll_drag: None,
            course_mouse_down: None,
            course_mouse_dragged: false,
            last_course_click: None,
            last_area: Rect::default(),
            should_quit: false,
        })
    }

    fn set_modal(&mut self, modal: Modal) {
        self.modal = modal;
        self.modal_button = ModalButton::Confirm;
    }

    fn set_notice(&mut self, notice: Notice) {
        if !notice.text.trim().is_empty() {
            self.logs.push_back(notice.clone());
            if self.logs.len() > LOG_CAPACITY {
                self.logs.pop_front();
            }
            self.log_scroll = usize::MAX;
        }
        self.notice = notice;
    }

    fn current_account(&self) -> Option<&crate::account::Account> {
        self.account_index
            .and_then(|index| self.store.accounts.get(index))
    }

    fn current_account_id(&self) -> Option<String> {
        self.current_account().map(|account| account.id.clone())
    }

    fn start_load(&mut self, tx: &UnboundedSender<UiMessage>) {
        let Some(index) = self.account_index else {
            self.set_notice(Notice::warning("请先添加账户"));
            return;
        };
        if self.loading {
            self.set_notice(Notice::warning("当前账户仍在加载，请稍候"));
            return;
        }
        let account = self.store.accounts[index].clone();
        let cached_name = account.name.clone();
        let credentials = match self.store.credentials(index) {
            Ok(credentials) => credentials,
            Err(error) => {
                self.set_notice(Notice::error(error_text(&error)));
                return;
            }
        };
        let cache_path = match AccountStore::cache_path(&account.id) {
            Ok(path) => path,
            Err(error) => {
                self.set_notice(Notice::error(error_text(&error)));
                return;
            }
        };
        self.loading = true;
        self.client = None;
        self.semester = None;
        self.courses.clear();
        self.selected_courses = None;
        self.course_kinds.clear();
        self.row = 0;
        self.course_scroll = 0;
        self.cache_state = CacheState::Loading;
        self.load_request_id = self.load_request_id.wrapping_add(1);
        let request_id = self.load_request_id;
        let id = account.id.clone();
        let sender = tx.clone();
        let task = tokio::spawn(async move {
            let result = load_account(account.id, credentials, cache_path, cached_name).await;
            let _ = sender.send(UiMessage::AccountLoaded {
                account_id: id,
                request_id,
                result,
            });
        });
        self.load_abort = Some(task.abort_handle());
    }

    fn start_load_with_client(
        &mut self,
        id: String,
        client: TisClient,
        display_name: Option<String>,
        tx: &UnboundedSender<UiMessage>,
    ) {
        let cache_path = match AccountStore::cache_path(&id) {
            Ok(path) => path,
            Err(error) => {
                self.set_notice(Notice::error(error_text(&error)));
                return;
            }
        };
        self.loading = true;
        self.client = None;
        self.semester = None;
        self.courses.clear();
        self.selected_courses = None;
        self.course_kinds.clear();
        self.row = 0;
        self.course_scroll = 0;
        self.cache_state = CacheState::Loading;
        self.load_request_id = self.load_request_id.wrapping_add(1);
        let request_id = self.load_request_id;
        let account_id = id.clone();
        let sender = tx.clone();
        let task = tokio::spawn(async move {
            let result = load_account_with_client(id, client, cache_path, display_name).await;
            let _ = sender.send(UiMessage::AccountLoaded {
                account_id,
                request_id,
                result,
            });
        });
        self.load_abort = Some(task.abort_handle());
    }

    fn refresh_content(&mut self, tx: &UnboundedSender<UiMessage>) {
        if self.cancel_content_refresh(true) {
            return;
        }
        let Some(index) = self.account_index else {
            self.set_notice(Notice::warning("请先选择账户"));
            return;
        };
        if self.loading {
            if self.cancel_load(true) {
                return;
            }
            self.set_notice(Notice::warning("当前仍有网络请求，请稍候"));
            return;
        }
        let Some(client) = self.client.clone() else {
            self.set_notice(Notice::warning("账户尚未加载完成"));
            return;
        };
        let Some(semester) = self.semester.clone() else {
            self.set_notice(Notice::warning("当前学期尚未加载完成"));
            return;
        };
        let account_id = self.store.accounts[index].id.clone();
        let cache_path = match AccountStore::cache_path(&account_id) {
            Ok(path) => path,
            Err(error) => {
                self.set_notice(Notice::error(error_text(&error)));
                return;
            }
        };
        self.refresh_targets_local();
        self.loading = true;
        self.refresh_request_id = self.refresh_request_id.wrapping_add(1);
        let request_id = self.refresh_request_id;
        let sender = tx.clone();
        let id_for_task = account_id.clone();
        let task = tokio::spawn(async move {
            let session_error = client
                .ensure_session()
                .await
                .err()
                .map(|error| error_text(&error));
            let selected_courses = match session_error.as_ref() {
                Some(error) => Err(error.clone()),
                None => client
                    .selected_courses(&semester)
                    .await
                    .map_err(|error| error_text(&error)),
            };
            let (courses, course_kinds) = match session_error {
                Some(error) => (Err(error), Vec::new()),
                None => match client.course_kinds(&semester).await {
                    Ok(course_kinds) if !course_kinds.is_empty() => {
                        let result = client
                            .all_courses(&semester, &course_kinds)
                            .await
                            .map_err(|error| error_text(&error));
                        if let Ok(ref all_courses) = result {
                            let training_type =
                                client.training_type().unwrap_or_else(|_| "2".to_owned());
                            let previous_selected = cache::load(&cache_path)
                                .ok()
                                .flatten()
                                .and_then(|cache| cache.selected_courses);
                            let mut cache = CacheFile::new(
                                id_for_task.clone(),
                                training_type,
                                semester.clone(),
                                course_kinds.clone(),
                                all_courses.clone(),
                            );
                            cache.selected_courses = selected_courses
                                .as_ref()
                                .ok()
                                .cloned()
                                .or(previous_selected);
                            let _ = cache::save(&cache_path, &cache);
                        }
                        (result, course_kinds)
                    }
                    Ok(_) => (Err("没有可用的课程轮次".to_owned()), Vec::new()),
                    Err(error) => (Err(error_text(&error)), Vec::new()),
                },
            };
            if courses.is_err() {
                if let Ok(selected) = &selected_courses {
                    if let Ok(Some(mut cache)) = cache::load(&cache_path) {
                        cache.selected_courses = Some(selected.clone());
                        let _ = cache::save(&cache_path, &cache);
                    }
                }
            }
            let _ = sender.send(UiMessage::ContentRefreshed {
                account_id: id_for_task,
                request_id,
                content: RefreshedContent {
                    selected_courses,
                    courses,
                    course_kinds,
                },
            });
        });
        self.refresh_abort = Some(task.abort_handle());
    }

    fn cancel_content_refresh(&mut self, notify: bool) -> bool {
        let Some(abort) = self.refresh_abort.take() else {
            return false;
        };
        abort.abort();
        self.refresh_request_id = self.refresh_request_id.wrapping_add(1);
        self.loading = false;
        if notify {
            self.set_notice(Notice::info("刷新已取消"));
        }
        true
    }

    fn cancel_load(&mut self, notify: bool) -> bool {
        let Some(abort) = self.load_abort.take() else {
            return false;
        };
        abort.abort();
        self.load_request_id = self.load_request_id.wrapping_add(1);
        self.loading = false;
        self.cache_state = CacheState::Empty;
        if notify {
            self.set_notice(Notice::info("加载已取消"));
        }
        true
    }

    fn cancel_login(&mut self, notify: bool) -> bool {
        let Some(abort) = self.login_abort.take() else {
            return false;
        };
        abort.abort();
        self.login_request_id = self.login_request_id.wrapping_add(1);
        self.loading = false;
        if notify {
            self.set_notice(Notice::info("登录已取消"));
        }
        true
    }

    fn refresh_targets_local(&mut self) {
        let Some(current_id) = self.current_account_id() else {
            return;
        };
        let Ok(store) = AccountStore::load() else {
            self.set_notice(Notice::warning("候选列表读取失败，保留当前内容"));
            return;
        };
        let Some(index) = store
            .accounts
            .iter()
            .position(|account| account.id == current_id)
        else {
            self.set_notice(Notice::warning("当前账户记录不存在，请重新选择账户"));
            return;
        };
        self.store = store;
        self.account_index = Some(index);
        self.account_cursor = index;
        self.account_scroll = self.account_scroll.min(index);
        self.row = self.row.min(self.visible_rows().len().saturating_sub(1));
    }

    fn apply_message(&mut self, message: UiMessage, tx: &UnboundedSender<UiMessage>) {
        match message {
            UiMessage::AccountValidated {
                id,
                password,
                request_id,
                result,
            } => {
                if request_id != self.login_request_id {
                    return;
                }
                self.login_abort = None;
                self.loading = false;
                match result {
                    Ok((client, display_name)) => {
                        match self.store.add_with_name(
                            id.clone(),
                            password.clone(),
                            display_name.clone(),
                        ) {
                            Ok(()) => {
                                let index = self.store.accounts.len().saturating_sub(1);
                                self.account_index = Some(index);
                                self.account_cursor = index;
                                self.account_scroll = index;
                                self.focus = Focus::Accounts;
                                self.tab = CourseTab::Targets;
                                self.row = 0;
                                self.course_scroll = 0;
                                self.set_notice(Notice::success("账户已添加，正在加载课程目录"));
                                self.start_load_with_client(id, client, display_name, tx);
                            }
                            Err(error) => {
                                self.set_notice(Notice::error(error_text(&error)));
                                self.set_modal(Modal::AddAccount {
                                    id,
                                    password,
                                    field: AddField::Id,
                                });
                            }
                        }
                    }
                    Err(error) => {
                        self.set_notice(Notice::error(error));
                        self.set_modal(Modal::AddAccount {
                            id,
                            password,
                            field: AddField::Password,
                        });
                    }
                }
            }
            UiMessage::AccountLoaded {
                account_id,
                request_id,
                result,
            } => {
                if self.current_account_id().as_deref() != Some(account_id.as_str()) {
                    return;
                }
                if request_id != self.load_request_id {
                    return;
                }
                self.load_abort = None;
                self.loading = false;
                match result {
                    Ok(loaded) => {
                        if let Some(name) = loaded.display_name.clone() {
                            if let Some(index) = self
                                .store
                                .accounts
                                .iter()
                                .position(|account| account.id == account_id)
                            {
                                let _ = self.store.set_name(index, Some(name));
                            }
                        }
                        self.client = Some(loaded.client);
                        self.semester = Some(loaded.semester);
                        self.course_kinds = loaded.course_kinds;
                        self.courses = loaded.courses;
                        self.selected_courses = loaded.selected_courses;
                        self.cache_state = loaded.cache_state;
                        self.row = self.row.min(self.visible_rows().len().saturating_sub(1));
                        self.set_notice(Notice::success(format!(
                            "课程目录已就绪，共 {} 门，缓存{}",
                            self.courses.len(),
                            self.cache_state.label()
                        )));
                    }
                    Err(error) => {
                        self.cache_state = CacheState::Empty;
                        self.set_notice(Notice::error(error));
                    }
                }
            }
            UiMessage::ContentRefreshed {
                account_id,
                request_id,
                content,
            } => {
                if self.current_account_id().as_deref() != Some(account_id.as_str()) {
                    return;
                }
                if request_id != self.refresh_request_id {
                    return;
                }
                self.refresh_abort = None;
                self.loading = false;
                let selected_error = match content.selected_courses {
                    Ok(courses) => {
                        self.selected_courses = Some(courses);
                        None
                    }
                    Err(error) => Some(format!(
                        "已选课程刷新失败：{}",
                        compact_notice_detail(&error).replace('\n', " ")
                    )),
                };
                let courses_error = match content.courses {
                    Ok(courses) => {
                        self.courses = courses;
                        self.course_kinds = content.course_kinds;
                        self.cache_state = CacheState::Refreshed;
                        None
                    }
                    Err(error) => Some(format!(
                        "全部课程刷新失败：{}",
                        compact_notice_detail(&error).replace('\n', " ")
                    )),
                };
                if selected_error.is_none() && courses_error.is_none() {
                    self.set_notice(Notice::success("课程已刷新"));
                } else {
                    if let Some(error) = selected_error {
                        self.set_notice(Notice::error(error));
                    }
                    if let Some(error) = courses_error {
                        self.set_notice(Notice::error(error));
                    }
                }
                self.row = self.row.min(self.visible_rows().len().saturating_sub(1));
                self.course_scroll = self
                    .course_scroll
                    .min(self.visible_rows().len().saturating_sub(1));
            }
            UiMessage::ActionProgress {
                account_id,
                succeeded,
                message,
            } => {
                if self.current_account_id().as_deref() != Some(account_id.as_str()) {
                    return;
                }
                if let Some(key) = succeeded {
                    self.remove_succeeded_targets(&[key]);
                }
                self.set_notice(Notice {
                    kind: message.kind,
                    text: message.text,
                });
            }
            UiMessage::ActionFinished {
                account_id,
                stopped,
                result,
            } => {
                if self.current_account_id().as_deref() != Some(account_id.as_str()) {
                    return;
                }
                self.loading = false;
                self.selection_stop = None;
                match result {
                    Ok(()) if stopped => self.set_notice(Notice::info("抢课已停止")),
                    Ok(()) => self.set_notice(Notice::success("已成功选取所有候选课程")),
                    Err(error) => self.set_notice(Notice::error(error)),
                }
            }
        }
    }

    fn remove_succeeded_targets(&mut self, keys: &[String]) {
        let Some(index) = self.account_index else {
            return;
        };
        if index >= self.store.accounts.len() || keys.is_empty() {
            return;
        }
        let mut targets = self.store.accounts[index].target_courses.clone();
        targets.retain(|target| !keys.iter().any(|key| key == target));
        if targets == self.store.accounts[index].target_courses {
            return;
        }
        if let Err(error) = self.store.set_targets(index, targets) {
            self.set_notice(Notice::error(error_text(&error)));
        }
        self.row = self.row.min(self.visible_rows().len().saturating_sub(1));
        self.course_scroll = self
            .course_scroll
            .min(self.visible_rows().len().saturating_sub(1));
    }

    fn handle_key(&mut self, key: KeyEvent, tx: &UnboundedSender<UiMessage>) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_selection_stop();
            self.cancel_content_refresh(false);
            self.cancel_load(false);
            self.cancel_login(false);
            self.should_quit = true;
            return;
        }
        if !matches!(self.modal, Modal::None) {
            self.handle_modal_key(key, tx);
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.set_modal(Modal::ExitConfirm);
            }
            KeyCode::Tab => {
                self.tab.toggle();
                self.focus = Focus::Courses;
                self.row = 0;
                self.course_scroll = 0;
                self.last_course_click = None;
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.focus = self.focus.next();
            }
            KeyCode::Char('l') | KeyCode::Char('L') => {
                self.focus = Focus::Accounts;
                self.open_add_account();
            }
            KeyCode::Char('u') | KeyCode::Char('U') => {
                let focus = self.focus;
                self.refresh_content(tx);
                self.focus = focus;
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                self.focus = Focus::Courses;
                self.set_modal(Modal::Search {
                    value: self.search.clone(),
                });
            }
            KeyCode::Char('s') | KeyCode::Char('S') => self.open_settings(),
            KeyCode::Up => self.move_focus_cursor(-1, tx),
            KeyCode::Down => self.move_focus_cursor(1, tx),
            KeyCode::PageUp => self.page_focus_cursor(-1, tx),
            KeyCode::PageDown => self.page_focus_cursor(1, tx),
            KeyCode::Home => self.home_focus_cursor(tx),
            KeyCode::End => self.end_focus_cursor(tx),
            KeyCode::Char(' ') if self.focus == Focus::Accounts && self.login_abort.is_some() => {
                self.cancel_login(true);
            }
            KeyCode::Char(' ') if self.focus == Focus::Courses => self.course_space(),
            KeyCode::Char('[') if self.focus == Focus::Courses => self.move_target_priority(-1),
            KeyCode::Char(']') if self.focus == Focus::Courses => self.move_target_priority(1),
            KeyCode::Char('d') | KeyCode::Char('D') if self.focus == Focus::Courses => {
                self.open_current_course_detail()
            }
            KeyCode::Char('x') | KeyCode::Char('X') if self.focus == Focus::Logs => {
                self.logs.clear();
                self.log_scroll = 0;
                self.notice = Notice::info("");
            }
            KeyCode::Enter => self.start_quick_select(tx),
            _ => {}
        }
    }

    fn request_selection_stop(&mut self) {
        if let Some(stop) = &self.selection_stop {
            stop.store(true, Ordering::Release);
        }
    }

    fn course_space(&mut self) {
        match self.tab {
            CourseTab::All => self.add_current_target(),
            CourseTab::Targets => self.remove_current_target(),
            CourseTab::Selected => {}
        }
    }

    fn move_focus_cursor(&mut self, delta: i32, tx: &UnboundedSender<UiMessage>) {
        let layout = self.ui_layout(self.last_area);
        match self.focus {
            Focus::Courses => self.move_course_cursor(delta, layout.course_rows.height),
            Focus::Logs => self.scroll_logs(delta, layout.info.height),
            Focus::Accounts => self.move_account_cursor(delta, layout.account_list.height, tx),
        }
    }

    fn page_focus_cursor(&mut self, direction: i32, tx: &UnboundedSender<UiMessage>) {
        let layout = self.ui_layout(self.last_area);
        match self.focus {
            Focus::Courses => {
                let amount = usize::from(layout.course_rows.height.max(1));
                self.move_course_cursor(
                    direction.saturating_mul(amount as i32),
                    layout.course_rows.height,
                );
            }
            Focus::Logs => {
                let amount = usize::from(layout.info.height.saturating_sub(2).max(1));
                self.scroll_logs(direction.saturating_mul(amount as i32), layout.info.height);
            }
            Focus::Accounts => {
                let amount = account_capacity(layout.account_list.height);
                self.move_account_cursor(
                    direction.saturating_mul(amount as i32),
                    layout.account_list.height,
                    tx,
                );
            }
        }
    }

    fn home_focus_cursor(&mut self, tx: &UnboundedSender<UiMessage>) {
        let layout = self.ui_layout(self.last_area);
        match self.focus {
            Focus::Courses => self.set_course_cursor(0, layout.course_rows.height),
            Focus::Logs => self.set_log_scroll(0, layout.info.height),
            Focus::Accounts => self.set_account_cursor(0, layout.account_list.height, tx),
        }
    }

    fn end_focus_cursor(&mut self, tx: &UnboundedSender<UiMessage>) {
        let layout = self.ui_layout(self.last_area);
        match self.focus {
            Focus::Courses => {
                let last = self.visible_rows().len().saturating_sub(1);
                self.set_course_cursor(last, layout.course_rows.height);
            }
            Focus::Logs => self.set_log_scroll(usize::MAX, layout.info.height),
            Focus::Accounts => {
                let last = self.store.accounts.len().saturating_sub(1);
                self.set_account_cursor(last, layout.account_list.height, tx);
            }
        }
    }

    fn scrollbar_for(
        &self,
        layout: UiLayout,
        target: ScrollTarget,
    ) -> Option<(Rect, Rect, usize, usize, usize)> {
        let (panel, visible, total, start) = match target {
            ScrollTarget::Accounts => (
                layout.accounts,
                account_capacity(layout.account_list.height),
                self.store.accounts.len(),
                self.account_window_start(layout.account_list.height),
            ),
            ScrollTarget::Courses => (
                layout.courses,
                usize::from(layout.course_rows.height.max(1)),
                self.visible_rows().len(),
                self.course_window_start(layout.course_rows.height),
            ),
            ScrollTarget::Logs => (
                layout.info,
                log_capacity(layout.info.height),
                self.logs.len(),
                self.log_window_start(layout.info.height),
            ),
        };
        scrollbar_geometry(panel, visible, total, start)
            .map(|(track, thumb)| (track, thumb, visible, total, start))
    }

    fn set_scroll_from_pointer(&mut self, target: ScrollTarget, track: Rect, thumb: Rect, y: u16) {
        let max_offset = track.height.saturating_sub(thumb.height);
        let grab_offset = self
            .scroll_drag
            .map(|drag| drag.grab_offset)
            .unwrap_or(thumb.height / 2);
        let offset = y
            .saturating_sub(track.y)
            .saturating_sub(grab_offset)
            .min(max_offset);
        let (visible, total) = match target {
            ScrollTarget::Accounts => (
                account_capacity(self.ui_layout(self.last_area).account_list.height),
                self.store.accounts.len(),
            ),
            ScrollTarget::Courses => (
                usize::from(self.ui_layout(self.last_area).course_rows.height.max(1)),
                self.visible_rows().len(),
            ),
            ScrollTarget::Logs => {
                let layout = self.ui_layout(self.last_area);
                (log_capacity(layout.info.height), self.logs.len())
            }
        };
        let max_start = total.saturating_sub(visible);
        let max_offset = usize::from(max_offset);
        let start = if max_offset == 0 {
            0
        } else {
            max_start.saturating_mul(usize::from(offset)) / max_offset
        };
        match target {
            ScrollTarget::Accounts => {
                self.account_scroll = start.min(max_start);
                self.focus = Focus::Accounts;
            }
            ScrollTarget::Courses => {
                self.course_scroll = start.min(max_start);
                self.row = self.row.max(self.course_scroll).min(
                    self.course_scroll
                        .saturating_add(visible)
                        .saturating_sub(1)
                        .min(total.saturating_sub(1)),
                );
                self.focus = Focus::Courses;
            }
            ScrollTarget::Logs => {
                self.log_scroll = start.min(max_start);
                self.focus = Focus::Logs;
            }
        }
    }

    fn handle_scrollbar_mouse(&mut self, mouse: MouseEvent, layout: UiLayout) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                for target in [
                    ScrollTarget::Accounts,
                    ScrollTarget::Courses,
                    ScrollTarget::Logs,
                ] {
                    let Some((track, thumb, _, _, _)) = self.scrollbar_for(layout, target) else {
                        continue;
                    };
                    if !point_in_rect(track, mouse.column, mouse.row) {
                        continue;
                    }
                    self.focus = match target {
                        ScrollTarget::Accounts => Focus::Accounts,
                        ScrollTarget::Courses => Focus::Courses,
                        ScrollTarget::Logs => Focus::Logs,
                    };
                    let grab_offset = if point_in_rect(thumb, mouse.column, mouse.row) {
                        mouse.row.saturating_sub(thumb.y)
                    } else {
                        thumb.height / 2
                    };
                    self.scroll_drag = Some(ScrollDrag {
                        target,
                        grab_offset,
                    });
                    self.set_scroll_from_pointer(target, track, thumb, mouse.row);
                    return true;
                }
                false
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                let Some(drag) = self.scroll_drag else {
                    return false;
                };
                if let Some((track, thumb, _, _, _)) = self.scrollbar_for(layout, drag.target) {
                    self.set_scroll_from_pointer(drag.target, track, thumb, mouse.row);
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) => self.scroll_drag.take().is_some(),
            _ => false,
        }
    }

    fn handle_main_mouse(&mut self, mouse: MouseEvent, tx: &UnboundedSender<UiMessage>) {
        let layout = self.ui_layout(self.last_area);
        if layout.tiny {
            return;
        }
        if point_in_rect(layout.footer, mouse.column, mouse.row) {
            return;
        }
        if self.handle_scrollbar_mouse(mouse, layout) {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.course_mouse_dragged = false;
                if point_in_rect(layout.accounts, mouse.column, mouse.row) {
                    self.course_mouse_down = None;
                    self.last_course_click = None;
                    self.focus = Focus::Accounts;
                    if point_in_rect(layout.account_list, mouse.column, mouse.row) {
                        self.handle_account_click(layout.account_list, mouse.column, mouse.row, tx);
                    }
                    return;
                }
                if point_in_rect(layout.courses, mouse.column, mouse.row) {
                    self.focus = Focus::Courses;
                }
                if let Some(tab) = course_tab_at(layout.courses, mouse.column, mouse.row) {
                    self.course_mouse_down = None;
                    self.tab = tab;
                    self.row = 0;
                    self.course_scroll = 0;
                    self.last_course_click = None;
                    return;
                }
                if let Some((index, row_area)) = self.course_row_at(layout, mouse.column, mouse.row)
                {
                    let rows = self.visible_rows();
                    self.row = index;
                    let action_area = course_remove_area(row_area);
                    let action_hit = self.tab != CourseTab::Selected
                        && point_in_rect(action_area, mouse.column, mouse.row);
                    if action_hit {
                        self.course_mouse_down = None;
                        self.last_course_click = None;
                        match self.tab {
                            CourseTab::All => self.add_current_target(),
                            CourseTab::Targets => self.remove_current_target(),
                            CourseTab::Selected => {}
                        }
                    } else if let Some(row) = rows.get(index) {
                        let key = row.key.clone();
                        if row.course.is_none() {
                            self.course_mouse_down = None;
                            self.last_course_click = None;
                            return;
                        }
                        self.course_mouse_down = Some((self.tab, key.clone()));
                    }
                    return;
                }
                self.course_mouse_down = None;
                self.last_course_click = None;
                if point_in_rect(layout.info, mouse.column, mouse.row) {
                    self.focus = Focus::Logs;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.course_mouse_down = None;
                self.course_mouse_dragged = false;
                self.last_course_click = None;
                let direction = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    -1
                } else {
                    1
                };
                if point_in_rect(layout.accounts, mouse.column, mouse.row) {
                    self.scroll_accounts(direction, layout.account_list.height);
                } else if point_in_rect(layout.courses, mouse.column, mouse.row) {
                    self.scroll_courses(direction, layout.course_rows.height);
                } else if point_in_rect(layout.info, mouse.column, mouse.row) {
                    self.scroll_logs(direction, layout.info.height);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.course_mouse_down = None;
                self.course_mouse_dragged = true;
                self.last_course_click = None;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let dragged = std::mem::replace(&mut self.course_mouse_dragged, false);
                let pressed = self.course_mouse_down.take();
                if dragged {
                    return;
                }
                let Some((index, row_area)) = self.course_row_at(layout, mouse.column, mouse.row)
                else {
                    return;
                };
                if self.tab != CourseTab::Selected
                    && point_in_rect(course_remove_area(row_area), mouse.column, mouse.row)
                {
                    return;
                }
                let rows = self.visible_rows();
                let Some(row) = rows.get(index) else {
                    return;
                };
                if pressed.as_ref().is_some_and(|(pressed_tab, pressed_key)| {
                    pressed_tab != &self.tab || pressed_key != &row.key
                }) {
                    return;
                }
                let Some(course) = row.course.clone() else {
                    return;
                };
                self.row = index;
                self.register_course_click(row.key.clone(), course);
            }
            _ => {}
        }
    }

    fn course_row_at(&self, layout: UiLayout, x: u16, y: u16) -> Option<(usize, Rect)> {
        if !point_in_rect(layout.course_rows, x, y) {
            return None;
        }
        let rows = self.visible_rows();
        let offset = usize::from(y.saturating_sub(layout.course_rows.y));
        let start = self.course_window_start(layout.course_rows.height);
        let index = start
            .checked_add(offset)
            .filter(|index| *index < rows.len())?;
        Some((
            index,
            Rect {
                x: layout.course_rows.x,
                y: layout.course_rows.y.saturating_add(offset as u16),
                width: layout.course_rows.width,
                height: 1,
            },
        ))
    }

    fn register_course_click(&mut self, key: String, course: Course) {
        let now = Instant::now();
        let is_double = self
            .last_course_click
            .as_ref()
            .is_some_and(|(previous_key, when)| {
                previous_key == &key
                    && now.saturating_duration_since(*when) <= Duration::from_millis(500)
            });
        if is_double {
            self.last_course_click = None;
            let conflicts = self.conflicting_courses(&course);
            self.set_modal(Modal::CourseDetail { course, conflicts });
        } else {
            self.last_course_click = Some((key, now));
        }
    }

    fn handle_account_click(
        &mut self,
        list_area: Rect,
        x: u16,
        y: u16,
        tx: &UnboundedSender<UiMessage>,
    ) {
        if self.store.accounts.is_empty() || list_area.height == 0 {
            return;
        }
        let start = self.account_window_start(list_area.height);
        let physical_offset = usize::from(y.saturating_sub(list_area.y));
        if physical_offset >= usize::from(list_area.height) {
            return;
        }
        let offset = physical_offset / usize::from(ACCOUNT_ITEM_HEIGHT);
        let index = start.saturating_add(offset);
        if index >= self.store.accounts.len() {
            return;
        }
        let content_width = list_area.width;
        if x < list_area.x || x >= list_area.x.saturating_add(content_width) {
            return;
        }
        let row = Rect {
            x: list_area.x,
            y: list_area
                .y
                .saturating_add((offset as u16).saturating_mul(ACCOUNT_ITEM_HEIGHT)),
            width: content_width,
            height: ACCOUNT_ITEM_HEIGHT.min(
                list_area.y.saturating_add(list_area.height).saturating_sub(
                    list_area
                        .y
                        .saturating_add((offset as u16).saturating_mul(ACCOUNT_ITEM_HEIGHT)),
                ),
            ),
        };
        let delete_width = row.width.min(3);
        let delete_x = row.x.saturating_add(row.width.saturating_sub(delete_width));
        if delete_width > 0 && x >= delete_x {
            self.open_delete_account_at(index);
        } else {
            self.focus = Focus::Accounts;
            self.select_account(index, tx);
        }
    }

    fn set_account_selection(&mut self, index: usize) {
        self.account_index = Some(index);
        self.account_cursor = index;
        self.focus = Focus::Accounts;
        self.row = 0;
        self.course_scroll = 0;
        self.search.clear();
        self.tab = CourseTab::Targets;
    }

    fn handle_modal_mouse(&mut self, mouse: MouseEvent, tx: &UnboundedSender<UiMessage>) {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }
        if matches!(self.modal, Modal::CourseDetail { .. }) {
            let popup = self.modal_rect(self.last_area);
            if point_in_rect(course_detail_button_rect(popup), mouse.column, mouse.row) {
                self.modal = Modal::None;
            }
            return;
        }
        let popup = self.modal_rect(self.last_area);
        let (confirm, cancel) = modal_button_rects(popup);
        if point_in_rect(confirm, mouse.column, mouse.row) {
            self.modal_button = ModalButton::Confirm;
            self.activate_modal_button(tx);
            return;
        }
        if point_in_rect(cancel, mouse.column, mouse.row) {
            self.modal_button = ModalButton::Cancel;
            self.activate_modal_button(tx);
            return;
        }

        match &mut self.modal {
            Modal::AddAccount { field, .. } => {
                let (id_area, password_area) = modal_field_rects(popup);
                if point_in_field(id_area, mouse.column, mouse.row) {
                    *field = AddField::Id;
                } else if point_in_field(password_area, mouse.column, mouse.row) {
                    *field = AddField::Password;
                }
            }
            Modal::Settings {
                proxy_mode,
                field,
                error,
                ..
            } => {
                let (proxy_area, interval_area, _) = modal_settings_rects(popup);
                if point_in_field(proxy_area, mouse.column, mouse.row) {
                    *field = SettingsField::Proxy;
                    proxy_mode.toggle();
                    *error = None;
                } else if point_in_field(interval_area, mouse.column, mouse.row) {
                    *field = SettingsField::Interval;
                }
            }
            _ => {}
        }
    }

    fn modal_rect(&self, area: Rect) -> Rect {
        let (width, height) = match &self.modal {
            Modal::ExitConfirm => (42, confirmation_modal_height(1)),
            Modal::AddAccount { .. } => (42, 10),
            Modal::Search { .. } => (44, 7),
            Modal::Settings { error, .. } => (32, if error.is_some() { 11 } else { 10 }),
            Modal::DeleteAccount { .. } => (42, confirmation_modal_height(1)),
            Modal::ConflictConfirm { course, conflicts } => {
                let message = conflict_confirmation_message(course, conflicts);
                let longest = message
                    .lines()
                    .map(|line| Line::from(line).width())
                    .max()
                    .unwrap_or(1);
                let width = even_modal_width(
                    u16::try_from(longest.saturating_add(4)).unwrap_or(u16::MAX),
                    42,
                    68,
                );
                let content_width = width
                    .min(area.width.saturating_sub(2))
                    .saturating_sub(4)
                    .max(1);
                let lines = wrapped_line_count(&message, content_width);
                let height = confirmation_modal_height(lines);
                (width, height)
            }
            Modal::CourseDetail { course, conflicts } => {
                let lines = course_detail_lines(course, conflicts);
                let longest = lines
                    .iter()
                    .map(|line| Line::from(line.as_str()).width())
                    .max()
                    .unwrap_or(1);
                let width = even_modal_width(
                    u16::try_from(longest.saturating_add(4)).unwrap_or(u16::MAX),
                    36,
                    64,
                );
                let effective_width = width.min(area.width.saturating_sub(2)).max(1);
                let content_width = effective_width.saturating_sub(4).max(1);
                let content_lines = lines
                    .iter()
                    .map(|line| wrapped_line_count(line, content_width))
                    .sum::<usize>()
                    .max(1);
                let height = u16::try_from(content_lines.saturating_add(4))
                    .unwrap_or(u16::MAX)
                    .max(8);
                (width, height)
            }
            Modal::None => (0, 0),
        };
        centered_rect(width, height, area)
    }

    fn open_current_course_detail(&mut self) {
        let rows = self.visible_rows();
        let Some(course) = rows.get(self.row).and_then(|row| row.course.clone()) else {
            return;
        };
        let conflicts = self.conflicting_courses(&course);
        self.set_modal(Modal::CourseDetail { course, conflicts });
    }

    fn open_add_account(&mut self) {
        if self.loading {
            self.set_notice(Notice::warning("当前仍有网络请求，请完成后再添加账户"));
            return;
        }
        self.set_modal(Modal::AddAccount {
            id: String::new(),
            password: String::new(),
            field: AddField::Id,
        });
    }

    fn open_settings(&mut self) {
        if self.loading || self.selection_stop.is_some() {
            self.set_notice(Notice::warning("请先停止当前网络任务再修改设置"));
            return;
        }
        self.set_modal(Modal::Settings {
            proxy_mode: self.settings.proxy_mode,
            request_interval: self.settings.request_interval_ms.to_string(),
            field: SettingsField::Proxy,
            error: None,
        });
    }

    fn open_delete_account_at(&mut self, index: usize) {
        if self.loading {
            self.set_notice(Notice::warning("当前仍有网络请求，请完成后再删除账户"));
            return;
        }
        if index < self.store.accounts.len() {
            self.set_modal(Modal::DeleteAccount { index });
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent, tx: &UnboundedSender<UiMessage>) {
        let selected_button = self.modal_button;
        let mut activate = false;
        let mut cancel = false;
        match &mut self.modal {
            Modal::AddAccount {
                id,
                password,
                field,
            } => match key.code {
                KeyCode::Esc => cancel = true,
                KeyCode::Left | KeyCode::Right => self.modal_button.toggle(),
                KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                    *field = match field {
                        AddField::Id => AddField::Password,
                        AddField::Password => AddField::Id,
                    };
                }
                KeyCode::Backspace => match field {
                    AddField::Id => {
                        id.pop();
                    }
                    AddField::Password => {
                        password.pop();
                    }
                },
                KeyCode::Enter => activate = true,
                KeyCode::Char(value) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match field {
                        AddField::Id => id.push(value),
                        AddField::Password => password.push(value),
                    }
                }
                _ => {}
            },
            Modal::Search { value } => match key.code {
                KeyCode::Esc => cancel = true,
                KeyCode::Left | KeyCode::Right => self.modal_button.toggle(),
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Enter => activate = true,
                KeyCode::Char(value_char) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    value.push(value_char);
                }
                _ => {}
            },
            Modal::Settings {
                proxy_mode,
                request_interval,
                field,
                error,
            } => match key.code {
                KeyCode::Esc => cancel = true,
                KeyCode::Tab if *field == SettingsField::Proxy => {
                    proxy_mode.toggle();
                    *error = None;
                }
                KeyCode::Down => *field = field.next(),
                KeyCode::Up => *field = field.previous(),
                KeyCode::Left | KeyCode::Right => self.modal_button.toggle(),
                KeyCode::Enter => activate = true,
                KeyCode::Backspace if *field == SettingsField::Interval => {
                    request_interval.pop();
                    *error = None;
                }
                KeyCode::Char(value)
                    if *field == SettingsField::Interval
                        && value.is_ascii_digit()
                        && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if request_interval.len() < 10 {
                        request_interval.push(value);
                    }
                    *error = None;
                }
                _ => {}
            },
            Modal::DeleteAccount { .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('n') => cancel = true,
                KeyCode::Left | KeyCode::Right => self.modal_button.toggle(),
                KeyCode::Enter | KeyCode::Char('y') => activate = true,
                _ => {}
            },
            Modal::ConflictConfirm { .. } => match key.code {
                KeyCode::Esc => cancel = true,
                KeyCode::Left | KeyCode::Right => self.modal_button.toggle(),
                KeyCode::Enter => activate = true,
                _ => {}
            },
            Modal::CourseDetail { .. } => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('d') | KeyCode::Char('D') => {
                    cancel = true
                }
                _ => {}
            },
            Modal::ExitConfirm => match key.code {
                KeyCode::Esc => cancel = true,
                KeyCode::Left | KeyCode::Right => self.modal_button.toggle(),
                KeyCode::Enter => activate = true,
                _ => {}
            },
            Modal::None => {}
        }
        if cancel || (activate && selected_button == ModalButton::Cancel) {
            self.modal = Modal::None;
        } else if activate {
            self.activate_modal_button(tx);
        }
    }

    fn activate_modal_button(&mut self, tx: &UnboundedSender<UiMessage>) {
        if self.modal_button == ModalButton::Cancel {
            self.modal = Modal::None;
            return;
        }
        let modal = std::mem::replace(&mut self.modal, Modal::None);
        match modal {
            Modal::ExitConfirm => {
                self.request_selection_stop();
                self.cancel_content_refresh(false);
                self.cancel_load(false);
                self.cancel_login(false);
                self.should_quit = true;
            }
            Modal::AddAccount { id, password, .. } => {
                let id = id.trim().to_owned();
                if self.store.accounts.iter().any(|account| account.id == id) {
                    self.set_notice(Notice::error("账户重复"));
                    self.set_modal(Modal::AddAccount {
                        id,
                        password,
                        field: AddField::Id,
                    });
                    return;
                }
                let credentials = match Credentials::new(id.clone(), password.clone()) {
                    Ok(credentials) => credentials,
                    Err(error) => {
                        self.set_notice(Notice::error(error_text(&error)));
                        self.set_modal(Modal::AddAccount {
                            id,
                            password,
                            field: AddField::Password,
                        });
                        return;
                    }
                };
                self.loading = true;
                self.login_request_id = self.login_request_id.wrapping_add(1);
                let request_id = self.login_request_id;
                let sender = tx.clone();
                let task = tokio::spawn(async move {
                    let result = async {
                        let client =
                            TisClient::new(credentials).map_err(|_| stage_error("登录"))?;
                        let display_name =
                            client.login_with_name().await.map_err(login_stage_error)?;
                        Ok((client, display_name))
                    }
                    .await;
                    let _ = sender.send(UiMessage::AccountValidated {
                        id,
                        password,
                        request_id,
                        result,
                    });
                });
                self.login_abort = Some(task.abort_handle());
            }
            Modal::Search { value, .. } => {
                self.search = value.trim().to_owned();
                self.row = 0;
                self.course_scroll = 0;
            }
            Modal::Settings {
                proxy_mode,
                request_interval,
                ..
            } => {
                let parsed = request_interval.trim().parse::<u64>();
                let candidate = parsed.map(|request_interval_ms| Settings {
                    proxy_mode,
                    request_interval_ms,
                });
                let candidate = match candidate {
                    Ok(candidate) => candidate,
                    Err(_) => {
                        self.set_modal(Modal::Settings {
                            proxy_mode,
                            request_interval,
                            field: SettingsField::Interval,
                            error: Some("请求间隔必须是正整数毫秒".to_owned()),
                        });
                        return;
                    }
                };
                if let Err(error) = candidate.validate() {
                    self.set_modal(Modal::Settings {
                        proxy_mode,
                        request_interval,
                        field: SettingsField::Interval,
                        error: Some(error_text(&error)),
                    });
                    return;
                }
                let updated_client = match self.client.clone() {
                    Some(mut client) => {
                        if let Err(error) = client.apply_settings(&candidate) {
                            self.set_modal(Modal::Settings {
                                proxy_mode,
                                request_interval,
                                field: SettingsField::Interval,
                                error: Some(error_text(&error)),
                            });
                            return;
                        }
                        Some(client)
                    }
                    None => None,
                };
                if let Err(error) = candidate.save() {
                    self.set_modal(Modal::Settings {
                        proxy_mode,
                        request_interval,
                        field: SettingsField::Interval,
                        error: Some(error_text(&error)),
                    });
                    return;
                }
                self.settings = candidate;
                self.client = updated_client;
                self.set_notice(Notice::success("设置已保存"));
            }
            Modal::DeleteAccount { index } => self.delete_account_at(index, tx),
            Modal::ConflictConfirm { course, .. } => self.commit_target(course),
            Modal::CourseDetail { .. } => {}
            Modal::None => {}
        }
    }

    fn delete_account_at(&mut self, index: usize, tx: &UnboundedSender<UiMessage>) {
        if index >= self.store.accounts.len() {
            return;
        }
        match self.store.remove(index) {
            Ok(account) => {
                if self.store.accounts.is_empty() {
                    self.account_index = None;
                    self.account_cursor = 0;
                    self.account_scroll = 0;
                    self.client = None;
                    self.semester = None;
                    self.courses.clear();
                    self.selected_courses = None;
                    self.course_kinds.clear();
                    self.cache_state = CacheState::Empty;
                    self.set_notice(Notice::success(format!("已删除账户 {}", account.id)));
                } else if self.account_index == Some(index) {
                    let next = index.min(self.store.accounts.len() - 1);
                    self.set_account_selection(next);
                    self.set_notice(Notice::success(format!("已删除账户 {}", account.id)));
                    self.start_load(tx);
                } else {
                    if let Some(current) = self.account_index {
                        if index < current {
                            self.account_index = Some(current - 1);
                            self.account_cursor = self.account_cursor.saturating_sub(1);
                        }
                    }
                    self.account_scroll = self
                        .account_scroll
                        .min(self.store.accounts.len().saturating_sub(1));
                    self.set_notice(Notice::success(format!("已删除账户 {}", account.id)));
                }
            }
            Err(error) => self.set_notice(Notice::error(error_text(&error))),
        }
    }

    fn account_window_start(&self, height: u16) -> usize {
        let capacity = account_capacity(height);
        self.account_scroll
            .min(self.store.accounts.len().saturating_sub(capacity))
    }

    fn scroll_accounts(&mut self, delta: i32, height: u16) {
        let capacity = account_capacity(height);
        let max_start = self.store.accounts.len().saturating_sub(capacity);
        if delta.is_negative() {
            self.account_scroll = self
                .account_scroll
                .saturating_sub(delta.unsigned_abs() as usize);
        } else {
            self.account_scroll = self
                .account_scroll
                .saturating_add(delta as usize)
                .min(max_start);
        }
    }

    fn move_account_cursor(&mut self, delta: i32, height: u16, tx: &UnboundedSender<UiMessage>) {
        let count = self.store.accounts.len();
        if count == 0 || self.loading {
            return;
        }
        let current = self.account_cursor.min(count.saturating_sub(1));
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current
                .saturating_add(delta as usize)
                .min(count.saturating_sub(1))
        };
        self.set_account_cursor(next, height, tx);
    }

    fn set_account_cursor(&mut self, index: usize, height: u16, tx: &UnboundedSender<UiMessage>) {
        if self.store.accounts.is_empty() {
            return;
        }
        let index = index.min(self.store.accounts.len() - 1);
        self.account_cursor = index;
        self.ensure_account_visible(height);
        if self.account_index == Some(index) {
            self.focus = Focus::Accounts;
            return;
        }
        self.select_account(index, tx);
    }

    fn ensure_account_visible(&mut self, height: u16) {
        let capacity = account_capacity(height);
        if self.account_cursor < self.account_scroll {
            self.account_scroll = self.account_cursor;
        } else if self.account_cursor >= self.account_scroll.saturating_add(capacity) {
            self.account_scroll = self
                .account_cursor
                .saturating_add(1)
                .saturating_sub(capacity);
        }
        self.account_scroll = self
            .account_scroll
            .min(self.store.accounts.len().saturating_sub(capacity));
    }

    fn move_course_cursor(&mut self, delta: i32, height: u16) {
        let count = self.visible_rows().len();
        if count == 0 {
            self.row = 0;
            self.course_scroll = 0;
            return;
        }
        let current = self.row.min(count - 1);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current
                .saturating_add(delta as usize)
                .min(count.saturating_sub(1))
        };
        self.set_course_cursor(next, height);
    }

    fn set_course_cursor(&mut self, index: usize, height: u16) {
        let count = self.visible_rows().len();
        if count == 0 {
            self.row = 0;
            self.course_scroll = 0;
            return;
        }
        self.row = index.min(count - 1);
        let capacity = usize::from(height.max(1));
        if self.row < self.course_scroll {
            self.course_scroll = self.row;
        } else if self.row >= self.course_scroll.saturating_add(capacity) {
            self.course_scroll = self.row.saturating_add(1).saturating_sub(capacity);
        }
        self.course_scroll = self.course_scroll.min(count.saturating_sub(capacity));
    }

    fn set_log_scroll(&mut self, offset: usize, height: u16) {
        let capacity = log_capacity(height);
        let max_start = self.logs.len().saturating_sub(capacity);
        self.log_scroll = offset.min(max_start);
    }

    fn scroll_logs(&mut self, delta: i32, height: u16) {
        let capacity = log_capacity(height);
        let max_start = self.logs.len().saturating_sub(capacity);
        let current = self.log_scroll.min(max_start);
        self.log_scroll = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current.saturating_add(delta as usize).min(max_start)
        };
    }

    fn log_window_start(&self, height: u16) -> usize {
        self.log_scroll
            .min(self.logs.len().saturating_sub(log_capacity(height)))
    }

    fn scroll_courses(&mut self, delta: i32, height: u16) {
        let count = self.visible_rows().len();
        if count == 0 {
            self.row = 0;
            self.course_scroll = 0;
            return;
        }
        let capacity = usize::from(height.max(1));
        let max_start = count.saturating_sub(capacity);
        let step = capacity.clamp(1, 3);
        let amount = step.saturating_mul(delta.unsigned_abs() as usize);
        self.course_scroll = if delta.is_negative() {
            self.course_scroll.saturating_sub(amount)
        } else {
            self.course_scroll.saturating_add(amount).min(max_start)
        };
        let visible_end = self
            .course_scroll
            .saturating_add(capacity)
            .min(count)
            .saturating_sub(1);
        self.row = self.row.max(self.course_scroll).min(visible_end);
    }

    fn course_window_start(&self, height: u16) -> usize {
        let capacity = usize::from(height.max(1));
        self.course_scroll
            .min(self.visible_rows().len().saturating_sub(capacity))
    }

    fn select_account(&mut self, index: usize, tx: &UnboundedSender<UiMessage>) {
        if index >= self.store.accounts.len() {
            return;
        }
        if self.account_index == Some(index) {
            self.account_cursor = index;
            self.focus = Focus::Accounts;
            return;
        }
        if self.loading {
            self.set_notice(Notice::warning("当前账户仍在加载，请稍候"));
            return;
        }
        self.set_account_selection(index);
        self.start_load(tx);
    }

    fn remove_target(&mut self, index: usize, key: &str, label: &str) {
        if index >= self.store.accounts.len() {
            return;
        }
        let mut targets = self.store.accounts[index].target_courses.clone();
        targets.retain(|target| target != key);
        if let Err(error) = self.store.set_targets(index, targets) {
            self.set_notice(Notice::error(error_text(&error)));
        } else {
            self.set_notice(Notice::success(format!("已从候选列表移除：{label}")));
            self.row = self.row.min(self.visible_rows().len().saturating_sub(1));
            self.course_scroll = self
                .course_scroll
                .min(self.visible_rows().len().saturating_sub(1));
        }
    }

    fn commit_target(&mut self, course: Course) {
        let Some(index) = self.account_index else {
            self.set_notice(Notice::warning("请先选择账户"));
            return;
        };
        if index >= self.store.accounts.len() {
            return;
        }
        let key = if course.id.trim().is_empty() {
            course.name.clone()
        } else {
            course.id.clone()
        };
        let mut targets = self.store.accounts[index].target_courses.clone();
        if targets
            .iter()
            .any(|target| target == &key || target == &course.id)
        {
            self.set_notice(Notice::info(format!("已在候选列表：{}", course.name)));
            return;
        }
        targets.push(key);
        if let Err(error) = self.store.set_targets(index, targets) {
            self.set_notice(Notice::error(error_text(&error)));
        } else {
            self.set_notice(Notice::success(format!("已加入候选列表：{}", course.name)));
        }
    }

    fn add_current_target(&mut self) {
        if self.tab != CourseTab::All {
            return;
        }
        if self.loading {
            self.set_notice(Notice::warning("当前仍有网络请求，请稍候"));
            return;
        }
        let Some(index) = self.account_index else {
            self.set_notice(Notice::warning("请先选择账户"));
            return;
        };
        let rows = self.visible_rows();
        let Some(row) = rows.get(self.row) else {
            return;
        };
        let Some(course) = row.course.clone() else {
            self.set_notice(Notice::warning("该课程不在当前目录中，无法加入候选"));
            return;
        };
        let key = if course.id.trim().is_empty() {
            course.name.clone()
        } else {
            course.id.clone()
        };
        let targets = &self.store.accounts[index].target_courses;
        if targets
            .iter()
            .any(|target| target == &key || target == &course.name || target == &course.id)
        {
            self.set_notice(Notice::info(format!("已在候选列表：{}", course.name)));
            return;
        }
        let conflicts = self.conflicting_courses(&course);
        if conflicts.is_empty() {
            self.commit_target(course);
        } else {
            self.set_modal(Modal::ConflictConfirm { course, conflicts });
        }
    }

    fn conflicting_courses(&self, course: &Course) -> Vec<String> {
        let mut conflicts = Vec::new();
        if let Some(selected) = &self.selected_courses {
            for other in selected.values() {
                if other.id != course.id && courses_overlap(course, other) {
                    push_unique(&mut conflicts, other.display_name());
                }
            }
        }
        if let Some(account) = self.current_account() {
            for key in &account.target_courses {
                let Some(other) = self.resolve_course(key) else {
                    continue;
                };
                if other.id != course.id && courses_overlap(course, &other) {
                    push_unique(&mut conflicts, other.display_name());
                }
            }
        }
        if let Some(server_conflict) = course
            .details
            .get("conflict_course")
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            let mut matched = false;
            if let Some(selected) = &self.selected_courses {
                for other in selected.values() {
                    if server_conflict.contains(&other.name) {
                        push_unique(&mut conflicts, other.display_name());
                        matched = true;
                    }
                }
            }
            if !matched && conflicts.is_empty() {
                push_unique(
                    &mut conflicts,
                    server_conflict
                        .trim()
                        .trim_end_matches(['。', '.', '；', ';'])
                        .to_owned(),
                );
            }
        }
        conflicts
    }

    fn remove_current_target(&mut self) {
        if self.tab != CourseTab::Targets {
            return;
        }
        if self.loading {
            self.set_notice(Notice::warning("当前仍有网络请求，请稍候"));
            return;
        }
        let rows = self.visible_rows();
        let Some(row) = rows.get(self.row) else {
            return;
        };
        let Some(index) = self.account_index else {
            return;
        };
        self.remove_target(index, &row.key, &row.name);
    }

    fn move_target_priority(&mut self, direction: i32) {
        if self.tab != CourseTab::Targets {
            return;
        }
        if self.loading {
            self.set_notice(Notice::warning("当前仍有网络请求，请稍候"));
            return;
        }
        let Some(account_index) = self.account_index else {
            return;
        };
        let Some(account) = self.store.accounts.get(account_index) else {
            return;
        };
        let original_targets = account.target_courses.clone();
        let rows = self.visible_rows();
        let Some(row) = rows.get(self.row) else {
            return;
        };
        let Some(current_index) = original_targets.iter().position(|key| key == &row.key) else {
            return;
        };
        let len = original_targets.len();
        if len < 2 {
            return;
        }
        let target_index = if direction < 0 {
            current_index.saturating_sub(1)
        } else {
            (current_index + 1).min(len - 1)
        };
        if target_index == current_index {
            return;
        }
        let mut targets = original_targets;
        let moved_key = targets[target_index].clone();
        targets.swap(current_index, target_index);
        if let Err(error) = self.store.set_targets(account_index, targets) {
            self.set_notice(Notice::error(error_text(&error)));
            return;
        }
        self.row = self
            .visible_rows()
            .iter()
            .position(|row| row.key == moved_key)
            .unwrap_or(self.row);
        self.set_notice(Notice::info("已调整候选优先级"));
    }

    fn start_quick_select(&mut self, tx: &UnboundedSender<UiMessage>) {
        if let Some(stop) = &self.selection_stop {
            stop.store(true, Ordering::Release);
            return;
        }
        let Some(account) = self.current_account() else {
            self.set_notice(Notice::warning("请先选择账户"));
            return;
        };
        let target_keys = account.target_courses.clone();
        let mut courses = Vec::new();
        for key in target_keys {
            let course = self.resolve_course(&key);
            if let Some(course) = course {
                courses.push((key, course));
            }
        }
        if courses.is_empty() {
            self.set_notice(Notice::warning("候选列表为空，或课程目录中暂时没有匹配项"));
            return;
        }
        self.begin_action(ConfirmAction::QuickSelect { courses }, tx);
    }

    fn begin_action(&mut self, action: ConfirmAction, tx: &UnboundedSender<UiMessage>) {
        if !matches!(action, ConfirmAction::QuickSelect { .. }) {
            self.set_notice(Notice::error("内部错误：无效的网络动作"));
            return;
        }
        let Some(client) = self.client.clone() else {
            self.set_notice(Notice::warning("课程目录尚未加载完成"));
            return;
        };
        let Some(semester) = self.semester.clone() else {
            self.set_notice(Notice::warning("当前学期尚未加载完成"));
            return;
        };
        let Some(account_id) = self.current_account_id() else {
            self.set_notice(Notice::warning("请先选择账户"));
            return;
        };
        if self.loading {
            self.set_notice(Notice::warning("当前仍有网络请求，请稍候"));
            return;
        }
        self.loading = true;
        let stop = Arc::new(AtomicBool::new(false));
        self.selection_stop = Some(stop.clone());
        let courses = match action {
            ConfirmAction::QuickSelect { courses } => courses,
        };
        let course_count = courses.len();
        self.set_notice(Notice::info(format!(
            "抢课已开始，共 {course_count} 门候选课程"
        )));
        let sender = tx.clone();
        let account_id_for_task = account_id.clone();
        tokio::spawn(async move {
            let result = submit_action_loop(
                client,
                semester,
                courses,
                stop.clone(),
                account_id_for_task,
                sender.clone(),
            )
            .await;
            let stopped = stop.load(Ordering::Acquire);
            let _ = sender.send(UiMessage::ActionFinished {
                account_id,
                stopped,
                result,
            });
        });
    }

    fn visible_rows(&self) -> Vec<CourseRow> {
        let query = self.search.clone();
        let matches = |name: &str| text_matches_query(name, &query);
        let matches_course = |course: &Course| course_matches_query(course, &query);
        match (self.tab, self.current_account()) {
            (CourseTab::Targets, Some(account)) => account
                .target_courses
                .iter()
                .filter(|key| {
                    matches(key)
                        || self
                            .resolve_course(key)
                            .as_ref()
                            .is_some_and(matches_course)
                })
                .map(|key| {
                    let course = self.resolve_course(key);
                    CourseRow {
                        key: key.clone(),
                        name: course
                            .as_ref()
                            .map(Course::display_name)
                            .unwrap_or_else(|| key.clone()),
                        course,
                        target: true,
                    }
                })
                .collect(),
            (CourseTab::All, _) => self
                .courses
                .iter()
                .filter(|(_, course)| matches_course(course))
                .map(|(key, course)| CourseRow {
                    key: key.clone(),
                    name: course.display_name(),
                    course: Some(course.clone()),
                    target: self.current_account().is_some_and(|account| {
                        account.target_courses.iter().any(|target| {
                            target == key || target == &course.name || target == &course.id
                        })
                    }),
                })
                .collect(),
            (CourseTab::Selected, _) => self
                .selected_courses
                .as_ref()
                .into_iter()
                .flat_map(|courses| courses.iter())
                .filter(|(_, course)| matches_course(course))
                .map(|(key, course)| CourseRow {
                    key: key.clone(),
                    name: course.display_name(),
                    course: Some(course.clone()),
                    target: false,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn resolve_course(&self, key: &str) -> Option<Course> {
        if let Some(course) = self.courses.get(key).cloned() {
            return Some(course);
        }
        let mut matches = self
            .courses
            .values()
            .filter(|course| {
                course.name == key
                    || course.id == key
                    || course.code.as_deref().is_some_and(|code| code == key)
            })
            .cloned();
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }
}

fn text_matches_query(value: &str, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    normalize_search_text(value).contains(&normalize_search_text(query))
}

fn course_matches_query(course: &Course, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let schedule = course
        .schedule
        .iter()
        .map(format_course_time)
        .collect::<Vec<_>>()
        .join(" ");
    let fields = [
        course.name.as_str(),
        course.code.as_deref().unwrap_or(""),
        course.class_name.as_deref().unwrap_or(""),
        course
            .details
            .get("teacher")
            .map(String::as_str)
            .unwrap_or(""),
        course
            .details
            .get("schedule_text")
            .map(String::as_str)
            .unwrap_or(""),
        schedule.as_str(),
    ];
    let semantic_order = fields.join(" ");
    let display_order = [
        fields[1], fields[0], fields[2], fields[3], fields[4], fields[5],
    ]
    .join(" ");
    text_matches_query(&semantic_order, query)
        || text_matches_query(&display_order, query)
        || text_matches_query(&course.id, query)
        || query
            .split_whitespace()
            .all(|token| fields.iter().any(|field| text_matches_query(field, token)))
}

fn normalize_search_text(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect()
}

impl App {
    fn panel_border(&self, panel: Focus) -> Color {
        if matches!(self.modal, Modal::None) && self.focus == panel {
            FOCUS_BORDER
        } else {
            BORDER
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.size();
        self.last_area = area;
        frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
        let layout = self.ui_layout(area);
        if layout.tiny {
            let message = Paragraph::new("请放大终端窗口")
                .alignment(Alignment::Center)
                .style(
                    Style::default()
                        .bg(BG)
                        .fg(TEXT)
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_widget(message, centered_line_rect(area));
            return;
        }

        if layout.compact {
            self.render_compact_accounts(frame, layout.accounts, layout.account_list);
        } else {
            self.render_accounts(frame, layout.accounts, layout.account_list);
        }
        self.render_courses(frame, layout);
        self.render_info(frame, layout.info);
        self.render_global_help(frame, layout.footer);
        self.render_modal(frame, area);
    }

    fn render_compact_accounts(&self, frame: &mut Frame<'_>, area: Rect, list_area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        frame.render_widget(
            Block::default()
                .title(border_title("账户"))
                .title_alignment(Alignment::Left)
                .title_style(Style::default().fg(MUTED))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(self.panel_border(Focus::Accounts)))
                .style(Style::default().bg(BG)),
            area,
        );
        self.render_account_rows(frame, area, list_area);
    }

    fn render_accounts(&self, frame: &mut Frame<'_>, area: Rect, list_area: Rect) {
        frame.render_widget(
            Block::default()
                .title(border_title("账户"))
                .title_alignment(Alignment::Left)
                .title_style(Style::default().fg(MUTED))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(self.panel_border(Focus::Accounts)))
                .style(Style::default().bg(BG)),
            area,
        );
        self.render_account_rows(frame, area, list_area);
    }

    fn render_account_rows(&self, frame: &mut Frame<'_>, panel_area: Rect, list_area: Rect) {
        if list_area.width == 0 || list_area.height == 0 {
            return;
        }
        if self.store.accounts.is_empty() {
            let empty = centered_line_rect(list_area);
            frame.render_widget(
                Paragraph::new("无用户")
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(MUTED)),
                empty,
            );
            return;
        }

        let visible_accounts = account_capacity(list_area.height);
        let content_area = list_area;
        let start = self.account_window_start(list_area.height);
        for (offset, account) in self.store.accounts.iter().skip(start).enumerate() {
            let index = start + offset;
            let row =
                Rect {
                    x: content_area.x,
                    y: content_area
                        .y
                        .saturating_add((offset as u16).saturating_mul(ACCOUNT_ITEM_HEIGHT)),
                    width: content_area.width,
                    height: ACCOUNT_ITEM_HEIGHT.min(
                        content_area
                            .y
                            .saturating_add(content_area.height)
                            .saturating_sub(content_area.y.saturating_add(
                                (offset as u16).saturating_mul(ACCOUNT_ITEM_HEIGHT),
                            )),
                    ),
                };
            if row.y >= content_area.y.saturating_add(content_area.height) {
                break;
            }
            let selected = self.account_index == Some(index);
            let row_style = if selected {
                Style::default().bg(PANEL_ALT)
            } else if index % 2 == 1 {
                Style::default().bg(PANEL)
            } else {
                Style::default().bg(BG)
            };
            frame.render_widget(Block::default().style(row_style), row);

            let delete_width = row.width.min(3);
            let id_area = Rect {
                width: row.width.saturating_sub(delete_width),
                ..row
            };
            let delete_area = Rect {
                x: id_area.x.saturating_add(id_area.width),
                width: delete_width,
                y: row.y,
                height: 1.min(row.height),
            };
            let text_area = Rect {
                x: id_area.x,
                width: id_area.width,
                y: row.y,
                height: row.height,
            };
            let name_style = Style::default().fg(TEXT).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
            let id_style = Style::default().fg(if selected { TEXT } else { MUTED });
            let name = account
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty());
            let mut line = vec![Span::raw(" ")];
            line.push(Span::styled(account.id.clone(), id_style));
            if let Some(name) = name {
                line.push(Span::raw(" "));
                line.push(Span::styled(name.to_owned(), name_style));
            }
            frame.render_widget(Paragraph::new(Line::from(line)), text_area);
            if delete_area.width > 0 {
                frame.render_widget(
                    Paragraph::new("×")
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(RED)),
                    delete_area,
                );
            }
        }
        render_scrollbar(
            frame,
            panel_area,
            visible_accounts,
            self.store.accounts.len(),
            start,
        );
    }

    fn render_courses(&self, frame: &mut Frame<'_>, layout: UiLayout) {
        let area = layout.courses;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focused = self.focus == Focus::Courses && matches!(self.modal, Modal::None);
        let active_style = Style::default()
            .fg(if focused { FOCUS_BORDER } else { TEXT })
            .add_modifier(Modifier::BOLD);
        let inactive_style = Style::default().fg(MUTED);
        let tab_style = |tab| {
            if self.tab == tab {
                active_style
            } else {
                inactive_style
            }
        };
        let mut title_spans = vec![
            Span::styled(" 已选 ", tab_style(CourseTab::Selected)),
            Span::styled("/", inactive_style),
            Span::styled(" 候选 ", tab_style(CourseTab::Targets)),
            Span::styled("/", inactive_style),
            Span::styled(" 全部 ", tab_style(CourseTab::All)),
        ];
        if !self.search.is_empty() {
            title_spans.push(Span::styled(format!("· {}", self.search), inactive_style));
        }
        frame.render_widget(
            Block::default()
                .title(Line::from(title_spans))
                .title_alignment(Alignment::Left)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if focused { FOCUS_BORDER } else { BORDER }))
                .style(Style::default().bg(BG)),
            area,
        );
        let rows = self.visible_rows();
        let start = self.course_window_start(layout.course_rows.height);
        if rows.is_empty() {
            let empty = if self.loading {
                "正在加载课程目录"
            } else if self.tab == CourseTab::Selected {
                "暂无已选课程"
            } else if self.tab == CourseTab::Targets {
                "还没有候选课程"
            } else if self.courses.is_empty() {
                "暂无课程目录"
            } else {
                "没有匹配课程"
            };
            frame.render_widget(
                Paragraph::new(empty)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(MUTED)),
                centered_line_rect(layout.course_rows),
            );
        } else {
            for (offset, row_data) in rows.iter().skip(start).enumerate() {
                let index = start + offset;
                let row_area = Rect {
                    x: layout.course_rows.x,
                    y: layout.course_rows.y.saturating_add(offset as u16),
                    width: layout.course_rows.width,
                    height: 1,
                };
                if row_area.y
                    >= layout
                        .course_rows
                        .y
                        .saturating_add(layout.course_rows.height)
                {
                    break;
                }
                let selected = focused && index == self.row;
                let row_bg = if selected {
                    PANEL_ALT
                } else if index % 2 == 1 {
                    PANEL
                } else {
                    BG
                };
                frame.render_widget(
                    Block::default().style(Style::default().bg(row_bg)),
                    row_area,
                );

                let action_width = if matches!(self.tab, CourseTab::Selected) {
                    0
                } else {
                    row_area.width.min(3)
                };
                let left_padding = row_area.width.min(1);
                let content_width = row_area.width.saturating_sub(left_padding + action_width);
                let code_width = if row_area.width >= 60 { 14 } else { 10 }.min(content_width);
                let text_width = content_width.saturating_sub(code_width);
                let content_x = row_area.x.saturating_add(left_padding);
                let code_rect = Rect {
                    x: content_x,
                    width: code_width,
                    ..row_area
                };
                let name_rect = Rect {
                    x: code_rect.x.saturating_add(code_rect.width),
                    width: text_width,
                    ..row_area
                };
                let action_rect = course_remove_area(row_area);
                let name_style = if selected {
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TEXT)
                };
                let code = row_data
                    .course
                    .as_ref()
                    .and_then(|course| course.code.as_deref())
                    .unwrap_or("--");
                frame.render_widget(
                    Paragraph::new(code).style(Style::default().fg(if selected {
                        TEXT
                    } else {
                        MUTED
                    })),
                    code_rect,
                );
                frame.render_widget(
                    Paragraph::new(row_data.name.clone()).style(name_style),
                    name_rect,
                );
                let action = if self.tab == CourseTab::Targets {
                    "×"
                } else if self.tab == CourseTab::All {
                    "+"
                } else {
                    ""
                };
                let action_color = if self.tab == CourseTab::All && row_data.target {
                    MUTED
                } else if selected {
                    if self.tab == CourseTab::Targets {
                        RED
                    } else {
                        GREEN
                    }
                } else if self.tab == CourseTab::Targets {
                    MUTED
                } else {
                    TEAL
                };
                if !action.is_empty() {
                    frame.render_widget(
                        Paragraph::new(action)
                            .alignment(Alignment::Center)
                            .style(Style::default().fg(action_color)),
                        action_rect,
                    );
                }
            }
        }
        render_scrollbar(
            frame,
            area,
            usize::from(layout.course_rows.height.max(1)),
            rows.len(),
            start,
        );
    }

    fn render_info(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focused = self.focus == Focus::Logs && matches!(self.modal, Modal::None);
        if area.height < 3 {
            let text = self
                .logs
                .back()
                .map(Self::display_notice)
                .map(|(message, _)| message)
                .filter(|message| !message.is_empty())
                .map_or_else(|| "日志".to_owned(), |message| format!("日志  {message}"));
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().bg(BG).fg(TEXT))
                    .alignment(Alignment::Left)
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        let viewport = log_capacity(area.height);
        frame.render_widget(
            Paragraph::new(self.log_lines(area.height))
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .title(border_title("日志"))
                        .title_alignment(Alignment::Left)
                        .title_style(Style::default().fg(MUTED))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(if focused {
                            FOCUS_BORDER
                        } else {
                            BORDER
                        }))
                        .style(Style::default().bg(BG))
                        .padding(Padding::horizontal(1)),
                ),
            area,
        );
        render_scrollbar(
            frame,
            area,
            viewport,
            self.logs.len(),
            self.log_window_start(area.height),
        );
    }

    fn log_lines(&self, panel_height: u16) -> Vec<Line<'static>> {
        let viewport = log_capacity(panel_height);
        let start = self.log_window_start(panel_height);
        self.logs
            .iter()
            .skip(start)
            .take(viewport)
            .flat_map(|notice| {
                let (message, _) = Self::display_notice(notice);
                if message.is_empty() {
                    return Vec::new();
                }
                let prefix = notice.prefix();
                let color = notice.color();
                message
                    .lines()
                    .enumerate()
                    .map(|(index, line)| {
                        let marker = if index == 0 { prefix } else { "  " };
                        Line::from(vec![
                            Span::styled(
                                marker,
                                Style::default().fg(color).add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(" "),
                            Span::styled(line.to_owned(), Style::default().fg(TEXT)),
                        ])
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn display_notice(notice: &Notice) -> (String, Color) {
        let text = notice.text.trim();
        if text.is_empty() {
            return (String::new(), MUTED);
        }
        match notice.kind {
            NoticeKind::Error => {
                let message = match text.strip_prefix("__stage__") {
                    Some("登录") => "登录失败，请检查账户或网络后重试".to_owned(),
                    Some("仅支持研究生") => "当前仅支持研究生账户".to_owned(),
                    Some("读取身份") => "读取研究生身份失败，请重新登录".to_owned(),
                    Some("读取学期") => "读取当前学期失败，请稍后重试".to_owned(),
                    Some("读取开放轮次") => "读取研究生选课轮次失败，请稍后重试".to_owned(),
                    Some("没有开放轮次") => "当前没有开放的研究生选课轮次".to_owned(),
                    Some("获取课程目录") => "获取课程目录失败，请检查网络后重试".to_owned(),
                    Some("保存缓存") => "保存课程缓存失败，请检查数据目录权限".to_owned(),
                    _ => compact_notice_detail(text),
                };
                (message, RED)
            }
            _ => (compact_notice_detail(text), notice.color()),
        }
    }

    fn account_panel_help(&self) -> &'static str {
        if self.login_abort.is_some() {
            "Space 取消登录"
        } else {
            "L 登录"
        }
    }

    fn content_panel_help_items(&self) -> Vec<&'static str> {
        let space = match self.tab {
            CourseTab::Targets => Some("Space 移除课程"),
            CourseTab::All => Some("Space 添加课程"),
            CourseTab::Selected => None,
        };
        let priority = (self.tab == CourseTab::Targets).then_some("[/] 调整课程优先级");
        let refresh = if self.refresh_abort.is_some() {
            "U 取消更新"
        } else if self.load_abort.is_some() {
            "U 取消加载"
        } else {
            "U 更新课程"
        };
        [
            Some("Tab 切换课程区"),
            Some("F 筛选课程"),
            space,
            priority,
            Some("D 课程详情"),
            Some(refresh),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    fn global_help_items(&self) -> Vec<&'static str> {
        if !matches!(self.modal, Modal::None) {
            return match &self.modal {
                Modal::ExitConfirm => vec!["←→ 切换按钮", "Enter 选择按钮"],
                Modal::AddAccount { .. } => {
                    vec!["↑↓ 切换字段", "←→ 切换按钮", "Enter 选择按钮"]
                }
                Modal::Search { .. } => {
                    vec!["Backspace 删除", "←→ 切换按钮", "Enter 选择按钮"]
                }
                Modal::Settings { .. } => vec![
                    "↑↓ 切换字段",
                    "←→ 切换按钮",
                    "Tab 切换选项",
                    "Enter 选择按钮",
                ],
                Modal::DeleteAccount { .. } | Modal::ConflictConfirm { .. } => {
                    vec!["←→ 切换按钮", "Enter 选择按钮"]
                }
                Modal::CourseDetail { .. } => vec!["Enter 选择按钮"],
                Modal::None => Vec::new(),
            };
        }
        let mut items = match self.focus {
            Focus::Accounts => vec![self.account_panel_help()],
            Focus::Courses => self.content_panel_help_items(),
            Focus::Logs => vec!["X 清除日志"],
        };
        items.extend([
            if self.selection_stop.is_some() {
                "Enter 停止抢课"
            } else {
                "Enter 开始抢课"
            },
            "C 切换焦点区域",
            "S 设置",
            "Q 退出",
        ]);
        items
    }

    fn render_global_help(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let items = self.global_help_items();
        let lines = help_lines(&items, area.width.saturating_sub(2));
        frame.render_widget(
            Paragraph::new(lines.join("\n"))
                .alignment(Alignment::Center)
                .style(Style::default().bg(PANEL_ALT).fg(MUTED))
                .block(Block::default().padding(Padding::horizontal(1))),
            area,
        );
    }

    fn render_modal(&self, frame: &mut Frame<'_>, area: Rect) {
        if matches!(&self.modal, Modal::None) {
            return;
        }
        let popup = self.modal_rect(area);
        clear_modal_left_overlap(frame, popup, area);
        frame.render_widget(Clear, popup);
        let (border, title, cancel_label, accent) = match &self.modal {
            Modal::ExitConfirm => (RED, "退出程序", "取消", RED),
            Modal::DeleteAccount { .. } => (RED, "删除账户", "取消", RED),
            Modal::Search { .. } => (CYAN, "筛选课程", "取消", CYAN),
            Modal::Settings { .. } => (CYAN, "设置", "取消", CYAN),
            Modal::AddAccount { .. } => (YELLOW, "添加账号", "取消", YELLOW),
            Modal::ConflictConfirm { .. } => (YELLOW, "课程冲突", "取消", YELLOW),
            Modal::CourseDetail { .. } => (CYAN, "课程详情", "返回", CYAN),
            Modal::None => (CYAN, "", "取消", CYAN),
        };
        frame.render_widget(
            Block::default()
                .title(if title.is_empty() {
                    String::new()
                } else {
                    border_title(title)
                })
                .title_alignment(Alignment::Left)
                .title_style(Style::default().fg(border).add_modifier(Modifier::BOLD))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border))
                .style(Style::default().bg(PANEL)),
            popup,
        );

        match &self.modal {
            Modal::ExitConfirm => {
                let message = "确定退出程序？";
                let message_area = modal_message_rect_for(popup, message);
                frame.render_widget(
                    Paragraph::new(message)
                        .alignment(Alignment::Center)
                        .wrap(Wrap { trim: true })
                        .style(Style::default().fg(TEXT)),
                    message_area,
                );
            }
            Modal::AddAccount {
                id,
                password,
                field,
            } => {
                let (id_area, password_area) = modal_field_rects(popup);
                render_input(frame, id_area, "账户", id, *field == AddField::Id, false);
                render_input(
                    frame,
                    password_area,
                    "密码",
                    password,
                    *field == AddField::Password,
                    true,
                );
            }
            Modal::Search { value, .. } => {
                let search_area = modal_search_rect(popup);
                render_input(frame, search_area, "筛选", value, true, false);
            }
            Modal::Settings {
                proxy_mode,
                request_interval,
                field,
                error,
            } => {
                let (proxy_area, interval_area, error_area) = modal_settings_rects(popup);
                render_proxy_selector(
                    frame,
                    proxy_area,
                    *proxy_mode,
                    *field == SettingsField::Proxy,
                );
                render_input(
                    frame,
                    interval_area,
                    "请求间隔（毫秒）",
                    request_interval,
                    *field == SettingsField::Interval,
                    false,
                );
                if let Some(error) = error {
                    frame.render_widget(
                        Paragraph::new(error.as_str())
                            .alignment(Alignment::Left)
                            .wrap(Wrap { trim: true })
                            .style(Style::default().fg(RED)),
                        error_area,
                    );
                }
            }
            Modal::DeleteAccount { index } => {
                let id = self
                    .store
                    .accounts
                    .get(*index)
                    .map(|account| account.id.as_str())
                    .unwrap_or("");
                let message = format!("确定删除账户 {}？", id);
                let message_area = modal_message_rect_for(popup, &message);
                frame.render_widget(
                    Paragraph::new(message)
                        .alignment(Alignment::Center)
                        .wrap(ratatui::widgets::Wrap { trim: true })
                        .style(Style::default().fg(TEXT)),
                    message_area,
                );
            }
            Modal::ConflictConfirm { course, conflicts } => {
                let message = conflict_confirmation_message(course, conflicts);
                let message_area = modal_message_rect_for(popup, &message);
                frame.render_widget(
                    Paragraph::new(message)
                        .alignment(Alignment::Center)
                        .wrap(Wrap { trim: true })
                        .style(Style::default().fg(TEXT)),
                    message_area,
                );
            }
            Modal::CourseDetail { course, conflicts } => {
                let content = course_detail_lines(course, conflicts).join("\n");
                let content_area = Rect {
                    x: popup.x.saturating_add(2),
                    y: popup.y.saturating_add(1),
                    width: popup.width.saturating_sub(4),
                    height: popup.height.saturating_sub(4),
                };
                frame.render_widget(
                    Paragraph::new(content)
                        .alignment(Alignment::Left)
                        .wrap(Wrap { trim: true })
                        .style(Style::default().fg(TEXT)),
                    content_area,
                );
                let return_area = course_detail_button_rect(popup);
                render_centered_control(
                    frame,
                    return_area,
                    Line::from("返回"),
                    Style::default()
                        .fg(BG)
                        .bg(CYAN)
                        .add_modifier(Modifier::BOLD),
                    button_borders(return_area),
                    CYAN,
                    CYAN,
                    1,
                );
            }
            Modal::None => {}
        }
        if !matches!(self.modal, Modal::CourseDetail { .. }) {
            render_modal_buttons(frame, popup, self.modal_button, cancel_label, accent);
        }
    }
}

async fn load_account(
    id: String,
    credentials: Credentials,
    cache_path: PathBuf,
    cached_name: Option<String>,
) -> std::result::Result<LoadedAccount, String> {
    let client = TisClient::new(credentials).map_err(|_| stage_error("登录"))?;
    if let Some(loaded) =
        load_cached_account(&id, &cache_path, client.clone(), cached_name.clone(), "2")
    {
        return Ok(loaded);
    }
    let display_name = if cached_name.is_some() {
        client.login().await.map_err(login_stage_error)?;
        cached_name
    } else {
        client.login_with_name().await.map_err(login_stage_error)?
    };
    load_account_with_client(id, client, cache_path, display_name).await
}

async fn load_account_with_client(
    id: String,
    client: TisClient,
    cache_path: PathBuf,
    display_name: Option<String>,
) -> std::result::Result<LoadedAccount, String> {
    let training_type = client
        .training_type()
        .map_err(|_| stage_error("读取身份"))?;
    if let Some(loaded) = load_cached_account(
        &id,
        &cache_path,
        client.clone(),
        display_name.clone(),
        &training_type,
    ) {
        return Ok(loaded);
    }
    let semester = client
        .semester()
        .await
        .map_err(|_| stage_error("读取学期"))?;
    let selected_courses = client.selected_courses(&semester).await.ok();
    let course_kinds = client
        .course_kinds(&semester)
        .await
        .map_err(|_| stage_error("读取开放轮次"))?;
    if course_kinds.is_empty() {
        return Err(stage_error("没有开放轮次"));
    }
    let courses = client
        .all_courses(&semester, &course_kinds)
        .await
        .map_err(|_| stage_error("获取课程目录"))?;
    let mut cache = CacheFile::new(
        id,
        training_type,
        semester.clone(),
        course_kinds.clone(),
        courses.clone(),
    );
    cache.selected_courses = selected_courses.clone();
    cache::save(&cache_path, &cache).map_err(|_| stage_error("保存缓存"))?;
    Ok(LoadedAccount {
        client,
        semester,
        course_kinds,
        courses,
        selected_courses,
        cache_state: CacheState::Refreshed,
        display_name,
    })
}

fn load_cached_account(
    id: &str,
    cache_path: &PathBuf,
    client: TisClient,
    display_name: Option<String>,
    training_type: &str,
) -> Option<LoadedAccount> {
    let cache = cache::load(cache_path).ok().flatten()?;
    if !cache.usable_offline(id, training_type) {
        return None;
    }
    Some(LoadedAccount {
        client,
        semester: cache.semester,
        course_kinds: cache.course_kinds,
        courses: cache.courses,
        selected_courses: cache.selected_courses,
        cache_state: CacheState::Cached,
        display_name,
    })
}

async fn submit_action_loop(
    client: TisClient,
    semester: Semester,
    mut courses: Vec<(String, Course)>,
    stop: Arc<AtomicBool>,
    account_id: String,
    sender: UnboundedSender<UiMessage>,
) -> std::result::Result<(), String> {
    let mut cursor = 0usize;
    loop {
        if stop.load(Ordering::Acquire) || courses.is_empty() {
            return Ok(());
        }
        if cursor >= courses.len() {
            cursor = 0;
        }
        let (key, course) = courses[cursor].clone();
        let course_name = course.display_name();
        let event_sender: SelectionEventHandler = {
            let sender = sender.clone();
            let account_id = account_id.clone();
            let key = key.clone();
            let course_name = course_name.clone();
            Arc::new(move |event| {
                let (succeeded, message) = match event {
                    SelectionEvent::RequestStarted => return,
                    SelectionEvent::Response(result) => {
                        action_message_for_result(&course_name, &key, result)
                    }
                    SelectionEvent::RequestFailed(error) => (
                        None,
                        ActionMessage {
                            kind: NoticeKind::Failure,
                            text: format!(
                                "{}：请求失败：{}",
                                course_name,
                                compact_notice_detail(&error)
                            ),
                        },
                    ),
                    SelectionEvent::AuthExpired => (
                        None,
                        ActionMessage {
                            kind: NoticeKind::Warning,
                            text: "登录态已失效，正在重新获取登录态".to_owned(),
                        },
                    ),
                    SelectionEvent::AuthRetryFailed(error) => (
                        None,
                        ActionMessage {
                            kind: NoticeKind::Failure,
                            text: format!("登录态恢复失败：{}", compact_notice_detail(&error)),
                        },
                    ),
                    SelectionEvent::AuthRecovered => (
                        None,
                        ActionMessage {
                            kind: NoticeKind::Success,
                            text: "登录态已恢复，继续抢课".to_owned(),
                        },
                    ),
                };
                send_action_progress(&sender, &account_id, succeeded, message);
            })
        };
        let result = client
            .select_direct_with_stop_report(&semester, &course, Some(&stop), Some(event_sender))
            .await;
        let stopping = stop.load(Ordering::Acquire);
        match result {
            Ok(SelectionResult::Success(_)) => {
                courses.remove(cursor);
                if stopping || courses.is_empty() {
                    return Ok(());
                }
            }
            Ok(SelectionResult::NotStarted(_)) => {
                if stopping {
                    return Ok(());
                }
            }
            Ok(SelectionResult::Skipped(_))
            | Ok(SelectionResult::RateLimited(_))
            | Ok(SelectionResult::Unknown(_)) => {
                if stopping {
                    return Ok(());
                }
                cursor = cursor.saturating_add(1);
            }
            Err(_) => {
                if stopping {
                    return Ok(());
                }
                cursor = cursor.saturating_add(1);
            }
        }
        if !courses.is_empty() {
            if cursor >= courses.len() {
                cursor = 0;
            }
            if wait_for_action_delay(&stop, Duration::from_millis(200)).await {
                return Ok(());
            }
        }
    }
}

fn action_message_for_result(
    course_name: &str,
    key: &str,
    result: SelectionResult,
) -> (Option<String>, ActionMessage) {
    match result {
        SelectionResult::Success(_) => (
            Some(key.to_owned()),
            ActionMessage {
                kind: NoticeKind::Success,
                text: format!("{}：选课成功", course_name),
            },
        ),
        SelectionResult::NotStarted(message)
        | SelectionResult::Skipped(message)
        | SelectionResult::Unknown(message) => (
            None,
            ActionMessage {
                kind: NoticeKind::Failure,
                text: selection_failure_text(course_name, &message),
            },
        ),
        SelectionResult::RateLimited(_) => (
            None,
            ActionMessage {
                kind: NoticeKind::Failure,
                text: format!("{}：请求频率过高", course_name),
            },
        ),
    }
}

fn send_action_progress(
    sender: &UnboundedSender<UiMessage>,
    account_id: &str,
    succeeded: Option<String>,
    message: ActionMessage,
) {
    let _ = sender.send(UiMessage::ActionProgress {
        account_id: account_id.to_owned(),
        succeeded,
        message,
    });
}

async fn wait_for_action_delay(stop: &AtomicBool, delay: Duration) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return stop.load(Ordering::Acquire);
        }
        sleep(remaining.min(Duration::from_millis(50))).await;
    }
}

fn selection_failure_text(course_name: &str, message: &str) -> String {
    let detail = compact_notice_detail(message);
    if detail.is_empty() {
        format!("{course_name}：服务器未返回失败原因")
    } else {
        format!("{course_name}：{detail}")
    }
}

fn compact_notice_detail(message: &str) -> String {
    const MAX_CHARS: usize = 180;
    let normalized = message
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let mut chars = normalized.chars();
    let short = chars.by_ref().take(MAX_CHARS + 1).collect::<String>();
    if short.chars().count() <= MAX_CHARS {
        short
    } else {
        let body = short
            .chars()
            .take(MAX_CHARS.saturating_sub(1))
            .collect::<String>();
        format!("{body}…")
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    // Modal widths are kept even whenever the terminal has enough room.  This
    // makes the centered button groups (whose total width is also even) have
    // equal left and right margins.  On an extremely narrow terminal there
    // may be only one usable column; in that case retaining a one-column
    // popup is preferable to underflowing the rectangle.
    let max_width = area.width.saturating_sub(2);
    let mut width = width.min(max_width).max(1);
    if width > 1 && width % 2 != 0 {
        width = width.saturating_sub(1).max(1);
    }
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
        y: area
            .y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    }
}

fn even_modal_width(value: u16, min: u16, max: u16) -> u16 {
    let min_even = if min % 2 == 0 {
        min
    } else {
        min.saturating_add(1)
    };
    let max_even = if max % 2 == 0 {
        max
    } else {
        max.saturating_sub(1)
    };
    let lower = min_even.min(max_even);
    let upper = max_even.max(lower);
    let mut width = value.clamp(lower, upper);
    if width % 2 != 0 {
        width = if width < upper {
            width.saturating_add(1)
        } else {
            width.saturating_sub(1).max(lower)
        };
    }
    width
}

fn footer_height_for(width: u16, height: u16, help: &[&str]) -> u16 {
    if width == 0 || height == 0 {
        return 0;
    }
    let content_width = width.saturating_sub(2).max(1);
    let lines = help_lines(help, content_width).len().max(1);
    u16::try_from(lines).unwrap_or(u16::MAX).min(height).max(1)
}

fn help_lines(items: &[&str], width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let separator = "   ";
    let separator_width = Line::from(separator).width();
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for item in items.iter().copied().filter(|item| !item.is_empty()) {
        let item_width = Line::from(item).width();
        if current.is_empty() {
            current.push_str(item);
            current_width = item_width;
        } else if current_width + separator_width + item_width <= width {
            current.push_str(separator);
            current.push_str(item);
            current_width += separator_width + item_width;
        } else {
            lines.push(current);
            current = item.to_owned();
            current_width = item_width;
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrapped_line_count(text: &str, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.lines()
        .map(|line| {
            let mut line_count = 1usize;
            let mut used = 0usize;
            for word in line.split_whitespace() {
                let word_width = Line::from(word).width().max(1);
                if word_width > width {
                    if used > 0 {
                        line_count += 1;
                    }
                    line_count += (word_width - 1) / width;
                    used = word_width % width;
                    continue;
                }
                let separator = usize::from(used > 0);
                if used + separator + word_width <= width {
                    used += separator + word_width;
                } else {
                    line_count += 1;
                    used = word_width;
                }
            }
            line_count
        })
        .sum::<usize>()
        .max(1)
}

fn border_title(title: &str) -> String {
    format!(" {title} ")
}

fn centered_line_rect(area: Rect) -> Rect {
    let height = 1.min(area.height);
    Rect {
        y: area
            .y
            .saturating_add(area.height.saturating_sub(height) / 2),
        height,
        ..area
    }
}

/// Clear a possible wide-character continuation immediately to the left of a
/// popup before drawing its border.  Ratatui's terminal diff skips the cell
/// following a wide glyph; when that glyph belongs to the underlying panel,
/// the popup's left border can therefore be omitted from the update even
/// though it is present in the current buffer.  Resetting only an affected
/// neighbour preserves ordinary text while making the border diffable.
fn clear_modal_left_overlap(frame: &mut Frame<'_>, popup: Rect, area: Rect) {
    if popup.x <= area.x || popup.height == 0 {
        return;
    }
    let left = popup.x - 1;
    let bottom = popup.y.saturating_add(popup.height).min(area.bottom());
    let buffer = frame.buffer_mut();
    for y in popup.y.min(area.bottom())..bottom {
        let cell = buffer.get_mut(left, y);
        let wide_or_continuation = cell.skip || Line::from(cell.symbol()).width() > 1;
        if wide_or_continuation {
            cell.reset();
            cell.set_bg(PANEL);
        }
    }
}

fn modal_message_rect(popup: Rect) -> Rect {
    let (buttons, _) = modal_button_rects(popup);
    let top = popup.y.saturating_add(2);
    let available = buttons.y.saturating_sub(top).saturating_sub(1);
    if available == 0 {
        return Rect::default();
    }
    Rect {
        x: popup.x.saturating_add(2),
        y: top,
        width: popup.width.saturating_sub(4),
        height: available.max(1),
    }
}

fn confirmation_modal_height(lines: usize) -> u16 {
    let lines = u16::try_from(lines.max(1)).unwrap_or(u16::MAX.saturating_sub(7));
    let compact = lines.saturating_add(5);
    if compact >= 12 {
        lines.saturating_add(7)
    } else {
        compact
    }
}

fn modal_message_rect_for(popup: Rect, message: &str) -> Rect {
    let available = modal_message_rect(popup);
    if available.width == 0 || available.height == 0 {
        return available;
    }
    let lines = wrapped_line_count(message, available.width)
        .max(1)
        .min(usize::from(available.height));
    Rect {
        y: available.y,
        height: u16::try_from(lines).unwrap_or(available.height),
        ..available
    }
}

fn point_in_rect(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn course_tab_at(area: Rect, x: u16, y: u16) -> Option<CourseTab> {
    if area.width < 3 || y != area.y {
        return None;
    }
    let mut cursor = area.x.saturating_add(1);
    for label in [" 已选 ", "/", " 候选 ", "/", " 全部 "] {
        let width = u16::try_from(Line::from(label).width()).unwrap_or(u16::MAX);
        if x >= cursor && x < cursor.saturating_add(width) {
            return match label {
                " 已选 " => Some(CourseTab::Selected),
                " 候选 " => Some(CourseTab::Targets),
                " 全部 " => Some(CourseTab::All),
                _ => None,
            };
        }
        cursor = cursor.saturating_add(width);
        if cursor >= area.x.saturating_add(area.width) {
            break;
        }
    }
    None
}

fn conflict_confirmation_message(course: &Course, conflicts: &[String]) -> String {
    format!(
        "{} 与以下课程时间冲突：\n{}\n\n是否仍加入候选？",
        course.display_name(),
        conflicts.join("\n")
    )
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.trim().is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn courses_overlap(left: &Course, right: &Course) -> bool {
    let left_times = effective_course_schedule(left);
    let right_times = effective_course_schedule(right);
    left_times.iter().any(|left| {
        right_times
            .iter()
            .any(|right| left.overlaps(right) == Some(true))
    })
}

fn effective_course_schedule(course: &Course) -> Vec<CourseTime> {
    if !course.schedule.is_empty() {
        return course.schedule.clone();
    }
    course
        .details
        .get("schedule_text")
        .into_iter()
        .flat_map(|text| text.lines())
        .filter_map(schedule_time_from_detail_line)
        .collect()
}

fn schedule_time_from_detail_line(line: &str) -> Option<CourseTime> {
    let weekday = [
        (("星期一", "周一"), 1),
        (("星期二", "周二"), 2),
        (("星期三", "周三"), 3),
        (("星期四", "周四"), 4),
        (("星期五", "周五"), 5),
        (("星期六", "周六"), 6),
        (("星期日", "周日"), 7),
        (("星期天", "周天"), 7),
    ]
    .into_iter()
    .find_map(|((long, short), day)| {
        line.contains(long)
            .then_some(day)
            .or_else(|| line.contains(short).then_some(day))
    });
    let sections = line
        .find('第')
        .and_then(|start| {
            let rest = &line[start + '第'.len_utf8()..];
            rest.find('节').map(|end| number_runs(&rest[..end]))
        })
        .unwrap_or_default();
    let (start_section, end_section) = match sections.as_slice() {
        [start, end, ..] => (Some(*start), Some(*end)),
        [section] => (Some(*section), Some(*section)),
        _ => (None, None),
    };
    let weeks = line
        .find(|character| matches!(character, ',' | '，'))
        .map(|end| line[..end].trim().to_owned())
        .filter(|value| !value.is_empty());
    let time = CourseTime {
        weekday,
        start_section,
        end_section,
        weeks,
    };
    (time.weekday.is_some()
        || time.start_section.is_some()
        || time.end_section.is_some()
        || time.weeks.is_some())
    .then_some(time)
}

fn number_runs(value: &str) -> Vec<u16> {
    let mut numbers = Vec::new();
    let mut digits = String::new();
    for character in value.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
        } else if !digits.is_empty() {
            if let Ok(number) = digits.parse::<u16>() {
                numbers.push(number);
            }
            digits.clear();
        }
    }
    if let Ok(number) = digits.parse::<u16>() {
        numbers.push(number);
    }
    numbers
}

fn course_detail_lines(course: &Course, conflicts: &[String]) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(detail_line("课程", &course.name));
    if let Some(code) = course
        .code
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(detail_line("代码", code));
    }
    if let Some(class_name) = course
        .class_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(detail_line("教学班", class_name));
    }
    for (label, key) in [
        ("课程性质", "course_nature"),
        ("课程类别", "course_category"),
        ("授课语言", "teaching_language"),
        ("计分方式", "grading_method"),
        ("学分", "credits"),
        ("学时", "hours"),
        ("开课学院", "opening_college"),
        ("校区", "campus"),
    ] {
        if let Some(value) = course
            .details
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(detail_line(label, value));
        }
    }
    lines.push(String::new());

    let teacher = course
        .details
        .get("teacher")
        .map(String::as_str)
        .unwrap_or("");
    lines.push(detail_line("上课教师", teacher));

    let schedule_text = course
        .details
        .get("schedule_text")
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty());
    let mut rendered_schedule = false;
    if let Some(schedule_text) = schedule_text {
        let schedule_lines = schedule_text
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        for (index, value) in schedule_lines.into_iter().enumerate() {
            if index == 0 {
                lines.push(detail_line("上课时间", value));
            } else {
                lines.push(detail_continuation(value));
            }
            rendered_schedule = true;
        }
    }
    if !rendered_schedule && course.schedule.is_empty() {
        lines.push(detail_line("上课时间", "未提供"));
    } else if !rendered_schedule {
        for (index, slot) in course.schedule.iter().enumerate() {
            let value = format_course_time(slot);
            if index == 0 {
                lines.push(detail_line("上课时间", &value));
            } else {
                lines.push(detail_continuation(&value));
            }
        }
    }

    let capacity = course.details.get("capacity").map(String::as_str);
    let selected_count = course.details.get("selected_count").map(String::as_str);
    let capacity = capacity.unwrap_or("未提供");
    let selected_count = selected_count.unwrap_or("未提供");
    lines.push(detail_line(
        "容量/已选",
        &format!("{capacity} / {selected_count}"),
    ));

    if conflicts.is_empty() {
        lines.push(detail_line("冲突课程", "无"));
    } else {
        for (index, conflict) in conflicts.iter().enumerate() {
            if index == 0 {
                lines.push(detail_line("冲突课程", conflict));
            } else {
                lines.push(detail_continuation(conflict));
            }
        }
    }
    lines
}

const DETAIL_LABEL_WIDTH: usize = 9;

fn detail_line(label: &str, value: &str) -> String {
    let padding = DETAIL_LABEL_WIDTH.saturating_sub(Line::from(label).width());
    format!("{label}{}：{value}", " ".repeat(padding))
}

fn detail_continuation(value: &str) -> String {
    let value_column = DETAIL_LABEL_WIDTH + Line::from("：").width();
    format!("{}{}", " ".repeat(value_column), value)
}

fn format_course_time(slot: &CourseTime) -> String {
    let weekday = slot
        .weekday
        .map(|day| match day {
            1 => "周一",
            2 => "周二",
            3 => "周三",
            4 => "周四",
            5 => "周五",
            6 => "周六",
            7 => "周日",
            _ => "星期未知",
        })
        .unwrap_or("星期未知");
    let section = match (slot.start_section, slot.end_section) {
        (Some(start), Some(end)) if start == end => format!("{start}节"),
        (Some(start), Some(end)) => format!("{start}-{end}节"),
        (Some(start), None) => format!("{start}节"),
        (None, Some(end)) => format!("至{end}节"),
        (None, None) => "节次未知".to_owned(),
    };
    match slot
        .weeks
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(weeks) => format!("{weekday} {section} · {weeks}"),
        None => format!("{weekday} {section}"),
    }
}

fn course_remove_area(row: Rect) -> Rect {
    let width = row.width.min(3);
    Rect {
        x: row.x.saturating_add(row.width.saturating_sub(width)),
        width,
        ..row
    }
}

fn point_in_field(area: Rect, x: u16, y: u16) -> bool {
    if point_in_rect(area, x, y) {
        return true;
    }
    let label_area = Rect {
        y: area.y.saturating_sub(1),
        height: 1,
        ..area
    };
    point_in_rect(label_area, x, y)
}

fn centered_control_block(
    area: Rect,
    borders: Borders,
    border_color: Color,
    background: Color,
    horizontal_padding: u16,
) -> Block<'static> {
    let top_border = u16::from(borders.contains(Borders::TOP));
    let bottom_border = u16::from(borders.contains(Borders::BOTTOM));
    let left_border = u16::from(borders.contains(Borders::LEFT));
    let right_border = u16::from(borders.contains(Borders::RIGHT));
    let inner_height = area.height.saturating_sub(top_border + bottom_border);
    let free_height = inner_height.saturating_sub(1);
    let top_padding = free_height.div_ceil(2);
    let bottom_padding = free_height.saturating_sub(top_padding);
    let inner_width = area.width.saturating_sub(left_border + right_border);
    let horizontal_padding = horizontal_padding.min(inner_width.saturating_sub(1) / 2);
    Block::default()
        .borders(borders)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(background))
        .padding(Padding::new(
            horizontal_padding,
            horizontal_padding,
            top_padding,
            bottom_padding,
        ))
}

fn button_borders(area: Rect) -> Borders {
    if area.height >= 3 {
        Borders::ALL
    } else if area.height == 2 {
        Borders::BOTTOM
    } else {
        Borders::NONE
    }
}

#[allow(clippy::too_many_arguments)]
fn render_centered_control<'a>(
    frame: &mut Frame<'_>,
    area: Rect,
    content: Line<'a>,
    text_style: Style,
    borders: Borders,
    border_color: Color,
    background: Color,
    horizontal_padding: u16,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(content)
            .alignment(Alignment::Center)
            .style(text_style)
            .block(centered_control_block(
                area,
                borders,
                border_color,
                background,
                horizontal_padding,
            )),
        area,
    );
}

fn account_capacity(height: u16) -> usize {
    usize::from((height / ACCOUNT_ITEM_HEIGHT).max(1))
}

fn log_capacity(panel_height: u16) -> usize {
    usize::from(panel_height.saturating_sub(2).max(1))
}

fn scrollbar_geometry(
    panel: Rect,
    visible: usize,
    total: usize,
    start: usize,
) -> Option<(Rect, Rect)> {
    if panel.width == 0 || panel.height < 3 || visible == 0 || total <= visible {
        return None;
    }
    let track = Rect {
        x: panel.x.saturating_add(panel.width.saturating_sub(1)),
        y: panel.y.saturating_add(1),
        width: 1,
        height: panel.height.saturating_sub(2),
    };
    if track.height == 0 {
        return None;
    }
    let thumb_height = usize::from(track.height)
        .saturating_mul(visible)
        .div_ceil(total)
        .max(1)
        .min(usize::from(track.height));
    let max_start = total.saturating_sub(visible);
    let max_offset = usize::from(track.height).saturating_sub(thumb_height);
    let thumb_offset = if max_start == 0 {
        0
    } else {
        max_offset.saturating_mul(start.min(max_start)) / max_start
    };
    let thumb = Rect {
        y: track.y.saturating_add(thumb_offset as u16),
        height: thumb_height as u16,
        ..track
    };
    Some((track, thumb))
}

fn render_scrollbar(
    frame: &mut Frame<'_>,
    panel: Rect,
    visible: usize,
    total: usize,
    start: usize,
) {
    let Some((track, thumb)) = scrollbar_geometry(panel, visible, total, start) else {
        return;
    };
    frame.render_widget(Block::default().style(Style::default().bg(PANEL)), track);
    frame.render_widget(Block::default().style(Style::default().bg(MUTED)), thumb);
}

fn modal_button_rects(popup: Rect) -> (Rect, Rect) {
    const BUTTON_GAP: u16 = 4;
    let width = 10.min(popup.width.saturating_sub(BUTTON_GAP) / 2).max(1);
    let gap = BUTTON_GAP.min(popup.width.saturating_sub(width.saturating_mul(2)));
    let total = width * 2 + gap;
    let x = popup
        .x
        .saturating_add(popup.width.saturating_sub(total) / 2);
    let height = if popup.height >= 12 { 3 } else { 1 };
    let y = popup
        .y
        .saturating_add(popup.height.saturating_sub(height + 1));
    (
        Rect {
            x,
            y,
            width,
            height,
        },
        Rect {
            x: x.saturating_add(width + gap),
            y,
            width,
            height,
        },
    )
}

fn course_detail_button_rect(popup: Rect) -> Rect {
    let width = 10.min(popup.width.saturating_sub(4)).max(1);
    let height = 1.min(popup.height);
    let y = popup
        .y
        .saturating_add(popup.height.saturating_sub(height + 1));
    Rect {
        x: popup
            .x
            .saturating_add(popup.width.saturating_sub(width) / 2),
        y,
        width,
        height,
    }
}

fn modal_field_rects(popup: Rect) -> (Rect, Rect) {
    let inner_x = popup.x.saturating_add(3);
    let inner_width = popup.width.saturating_sub(6);
    let field_width = inner_width;
    let field_height = 1.min(popup.height);
    let id_y = popup.y.saturating_add(3);
    let password_y = id_y.saturating_add(3);
    (
        Rect {
            x: inner_x,
            y: id_y,
            width: field_width,
            height: field_height,
        },
        Rect {
            x: inner_x,
            y: password_y,
            width: field_width,
            height: field_height,
        },
    )
}

fn modal_search_rect(popup: Rect) -> Rect {
    Rect {
        x: popup.x.saturating_add(3),
        y: popup.y.saturating_add(3),
        width: popup.width.saturating_sub(6),
        height: 1.min(popup.height),
    }
}

fn modal_settings_rects(popup: Rect) -> (Rect, Rect, Rect) {
    let width = popup.width.saturating_sub(6);
    let x = popup.x.saturating_add(3);
    (
        Rect {
            x,
            y: popup.y.saturating_add(3),
            width,
            height: 1.min(popup.height),
        },
        Rect {
            x,
            y: popup.y.saturating_add(6),
            width,
            height: 1.min(popup.height),
        },
        Rect {
            x,
            y: popup.y.saturating_add(7),
            width,
            height: 1.min(popup.height),
        },
    )
}

fn render_proxy_selector(frame: &mut Frame<'_>, area: Rect, proxy_mode: ProxyMode, active: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let label_area = Rect {
        y: area.y.saturating_sub(1),
        height: 1,
        ..area
    };
    frame.render_widget(
        Paragraph::new("代理模式")
            .style(Style::default().fg(TEXT))
            .alignment(Alignment::Left),
        label_area,
    );
    let style = if active {
        Style::default().fg(TEXT).bg(INPUT_BG)
    } else {
        Style::default().fg(TEXT).bg(PANEL_ALT)
    };
    let inner_width = area.width.saturating_sub(2);
    let label_width = Line::from(proxy_mode.label()).width();
    let arrow_width = Line::from("▼").width();
    let right_padding = 1;
    let spaces = usize::from(inner_width)
        .saturating_sub(label_width)
        .saturating_sub(arrow_width)
        .saturating_sub(right_padding)
        .max(1);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(proxy_mode.label(), Style::default().fg(TEXT)),
            Span::raw(" ".repeat(spaces)),
            Span::styled("▼", Style::default().fg(Color::White)),
            Span::raw(" ".repeat(right_padding)),
        ]))
        .alignment(Alignment::Left)
        .style(style)
        .block(Block::default().padding(Padding::horizontal(1))),
        area,
    );
}

fn render_input(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    value: &str,
    active: bool,
    secret: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let label_area = Rect {
        y: area.y.saturating_sub(1),
        height: 1,
        ..area
    };
    frame.render_widget(
        Paragraph::new(label)
            .style(Style::default().fg(TEXT))
            .alignment(Alignment::Left),
        label_area,
    );
    let text = if secret {
        "*".repeat(value.chars().count())
    } else {
        value.to_owned()
    };
    let display = if active { text.clone() } else { text };
    let content = if active {
        Line::from(vec![
            Span::styled(display, Style::default().fg(INPUT_FG)),
            Span::styled("│", Style::default().fg(CURSOR)),
        ])
    } else {
        Line::from(Span::styled(display, Style::default().fg(INPUT_FG)))
    };
    frame.render_widget(
        Paragraph::new(content).alignment(Alignment::Left).block(
            Block::default()
                .borders(Borders::NONE)
                .style(Style::default().bg(INPUT_BG))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

fn render_modal_buttons(
    frame: &mut Frame<'_>,
    popup: Rect,
    selected: ModalButton,
    cancel_label: &str,
    accent: Color,
) {
    let (confirm, cancel) = modal_button_rects(popup);
    let button = |active: bool| {
        if active {
            Style::default()
                .fg(BG)
                .bg(accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT).bg(PANEL_ALT)
        }
        .add_modifier(Modifier::BOLD)
    };
    let confirm_active = selected == ModalButton::Confirm;
    render_centered_control(
        frame,
        confirm,
        Line::from("确定"),
        button(confirm_active),
        button_borders(confirm),
        if confirm_active { accent } else { BORDER },
        if confirm_active { accent } else { PANEL_ALT },
        1,
    );
    let cancel_active = selected == ModalButton::Cancel;
    render_centered_control(
        frame,
        cancel,
        Line::from(cancel_label.to_owned()),
        button(cancel_active),
        button_borders(cancel),
        if cancel_active { accent } else { BORDER },
        if cancel_active { accent } else { PANEL_ALT },
        1,
    );
}

fn error_text(error: &AppError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str("; ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn stage_error(stage: &str) -> String {
    format!("__stage__{stage}")
}

fn login_stage_error(error: AppError) -> String {
    match error {
        AppError::Auth(message)
            if message == "this application only supports graduate student accounts" =>
        {
            stage_error("仅支持研究生")
        }
        _ => stage_error("登录"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, AccountStore};
    use ratatui::{backend::TestBackend, Terminal};

    fn sample_app() -> App {
        App {
            store: AccountStore {
                accounts: vec![Account {
                    id: "123".into(),
                    target_courses: vec!["Missing".into(), "1".into()],
                    name: None,
                }],
            },
            settings: Settings::default(),
            account_index: Some(0),
            account_cursor: 0,
            account_scroll: 0,
            focus: Focus::Courses,
            tab: CourseTab::Targets,
            row: 0,
            course_scroll: 0,
            search: String::new(),
            courses: BTreeMap::from([(
                "1".into(),
                Course {
                    id: "1".into(),
                    name: "Present".into(),
                    kind: "bxxk".into(),
                    code: Some("CS101".into()),
                    class_name: None,
                    xkms: None,
                    xkxs: None,
                    jfxs: None,
                    schedule: Vec::new(),
                    details: BTreeMap::new(),
                },
            )]),
            selected_courses: None,
            course_kinds: Vec::new(),
            semester: None,
            client: None,
            cache_state: CacheState::Empty,
            loading: false,
            login_abort: None,
            login_request_id: 0,
            selection_stop: None,
            load_abort: None,
            load_request_id: 0,
            refresh_abort: None,
            refresh_request_id: 0,
            notice: Notice::info("test"),
            logs: VecDeque::new(),
            log_scroll: 0,
            modal: Modal::None,
            modal_button: ModalButton::Confirm,
            scroll_drag: None,
            course_mouse_down: None,
            course_mouse_dragged: false,
            last_course_click: None,
            last_area: Rect::default(),
            should_quit: false,
        }
    }

    #[test]
    fn target_rows_preserve_account_order_and_mark_missing_courses() {
        let app = sample_app();
        let rows = app.visible_rows();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].course.is_none());
        assert!(rows[1].course.is_some());
    }

    #[test]
    fn enter_starts_selection_without_confirmation() {
        let mut app = sample_app();
        let (tx, _rx) = mpsc::unbounded_channel();
        app.client = None;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
        assert!(app.notice.text.contains("尚未加载"));
    }

    #[test]
    fn enter_on_all_tab_still_starts_selection() {
        let mut app = sample_app();
        app.tab = CourseTab::All;
        let (tx, _rx) = mpsc::unbounded_channel();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
        assert!(app.notice.text.contains("尚未加载"));
        assert!(!app.loading);
    }

    #[test]
    fn standalone_cart_submit_key_is_inert() {
        let mut app = sample_app();
        let (tx, _rx) = mpsc::unbounded_channel();
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.notice.text, "test");
        assert!(!app.loading);
    }

    #[test]
    fn all_space_adds_without_confirmation() {
        let mut app = sample_app();
        app.tab = CourseTab::All;
        app.store.accounts[0].target_courses = vec!["Missing".into()];
        app.row = 0;
        app.course_space();
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn target_space_removes_without_confirmation() {
        let mut app = sample_app();
        app.row = 1;
        app.course_space();
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.store.accounts[0].target_courses, vec!["Missing"]);
    }

    #[test]
    fn enter_action_is_global_and_priority_help_is_target_only() {
        let app = sample_app();
        let help = app.content_panel_help_items().join("  ");
        assert!(!help.contains("Enter 开始抢课"));
        assert!(help.contains("[/] 调整课程优先级"));
        assert!(app.global_help_items().contains(&"Enter 开始抢课"));
        let mut all = app;
        all.tab = CourseTab::All;
        let help = all.content_panel_help_items().join("  ");
        assert!(!help.contains("[/] 调整课程优先级"));
    }

    #[test]
    fn tab_switches_course_lists_and_c_switches_focus() {
        let mut app = sample_app();
        let (tx, _rx) = mpsc::unbounded_channel();
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(app.tab, CourseTab::All);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &tx);
        assert_eq!(app.focus, Focus::Logs);
    }

    #[test]
    fn empty_account_state_uses_l_for_add_account() {
        let mut app = sample_app();
        let (tx, _rx) = mpsc::unbounded_channel();
        app.store.accounts.clear();
        app.account_index = None;
        app.account_cursor = 0;
        app.account_scroll = 0;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::AddAccount { .. }));
    }

    #[test]
    fn account_mutations_are_blocked_during_network_work() {
        let mut app = sample_app();
        app.loading = true;
        app.open_add_account();
        assert!(matches!(app.modal, Modal::None));
        app.open_delete_account_at(0);
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.account_index, Some(0));
    }

    #[test]
    fn renders_main_panels_with_terminal_backend() {
        let mut app = sample_app();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("账 "));
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "候"));
        assert!(content.contains("×"));
        assert!(!content.contains("搜索课程名或课程号"));
    }

    #[test]
    fn priority_shortcuts_reorder_persisted_targets() {
        let mut app = sample_app();
        app.row = 1;
        app.move_target_priority(-1);
        assert_eq!(app.store.accounts[0].target_courses, vec!["1", "Missing"]);
    }

    #[test]
    fn only_successful_courses_are_removed_from_candidates() {
        let mut app = sample_app();
        app.remove_succeeded_targets(&["1".into()]);
        assert_eq!(app.store.accounts[0].target_courses, vec!["Missing"]);
    }

    #[test]
    fn renders_compact_footer_without_layout_error() {
        let mut app = sample_app();
        let backend = TestBackend::new(77, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "账"));
    }

    #[test]
    fn modal_borders_survive_wide_text_underlay() {
        fn course() -> Course {
            Course {
                id: "c".into(),
                name: "测试课程".into(),
                kind: "x".into(),
                code: Some("C".into()),
                class_name: Some("1班".into()),
                xkms: None,
                xkxs: None,
                jfxs: None,
                schedule: Vec::new(),
                details: BTreeMap::new(),
            }
        }
        let modal_cases = || {
            vec![
                Modal::ExitConfirm,
                Modal::AddAccount {
                    id: String::new(),
                    password: String::new(),
                    field: AddField::Id,
                },
                Modal::Search {
                    value: String::new(),
                },
                Modal::Settings {
                    proxy_mode: ProxyMode::System,
                    request_interval: "1000".into(),
                    field: SettingsField::Proxy,
                    error: None,
                },
                Modal::DeleteAccount { index: 0 },
                Modal::ConflictConfirm {
                    course: course(),
                    conflicts: Vec::new(),
                },
                Modal::CourseDetail {
                    course: course(),
                    conflicts: Vec::new(),
                },
            ]
        };

        for (width, height) in [(50, 15), (80, 20), (100, 24)] {
            for modal in modal_cases() {
                let mut app = sample_app();
                app.modal = modal;
                let area = Rect::new(0, 0, width, height);
                let popup = app.modal_rect(area);
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();
                for _ in 0..2 {
                    terminal
                        .draw(|frame| {
                            // Put a CJK glyph immediately before the popup so
                            // the test exercises ratatui's wide-cell diff.
                            if popup.x > area.x {
                                for y in popup.y..popup.y.saturating_add(popup.height) {
                                    frame.render_widget(
                                        Paragraph::new("课"),
                                        Rect::new(popup.x - 1, y, 2, 1),
                                    );
                                }
                            }
                            app.render_modal(frame, area);
                        })
                        .unwrap();
                }
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer.get(popup.x, popup.y).symbol(), "┌");
                assert_eq!(buffer.get(popup.x + popup.width - 1, popup.y).symbol(), "┐");
                assert_eq!(
                    buffer.get(popup.x, popup.y + popup.height - 1).symbol(),
                    "└"
                );
                assert_eq!(
                    buffer
                        .get(popup.x + popup.width - 1, popup.y + popup.height - 1)
                        .symbol(),
                    "┘"
                );
                for y in popup.y + 1..popup.y + popup.height - 1 {
                    assert_eq!(buffer.get(popup.x, y).symbol(), "│");
                    assert_eq!(buffer.get(popup.x + popup.width - 1, y).symbol(), "│");
                }
            }
        }
    }

    #[test]
    fn graduate_only_loading_errors_are_localized_without_raw_details() {
        let (message, _) = App::display_notice(&Notice::error(stage_error("仅支持研究生")));
        assert_eq!(message, "当前仅支持研究生账户");

        let (message, _) = App::display_notice(&Notice::error(stage_error("没有开放轮次")));
        assert_eq!(message, "当前没有开放的研究生选课轮次");

        let mapped = login_stage_error(AppError::Auth(
            "this application only supports graduate student accounts".into(),
        ));
        assert_eq!(mapped, stage_error("仅支持研究生"));
    }
}
