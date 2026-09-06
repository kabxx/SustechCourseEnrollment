use crate::{
    account::{AccountStore, Credentials},
    cache::{Course, CourseTime, Semester},
    error::{AppError, Result},
    settings::{ProxyMode, Settings},
};
use reqwest::{
    header::{HeaderMap, ACCEPT, ACCEPT_LANGUAGE, COOKIE, LOCATION, ORIGIN, REFERER, SET_COOKIE},
    Client, Method, StatusCode,
};
use scraper::{Html, Selector};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::sleep;

const NOT_STARTED_RETRY_INITIAL_MS: u64 = 1_000;
const NOT_STARTED_RETRY_MAX_MS: u64 = 10_000;
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";

const TIS_ROOT: &str = "https://tis.sustech.edu.cn";
const CAS_ROOT: &str = "https://cas.sustech.edu.cn";
const TIS_MAIN: &str = "https://tis.sustech.edu.cn/authentication/main";
const TIS_USER_INFO: &str = "https://tis.sustech.edu.cn/UserManager/queryxsxx";
const CAS_LOGIN: &str =
    "https://cas.sustech.edu.cn/cas/login?service=https%3A%2F%2Ftis.sustech.edu.cn%2Fcas";
const GRADUATE_TRAINING_TYPE: &str = "2";
const GRADUATE_ROLE_CODE: &str = "02";
const XSK_PAGE_REFERER: &str = "https://tis.sustech.edu.cn/Xsxk/query/2";
const CATALOG_PAGE_SIZE: &str = "1000";

pub const XKTJZ_DIRECT_TO_ENROLLED: &str = "rwtjzyx";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionResult {
    Success(String),
    Skipped(String),
    RateLimited(String),
    Unknown(String),
    NotStarted(String),
}

#[derive(Debug, Clone)]
pub enum SelectionEvent {
    RequestStarted,
    Response(SelectionResult),
    RequestFailed(String),
    AuthExpired,
    AuthRetryFailed(String),
    AuthRecovered,
}

pub type SelectionEventHandler = Arc<dyn Fn(SelectionEvent) + Send + Sync>;

#[derive(Debug, Clone, Default)]
struct CookieJar {
    values: Arc<Mutex<BTreeMap<String, BTreeMap<String, String>>>>,
}

impl CookieJar {
    #[cfg(test)]
    fn from_map(values: BTreeMap<String, String>) -> Self {
        let mut tis = BTreeMap::new();
        let mut cas = BTreeMap::new();
        for (name, value) in values {
            let name = canonical_cookie_name(name.trim());
            if name.is_empty() || value.chars().any(|ch| matches!(ch, '\r' | '\n')) {
                continue;
            }
            if name == "TGC" {
                cas.insert(name, value);
            } else {
                tis.insert(name, value);
            }
        }
        let mut grouped = BTreeMap::new();
        grouped.insert("tis.sustech.edu.cn".to_owned(), tis);
        grouped.insert("cas.sustech.edu.cn".to_owned(), cas);
        Self {
            values: Arc::new(Mutex::new(grouped)),
        }
    }

    fn clear(&self) {
        self.values
            .lock()
            .expect("cookie mutex poisoned")
            .values_mut()
            .for_each(BTreeMap::clear);
    }

    fn from_persisted(values: BTreeMap<String, BTreeMap<String, String>>) -> Self {
        let jar = Self::default();
        let mut sanitized = BTreeMap::new();
        for (host, cookies) in values {
            if !matches!(host.as_str(), "tis.sustech.edu.cn" | "cas.sustech.edu.cn") {
                continue;
            }
            let mut clean = BTreeMap::new();
            for (name, value) in cookies {
                let name = canonical_cookie_name(name.trim());
                let value = value.trim();
                if !name.is_empty()
                    && !name.chars().any(|ch| matches!(ch, '\r' | '\n'))
                    && !value.chars().any(|ch| matches!(ch, '\r' | '\n'))
                {
                    clean.insert(name, value.to_owned());
                }
            }
            if !clean.is_empty() {
                sanitized.insert(host, clean);
            }
        }
        *jar.values.lock().expect("cookie mutex poisoned") = sanitized;
        jar
    }

    fn snapshot(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        self.values.lock().expect("cookie mutex poisoned").clone()
    }

    fn has_tis_session(&self) -> bool {
        let values = self.values.lock().expect("cookie mutex poisoned");
        let Some(tis) = values.get("tis.sustech.edu.cn") else {
            return false;
        };
        tis.get("SESSION").is_some_and(|value| !value.is_empty())
    }

    fn header_value_for(&self, host: &str) -> Option<String> {
        let values = self.values.lock().expect("cookie mutex poisoned");
        let values = values.get(host)?;
        if values.is_empty() {
            None
        } else {
            Some(
                values
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        }
    }

    fn absorb(&self, host: &str, headers: &HeaderMap) {
        let mut values = self.values.lock().expect("cookie mutex poisoned");
        let jar = values.entry(host.to_owned()).or_default();
        for header in headers.get_all(SET_COOKIE).iter() {
            let Ok(text) = header.to_str() else { continue };
            let pair = text.split_once(';').map_or(text, |(pair, _)| pair);
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let name = canonical_cookie_name(name.trim());
            let value = value.trim();
            if !name.is_empty() && !value.chars().any(|ch| matches!(ch, '\r' | '\n')) {
                jar.insert(name, value.to_owned());
            }
        }
    }
}

fn load_cookie_snapshot(path: &Path) -> Option<BTreeMap<String, BTreeMap<String, String>>> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_cookie_snapshot(
    path: &Path,
    values: &BTreeMap<String, BTreeMap<String, String>>,
) -> std::io::Result<()> {
    let tis_has_session = values.get("tis.sustech.edu.cn").is_some_and(|cookies| {
        cookies
            .get("SESSION")
            .is_some_and(|value| !value.is_empty())
    });
    if !tis_has_session {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(values)
        .map_err(|error| std::io::Error::other(format!("serialize cookie snapshot: {error}")))?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp, path)
}

fn canonical_cookie_name(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "tgc" => "TGC".to_owned(),
        "session" => "SESSION".to_owned(),
        _ => name.to_owned(),
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

#[derive(Clone)]
pub struct TisClient {
    credentials: Credentials,
    client: Client,
    cookies: CookieJar,
    cookie_path: PathBuf,
    graduate_verified: Arc<Mutex<bool>>,
    last_request: Arc<AsyncMutex<Option<Instant>>>,
    request_gate: Arc<AsyncMutex<()>>,
    request_interval: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StudentIdentity {
    display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CourseRound {
    code: String,
    title: String,
}

impl TisClient {
    pub fn new(credentials: Credentials) -> Result<Self> {
        let settings = Settings::load()?;
        Self::new_with_settings(credentials, &settings)
    }

    pub fn new_with_settings(credentials: Credentials, settings: &Settings) -> Result<Self> {
        settings.validate()?;
        let client = build_http_client(settings.proxy_mode)?;
        let cookie_path = AccountStore::cookie_path(&credentials.id)?;
        let cookies = load_cookie_snapshot(&cookie_path)
            .map(CookieJar::from_persisted)
            .unwrap_or_default();
        Ok(Self {
            cookies,
            cookie_path,
            credentials,
            client,
            graduate_verified: Arc::new(Mutex::new(false)),
            last_request: Arc::new(AsyncMutex::new(None)),
            request_gate: Arc::new(AsyncMutex::new(())),
            request_interval: Duration::from_millis(settings.request_interval_ms),
        })
    }

    pub fn apply_settings(&mut self, settings: &Settings) -> Result<()> {
        settings.validate()?;
        self.client = build_http_client(settings.proxy_mode)?;
        self.request_interval = Duration::from_millis(settings.request_interval_ms);
        Ok(())
    }

    fn persist_cookies(&self) {
        let values = self.cookies.snapshot();
        let _ = save_cookie_snapshot(&self.cookie_path, &values);
    }

    async fn pace(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            if let Some(wait_for) = self.request_interval.checked_sub(previous.elapsed()) {
                if !wait_for.is_zero() {
                    sleep(wait_for).await;
                }
            }
        }
        *last = Some(Instant::now());
    }

    async fn send_once(
        &self,
        method: Method,
        url: &str,
        form: Option<&[(String, String)]>,
    ) -> Result<HttpResponse> {
        let _request_guard = self.request_gate.lock().await;
        self.pace().await;
        let parsed =
            url::Url::parse(url).map_err(|_| AppError::Auth("invalid request URL".into()))?;
        ensure_allowed_host(url)?;
        let mut request = self.client.request(method, url);
        let host = parsed
            .host_str()
            .ok_or_else(|| AppError::Auth("request URL did not contain a host".into()))?;
        if let Some(cookie) = self.cookies.header_value_for(host) {
            request = request.header(COOKIE, cookie);
        }
        if host == "tis.sustech.edu.cn" {
            let referer = if parsed.path().starts_with("/Xsxk/") {
                XSK_PAGE_REFERER
            } else {
                TIS_MAIN
            };
            request = request
                .header(ACCEPT, "application/json, text/javascript, */*; q=0.01")
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
                .header(ORIGIN, TIS_ROOT)
                .header(REFERER, referer)
                .header("X-Requested-With", "XMLHttpRequest")
                .header("RoleCode", GRADUATE_ROLE_CODE);
        }
        if let Some(form) = form {
            request = request.form(form);
        }
        let response = request
            .send()
            .await
            .map_err(|error| AppError::HttpContext {
                endpoint: request_endpoint(url),
                source: error.without_url(),
            })?;
        let status = response.status();
        let headers = response.headers().clone();
        self.cookies.absorb(host, &headers);
        self.persist_cookies();
        let body = response
            .text()
            .await
            .map_err(|error| AppError::HttpContext {
                endpoint: request_endpoint(url),
                source: error.without_url(),
            })?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        form: Option<&[(String, String)]>,
    ) -> Result<HttpResponse> {
        self.send_once(method, url, form).await
    }

    async fn authenticated_request(
        &self,
        method: Method,
        url: &str,
        form: Option<&[(String, String)]>,
    ) -> Result<HttpResponse> {
        self.ensure_session().await?;
        let response = self.send(method, url, form).await?;
        if response_indicates_session_expired(&response) {
            self.cookies.clear();
            self.persist_cookies();
            *self
                .graduate_verified
                .lock()
                .expect("graduate verification mutex poisoned") = false;
            return Err(AppError::SessionExpired);
        }
        Ok(response)
    }

    pub async fn ensure_session(&self) -> Result<()> {
        if self.cookies.has_tis_session() {
            let already_verified = self
                .graduate_verified
                .lock()
                .expect("graduate verification mutex poisoned")
                .to_owned();
            if already_verified {
                return Ok(());
            }
            match self.restore_session().await {
                Ok(()) => {
                    return Ok(());
                }
                Err(_) => {
                    self.cookies.clear();
                    self.persist_cookies();
                }
            }
        }
        self.login_once(false).await.map(|_| ())
    }

    async fn restore_session(&self) -> Result<()> {
        let _ = self.fetch_student_identity().await?;
        *self
            .graduate_verified
            .lock()
            .expect("graduate verification mutex poisoned") = true;
        Ok(())
    }

    pub async fn login(&self) -> Result<()> {
        if self
            .graduate_verified
            .lock()
            .expect("graduate verification mutex poisoned")
            .to_owned()
        {
            return Ok(());
        }
        if self.cookies.has_tis_session() {
            if self.restore_session().await.is_ok() {
                return Ok(());
            }
            self.cookies.clear();
            self.persist_cookies();
        }
        self.login_once(false).await.map(|_| ())
    }

    pub async fn login_with_name(&self) -> Result<Option<String>> {
        if self.cookies.has_tis_session() {
            if self.ensure_session().await.is_ok() {
                return Ok(self
                    .fetch_student_identity()
                    .await
                    .ok()
                    .and_then(|identity| identity.display_name));
            }
        }
        self.login_once(true).await
    }

    async fn login_once(&self, read_name: bool) -> Result<Option<String>> {
        let password = &self.credentials.password;
        self.cookies.clear();
        self.persist_cookies();
        *self
            .graduate_verified
            .lock()
            .expect("graduate verification mutex poisoned") = false;

        let _ = self.send(Method::GET, TIS_MAIN, None).await?;
        let page = self.send(Method::GET, CAS_LOGIN, None).await?;
        if !page.status.is_success() {
            return Err(AppError::Auth(format!(
                "CAS login page returned {}",
                page.status
            )));
        }
        let execution = parse_execution_token(&page.body).ok_or_else(|| {
            AppError::Auth("CAS login page did not contain an execution token".into())
        })?;
        let form = vec![
            ("username".to_owned(), self.credentials.id.clone()),
            ("password".to_owned(), password.to_owned()),
            ("execution".to_owned(), execution),
            ("_eventId".to_owned(), "submit".to_owned()),
            ("geolocation".to_owned(), String::new()),
        ];
        let login = self.send(Method::POST, CAS_LOGIN, Some(&form)).await?;
        if login.status.is_client_error() || login.status.is_server_error() {
            return Err(AppError::Auth(format!(
                "CAS login returned {}",
                login.status
            )));
        }

        let ticket_url = login
            .headers
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.contains("ticket="))
            .map(ToOwned::to_owned)
            .ok_or_else(|| AppError::Auth("CAS did not return a service ticket".into()))?;
        let ticket_url = absolutize_url(&ticket_url, CAS_ROOT)?;
        ensure_allowed_host(&ticket_url)?;
        let ticket = self.send(Method::GET, &ticket_url, None).await?;
        let location = ticket
            .headers
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AppError::Auth("TIS ticket exchange did not return a redirect".into())
            })?;
        if !ticket.status.is_redirection() || !is_tis_main_location(location) {
            return Err(AppError::Auth(format!(
                "TIS ticket exchange returned an unexpected response ({})",
                ticket.status
            )));
        }
        let main_url = absolutize_url(location, TIS_ROOT)?;
        ensure_allowed_host(&main_url)?;
        let main = self.send(Method::GET, &main_url, None).await?;
        if !main.status.is_success() {
            return Err(AppError::Auth(format!(
                "TIS main page returned {}",
                main.status
            )));
        }
        if !self.cookies.has_tis_session() {
            return Err(AppError::Auth("TIS did not issue a SESSION cookie".into()));
        }

        let identity = match self.fetch_student_identity().await {
            Ok(identity) => identity,
            Err(error) => {
                self.cookies.clear();
                self.persist_cookies();
                return Err(error);
            }
        };
        *self
            .graduate_verified
            .lock()
            .expect("graduate verification mutex poisoned") = true;
        Ok(read_name.then_some(identity.display_name).flatten())
    }

