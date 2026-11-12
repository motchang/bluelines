//! The render server: plain HTTP on the loopback port. It serves rendered
//! pages for registered files, the embedded assets, and a reload stream per
//! page that fires when the file is saved.
//!
//! Which file a page shows is carried in its own URL, `/?file=<path>`, so every
//! surface pointed at this server is independent: a pane and a browser tab, or
//! two tabs, can hold different files. The reload channels are keyed the same
//! way, so saving a file only reloads the pages showing it. A bare `/` renders
//! the file the pane modes last opened, and its reload channel follows that.
//!
//! Only paths that have been registered are ever read. A path is registered by
//! `POST /open`, which only local tooling can reach, or by being the target of a
//! relative link inside a document that was already registered. Anything else
//! answers 404, the same answer for a file that exists and one that does not.
//!
//! Nothing here trusts a request for coming from this machine: the loopback
//! port is open to every local user and process. The tooling routes need the
//! token in this user's token file (see `auth`), and pages, their images and
//! their reload streams need the view key, which a browser gets by redeeming a
//! ticket the CLI asked for. A ticket is good once, so the URL the CLI hands to
//! a browser, which other users can read in its command line, is spent by the
//! time they could use it. The key then rides in the page's own URLs rather
//! than in a cookie: cookies are not kept apart by port, so a browser would
//! hand one to every other server on 127.0.0.1 it visits.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures_util::Stream;
use notify::{EventKind, RecursiveMode, Watcher as _};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::assets;
use crate::auth;
use crate::cli::ServeArgs;
use crate::render::{self, Reference, escape_html};

/// The image suffixes `/image` serves, and the content type for each. A suffix
/// with no entry is left alone rather than guessed at, since the content type
/// is what the browser acts on.
const IMAGE_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
    ("ico", "image/x-icon"),
    ("svg", "image/svg+xml"),
];

/// How long after the last file event a reload goes out. Editors that save by
/// writing a temporary file and renaming it produce a burst of events.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(50);

/// How long a ticket from `/ticket` can wait to be redeemed. A cold
/// terminal-browser takes a few seconds to load its first page.
const TICKET_LIFETIME: Duration = Duration::from_secs(120);

/// Unredeemed tickets kept at most; the oldest goes first.
const TICKETS_KEPT: usize = 32;

/// How often an idle reload stream gets a comment line. Writing is the only way
/// to notice that a page has gone away, and `/viewers` counts on noticing.
const KEEP_ALIVE: Duration = Duration::from_secs(1);

/// Runs the server in the foreground until it is shut down.
pub fn run(args: &ServeArgs) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("blueline-server: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(serve(args.port))
}

async fn serve(port: u16) -> ExitCode {
    let listener =
        match tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("blueline-server: cannot listen on 127.0.0.1:{port}: {error}");
                return ExitCode::FAILURE;
            }
        };
    // Port 0 asks for any free port; everything after this uses the real one.
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(error) => {
            eprintln!("blueline-server: cannot read the bound address: {error}");
            return ExitCode::FAILURE;
        }
    };
    ignore_hangup();

    // Written once the port is ours, so a server that lost the race for it
    // never overwrites the token of the one that won.
    let token = auth::new_token();
    if let Err(error) = auth::write_token(port, &token) {
        eprintln!("blueline-server: cannot write the token file: {error}");
        return ExitCode::FAILURE;
    }
    let state = Shared::new(port, token.clone());
    let app = Router::new().fallback(handle).with_state(state);
    println!("blueline-server listening on http://127.0.0.1:{port}");
    let served = axum::serve(listener, app).await;
    auth::remove_token(port, &token);
    if let Err(error) = served {
        eprintln!("blueline-server: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// The CLI starts the server detached from the terminal it was run in, and
/// closing that terminal must not take the server with it. Handling SIGHUP is
/// what `nohup` did for the sh version.
fn ignore_hangup() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::hangup()) {
            Ok(mut hangups) => {
                tokio::spawn(async move { while hangups.recv().await.is_some() {} });
            }
            Err(error) => eprintln!("blueline-server: cannot handle SIGHUP: {error}"),
        }
    }
}

// ---------------------------------------------------------------------------
// State

/// A reload channel's key: a registered path, or `None` for the channel that
/// follows whatever file is current.
type Key = Option<PathBuf>;

/// The surface a page declared. Both are a browser speaking HTTP; nothing about
/// the connection says which is which, so the page says so in its URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    Pane,
    Tab,
}

impl Surface {
    /// An unknown surface is read as a tab: miscounting a pane as a tab at
    /// worst suppresses one tab, while the reverse stacks tabs on every run.
    fn parse(value: Option<&str>) -> Self {
        if value == Some("pane") {
            Surface::Pane
        } else {
            Surface::Tab
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Surface::Pane => "pane",
            Surface::Tab => "tab",
        }
    }
}

struct Client {
    surface: Surface,
    reload: mpsc::UnboundedSender<()>,
}

/// The pages looking at one file, and the watch that reloads them. A path is
/// watched while at least one page is connected to it and not otherwise.
#[derive(Default)]
struct Channel {
    clients: HashMap<u64, Client>,
    watch: Option<Watch>,
}