    async fn fetch_student_identity(&self) -> Result<StudentIdentity> {
        let response = self.send(Method::POST, TIS_USER_INFO, None).await?;
        ensure_success(&response, "query student profile")?;
        parse_student_identity(&response.body, &self.credentials.id)
    }

    pub fn training_type(&self) -> Result<String> {
        self.graduate_verified
            .lock()
            .expect("graduate verification mutex poisoned")
            .then(|| GRADUATE_TRAINING_TYPE.to_owned())
            .ok_or_else(|| AppError::Auth("graduate account has not been verified".into()))
    }

    pub async fn semester(&self) -> Result<Semester> {
        let form = vec![("mxpylx".to_owned(), self.training_type()?)];
        let response = self
            .authenticated_request(
                Method::POST,
                &format!("{TIS_ROOT}/Xsxk/queryXkdqXnxq"),
                Some(&form),
            )
            .await?;
        ensure_success(&response, "query semester")?;
        let value: Value = serde_json::from_str(&response.body)?;
        Semester::from_json(&value)
    }

    pub async fn selected_courses(&self, semester: &Semester) -> Result<BTreeMap<String, Course>> {
        let _ = self.training_type()?;
        let form = selected_courses_form(semester);
        let response = self
            .authenticated_request(
                Method::POST,
                &format!("{TIS_ROOT}/Xsxk/queryYxkc"),
                Some(&form),
            )
            .await?;
        ensure_success(&response, "query selected courses")?;
        parse_selected_courses(&serde_json::from_str(&response.body)?)
    }

    pub async fn course_kinds(&self, semester: &Semester) -> Result<Vec<String>> {
        let _ = self.training_type()?;
        let form = course_rounds_form(semester);
        let response = self
            .authenticated_request(
                Method::POST,
                &format!("{TIS_ROOT}/Xsxk/queryYxkc"),
                Some(&form),
            )
            .await?;
        ensure_success(&response, "query course rounds")?;
        Ok(parse_course_rounds(&serde_json::from_str(&response.body)?)?
            .into_iter()
            .map(|round| round.code)
            .collect())
    }

    pub async fn courses(
        &self,
        semester: &Semester,
        kind: &str,
    ) -> Result<BTreeMap<String, Course>> {
        let _ = self.training_type()?;
        let form = graduate_course_query_form(semester, kind);
        let response = self
            .authenticated_request(
                Method::POST,
                &format!("{TIS_ROOT}/Xsxk/queryKxrw"),
                Some(&form),
            )
            .await?;
        ensure_success(&response, "query available courses")?;
        parse_courses(&serde_json::from_str(&response.body)?, kind)
    }

    pub async fn all_courses(
        &self,
        semester: &Semester,
        kinds: &[String],
    ) -> Result<BTreeMap<String, Course>> {
        let mut all: BTreeMap<String, Course> = BTreeMap::new();
        for kind in kinds {
            let courses = self.courses(semester, kind).await.map_err(|error| {
                AppError::Course(format!("course round {kind} failed: {error}"))
            })?;
            for (id, course) in courses {
                if let Some(existing) = all.get(&id) {
                    if existing.name != course.name || existing.kind != course.kind {
                        return Err(AppError::Course(format!(
                            "duplicate course id {id:?} with conflicting categories"
                        )));
                    }
                }
                all.insert(id, course);
            }
        }
        Ok(all)
    }

    pub async fn select_direct(
        &self,
        semester: &Semester,
        course: &Course,
    ) -> Result<SelectionResult> {
        self.select_direct_with_stop(semester, course, None).await
    }

    pub async fn select_direct_with_stop(
        &self,
        semester: &Semester,
        course: &Course,
        stop: Option<&AtomicBool>,
    ) -> Result<SelectionResult> {
        self.select_direct_with_stop_report(semester, course, stop, None)
            .await
    }

    pub async fn select_direct_with_stop_report(
        &self,
        semester: &Semester,
        course: &Course,
        stop: Option<&AtomicBool>,
        reporter: Option<SelectionEventHandler>,
    ) -> Result<SelectionResult> {
        if course.id.trim().is_empty() {
            let error = AppError::Course(
                "course row has no server selection id; refresh the course catalogue".into(),
            );
            Self::report_selection_event(
                reporter.as_ref(),
                SelectionEvent::RequestFailed(error.to_string()),
            );
            return Err(error);
        }
        let xkxs = match course_selection_coefficient(course) {
            Ok(value) => value,
            Err(error) => {
                Self::report_selection_event(
                    reporter.as_ref(),
                    SelectionEvent::RequestFailed(error.to_string()),
                );
                return Err(error);
            }
        };
        if !self
            .ensure_selection_session(stop, reporter.as_ref())
            .await?
        {
            return Ok(SelectionResult::NotStarted("抢课已停止".to_owned()));
        }
        let pylx = match self.training_type() {
            Ok(pylx) => pylx,
            Err(error) => {
                Self::report_selection_event(
                    reporter.as_ref(),
                    SelectionEvent::RequestFailed(error.to_string()),
                );
                return Err(error);
            }
        };
        let form = selection_form(
            semester,
            &pylx,
            &course.kind,
            Some(XKTJZ_DIRECT_TO_ENROLLED),
            Some(&course.id),
            xkxs.as_deref(),
        );
        self.retry_selection_with_stop(|| self.post_selection(&form), stop, reporter.as_ref())
            .await
    }

    async fn retry_selection_with_stop<F, Fut>(
        &self,
        mut operation: F,
        stop: Option<&AtomicBool>,
        reporter: Option<&SelectionEventHandler>,
    ) -> Result<SelectionResult>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<SelectionResult>>,
    {
        let mut delay_ms = NOT_STARTED_RETRY_INITIAL_MS;
        loop {
            if stop.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Ok(SelectionResult::NotStarted("抢课已停止".to_owned()));
            }
            Self::report_selection_event(reporter, SelectionEvent::RequestStarted);
            let result = match operation().await {
                Ok(result) => {
                    Self::report_selection_event(
                        reporter,
                        SelectionEvent::Response(result.clone()),
                    );
                    result
                }
                Err(AppError::SessionExpired) => {
                    Self::report_selection_event(reporter, SelectionEvent::AuthExpired);
                    if !self.ensure_selection_session(stop, reporter).await? {
                        return Ok(SelectionResult::NotStarted("抢课已停止".to_owned()));
                    }
                    continue;
                }
                Err(error) => {
                    Self::report_selection_event(
                        reporter,
                        SelectionEvent::RequestFailed(error.to_string()),
                    );
                    return Err(error);
                }
            };
            if let SelectionResult::NotStarted(_) = result {
                if wait_with_stop(delay_ms, stop).await {
                    return Ok(SelectionResult::NotStarted("抢课已停止".to_owned()));
                }
                delay_ms = delay_ms.saturating_mul(2).min(NOT_STARTED_RETRY_MAX_MS);
                continue;
            }
            return Ok(result);
        }
    }

    async fn ensure_selection_session(
        &self,
        stop: Option<&AtomicBool>,
        reporter: Option<&SelectionEventHandler>,
    ) -> Result<bool> {
        let mut delay_ms = NOT_STARTED_RETRY_INITIAL_MS;
        let mut recovery_announced = false;
        loop {
            if stop.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Ok(false);
            }
            match self.ensure_session().await {
                Ok(()) => {
                    if recovery_announced {
                        Self::report_selection_event(reporter, SelectionEvent::AuthRecovered);
                    }
                    return Ok(true);
                }
                Err(error) if stop.is_none() => return Err(error),
                Err(error) => {
                    if !recovery_announced {
                        Self::report_selection_event(reporter, SelectionEvent::AuthExpired);
                        recovery_announced = true;
                    }
                    Self::report_selection_event(
                        reporter,
                        SelectionEvent::AuthRetryFailed(error.to_string()),
                    );
                    if wait_with_stop(delay_ms, stop).await {
                        return Ok(false);
                    }
                    delay_ms = delay_ms.saturating_mul(2).min(NOT_STARTED_RETRY_MAX_MS);
                }
            }
        }
    }

    fn report_selection_event(reporter: Option<&SelectionEventHandler>, event: SelectionEvent) {
        if let Some(reporter) = reporter {
            reporter(event);
        }
    }

    async fn post_selection(&self, form: &[(String, String)]) -> Result<SelectionResult> {
        let endpoint = format!("{TIS_ROOT}/Xsxk/addGouwuche");
        let response = self
            .authenticated_request(Method::POST, &endpoint, Some(form))
            .await?;
        if response.status == StatusCode::TOO_MANY_REQUESTS {
            return Ok(SelectionResult::RateLimited(response_message(
                &response.body,
            )));
        }
        ensure_success(&response, "addGouwuche")?;
        match serde_json::from_str::<Value>(&response.body) {
            Ok(value) => Ok(parse_selection_response(&value)),
            Err(_) => Ok(classify_selection_body(&response.body)),
        }
    }
}

fn build_http_client(proxy_mode: ProxyMode) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none());
    if proxy_mode == ProxyMode::Disabled {
        builder = builder.no_proxy();
    }
    Ok(builder.build()?)
}

async fn wait_with_stop(delay_ms: u64, stop: Option<&AtomicBool>) -> bool {
    let Some(stop) = stop else {
        sleep(Duration::from_millis(delay_ms)).await;
        return false;
    };
    let deadline = Instant::now() + Duration::from_millis(delay_ms);
    loop {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return stop.load(Ordering::Acquire);
        }
        sleep(remaining.min(Duration::from_millis(100))).await;
    }
}

fn selection_form(
    semester: &Semester,
    pylx: &str,
    round_code: &str,
    xktjz: Option<&str>,
    id: Option<&str>,
    xkxs: Option<&str>,
) -> Vec<(String, String)> {
    let p_xn = Semester::form_value(&semester.p_xn);
    let p_xq = Semester::form_value(&semester.p_xq);
    let p_xnxq = Semester::form_value(&semester.p_xnxq);
    let p_dqxn = semester_context_value(semester, "p_dqxn", &p_xn);
    let p_dqxq = semester_context_value(semester, "p_dqxq", &p_xq);
    let p_dqxnxq = semester_context_value(semester, "p_dqxnxq", &p_xnxq);
    let cxsfmt = semester_context_value(semester, "cxsfmt", "0");
    let sfgldjr = semester_context_value(semester, "p_sfgldjr", "0");
    let sfredis = semester_context_value(semester, "p_sfredis", "0");
    let sfsyxkgwc = semester_context_value(semester, "p_sfsyxkgwc", "0");
    let kkxnxq = semester_context_value(semester, "p_kkxnxq", "");
    let sfmxzj = semester_context_value(semester, "p_sfmxzj", "");
    let xktjz = xktjz.unwrap_or("");
    let id = id.unwrap_or("");
    let xkxs = xkxs.unwrap_or("");

    vec![
        ("cxsfmt".to_owned(), cxsfmt),
        ("p_pylx".to_owned(), pylx.to_owned()),
        ("mxpylx".to_owned(), pylx.to_owned()),
        ("p_sfgldjr".to_owned(), sfgldjr),
        ("p_sfredis".to_owned(), sfredis),
        ("p_sfsyxkgwc".to_owned(), sfsyxkgwc),
        ("p_xktjz".to_owned(), xktjz.to_owned()),
        ("p_chaxunxh".to_owned(), String::new()),
        ("p_chaxunxkfsdm".to_owned(), String::new()),
        ("p_gjz".to_owned(), String::new()),
        ("p_skjs".to_owned(), String::new()),
        ("p_xn".to_owned(), p_xn),
        ("p_xq".to_owned(), p_xq),
        ("p_xnxq".to_owned(), p_xnxq),
        ("p_dqxn".to_owned(), p_dqxn),
        ("p_dqxq".to_owned(), p_dqxq),
        ("p_dqxnxq".to_owned(), p_dqxnxq),
        ("p_xkfsdm".to_owned(), round_code.to_owned()),
        ("p_xiaoqu".to_owned(), String::new()),
        ("p_kkyx".to_owned(), String::new()),
        ("p_kclb".to_owned(), String::new()),
        ("p_xkxs".to_owned(), xkxs.to_owned()),
        ("p_dyc".to_owned(), String::new()),
        ("p_kkxnxq".to_owned(), kkxnxq),
        ("p_id".to_owned(), id.to_owned()),
        ("p_sfhlctkc".to_owned(), "0".to_owned()),
        ("p_sfhllrlkc".to_owned(), "0".to_owned()),
        ("p_kxsj_xqj".to_owned(), String::new()),
        ("p_kxsj_ksjc".to_owned(), String::new()),
        ("p_kxsj_jsjc".to_owned(), String::new()),
        ("p_kcdm_js".to_owned(), String::new()),
        ("p_kcdm_cxrw".to_owned(), String::new()),
        ("p_kcdm_cxrw_zckc".to_owned(), String::new()),
        ("p_kc_gjz".to_owned(), String::new()),
        ("p_xzcxtjz_nj".to_owned(), String::new()),
        ("p_xzcxtjz_yx".to_owned(), String::new()),
        ("p_xzcxtjz_zy".to_owned(), String::new()),
        ("p_xzcxtjz_zyfx".to_owned(), String::new()),
        ("p_xzcxtjz_bj".to_owned(), String::new()),
        ("p_sfxsgwckb".to_owned(), "1".to_owned()),
        ("p_skyy".to_owned(), String::new()),
        ("p_sfmxzj".to_owned(), sfmxzj),
        ("pageNum".to_owned(), "1".to_owned()),
        ("pageSize".to_owned(), CATALOG_PAGE_SIZE.to_owned()),
    ]
}

fn course_rounds_form(semester: &Semester) -> Vec<(String, String)> {
    selection_form(semester, GRADUATE_TRAINING_TYPE, "", None, None, None)
}

fn selected_courses_form(semester: &Semester) -> Vec<(String, String)> {
    selection_form(semester, GRADUATE_TRAINING_TYPE, "yixuan", None, None, None)
}

fn graduate_course_query_form(semester: &Semester, round_code: &str) -> Vec<(String, String)> {
    selection_form(
        semester,
        GRADUATE_TRAINING_TYPE,
        round_code,
        None,
        None,
        None,
    )
}