/// A directory watch and the task that turns its events into reloads. Dropping
/// it stops both.
struct Watch {
    _watchers: Vec<notify::RecommendedWatcher>,
    task: JoinHandle<()>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Inner {
    port: u16,
    /// The tooling routes' bearer token, also in this user's token file.
    token: String,
    /// The key a browser gets for redeeming a ticket, which lets it read
    /// pages. Made fresh for each run: a tab left over from an earlier server
    /// has to be handed a ticket again.
    view_key: String,
    /// Tickets `/ticket` handed out and nobody has redeemed yet.
    tickets: Vec<(String, Instant)>,
    /// Every path `/open` accepted or a registered document linked to; the only
    /// paths `?file=` renders. It only grows: evicting would mean refusing a
    /// path someone may still have a tab on.
    registered: HashSet<PathBuf>,
    /// Every image a rendered document pointed at, with its content type; the
    /// only files `/image` serves. Kept apart from `registered` so that an image
    /// reference cannot get a file rendered and a link cannot get one served raw.
    images: HashMap<PathBuf, &'static str>,
    /// The file a bare `/` renders. Only `/open` with `current` not false moves
    /// it, so it is the file a pane last opened.
    last_opened: Option<PathBuf>,
    /// The most recent `/open` of any kind.
    last_registered: Option<PathBuf>,
    channels: HashMap<Key, Channel>,
    next_client: u64,
}

#[derive(Clone)]
struct Shared(Arc<Mutex<Inner>>);

impl Shared {
    fn new(port: u16, token: String) -> Self {
        Shared(Arc::new(Mutex::new(Inner {
            port,
            token,
            view_key: auth::random_hex(32),
            tickets: Vec::new(),
            registered: HashSet::new(),
            images: HashMap::new(),
            last_opened: None,
            last_registered: None,
            channels: HashMap::new(),
            next_client: 0,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Nothing holding the lock leaves the state half-updated in a way a
        // later request could trip over, so a poisoned lock is still usable.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Tells every page on `key` to reload. A page whose stream has gone away
    /// is dropped, and a channel left with nobody is closed.
    fn broadcast_reload(&self, key: &Key) {
        let mut inner = self.lock();
        let Some(channel) = inner.channels.get_mut(key) else {
            return;
        };
        channel
            .clients
            .retain(|_, client| client.reload.send(()).is_ok());
        if channel.clients.is_empty() {
            inner.channels.remove(key);
        }
    }

    fn subscribe(&self, key: Key, surface: Surface) -> (u64, mpsc::UnboundedReceiver<()>) {
        let (reload, receiver) = mpsc::unbounded_channel();
        let mut inner = self.lock();
        let id = inner.next_client;
        inner.next_client += 1;
        let channel = inner.channels.entry(key.clone()).or_default();
        let first = channel.clients.is_empty();
        channel.clients.insert(id, Client { surface, reload });
        if first {
            self.start_watching(&mut inner, &key);
        }
        (id, receiver)
    }

    fn unsubscribe(&self, key: &Key, id: u64) {
        let mut inner = self.lock();
        let Some(channel) = inner.channels.get_mut(key) else {
            return;
        };
        channel.clients.remove(&id);
        if channel.clients.is_empty() {
            inner.channels.remove(key);
        }
    }

    /// (Re)starts the watch for `key`'s channel.
    ///
    /// The containing directory is watched rather than the file, because
    /// editors that save by renaming a temporary file over the original break a
    /// watch on the file itself. A symlinked file has its real directory watched
    /// as well, since writes land there. Any event in those directories after
    /// which the file's stat fingerprint has changed reloads.
    ///
    /// The fingerprint is the whole test, and the event's file name plays no
    /// part. macOS reports only the source name of a rename, so the name cannot
    /// be relied on to show up, and FSEvents can deliver an event for a write
    /// made just before the watch started, which would reload a page for a
    /// change it already shows. A save always changes the fingerprint: the
    /// inode on a save by rename, the size or the nanosecond mtime otherwise.
    ///
    /// A directory that cannot be watched is skipped. If none can, the channel
    /// is left connected and silent until `/open` registers the path again.
    fn start_watching(&self, inner: &mut Inner, key: &Key) {
        let file = match key {
            Some(path) => Some(path.clone()),
            None => inner.last_opened.clone(),
        };
        let Some(channel) = inner.channels.get_mut(key) else {
            return;
        };
        // Drop the old watch first so its task cannot reload after this.
        channel.watch = None;
        let Some(file) = file else {
            return;
        };

        let (events, incoming) = mpsc::unbounded_channel::<()>();
        let watchers = watch_directories(&file, &events);
        if watchers.is_empty() {
            return;
        }
        let task = tokio::spawn(reload_on_change(self.clone(), key.clone(), file, incoming));
        channel.watch = Some(Watch {
            _watchers: watchers,
            task,
        });
    }

    /// Re-arms the watch on a path that has pages connected. This is how a
    /// channel whose directory went away recovers: running `blueline` again
    /// re-registers the file.
    fn rearm_watching(&self, key: &Key) {
        let mut inner = self.lock();
        if inner
            .channels
            .get(key)
            .is_some_and(|channel| !channel.clients.is_empty())
        {
            self.start_watching(&mut inner, key);
        }
    }

    /// Points the follow-the-current-file channel at the new current file and
    /// reloads the pages on it.
    fn retarget_current(&self) {
        {
            let mut inner = self.lock();
            if !inner.channels.contains_key(&None) {
                return;
            }
            self.start_watching(&mut inner, &None);
        }
        self.broadcast_reload(&None);
    }

    /// Registers a markdown file and, if `make_current`, makes it the file a
    /// bare `/` shows. A browser-tab preview registers without moving it, so a
    /// tab never drags a pane on `/` onto a different file.
    fn register_file(&self, raw: &str, make_current: bool) -> Result<PathBuf, String> {
        let file = resolve(&cwd(), Path::new(raw));
        if !render::is_markdown(&file) {
            return Err(format!("not a markdown file: {}", file.display()));
        }
        if !file.is_file() {
            return Err(format!("no such file: {}", file.display()));
        }
        let mut inner = self.lock();
        inner.registered.insert(file.clone());
        inner.last_registered = Some(file.clone());
        if make_current {
            inner.last_opened = Some(file.clone());
        }
        Ok(file)
    }

    /// The channel and file a request's `?file=` asks for, or `None` when it
    /// names a path that was never registered.
    fn target_for(&self, query: &Query) -> Option<(Key, Option<PathBuf>)> {
        let inner = self.lock();
        match query.get("file") {
            None => Some((None, inner.last_opened.clone())),
            Some(raw) => {
                let file = resolve(&cwd(), Path::new(raw));
                inner
                    .registered
                    .contains(&file)
                    .then(|| (Some(file.clone()), Some(file)))
            }
        }
    }

    /// A new single-use ticket. Unredeemed ones expire, and only the newest
    /// few are kept.
    fn issue_ticket(&self) -> String {
        let ticket = auth::random_hex(32);
        let mut inner = self.lock();
        inner
            .tickets
            .retain(|(_, issued)| issued.elapsed() < TICKET_LIFETIME);
        if inner.tickets.len() >= TICKETS_KEPT {
            inner.tickets.remove(0);
        }
        inner.tickets.push((ticket.clone(), Instant::now()));
        ticket
    }

    /// Spends `ticket`, returning whether it was a live one.
    fn redeem_ticket(&self, ticket: &str) -> bool {
        let mut inner = self.lock();
        inner
            .tickets
            .retain(|(_, issued)| issued.elapsed() < TICKET_LIFETIME);
        let found = inner
            .tickets
            .iter()
            .position(|(issued, _)| auth::same_secret(issued, ticket));
        found.map(|at| inner.tickets.remove(at)).is_some()
    }

    /// Whether a request carries the tooling token.
    fn has_token(&self, headers: &HeaderMap) -> bool {
        let token = self.lock().token.clone();
        headers
            .get_all(header::AUTHORIZATION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.strip_prefix("Bearer "))
            .any(|offered| auth::same_secret(offered.trim(), &token))
    }

    /// Whether a request's query carries the view key.
    fn has_view_key(&self, query: &Query) -> bool {
        let key = self.view_key();
        query
            .get("key")
            .is_some_and(|offered| auth::same_secret(offered, &key))
    }

    fn view_key(&self) -> String {
        self.lock().view_key.clone()
    }

    /// Browser tabs connected to `key`, not counting panes.
    fn tabs_showing(&self, key: &Key) -> usize {
        self.lock().channels.get(key).map_or(0, |channel| {
            channel
                .clients
                .values()
                .filter(|client| client.surface == Surface::Tab)
                .count()
        })
    }
}

/// Watches the directory holding `file`, and its real directory when it is a
/// symlink, sending a signal on every event but an access. Directories that
/// cannot be watched are skipped.
fn watch_directories(
    file: &Path,
    events: &mpsc::UnboundedSender<()>,
) -> Vec<notify::RecommendedWatcher> {
    let dirs: HashSet<PathBuf> = [file.to_path_buf(), real_path(file)]
        .iter()
        .filter_map(|path| path.parent().map(real_path))
        .collect();
    dirs.iter()
        .filter_map(|dir| {
            let events = events.clone();
            let mut watcher =
                notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                    // Reading the file to render it is itself an access event.
                    if result.is_ok_and(|event| !matches!(event.kind, EventKind::Access(_))) {
                        let _ = events.send(());
                    }
                })
                .ok()?;
            watcher.watch(dir, RecursiveMode::NonRecursive).ok()?;
            Some(watcher)
        })
        .collect()
}

/// Reloads `key`'s pages, debounced, whenever a directory event finds the
/// file's fingerprint changed.
async fn reload_on_change(
    shared: Shared,
    key: Key,
    file: PathBuf,
    mut incoming: mpsc::UnboundedReceiver<()>,
) {
    // The path as opened, not the resolved one: metadata follows the link, so
    // one fingerprint covers an edit to the real file and a link repointed
    // somewhere else.
    let mut watched = fingerprint(&file);
    let mut deadline: Option<Instant> = None;
    loop {
        tokio::select! {
            event = incoming.recv() => {
                if event.is_none() {
                    return;
                }
                let state = fingerprint(&file);
                if state != watched {
                    watched = state;
                    deadline = Some(Instant::now() + RELOAD_DEBOUNCE);
                }
            }
            () = sleep_until(deadline), if deadline.is_some() => {
                deadline = None;
                shared.broadcast_reload(&key);
            }
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// A stat fingerprint of `file`. The inode changes when an editor renames a
/// new file over the original, and `None` covers the moment it is missing.
fn fingerprint(file: &Path) -> Option<(u64, u64, Option<std::time::SystemTime>)> {
    let meta = std::fs::metadata(file).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some((inode, meta.len(), meta.modified().ok()))
}

/// `path` with every symlink resolved, or as given when that fails.
fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// `path` made absolute against `base` and normalized lexically: `.` dropped
/// and `..` applied, without touching the file system. Symlinks are kept, so a
/// path registers under the spelling it was opened with.
fn resolve(base: &Path, path: &Path) -> PathBuf {
    let joined = base.join(path);
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Requests

/// A parsed query string. `get` returns the first value, as
/// `URLSearchParams.get` does.
struct Query(Vec<(String, String)>);

impl Query {
    fn parse(request: &Request) -> Self {
        let pairs = request
            .uri()
            .query()
            .map(|query| {
                form_urlencoded::parse(query.as_bytes())
                    .into_owned()
                    .collect()
            })
            .unwrap_or_default();
        Query(pairs)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// `/` with this query, less every `old` and `new` parameter, and with
    /// `new` set to `value` at the end.
    fn replacing(&self, old: &str, new: &str, value: &str) -> String {
        let mut params = form_urlencoded::Serializer::new(String::new());
        for (key, value) in self.0.iter().filter(|(key, _)| key != old && key != new) {
            params.append_pair(key, value);
        }
        params.append_pair(new, value);
        format!("/?{}", params.finish())
    }
}

async fn handle(State(state): State<Shared>, request: Request) -> Response {
    if !host_allowed(&request, state.lock().port) {
        return text(StatusCode::FORBIDDEN, "forbidden");
    }
    let query = Query::parse(&request);
    match request.uri().path() {
        "/open" | "/shutdown" | "/viewers" | "/ticket" => tooling(&state, request, &query).await,
        "/health" => health(&state, request.headers()),
        "/" | "/events" | "/image" => viewing(&state, request, &query).await,
        path => asset(path),
    }
}

/// The routes that read the user's files: pages, their images and their reload
/// streams. Each needs the view key in its query, and a page URL carrying a
/// ticket is how a browser gets it.
async fn viewing(state: &Shared, request: Request, query: &Query) -> Response {
    let path = request.uri().path().to_owned();
    if path == "/" {
        if let Some(ticket) = query.get("ticket") {
            return redeem(state, query, ticket);
        }
    }
    if !state.has_view_key(query) {
        return not_handed_over();
    }
    match path.as_str() {
        "/events" => events(state, query),
        "/image" => image(state, query).await,
        _ => page(state, query).await,
    }
}

fn not_handed_over() -> Response {
    text(
        StatusCode::FORBIDDEN,
        "This preview has not been handed to this browser. Run 'blueline <file.md>' to open it.",
    )
}

/// Spends a ticket and sends the browser on to the same URL with the view key
/// in place of it, so that a reload or a bookmark never carries a spent
/// ticket. A spent or unknown ticket gets nothing.
fn redeem(state: &Shared, query: &Query, ticket: &str) -> Response {
    if !state.redeem_ticket(ticket) {
        return not_handed_over();
    }
    let location = query.replacing("ticket", "key", &state.view_key());
    let mut headers = HeaderMap::new();
    if let Ok(location) = HeaderValue::from_str(&location) {
        headers.insert(header::LOCATION, location);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    (StatusCode::SEE_OTHER, headers).into_response()
}

/// Whether the Host header names this server. A DNS rebinding attack resolves
/// the attacker's hostname to 127.0.0.1, and the Host header gives it away.
fn host_allowed(request: &Request, port: u16) -> bool {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    [format!("127.0.0.1:{port}"), format!("localhost:{port}")].contains(&host.to_owned())
}

/// The routes only the CLI uses, which need the token from this user's token
/// file. A page in a browser attaches an Origin to every cross-origin POST, so
/// its presence means the request came from somewhere else.
async fn tooling(state: &Shared, request: Request, query: &Query) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return text(StatusCode::FORBIDDEN, "forbidden");
    }
    if !state.has_token(request.headers()) {
        return text(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let path = request.uri().path().to_owned();
    match (path.as_str(), request.method()) {
        ("/open", &Method::POST) => open(state, request).await,
        ("/shutdown", &Method::POST) => shutdown(state),
        ("/ticket", &Method::POST) => json_response(
            StatusCode::OK,
            json!({ "ok": true, "ticket": state.issue_ticket() }),
        ),
        ("/viewers", _) => viewers(state, query),
        _ => text(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Browser tabs showing a path. An unregistered path has no viewers; answering
/// 0 rather than 404 keeps this from telling anyone which paths are registered.
fn viewers(state: &Shared, query: &Query) -> Response {
    let count = state
        .target_for(query)
        .map_or(0, |(key, _)| state.tabs_showing(&key));
    json_response(StatusCode::OK, json!({ "ok": true, "count": count }))
}

fn asset(path: &str) -> Response {
    match assets::get(path) {
        Some(asset) => (
            [(header::CONTENT_TYPE, asset.content_type)],
            Bytes::from_static(asset.bytes),
        )
            .into_response(),
        None => text(StatusCode::NOT_FOUND, "not found"),
    }
}

fn text(status: StatusCode, body: &'static str) -> Response {
    (status, body).into_response()
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    (status, axum::Json(body)).into_response()
}

fn path_json(path: Option<&Path>) -> serde_json::Value {
    path.map_or(serde_json::Value::Null, |path| {
        json!(path.to_string_lossy())
    })
}

/// Says which program and version this is, so the CLI can tell blueline
/// apart from whatever else might hold the port. With a challenge it also
/// answers with a proof of knowing the token, which is how the CLI tells its
/// own server from a process that took the port first.
///
/// `file` (what a bare `/` renders) and `recent` (the most recent `/open` of
/// any kind) name the user's files, so only a request with the token gets them.
fn health(state: &Shared, headers: &HeaderMap) -> Response {
    let exe = std::env::current_exe()
        .map(|exe| json!(exe.to_string_lossy()))
        .unwrap_or(serde_json::Value::Null);
    let mut body = json!({
        "ok": true,
        "server": "blueline",
        "version": env!("CARGO_PKG_VERSION"),
        "exe": exe,
    });
    let challenge = headers
        .get(auth::CHALLENGE_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|challenge| !challenge.is_empty() && challenge.len() <= 128);
    if let Some(challenge) = challenge {
        body["proof"] = json!(auth::proof(&state.lock().token, challenge));
    }
    if state.has_token(headers) {
        let inner = state.lock();
        body["file"] = path_json(inner.last_opened.as_deref());
        body["recent"] = path_json(inner.last_registered.as_deref());
    }
    json_response(StatusCode::OK, body)
}

#[derive(Deserialize)]
struct OpenRequest {
    path: Option<String>,
    current: Option<bool>,
}

async fn open(state: &Shared, request: Request) -> Response {
    let fail = |error: String| {
        json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": error }),
        )
    };
    let body = match axum::body::to_bytes(request.into_body(), 1 << 20).await {
        Ok(body) => body,
        Err(error) => return fail(format!("cannot read the request: {error}")),
    };
    let open: OpenRequest = match serde_json::from_slice(&body) {
        Ok(open) => open,
        Err(error) => return fail(format!("bad request: {error}")),
    };
    let Some(path) = open.path.filter(|path| !path.is_empty()) else {
        return fail("missing path".to_owned());
    };
    // Absent means current, so an older caller still works.
    let make_current = open.current != Some(false);
    let file = match state.register_file(&path, make_current) {
        Ok(file) => file,
        Err(error) => return fail(error),
    };
    let key = Some(file.clone());
    // Before the reload: a channel whose watch was given up on is watching
    // nothing until this runs, and a page reloaded first would come back to a
    // still-dead stream.
    state.rearm_watching(&key);
    state.broadcast_reload(&key);
    if make_current {
        state.retarget_current();
    }
    json_response(
        StatusCode::OK,
        json!({ "ok": true, "file": file.to_string_lossy() }),
    )
}

/// Answers, then exits. A graceful shutdown would wait for the reload streams,
/// which never end on their own.
fn shutdown(state: &Shared) -> Response {
    eprintln!("blueline-server: shutting down on request");
    let (port, token) = {
        let inner = state.lock();
        (inner.port, inner.token.clone())
    };
    auth::remove_token(port, &token);
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::process::exit(0);
    });
    json_response(StatusCode::OK, json!({ "ok": true }))
}

/// Holds a page's place in its channel for as long as its stream lives.
struct Subscription {
    state: Shared,
    key: Key,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.state.unsubscribe(&self.key, self.id);
    }
}

fn events(state: &Shared, query: &Query) -> Response {
    let Some((key, _)) = state.target_for(query) else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let surface = Surface::parse(query.get("surface"));
    let (id, receiver) = state.subscribe(key.clone(), surface);
    let subscription = Subscription {
        state: state.clone(),
        key,
        id,
    };
    let stream = reload_stream(subscription, receiver);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

fn reload_stream(
    subscription: Subscription,
    receiver: mpsc::UnboundedReceiver<()>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold(
        (subscription, receiver),
        |(subscription, mut receiver)| async move {
            receiver.recv().await?;
            Some((
                Ok(Event::default().data("reload")),
                (subscription, receiver),
            ))
        },
    )
}

/// One image a rendered document pointed at. Whether it may be served was
/// settled when the document was rendered; this only looks it up.
///
/// The CSP and nosniff are for SVG, which is also a document: navigated to
/// directly, it would otherwise run as a page on this origin. `no-store`
/// because nothing reloads a page when an image under it changes.
async fn image(state: &Shared, query: &Query) -> Response {
    let Some(raw) = query.get("file") else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let file = resolve(&cwd(), Path::new(raw));
    let Some(content_type) = state.lock().images.get(&file).copied() else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let Ok(bytes) = tokio::fs::read(&file).await else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    (headers, Body::from(bytes)).into_response()
}

// ---------------------------------------------------------------------------
// Pages

async fn page(state: &Shared, query: &Query) -> Response {
    let nonce = new_nonce();
    let Some((_, file)) = state.target_for(query) else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let surface = Surface::parse(query.get("surface"));
    let key = state.view_key();
    let events = events_url(query.get("file"), surface, &key);
    let Some(file) = file else {
        let shell = Shell {
            nonce: &nonce,
            title: "blueline",
            path: "",
            body: "<p>No file selected.</p>",
            events: &events,
            toc: "",
        };
        return html_response(&nonce, shell.render());
    };
    let title = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let shown = file.to_string_lossy();
    let source = match tokio::fs::read_to_string(&file).await {
        Ok(source) => source,
        Err(error) => {
            // An editor or a checkout can leave the file missing for a moment.
            // Serve the page anyway so it keeps its reload stream and recovers
            // on the next change.
            let body = format!("<p>{}</p>", escape_html(&error.to_string()));
            let shell = Shell {
                nonce: &nonce,
                title: &title,
                path: &shown,
                body: &body,
                events: &events,
                toc: "",
            };
            return html_response(&nonce, shell.render());
        }
    };

    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut links = LinkRewriter::new(dir, surface, key);
    let rendered = render::render(&source, &mut |kind, url| links.rewrite(kind, url));
    {
        let mut inner = state.lock();
        inner.registered.extend(links.documents);
        inner.images.extend(links.images);
    }
    let toc = render_toc(&rendered.headings);
    let shell = Shell {
        nonce: &nonce,
        title: &title,
        path: &shown,
        body: &rendered.html,
        events: &events,
        toc: &toc,
    };
    html_response(&nonce, shell.render())
}

fn html_response(nonce: &str, body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    if let Ok(csp) = HeaderValue::from_str(&content_security_policy(nonce)) {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    // The page's URL carries the view key, and a link or a remote image must
    // not take it along.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    (headers, body).into_response()
}

/// 16 random bytes, base64. A fresh one for every page.
fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    // The system RNG failing is not something a page can recover from, but a
    // zero nonce is still unique to nothing an attacker could have injected.
    let _ = getrandom::fill(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The page's own scripts carry this response's nonce, and script-src trusts
/// nothing else, so raw HTML in a document cannot run script: an injected
/// `<script>` has no nonce and an `onerror=` attribute cannot have one.
fn content_security_policy(nonce: &str) -> String {
    [
        "default-src 'none'".to_owned(),
        format!("script-src 'nonce-{nonce}'"),
        // The stylesheets are 'self'; mermaid injects <style> at runtime.
        "style-src 'self' 'unsafe-inline'".to_owned(),
        // README badges are remote, and github-markdown-css inlines data: SVGs.
        "img-src 'self' data: https:".to_owned(),
        // The page's EventSource.
        "connect-src 'self'".to_owned(),
        "object-src 'none'".to_owned(),
        "base-uri 'none'".to_owned(),
    ]
    .join("; ")
}

/// The `/events` URL for a page, carrying its file, surface and view key.
fn events_url(file: Option<&str>, surface: Surface, key: &str) -> String {
    let mut params = form_urlencoded::Serializer::new(String::new());
    if let Some(file) = file {
        params.append_pair("file", file);
    }
    params.append_pair("surface", surface.as_str());
    params.append_pair("key", key);
    format!("/events?{}", params.finish())
}

fn render_toc(headings: &[render::Heading]) -> String {
    let shown: Vec<_> = headings
        .iter()
        .filter(|heading| heading.level <= 3)
        .collect();
    if shown.len() < 2 {
        return String::new();
    }
    let items: String = shown
        .iter()
        .map(|heading| {
            format!(
                r##"<li class="toc-l{}"><a href="#{}">{}</a></li>"##,
                heading.level,
                escape_html(&heading.id),
                heading.html
            )
        })
        .collect();
    format!(r#"<nav class="toc"><div class="toc-title">Contents</div><ul>{items}</ul></nav>"#)
}

/// Rewrites the relative links and images in one document into this server's
/// URLs, and collects what that registers. The registrations are applied after
/// rendering, so the renderer never runs under the state lock.
struct LinkRewriter {
    dir: PathBuf,
    surface: Surface,
    /// The view key, which every URL to this server has to carry.
    key: String,
    documents: Vec<PathBuf>,
    images: Vec<(PathBuf, &'static str)>,
}

impl LinkRewriter {
    fn new(dir: PathBuf, surface: Surface, key: String) -> Self {
        LinkRewriter {
            dir,
            surface,
            key,
            documents: Vec::new(),
            images: Vec::new(),
        }
    }

    fn rewrite(&mut self, kind: Reference, url: &str) -> Option<String> {
        match kind {
            Reference::Link => self.link(url),
            Reference::Image => self.image(url),
        }
    }

    /// A relative link to a markdown file next to the document becomes that
    /// file's preview URL, and the file is registered so the URL is served.
    /// Anything else is left as written. The surface rides along so a link
    /// followed inside a pane does not open a page that counts as a tab.
    fn link(&mut self, href: &str) -> Option<String> {
        let (file, hash) = local_target(&self.dir, href)?;
        if !render::is_markdown(&file) {
            return None;
        }
        let params = form_urlencoded::Serializer::new(String::new())
            .append_pair("file", &file.to_string_lossy())
            .append_pair("surface", self.surface.as_str())
            .append_pair("key", &self.key)
            .finish();
        self.documents.push(file);
        Some(format!("/?{params}{hash}"))
    }

    /// A relative image next to the document becomes an `/image` URL. The
    /// fragment is kept for SVG, which can be addressed by view or element id.
    fn image(&mut self, src: &str) -> Option<String> {
        let (file, hash) = local_target(&self.dir, src)?;
        let content_type = image_type(&file)?;
        let params = form_urlencoded::Serializer::new(String::new())
            .append_pair("file", &file.to_string_lossy())
            .append_pair("key", &self.key)
            .finish();
        self.images.push((file, content_type));
        Some(format!("/image?{params}{hash}"))
    }
}

fn image_type(file: &Path) -> Option<&'static str> {
    let ext = file.extension()?.to_str()?;
    IMAGE_TYPES
        .iter()
        .find(|(known, _)| ext.eq_ignore_ascii_case(known))
        .map(|(_, content_type)| *content_type)
}

/// Whether a URL already says where it points: a scheme of its own, or a
/// protocol-relative `//host/path`.
fn is_absolute_url(url: &str) -> bool {
    if url.starts_with("//") {
        return true;
    }
    let mut chars = url.chars();
    if !chars.next().is_some_and(|ch| ch.is_ascii_alphabetic()) {
        return false;
    }
    for ch in chars {
        if ch == ':' {
            return true;
        }
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '+' | '.' | '-')) {
            return false;
        }
    }
    false
}

/// The existing file a relative reference names, with its fragment, or `None`
/// when there is no local file in it. A reference to something that does not
/// exist is left alone, so the browser reports it broken in its own way.
fn local_target(dir: &Path, reference: &str) -> Option<(PathBuf, String)> {
    let raw = reference.trim();
    if raw.is_empty() || raw.starts_with('#') || is_absolute_url(raw) {
        return None;
    }
    let (path, hash) = match raw.find('#') {
        Some(at) => (&raw[..at], &raw[at..]),
        None => (raw, ""),
    };
    if path.is_empty() {
        return None;
    }
    // A reference is a URL, so a space in a file name arrives as %20.
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .map_or_else(|_| path.to_owned(), |decoded| decoded.into_owned());
    let file = resolve(dir, Path::new(&decoded));
    file.is_file().then(|| (file, hash.to_owned()))
}

struct Shell<'a> {
    nonce: &'a str,
    title: &'a str,
    path: &'a str,
    body: &'a str,
    events: &'a str,
    toc: &'a str,
}

impl Shell<'_> {
    fn render(&self) -> String {
        // A JSON string is a valid JS string literal. The URL is already
        // percent-encoded, so no `<` can reach it; escaping it anyway keeps a
        // later change to how the URL is built from turning a path into markup.
        let events = serde_json::to_string(self.events)
            .unwrap_or_else(|_| "\"\"".to_owned())
            .replace('<', "\\u003c");
        let nonce = self.nonce;
        let title = escape_html(self.title);
        let path = escape_html(self.path);
        let body = self.body;
        let toc = self.toc;
        format!(
            r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="stylesheet" href="/assets/github-markdown.css">
<link rel="stylesheet" href="/assets/hljs-light.css" media="(prefers-color-scheme: light)">
<link rel="stylesheet" href="/assets/hljs-dark.css" media="(prefers-color-scheme: dark)">
<style>{STYLE}</style>
</head>
<body>
<div class="layout">
  <div class="content">
    <div class="path">{path}</div>
    <article class="markdown-body">{body}</article>
  </div>
  {toc}
</div>
<script src="/assets/highlight.js" nonce="{nonce}"></script>
<script src="/assets/mermaid.min.js" nonce="{nonce}"></script>
<script nonce="{nonce}">
  hljs.highlightAll();
  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  mermaid.initialize({{ startOnLoad: false, theme: dark ? "dark" : "default" }});
  mermaid.run({{ querySelector: "pre.mermaid" }});
  new EventSource({events}).onmessage = () => location.reload();
</script>
</body>
</html>"#
        )
    }
}

/// The page's own styles.
///
/// github-markdown-css scopes its color variables to `.markdown-body`, so the
/// chrome around the article (body, path, table of contents) cannot use them.
/// These few values are copied from github-markdown-css 5.9.0 for light and
/// dark; bumping that stylesheet means copying them again.
const STYLE: &str = r#"
  :root {
    color-scheme: light dark;
    --bgColor-default: #ffffff;
    --fgColor-default: #1f2328;
    --fgColor-muted: #59636e;
    --borderColor-default: #d1d9e0;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bgColor-default: #0d1117;
      --fgColor-default: #f0f6fc;
      --fgColor-muted: #9198a1;
      --borderColor-default: #3d444d;
    }
  }
  body { margin: 0; background: var(--bgColor-default); color: var(--fgColor-default); }
  .layout { display: flex; gap: 32px; align-items: flex-start; padding: 32px; }
  .content { flex: 1 1 auto; min-width: 0; }
  .path { font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace;
          color: var(--fgColor-muted); margin-bottom: 8px; }
  .markdown-body { border: 1px solid var(--borderColor-default);
                   border-radius: 6px; padding: 32px; }
  .toc { position: sticky; top: 32px; flex: 0 0 240px; font-size: 13px;
         border-left: 1px solid var(--borderColor-default); padding-left: 16px; }
  .toc-title { font-weight: 600; margin-bottom: 8px;
               color: var(--fgColor-muted); }
  .toc ul { list-style: none; margin: 0; padding: 0; }
  .toc li { margin: 4px 0; }
  .toc a { color: var(--fgColor-default); text-decoration: none; }
  .toc a:hover { text-decoration: underline; }
  .toc-l2 { padding-left: 12px; }
  .toc-l3 { padding-left: 24px; }
  pre.mermaid { background: none; text-align: center; }
  @media (max-width: 900px) { .toc { display: none; } }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_is_lexical() {
        let base = Path::new("/home/me/docs");
        assert_eq!(
            resolve(base, Path::new("a.md")),
            PathBuf::from("/home/me/docs/a.md")
        );
        assert_eq!(
            resolve(base, Path::new("../b/./c.md")),
            PathBuf::from("/home/me/b/c.md")
        );
        assert_eq!(
            resolve(base, Path::new("/etc/x.md")),
            PathBuf::from("/etc/x.md")
        );
        assert_eq!(
            resolve(base, Path::new("/../../x.md")),
            PathBuf::from("/x.md")
        );
    }

    #[test]
    fn absolute_urls() {
        for url in [
            "http://x",
            "mailto:a@b",
            "vscode:open",
            "//cdn/x.png",
            "a+b.c-d:e",
        ] {
            assert!(is_absolute_url(url), "{url}");
        }
        for url in ["a.md", "dir/a.md", "./a:b", "1http://x", "#top", ""] {
            assert!(!is_absolute_url(url), "{url}");
        }
    }

    #[test]
    fn links_and_images_rewrite_only_existing_local_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = real_path(dir.path());
        std::fs::write(root.join("other doc.md"), "# x").unwrap();
        std::fs::write(root.join("pic.svg"), "<svg/>").unwrap();
        std::fs::write(root.join("notes.txt"), "x").unwrap();
        std::fs::write(root.join("data.bin"), "x").unwrap();

        let mut links = LinkRewriter::new(root.clone(), Surface::Pane, "k".to_owned());
        let doc = root.join("other doc.md");
        let encoded: String =
            form_urlencoded::byte_serialize(doc.to_string_lossy().as_bytes()).collect();
        assert_eq!(
            links.rewrite(Reference::Link, "other%20doc.md#Part"),
            Some(format!("/?file={encoded}&surface=pane&key=k#Part"))
        );
        assert_eq!(links.rewrite(Reference::Link, "notes.txt"), None);
        assert_eq!(links.rewrite(Reference::Link, "missing.md"), None);
        assert_eq!(
            links.rewrite(Reference::Link, "https://example.com/a.md"),
            None
        );
        assert_eq!(links.rewrite(Reference::Link, "#heading"), None);
        assert_eq!(links.documents, vec![doc]);

        let pic = root.join("pic.svg");
        let encoded: String =
            form_urlencoded::byte_serialize(pic.to_string_lossy().as_bytes()).collect();
        assert_eq!(
            links.rewrite(Reference::Image, "./pic.svg#view"),
            Some(format!("/image?file={encoded}&key=k#view"))
        );
        assert_eq!(links.rewrite(Reference::Image, "data.bin"), None);
        assert_eq!(links.images, vec![(pic, "image/svg+xml")]);
    }

    #[test]
    fn toc_needs_two_shallow_headings() {
        let heading = |level, id: &str| render::Heading {
            level,
            id: id.to_owned(),
            html: format!("<em>{id}</em>"),
        };
        assert_eq!(render_toc(&[heading(1, "a"), heading(4, "b")]), "");
        let toc = render_toc(&[heading(1, "a"), heading(2, "b\""), heading(5, "c")]);
        assert!(toc.contains(r##"<li class="toc-l2"><a href="#b&quot;"><em>b"</em></a></li>"##));
        assert!(!toc.contains("toc-l5"));
    }

    #[test]
    fn events_url_encodes_like_url_search_params() {
        assert_eq!(
            events_url(Some("/a b/</script>&#.md"), Surface::Tab, "k"),
            "/events?file=%2Fa+b%2F%3C%2Fscript%3E%26%23.md&surface=tab&key=k"
        );
        assert_eq!(
            events_url(None, Surface::Pane, "k"),
            "/events?surface=pane&key=k"
        );
    }

    #[test]
    fn nonces_are_fresh() {
        let a = new_nonce();
        assert_eq!(a.len(), 24);
        assert_ne!(a, new_nonce());
    }
}