fn semester_context_value(semester: &Semester, key: &str, fallback: &str) -> String {
    semester
        .extra
        .get(key)
        .map(Semester::form_value)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn course_selection_coefficient(course: &Course) -> Result<Option<String>> {
    let mode = course.xkms.as_deref().unwrap_or("").trim();
    if mode.is_empty() {
        return Err(AppError::Course(
            "course selection mode is missing; refresh the course catalogue before selecting"
                .into(),
        ));
    }
    if mode == "1" {
        return Ok(None);
    }
    if mode != "2" {
        return Err(AppError::Course(format!(
            "unsupported course selection mode {mode:?}"
        )));
    }

    let Some(raw) = course.xkxs.as_deref().map(str::trim) else {
        if coefficient_required(course.jfxs.as_deref()) {
            return Err(AppError::Course(
                "this lottery course requires a positive integer selection coefficient".into(),
            ));
        }
        return Ok(None);
    };
    if raw.is_empty() {
        if coefficient_required(course.jfxs.as_deref()) {
            return Err(AppError::Course(
                "this lottery course requires a positive integer selection coefficient".into(),
            ));
        }
        return Ok(None);
    }
    if is_positive_integer(raw) {
        Ok(Some(raw.to_owned()))
    } else {
        Err(AppError::Course(
            "selection coefficient must be a positive integer".into(),
        ))
    }
}

fn coefficient_required(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .is_some_and(|value| !value.is_empty() && value != "0")
}

fn is_positive_integer(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|ch| ch.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|number| number > 0)
}

fn ensure_success(response: &HttpResponse, operation: &str) -> Result<()> {
    if response.status.is_success() {
        Ok(())
    } else if is_auth_redirect(response.status) {
        Err(AppError::SessionExpired)
    } else {
        Err(AppError::HttpStatus {
            status: response.status.as_u16(),
            url: operation.to_owned(),
            body: truncate(&response.body),
        })
    }
}

fn response_indicates_session_expired(response: &HttpResponse) -> bool {
    if is_auth_redirect(response.status)
        || matches!(
            response.status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        )
    {
        return true;
    }
    if !response.status.is_success() {
        return false;
    }
    if parse_execution_token(&response.body).is_some() {
        return true;
    }
    let lower = response.body.to_ascii_lowercase();
    [
        "登录超时",
        "会话已过期",
        "会话过期",
        "请先登录",
        "未登录",
        "session expired",
        "session timeout",
        "not authenticated",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn parse_selection_response(value: &Value) -> SelectionResult {
    let message = response_message_value(value);
    let flag = response_flag(value);
    if flag.as_deref() == Some("1") {
        return SelectionResult::Success(if message.trim().is_empty() {
            "operation succeeded".to_owned()
        } else {
            message
        });
    }
    if is_rate_limited_text(&message) {
        return SelectionResult::RateLimited(message);
    }
    if is_not_started_text(&message) {
        return SelectionResult::NotStarted(if message.trim().is_empty() {
            "selection round has not started".to_owned()
        } else {
            message.clone()
        });
    }
    let Some(flag) = flag else {
        return SelectionResult::Unknown(message);
    };
    match flag.as_str() {
        "-1" | "-9" => SelectionResult::Skipped(message),
        "0" => match classify_non_success_message(&message) {
            SelectionResult::Skipped(_) => SelectionResult::Skipped(message),
            SelectionResult::RateLimited(_) => SelectionResult::RateLimited(message),
            SelectionResult::NotStarted(_) => SelectionResult::NotStarted(message),
            SelectionResult::Unknown(_) => SelectionResult::Unknown(message),
            SelectionResult::Success(_) => unreachable!(),
        },
        _ => SelectionResult::Unknown(message),
    }
}

fn classify_selection_body(body: &str) -> SelectionResult {
    let message = truncate(body.trim());
    if is_rate_limited_text(&message) {
        SelectionResult::RateLimited(message)
    } else if is_not_started_text(&message) {
        SelectionResult::NotStarted(message)
    } else {
        SelectionResult::Unknown(message)
    }
}

fn response_flag(value: &Value) -> Option<String> {
    value.get("jg").and_then(normalize_response_flag)
}

fn normalize_response_flag(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let text = text.trim();
            text.parse::<i64>().ok().map(|_| text.to_owned())
        }
        Value::Number(number) => Some(normalize_numeric_text(number)),
        Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn response_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .map(|value| response_message_value(&value))
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| truncate(body))
}

fn response_message_value(value: &Value) -> String {
    value
        .get("message")
        .map(value_to_string)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| truncate(&value.to_string()))
}

fn is_rate_limited_text(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    message.contains("请求频率过高")
        || message.contains("访问过于频繁")
        || lower.contains("too many requests")
        || lower.contains("rate limit")
}

fn classify_non_success_message(message: &str) -> SelectionResult {
    if is_rate_limited_text(message) {
        SelectionResult::RateLimited(message.to_owned())
    } else if is_not_started_text(message) {
        SelectionResult::NotStarted(message.to_owned())
    } else if [
        "冲突",
        "已选",
        "已满",
        "超过可选分数",
        "不允许选",
        "不在选课规则",
        "不符合选课",
        "不在选课范围",
        "没有名额",
        "无容量",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        SelectionResult::Skipped(message.to_owned())
    } else {
        SelectionResult::Unknown(message.to_owned())
    }
}

fn is_not_started_text(message: &str) -> bool {
    let normalized = message.trim();
    if normalized.is_empty() {
        return false;
    }
    if [
        "已结束",
        "已经结束",
        "已开始",
        "已经开始",
        "已开放",
        "已经开放",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        return false;
    }
    const PHRASES: &[&str] = &[
        "选课尚未开始",
        "抢课尚未开始",
        "选课未开始",
        "抢课未开始",
        "选课还未开始",
        "抢课还未开始",
        "尚未到选课时间",
        "尚未到抢课时间",
        "未到选课时间",
        "未到抢课时间",
        "未到选课开始时间",
        "未到抢课开始时间",
        "不在选课时间",
        "不在抢课时间",
        "不在选课时间段",
        "不在抢课时间段",
        "当前不在选课阶段",
        "当前不在抢课阶段",
        "您不在选课阶段",
        "您不在抢课阶段",
        "不在选课阶段",
        "不在抢课阶段",
        "选课时间未开始",
        "抢课时间未开始",
        "选课尚未开放",
        "抢课尚未开放",
        "未开放选课",
        "未开放抢课",
        "当前选课尚未开始",
        "当前抢课尚未开始",
        "selection has not started",
        "course selection has not started",
    ];
    PHRASES.iter().any(|phrase| normalized.contains(phrase))
}

fn is_auth_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn truncate(value: &str) -> String {
    const MAX: usize = 500;
    if value.len() <= MAX {
        value.to_owned()
    } else {
        let boundary = value
            .char_indices()
            .take_while(|(index, _)| *index < MAX)
            .last()
            .map_or(0, |(index, _)| index);
        format!("{}…", &value[..boundary])
    }
}

pub fn parse_execution_token(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("input[name='execution']").ok()?;
    document
        .select(&selector)
        .next()
        .and_then(|node| node.value().attr("value"))
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_student_identity(body: &str, account_id: &str) -> Result<StudentIdentity> {
    let value: Value = serde_json::from_str(body)?;
    let training_type = parse_training_type_value(&value).ok_or_else(|| {
        AppError::Auth("student profile did not contain a valid training type".into())
    })?;
    if training_type != GRADUATE_TRAINING_TYPE {
        return Err(AppError::Auth(
            "this application only supports graduate student accounts".into(),
        ));
    }
    Ok(StudentIdentity {
        display_name: parse_display_name_value(&value, account_id),
    })
}

fn parse_training_type_value(value: &Value) -> Option<String> {
    value
        .get("PYLX")
        .map(value_to_string)
        .and_then(|value| normalize_training_type(&value))
}

fn normalize_training_type(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 8 && value.chars().all(|ch| ch.is_ascii_digit()))
        .then(|| value.to_owned())
}

fn parse_display_name_value(value: &Value, account_id: &str) -> Option<String> {
    value
        .get("XM")
        .and_then(Value::as_str)
        .and_then(|name| normalize_display_name(name, account_id))
}

fn normalize_display_name(value: &str, account_id: &str) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let value = value.trim();
    if value.is_empty()
        || value.len() > 96
        || value.eq_ignore_ascii_case(account_id)
        || value.chars().any(char::is_control)
        || value.contains("登录")
        || value.contains("退出")
        || value.contains("课程")
        || value.contains("姓名")
    {
        return None;
    }
    Some(value.to_owned())
}

fn absolutize_url(location: &str, base: &str) -> Result<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_owned());
    }
    let base =
        url::Url::parse(base).map_err(|_| AppError::Auth("invalid redirect base URL".into()))?;
    base.join(location)
        .map(|value| value.to_string())
        .map_err(|_| AppError::Auth("invalid redirect URL".into()))
}

fn ensure_allowed_host(value: &str) -> Result<()> {
    let parsed = url::Url::parse(value).map_err(|_| AppError::Auth("invalid URL".into()))?;
    if parsed.scheme() != "https" {
        return Err(AppError::Auth(format!(
            "refusing non-HTTPS URL with scheme {}",
            parsed.scheme()
        )));
    }
    if parsed.port().is_some_and(|port| port != 443) {
        return Err(AppError::Auth(
            "refusing URL on a non-standard HTTPS port".into(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(AppError::Auth("refusing URL containing userinfo".into()));
    }
    match parsed.host_str() {
        Some("tis.sustech.edu.cn") | Some("cas.sustech.edu.cn") => Ok(()),
        Some(host) => Err(AppError::Auth(format!(
            "refusing redirect to unexpected host {host}"
        ))),
        None => Err(AppError::Auth("redirect did not contain a host".into())),
    }
}

fn request_endpoint(value: &str) -> String {
    let Ok(parsed) = url::Url::parse(value) else {
        return "<invalid URL>".into();
    };
    let host = parsed.host_str().unwrap_or("<unknown host>");
    let path = if parsed.path().is_empty() {
        "/"
    } else {
        parsed.path()
    };
    format!("{host}{path}")
}

fn is_tis_main_location(location: &str) -> bool {
    let Ok(absolute) = absolutize_url(location, TIS_ROOT) else {
        return false;
    };
    let Ok(parsed) = url::Url::parse(&absolute) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str() == Some("tis.sustech.edu.cn")
        && parsed.path() == "/authentication/main"
        && parsed.username().is_empty()
        && parsed.password().is_none()
}

fn parse_course_rounds(value: &Value) -> Result<Vec<CourseRound>> {
    let list = value
        .get("xkgzszList")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Course("course-round response missing xkgzszList".into()))?;

    let mut rounds = BTreeMap::<String, CourseRound>::new();
    for (index, item) in list.iter().enumerate() {
        let code = item
            .get("xkfsdm")
            .map(value_to_string)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AppError::Course(format!(
                    "course-round response item {index} has no round code"
                ))
            })?;
        if matches!(code.as_str(), "yixuan" | "gouwuche") {
            continue;
        }
        let title = item
            .get("xkfsmc")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&code)
            .to_owned();
        if let Some(existing) = rounds.get_mut(&code) {
            if existing.title == existing.code && title != code {
                existing.title = title;
            }
        } else {
            rounds.insert(code.clone(), CourseRound { code, title });
        }
    }
    Ok(rounds.into_values().collect())
}

fn parse_selected_courses(value: &Value) -> Result<BTreeMap<String, Course>> {
    let list = value
        .get("yxkcList")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Course("selected-course response missing yxkcList".into()))?;
    let mut courses = BTreeMap::new();
    for (index, item) in list.iter().enumerate() {
        let object = item.as_object().ok_or_else(|| {
            AppError::Course(format!(
                "selected-course response item {index} is not an object"
            ))
        })?;
        let name = object
            .get("kcmc")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                AppError::Course(format!(
                    "selected-course response item {index} has no course name"
                ))
            })?
            .to_owned();
        let id = object
            .get("id")
            .map(value_to_string)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AppError::Course(format!(
                    "selected-course response item {index} has no course id"
                ))
            })?;
        if courses.contains_key(&id) {
            return Err(AppError::Course(format!(
                "duplicate selected course id {:?}",
                id
            )));
        }
        let course = Course {
            id: id.clone(),
            name: name.clone(),
            kind: object
                .get("xkfsdm")
                .map(value_to_string)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "yixuan".to_owned()),
            code: parse_course_code(item),
            class_name: parse_course_class(item, &name),
            xkms: optional_value_to_string(object.get("xkms")),
            xkxs: optional_value_to_string(object.get("xkxs")),
            jfxs: optional_value_to_string(object.get("jfxs")),
            schedule: parse_course_schedule(item),
            details: parse_course_details(item),
        };
        courses.insert(id, course);
    }
    Ok(courses)
}

fn parse_courses(value: &Value, kind: &str) -> Result<BTreeMap<String, Course>> {
    let round_config = value.pointer("/xsxkPage/xkgzszOne");
    let default_xkms = round_config.and_then(|round| optional_value_to_string(round.get("xkms")));
    let default_jfxs = round_config.and_then(|round| optional_value_to_string(round.get("jfxs")));
    let list = value.pointer("/kxrwList/list").and_then(Value::as_array);
    let Some(list) = list else {
        if course_category_is_inapplicable(value) {
            return Ok(BTreeMap::new());
        }
        return Err(AppError::Course(
            "available-course response missing kxrwList.list".into(),
        ));
    };
    let mut courses: BTreeMap<String, Course> = BTreeMap::new();
    for (index, item) in list.iter().enumerate() {
        let id = item
            .get("id")
            .map(value_to_string)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                AppError::Course(format!(
                    "available-course response item {index} has no course id"
                ))
            })?;
        let name = item
            .get("kcmc")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                AppError::Course(format!(
                    "available-course response item {index} has no course name"
                ))
            })?;
        let course = Course {
            id,
            name: name.to_owned(),
            kind: kind.to_owned(),
            code: parse_course_code(item),
            class_name: parse_course_class(item, name),
            xkms: optional_value_to_string(item.get("xkms")).or_else(|| default_xkms.clone()),
            xkxs: optional_value_to_string(item.get("xkxs")),
            jfxs: optional_value_to_string(item.get("jfxs")).or_else(|| default_jfxs.clone()),
            schedule: parse_course_schedule(item),
            details: parse_course_details(item),
        };
        if let Some(existing) = courses.get(&course.id) {
            if existing.name != course.name || existing.kind != course.kind {
                return Err(AppError::Course(format!(
                    "duplicate course id {:?} with conflicting rows",
                    course.id
                )));
            }
        }
        courses.insert(course.id.clone(), course);
    }
    Ok(courses)
}

fn parse_course_code(item: &Value) -> Option<String> {
    let object = item.as_object()?;
    object
        .get("kcdm")
        .map(value_to_string)
        .map(|value| value.trim().to_owned())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && !value.chars().any(|character| character.is_control())
        })
}

fn parse_course_class(item: &Value, course_name: &str) -> Option<String> {
    let object = item.as_object()?;
    for key in ["rwbjmc", "syrwbjmc"] {
        if let Some(value) = object.get(key).and_then(|value| {
            optional_value_to_string(Some(value))
                .and_then(|value| normalize_course_class(&value, course_name))
        }) {
            return Some(value);
        }
    }

    let task_name = object
        .get("rwmc")
        .and_then(|value| optional_value_to_string(Some(value)))?;
    if task_name.contains('班') {
        normalize_course_class(&task_name, course_name)
    } else {
        None
    }
}

fn normalize_course_class(value: &str, course_name: &str) -> Option<String> {
    let mut value = value.trim();
    if value.is_empty() || value == course_name {
        return None;
    }
    if let Some(rest) = value.strip_prefix(course_name) {
        value = rest.trim_matches(|character: char| {
            matches!(
                character,
                '-' | '_' | ' ' | '·' | ':' | '：' | '－' | '—' | '–'
            )
        });
    }
    (!value.is_empty() && value.len() <= 96 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

fn parse_course_details(item: &Value) -> BTreeMap<String, String> {
    let Some(object) = item.as_object() else {
        return BTreeMap::new();
    };
    let mut details = BTreeMap::new();
    for (key, source) in [
        ("task_id", "rwh"),
        ("task_type", "rwlxmc"),
        ("course_category", "kclbmc"),
        ("course_nature", "kcxzmc"),
        ("teaching_language", "skyymc"),
        ("grading_method", "jfzlbmc"),
        ("credits", "xf"),
        ("hours", "zxs"),
        ("capacity", "yjsrl"),
        ("selected_count", "yjsyxrlrs"),
        ("conflict_course", "ctkcxx"),
        ("teacher", "dgjsmc"),
        ("campus", "xiaoqumc"),
        ("opening_college", "kkyxmc"),
        ("course_sequence", "kxh"),
        ("term", "xnxqmc"),
        ("selection_status", "sfkxk"),
    ] {
        if let Some(mut value) = object
            .get(source)
            .and_then(|value| optional_value_to_string(Some(value)))
        {
            if key == "conflict_course" {
                value = plain_html_text(&value);
            }
            if !value.is_empty() {
                details.insert(key.to_owned(), value);
            }
        }
    }
    let schedule_lines = object
        .get("pkjgmx")
        .and_then(Value::as_str)
        .map(html_schedule_lines)
        .unwrap_or_default();
    if !schedule_lines.is_empty() {
        details.insert("schedule_text".to_owned(), schedule_lines.join("\n"));
    }
    details
}

fn plain_html_text(value: &str) -> String {
    let document = Html::parse_fragment(value);
    document
        .root_element()
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_schedule_lines(value: &str) -> Vec<String> {
    let document = Html::parse_fragment(value);
    let selector = Selector::parse("p").ok();
    let mut lines = selector
        .as_ref()
        .map(|selector| {
            document
                .select(selector)
                .map(|node| node.text().collect::<String>())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if lines.is_empty() {
        lines.push(document.root_element().text().collect::<String>());
    }
    lines
        .into_iter()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .map(|line| {
            line.trim_matches(|character: char| matches!(character, '、' | ',' | ';' | '；'))
                .to_owned()
        })
        .filter(|line| {
            !line.is_empty()
                && !line.contains("上课信息")
                && !line.contains("Class Information")
                && (line.contains('周')
                    || line.contains("星期")
                    || line.contains("Weeks")
                    || line.contains("Week"))
        })
        .collect()
}

fn parse_course_schedule(item: &Value) -> Vec<CourseTime> {
    let Some(object) = item.as_object() else {
        return Vec::new();
    };
    let mut slots = Vec::new();
    for key in ["pkjgSlots", "kssjapSlots"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        let parsed = match value {
            Value::String(text) => serde_json::from_str::<Value>(text).ok(),
            Value::Array(_) | Value::Object(_) => Some(value.clone()),
            _ => None,
        };
        let Some(parsed) = parsed else {
            continue;
        };
        let entries: Vec<&Value> = match &parsed {
            Value::Array(items) => items.iter().collect(),
            Value::Object(_) => vec![&parsed],
            _ => Vec::new(),
        };
        for entry in entries {
            let Some(slot) = parse_course_time(entry) else {
                continue;
            };
            if !slots.contains(&slot) {
                slots.push(slot);
            }
        }
    }
    if slots.is_empty() {
        if let Some(text) = object.get("pkjgmx").and_then(Value::as_str) {
            for line in html_schedule_lines(text) {
                if let Some(slot) = parse_schedule_text_line(&line) {
                    if !slots.contains(&slot) {
                        slots.push(slot);
                    }
                }
            }
        }
    }
    slots
}

fn parse_schedule_text_line(line: &str) -> Option<CourseTime> {
    let weekday = parse_weekday(&Value::String(line.to_owned()));
    let sections = line
        .find('第')
        .and_then(|start| {
            line[start + '第'.len_utf8()..].find('节').map(|end| {
                let end = start + '第'.len_utf8() + end;
                schedule_section_numbers(&line[start + '第'.len_utf8()..end])
            })
        })
        .unwrap_or_default();
    let (start_section, end_section) = match sections.as_slice() {
        [start, end, ..] => (Some(*start), Some(*end)),
        [section] => (Some(*section), Some(*section)),
        _ => (None, None),
    };
    let weeks = line
        .split_once(|character| matches!(character, ',' | '，'))
        .map(|(weeks, _)| normalize_weeks_text(format!("{}周", weeks.trim_end_matches('周'))))
        .filter(|weeks| !weeks.is_empty());
    let slot = CourseTime {
        weekday,
        start_section,
        end_section,
        weeks,
    };
    (slot.weekday.is_some()
        || slot.start_section.is_some()
        || slot.end_section.is_some()
        || slot.weeks.is_some())
    .then_some(slot)
}

fn schedule_section_numbers(value: &str) -> Vec<u16> {
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

fn parse_course_time(value: &Value) -> Option<CourseTime> {
    let object = value.as_object()?;
    let slot = CourseTime {
        weekday: object.get("xqj").and_then(parse_weekday),
        start_section: object.get("ksjc").and_then(parse_section),
        end_section: object.get("jsjc").and_then(parse_section),
        weeks: object
            .get("zc")
            .map(value_to_string)
            .map(normalize_weeks_text)
            .filter(|value| !value.is_empty()),
    };
    (slot.weekday.is_some()
        || slot.start_section.is_some()
        || slot.end_section.is_some()
        || slot.weeks.is_some())
    .then_some(slot)
}

fn parse_weekday(value: &Value) -> Option<u8> {
    let text = value_to_string(value);
    let text = text.trim();
    if let Ok(number) = text.parse::<u8>() {
        return (1..=7).contains(&number).then_some(number);
    }
    for prefix in ["周", "星期", "礼拜"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let digits = rest
                .trim_start()
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>();
            if let Some(number) = digits
                .parse::<u8>()
                .ok()
                .filter(|number| (1..=7).contains(number))
            {
                return Some(number);
            }
        }
    }
    for (needle, day) in [
        ("一", 1),
        ("二", 2),
        ("三", 3),
        ("四", 4),
        ("五", 5),
        ("六", 6),
        ("日", 7),
        ("天", 7),
    ] {
        if text.contains(&format!("周{needle}")) || text.contains(&format!("星期{needle}")) {
            return Some(day);
        }
    }
    None
}

fn parse_section(value: &Value) -> Option<u16> {
    let text = value_to_string(value);
    let mut digits = String::new();
    for character in text.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
        } else if !digits.is_empty() {
            break;
        }
    }
    digits.parse::<u16>().ok().filter(|number| *number > 0)
}

fn normalize_weeks_text(value: String) -> String {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 20 && bytes.iter().all(|byte| matches!(byte, b'0' | b'1')) {
        let offset = usize::from(bytes.first() == Some(&b'0'));
        let mut weeks = Vec::new();
        for (index, byte) in bytes.iter().skip(offset).enumerate() {
            if *byte == b'1' {
                weeks.push(index + 1);
            }
        }
        if !weeks.is_empty() {
            return format!("{}周", compress_week_numbers(&weeks));
        }
    }
    trimmed.to_owned()
}

fn compress_week_numbers(weeks: &[usize]) -> String {
    let mut parts = Vec::new();
    let mut start = weeks[0];
    let mut end = start;
    for &week in &weeks[1..] {
        if week == end + 1 {
            end = week;
            continue;
        }
        parts.push(format_week_range(start, end));
        start = week;
        end = week;
    }
    parts.push(format_week_range(start, end));
    parts.join(",")
}

fn format_week_range(start: usize, end: usize) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn course_category_is_inapplicable(value: &Value) -> bool {
    value
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("不在选课规则面向年级内"))
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => normalize_numeric_text(value),
        Value::Bool(value) => value.to_string(),
        _ => String::new(),
    }
}

fn normalize_numeric_text(value: &serde_json::Number) -> String {
    if let Some(integer) = value.as_i64() {
        integer.to_string()
    } else if let Some(integer) = value.as_u64() {
        integer.to_string()
    } else if let Some(float) = value.as_f64() {
        if float.is_finite() && float.fract() == 0.0 {
            format!("{float:.0}")
        } else {
            value.to_string()
        }
    } else {
        value.to_string()
    }
}

fn optional_value_to_string(value: Option<&Value>) -> Option<String> {
    value
        .map(value_to_string)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn classify_selection_message(message: String) -> SelectionResult {
    let message = message.trim().to_owned();
    if is_rate_limited_text(&message) {
        SelectionResult::RateLimited(message)
    } else if message.trim().is_empty() {
        SelectionResult::Unknown(String::new())
    } else {
        match classify_non_success_message(&message) {
            SelectionResult::Skipped(message) => SelectionResult::Skipped(message),
            SelectionResult::RateLimited(message) => SelectionResult::RateLimited(message),
            SelectionResult::NotStarted(message) => SelectionResult::NotStarted(message),
            SelectionResult::Unknown(_) => SelectionResult::Unknown(message),
            SelectionResult::Success(_) => unreachable!("message classifier cannot prove success"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cas_execution_token() {
        assert_eq!(
            parse_execution_token("<input name='execution' value='abc123'>"),
            Some("abc123".into())
        );
        assert_eq!(parse_execution_token("<html></html>"), None);
    }

    #[test]
    fn parses_current_profile_fields() {
        let profile = json!({"XH":"12345678","XM":"王五","PYLX":"2"});
        assert_eq!(
            parse_display_name_value(&profile, "12345678").as_deref(),
            Some("王五")
        );
        assert_eq!(
            parse_display_name_value(&json!({"name":"张三"}), "12345678"),
            None
        );
    }

    #[test]
    fn accepts_only_graduate_student_profiles() {
        let graduate =
            parse_student_identity(r#"{"XH":"12345678","XM":"李四","PYLX":2}"#, "12345678")
                .unwrap();
        assert_eq!(graduate.display_name.as_deref(), Some("李四"));

        let undergraduate =
            parse_student_identity(r#"{"XH":"12345678","XM":"王五","PYLX":"1"}"#, "12345678")
                .unwrap_err();
        assert!(
            matches!(undergraduate, AppError::Auth(message) if message == "this application only supports graduate student accounts")
        );

        assert!(parse_student_identity(r#"{"XH":"12345678","XM":"王五"}"#, "12345678").is_err());
        assert!(parse_student_identity(
            r#"{"XH":"12345678","XM":"王五","PYLX":"graduate"}"#,
            "12345678"
        )
        .is_err());
    }

    #[test]
    fn does_not_use_account_id_as_display_name() {
        let body = json!({"XM":"12345678","PYLX":"2"});
        assert_eq!(parse_display_name_value(&body, "12345678"), None);
    }

    #[test]
    fn classifies_server_messages() {
        assert!(matches!(
            classify_selection_message("选课成功".into()),
            SelectionResult::Unknown(message) if message == "选课成功"
        ));
        assert!(matches!(
            classify_selection_message("课程已满".into()),
            SelectionResult::Skipped(_)
        ));
        assert!(matches!(
            classify_selection_message("选课请求频率过高".into()),
            SelectionResult::RateLimited(_)
        ));
        assert!(matches!(
            classify_selection_message("请求频率过高，请稍后再试".into()),
            SelectionResult::RateLimited(_)
        ));
        assert!(matches!(
            classify_selection_message("稍后再试".into()),
            SelectionResult::Unknown(_)
        ));
        assert!(matches!(
            classify_selection_message("选课尚未开始".into()),
            SelectionResult::NotStarted(message) if message == "选课尚未开始"
        ));
        assert!(matches!(
            classify_selection_message("系统提示：选课尚未开始，请稍后再试".into()),
            SelectionResult::NotStarted(_)
        ));
        assert!(matches!(
            classify_selection_body("抢课尚未开始"),
            SelectionResult::NotStarted(message) if message == "抢课尚未开始"
        ));
        assert!(matches!(
            classify_selection_message("请稍后再试".into()),
            SelectionResult::Unknown(_)
        ));
        assert!(matches!(
            classify_selection_message("选课尚未开始，但本轮已经结束".into()),
            SelectionResult::Unknown(_)
        ));
    }

    #[test]
    fn parses_and_deduplicates_available_course_rounds() {
        let value = json!({
            "xkgzszList": [
                {"xkfsdm": "jhnxk", "xkfsmc": "计划内选课"},
                {"xkfsdm": "jhnxk", "xkfsmc": "重复项"},
                {"xkfsdm": "gouwuche", "xkfsmc": "购物车"},
                {"xkfsdm": "yixuan", "xkfsmc": "已选课程"}
            ]
        });
        assert_eq!(
            parse_course_rounds(&value).unwrap(),
            vec![CourseRound {
                code: "jhnxk".into(),
                title: "计划内选课".into(),
            }]
        );

        assert!(parse_course_rounds(&json!({
            "xsxkPage": {"xkgzszList": [{"xkfsdm": "cxxk", "lcmc": "重修"}]}
        }))
        .is_err());
    }

    #[test]
    fn accepts_an_explicitly_empty_course_round_list() {
        assert!(parse_course_rounds(&json!({"xkgzszList": []}))
            .unwrap()
            .is_empty());
        assert!(parse_course_rounds(&json!({"jg": false})).is_err());
    }

    #[test]
    fn builds_graduate_round_and_course_query_forms() {
        let semester = Semester::from_json(&json!({
            "p_xn": "2026",
            "p_xq": "1",
            "p_xnxq": "2026-2027-1",
            "p_dqxn": "2026",
            "p_dqxq": "1",
            "p_dqxnxq": "2026-2027-1",
            "cxsfmt": "0"
        }))
        .unwrap();

        let rounds_form = course_rounds_form(&semester)
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(rounds_form["p_xn"], "2026");
        assert_eq!(rounds_form["p_xq"], "1");
        assert_eq!(rounds_form["p_pylx"], "2");
        assert_eq!(rounds_form["cxsfmt"], "0");

        let form = graduate_course_query_form(&semester, "jhnxk")
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        for (key, expected) in [
            ("p_pylx", "2"),
            ("mxpylx", "2"),
            ("p_xkfsdm", "jhnxk"),
            ("p_xn", "2026"),
            ("p_xq", "1"),
            ("p_xnxq", "2026-2027-1"),
            ("p_dqxn", "2026"),
            ("p_dqxq", "1"),
            ("p_dqxnxq", "2026-2027-1"),
            ("cxsfmt", "0"),
            ("p_chaxunxkfsdm", ""),
            ("pageNum", "1"),
            ("pageSize", CATALOG_PAGE_SIZE),
        ] {
            assert_eq!(form.get(key).map(String::as_str), Some(expected));
        }
    }

    #[test]
    fn course_query_form_falls_back_to_selected_term_context() {
        let semester = Semester::from_json(&json!({
            "p_xn": "2026",
            "p_xq": "2",
            "p_xnxq": "2026-2027-2"
        }))
        .unwrap();
        let form = graduate_course_query_form(&semester, "jhnxk")
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(form["p_dqxn"], "2026");
        assert_eq!(form["p_dqxq"], "2");
        assert_eq!(form["p_dqxnxq"], "2026-2027-2");
        assert_eq!(form["cxsfmt"], "0");
    }

    #[test]
    fn selection_form_matches_current_page_hidden_fields() {
        let semester = Semester::from_json(&json!({
            "p_xn": "2026",
            "p_xq": "1",
            "p_xnxq": "2026-2027-1",
            "p_dqxn": "2026",
            "p_dqxq": "1",
            "p_dqxnxq": "2026-2027-1",
            "cxsfmt": "1"
        }))
        .unwrap();
        let form = selection_form(
            &semester,
            "2",
            "bxxk",
            Some(XKTJZ_DIRECT_TO_ENROLLED),
            Some("row-id"),
            None,
        )
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        for (key, expected) in [
            ("cxsfmt", "1"),
            ("p_pylx", "2"),
            ("mxpylx", "2"),
            ("p_sfgldjr", "0"),
            ("p_sfredis", "0"),
            ("p_sfsyxkgwc", "0"),
            ("p_xktjz", "rwtjzyx"),
            ("p_xkfsdm", "bxxk"),
            ("p_id", "row-id"),
            ("p_kkxnxq", ""),
            ("p_sfxsgwckb", "1"),
            ("pageNum", "1"),
            ("pageSize", CATALOG_PAGE_SIZE),
        ] {
            assert_eq!(form.get(key).map(String::as_str), Some(expected));
        }
        assert!(!form
            .iter()
            .any(|(key, _)| key == "p_ids" || key == "p_ids[]"));
    }

    #[test]
    fn direct_selection_form_targets_enrolled_without_cart_ids() {
        let semester = Semester::from_json(&json!({
            "p_xn": "2026",
            "p_xq": "1",
            "p_xnxq": "2026-2027-1"
        }))
        .unwrap();
        let form = selection_form(
            &semester,
            GRADUATE_TRAINING_TYPE,
            "jhnxk",
            Some(XKTJZ_DIRECT_TO_ENROLLED),
            Some("row-id"),
            None,
        );
        let values = form.into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(values.get("p_xktjz").map(String::as_str), Some("rwtjzyx"));
        assert_eq!(values.get("p_xkfsdm").map(String::as_str), Some("jhnxk"));
        assert_eq!(values.get("p_id").map(String::as_str), Some("row-id"));
        assert!(!values.keys().any(|key| key == "p_ids" || key == "p_ids[]"));
    }

    #[test]
    fn course_query_form_leaves_page_only_filters_empty() {
        let semester = Semester::from_json(&json!({
            "p_xn": "2026",
            "p_xq": "1",
            "p_xnxq": "2026-2027-1"
        }))
        .unwrap();
        let values = graduate_course_query_form(&semester, "jhnxk")
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(values.get("p_kkxnxq").map(String::as_str), Some(""));
        assert_eq!(values.get("p_sfmxzj").map(String::as_str), Some(""));
        assert_eq!(values.get("pageSize").map(String::as_str), Some("1000"));
    }

    #[test]
    fn validates_lottery_selection_coefficients() {
        let base = Course {
            id: "id".into(),
            name: "lottery".into(),
            kind: "jhnxk".into(),
            xkms: Some("2".into()),
            xkxs: None,
            jfxs: Some("1".into()),
            ..Course::default()
        };
        assert!(course_selection_coefficient(&base).is_err());
        let mut valid = base.clone();
        valid.xkxs = Some("3".into());
        assert_eq!(
            course_selection_coefficient(&valid).unwrap().as_deref(),
            Some("3")
        );
        let mut first_come = valid;
        first_come.xkms = Some("1".into());
        assert_eq!(course_selection_coefficient(&first_come).unwrap(), None);
        let mut decimal = base;
        decimal.xkxs = Some("1.5".into());
        assert!(course_selection_coefficient(&decimal).is_err());

        let unknown_mode = Course {
            id: "id".into(),
            name: "unknown".into(),
            kind: "jhnxk".into(),
            xkms: Some("3".into()),
            xkxs: Some("1".into()),
            jfxs: None,
            ..Course::default()
        };
        assert!(course_selection_coefficient(&unknown_mode).is_err());

        let malformed_points = Course {
            id: "id".into(),
            name: "malformed".into(),
            kind: "jhnxk".into(),
            xkms: Some("2".into()),
            xkxs: None,
            jfxs: Some("not-a-number".into()),
            ..Course::default()
        };
        assert!(course_selection_coefficient(&malformed_points).is_err());
    }

    #[test]
    fn rejects_missing_selection_mode_before_a_write() {
        let course = Course {
            id: "id".into(),
            name: "unknown mode".into(),
            kind: "bxxk".into(),
            xkms: None,
            xkxs: None,
            jfxs: None,
            ..Course::default()
        };
        let error = course_selection_coefficient(&course).unwrap_err();
        assert!(error.to_string().contains("selection mode is missing"));
    }

    #[test]
    fn selection_success_requires_business_flag_not_message_text() {
        assert!(matches!(
            parse_selection_response(&json!({"jg": "1", "message": "已加入购物车"})),
            SelectionResult::Success(message) if message == "已加入购物车"
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": "0", "message": "选课成功"})),
            SelectionResult::Unknown(message) if message == "选课成功"
        ));
        assert!(matches!(
            parse_selection_response(&json!({"message": "选课成功"})),
            SelectionResult::Unknown(_)
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": true, "message": "选课成功"})),
            SelectionResult::Unknown(_)
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": 1.0, "message": "数值成功"})),
            SelectionResult::Success(message) if message == "数值成功"
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": 1, "message": "选课尚未开始"})),
            SelectionResult::Success(message) if message == "选课尚未开始"
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": 1, "message": "请求频率过高"})),
            SelectionResult::Success(message) if message == "请求频率过高"
        ));
    }

    #[test]
    fn selection_response_uses_current_fields_and_rate_limit() {
        assert!(matches!(
            parse_selection_response(&json!({"jg": "-1", "message": "不允许选课"})),
            SelectionResult::Skipped(message) if message == "不允许选课"
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": "0", "message": "请求频率过高"})),
            SelectionResult::RateLimited(message) if message == "请求频率过高"
        ));
        assert_eq!(response_message("not-json"), "not-json");
    }

    #[test]
    fn classifies_only_explicit_not_started_messages() {
        assert!(matches!(
            parse_selection_response(&json!({"jg": 0, "message": "选课尚未开始"})),
            SelectionResult::NotStarted(_)
        ));
        assert!(matches!(
            parse_selection_response(&json!({"notStarted": true, "jg": 0})),
            SelectionResult::Unknown(_)
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": 0, "message": "系统繁忙，请稍后再试"})),
            SelectionResult::Unknown(_)
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": 0, "message": "请求频率过高"})),
            SelectionResult::RateLimited(_)
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": 0, "message": "选课尚未开始，请求频率过高"})),
            SelectionResult::RateLimited(_)
        ));
        assert!(matches!(
            parse_selection_response(&json!({"jg": 0, "message": "当前不在选课时间"})),
            SelectionResult::NotStarted(_)
        ));
        assert!(matches!(
            classify_selection_body("选课尚未开始，请求频率过高"),
            SelectionResult::RateLimited(_)
        ));
    }

    #[test]
    fn non_json_selection_bodies_never_claim_success() {
        assert!(matches!(
            classify_selection_body("选课成功"),
            SelectionResult::Unknown(message) if message == "选课成功"
        ));
        assert!(matches!(
            classify_selection_body("请求频率过高"),
            SelectionResult::RateLimited(message) if message == "请求频率过高"
        ));
    }

    #[test]
    fn parses_course_payload() {
        let value = json!({"kxrwList":{"list":[{"id":12,"kcmc":"数据挖掘"}]}});
        let courses = parse_courses(&value, "xxxk").unwrap();
        assert_eq!(courses["12"].id, "12");
        assert_eq!(courses["12"].xkms, None);
    }

    #[test]
    fn preserves_current_selection_metadata_from_course_rows() {
        let value = json!({
            "kxrwList": {"list": [{
                "id": "row-1",
                "kcmc": "抽签课程",
                "xkms": 2,
                "xkxs": 3,
                "jfxs": "1"
            }]}
        });
        let courses = parse_courses(&value, "jhnxk").unwrap();
        let course = &courses["row-1"];
        assert_eq!(course.xkms.as_deref(), Some("2"));
        assert_eq!(course.xkxs.as_deref(), Some("3"));
        assert_eq!(course.jfxs.as_deref(), Some("1"));
    }

    #[test]
    fn parses_human_course_code_and_schedule_fields() {
        let value = json!({
            "kxrwList": {"list": [{
                "id": "row-1",
                "kcdm": "CS101",
                "kcmc": "程序设计",
                "pkjgSlots": "[{\"xqj\":2,\"ksjc\":3,\"jsjc\":4,\"zc\":\"1-16周\"}]"
            }]}
        });
        let courses = parse_courses(&value, "jhnxk").unwrap();
        let course = &courses["row-1"];
        assert_eq!(course.code.as_deref(), Some("CS101"));
        assert_eq!(course.name, "程序设计");
        assert_eq!(course.schedule.len(), 1);
        assert_eq!(course.schedule[0].weekday, Some(2));
        assert_eq!(course.schedule[0].start_section, Some(3));
        assert_eq!(course.schedule[0].end_section, Some(4));
        assert_eq!(course.schedule[0].weeks.as_deref(), Some("1-16周"));
    }

    #[test]
    fn ignores_noncanonical_schedule_fields() {
        let value = json!({
            "kxrwList": {"list": [{
                "id": "row-1",
                "kcmc": "数学",
                "schedule": [
                    {"weekday": "周一", "startSection": 1, "endSection": 2, "weeks": "1-8周"},
                    {"weekday": 3, "startSection": 5, "endSection": 6, "weeks": "9-16周"}
                ]
            }]}
        });
        let courses = parse_courses(&value, "jhnxk").unwrap();
        let course = &courses["row-1"];
        assert!(course.code.is_none());
        assert!(course.schedule.is_empty());
    }

    #[test]
    fn parses_graduate_catalogue_code_class_and_bitmap_slots() {
        let value = json!({
            "kxrwList": {"list": [{
                "id": "row-1",
                "kcdm": "CSE5020",
                "kcmc": "高级分布式系统",
                "rwmc": "高级分布式系统-01班-英文",
                "rwbjmc": "",
                "syrwbjmc": "",
                "xkms": "1",
                "pkjgSlots": "[{\"xqj\":\"4\",\"ksjc\":\"3\",\"jsjc\":\"4\",\"zc\":\"0111111111111111100000000000000000\"}]"
            }]}
        });
        let courses = parse_courses(&value, "jhnxk").unwrap();
        let course = &courses["row-1"];
        assert_eq!(course.code.as_deref(), Some("CSE5020"));
        assert_eq!(course.class_name.as_deref(), Some("01班-英文"));
        assert_eq!(course.display_name(), "高级分布式系统 · 01班-英文");
        assert_eq!(course.schedule.len(), 1);
        assert_eq!(course.schedule[0].weekday, Some(4));
        assert_eq!(course.schedule[0].start_section, Some(3));
        assert_eq!(course.schedule[0].end_section, Some(4));
        assert_eq!(course.schedule[0].weeks.as_deref(), Some("1-16周"));
    }

    #[test]
    fn normalizes_bitmap_week_ranges_and_keeps_regular_text() {
        assert_eq!(
            normalize_weeks_text("0111111111111111100000000000000000".into()),
            "1-16周"
        );
        assert_eq!(
            normalize_weeks_text("0101010101010101000000000000000000".into()),
            "1,3,5,7,9,11,13,15周"
        );
        assert_eq!(normalize_weeks_text("1-16周".into()), "1-16周");
    }

    #[test]
    fn ignores_noncanonical_schedule_text_field() {
        let value = json!({
            "kxrwList": {"list": [{
                "id": "row-1",
                "kcdm": "CS101",
                "kcmc": "课程",
                "sksj": "周二第3-4节"
            }]}
        });
        let courses = parse_courses(&value, "jhnxk").unwrap();
        assert!(courses["row-1"].schedule.is_empty());
    }

    #[test]
    fn canonicalizes_integral_json_metadata_numbers() {
        let value = json!({
            "kxrwList": {"list": [{
                "id": 12.0,
                "kcmc": "课程",
                "xkms": 2.0,
                "xkxs": 3.0,
                "jfxs": 1.0
            }]}
        });
        let courses = parse_courses(&value, "jhnxk").unwrap();
        let course = &courses["12"];
        assert_eq!(course.id, "12");
        assert_eq!(course.xkms.as_deref(), Some("2"));
        assert_eq!(course.xkxs.as_deref(), Some("3"));
        assert_eq!(course.jfxs.as_deref(), Some("1"));
    }

    #[test]
    fn uses_round_metadata_when_course_rows_omit_selection_mode() {
        let value = json!({
            "xsxkPage": {"xkgzszOne": {"xkms": 2, "jfxs": 1}},
            "kxrwList": {"list": [{
                "id": "row-1",
                "kcmc": "抽签课程",
                "xkxs": 4
            }]}
        });
        let courses = parse_courses(&value, "jhnxk").unwrap();
        let course = &courses["row-1"];
        assert_eq!(course.xkms.as_deref(), Some("2"));
        assert_eq!(course.xkxs.as_deref(), Some("4"));
        assert_eq!(course.jfxs.as_deref(), Some("1"));
    }

    #[test]
    fn treats_an_inapplicable_course_category_as_empty() {
        let value = json!({
            "jg": false,
            "message": "通识必修选课：不在选课规则面向年级内",
            "xsxkPage": null
        });
        assert!(parse_courses(&value, "bxxk").unwrap().is_empty());

        let wrapped = json!({
            "data": {"message": "当前学生不在选课规则面向年级内"}
        });
        assert!(parse_courses(&wrapped, "bxxk").is_err());
    }

    #[test]
    fn rejects_unknown_course_business_responses() {
        let value = json!({"jg": false, "message": "系统繁忙，请稍后再试"});
        assert!(parse_courses(&value, "bxxk").is_err());
    }

    #[test]
    fn keeps_cas_and_tis_cookies_isolated() {
        let jar = CookieJar::from_map(BTreeMap::from([
            ("TGC".into(), "cas-secret".into()),
            ("SESSION".into(), "tis-session".into()),
            ("x-csrf-token".into(), "tis-csrf".into()),
        ]));
        assert_eq!(
            jar.header_value_for("cas.sustech.edu.cn").unwrap(),
            "TGC=cas-secret"
        );
        let tis_header = jar.header_value_for("tis.sustech.edu.cn").unwrap();
        assert!(tis_header.contains("SESSION=tis-session"));
        assert!(tis_header.contains("x-csrf-token=tis-csrf"));
        assert!(!jar
            .header_value_for("cas.sustech.edu.cn")
            .unwrap()
            .contains("x-csrf-token"));
    }

    #[test]
    fn normalizes_cookie_names_and_rejects_insecure_redirects() {
        let jar = CookieJar::from_map(BTreeMap::from([("session".into(), "value".into())]));
        assert!(jar.has_tis_session());
        assert!(ensure_allowed_host("http://tis.sustech.edu.cn/cas").is_err());
        assert!(ensure_allowed_host("https://tis.sustech.edu.cn:8443/cas").is_err());
        assert!(ensure_allowed_host("https://user:pass@tis.sustech.edu.cn/cas").is_err());
    }

    #[test]
    fn truncation_is_utf8_safe() {
        let value = "课".repeat(300);
        let result = truncate(&value);
        assert!(result.ends_with('…'));
        assert!(result.is_char_boundary(result.len() - '…'.len_utf8()));
    }

    #[test]
    fn accepts_only_tis_main_ticket_redirects() {
        assert!(is_tis_main_location(
            "https://tis.sustech.edu.cn/authentication/main"
        ));
        assert!(is_tis_main_location("/authentication/main"));
        assert!(!is_tis_main_location("https://tis.sustech.edu.cn/"));
        assert!(!is_tis_main_location(
            "https://cas.sustech.edu.cn/authentication/main"
        ));
        assert!(!is_tis_main_location(
            "https://tis.sustech.edu.cn/authentication/main/other"
        ));
    }

    #[test]
    fn request_endpoint_omits_query_and_fragment() {
        assert_eq!(
            request_endpoint("https://cas.sustech.edu.cn/cas/login?service=secret#fragment"),
            "cas.sustech.edu.cn/cas/login"
        );
    }

    #[test]
    fn rejects_malformed_course_items() {
        let value = json!({"kxrwList":{"list":[{"rwmc":"missing id"}]}});
        assert!(parse_courses(&value, "xxxk").is_err());
    }
}
