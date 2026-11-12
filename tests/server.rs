//! Integration tests for the render server: each test starts `blueline serve`
//! on a free port and talks to it over plain HTTP.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long to wait for a reload that should come. File events can take a
/// moment on macOS.
const RELOAD_WAIT: Duration = Duration::from_secs(10);
/// How long to watch for a reload that should not come.
const QUIET_WAIT: Duration = Duration::from_millis(1500);

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_blueline"))
            .args(["serve", "--port", "0"])
            .env_remove("BLUELINE_PORT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the server");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .expect("read the listening line");
        let port = line
            .trim()
            .rsplit(':')
            .next()
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("unexpected first line: {line:?}"));
        Server { child, port }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn get(&self, target: &str) -> Reply {
        self.request("GET", target, &[], None)
    }

    fn open(&self, path: &Path, current: Option<bool>) -> Reply {
        let mut body = serde_json::json!({ "path": path });
        if let Some(current) = current {
            body["current"] = current.into();
        }
        self.request(
            "POST",
            "/open",
            &[("content-type", "application/json")],
            Some(&body.to_string()),
        )
    }

    fn request(
        &self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Reply {
        let host = self.host();
        let mut all = vec![("host", host.as_str())];
        all.extend_from_slice(headers);
        request(self.port, method, target, &all, body)
    }

    /// Opens a reload stream and waits until the server has answered it, so the
    /// page is counted by the time this returns.
    fn events(&self, target: &str) -> Events {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nhost: {}\r\naccept: text/event-stream\r\n\r\n",
            self.host()
        )
        .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        let (lines, received) = mpsc::channel();
        thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        Events {
            status: status.split_whitespace().nth(1).unwrap().parse().unwrap(),
            stream,
            lines: received,
        }
    }
}

impl Drop for Server {
    /// Asks the server to exit rather than killing it, so that a coverage run
    /// gets to write the server's profile on the way out. Killed only if it
    /// does not go.
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let host = self.host();
            let _ = std::panic::catch_unwind(|| {
                request(self.port, "POST", "/shutdown", &[("host", &host)], None)
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|_| panic!("not JSON ({}): {}", self.status, self.body))
    }
}

fn request(
    port: u16,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut head = format!("{method} {target} HTTP/1.1\r\nconnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let body = body.unwrap_or("");
    head.push_str(&format!("content-length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(head.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").expect("a complete reply");
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: body.to_owned(),
    }
}

struct Events {
    status: u16,
    stream: TcpStream,
    lines: mpsc::Receiver<String>,
}

impl Events {
    /// Whether a reload arrives within `wait`.
    fn reloaded(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line == "data: reload" => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }

    fn close(self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn query(path: &Path) -> String {
    form_urlencoded::byte_serialize(path.to_string_lossy().as_bytes()).collect()
}

/// A temporary directory with its symlinks resolved, so the paths the server
/// reports compare equal to the ones the test built.
fn workdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
}

/// Gives a just-connected stream's directory watch time to arm.
fn settle() {
    thread::sleep(Duration::from_millis(300));
}

#[test]
fn health_reports_the_server_and_its_files() {
    let server = Server::start();
    let health = server.get("/health").json();
    assert_eq!(health["ok"], true);
    assert_eq!(health["server"], "blueline");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(health["pid"], server.child.id());
    assert!(health["exe"].is_string());
    assert!(health["file"].is_null());
    assert!(health["recent"].is_null());

    let (_dir, root) = workdir();
    let pane = root.join("pane.md");
    let tab = root.join("tab.md");
    write(&pane, "pane");
    write(&tab, "tab");
    assert_eq!(server.open(&pane, None).status, 200);
    assert_eq!(server.open(&tab, Some(false)).status, 200);
    let health = server.get("/health").json();
    assert_eq!(health["file"], pane.to_str().unwrap());
    assert_eq!(health["recent"], tab.to_str().unwrap());
}

#[test]
fn foreign_hosts_are_refused() {
    let server = Server::start();
    for host in ["evil.example:80", "127.0.0.1", "localhost:1"] {
        let reply = request(server.port, "GET", "/health", &[("host", host)], None);
        assert_eq!(reply.status, 403, "{host}");
    }
    let localhost = format!("localhost:{}", server.port);
    let reply = request(server.port, "GET", "/health", &[("host", &localhost)], None);
    assert_eq!(reply.status, 200);
}

#[test]
fn open_validates_its_input() {
    let server = Server::start();
    let (_dir, root) = workdir();
    let doc = root.join("doc.md");
    let text = root.join("doc.txt");
    write(&doc, "# doc");
    write(&text, "text");

    let reply = server.request(
        "POST",
        "/open",
        &[("origin", "https://evil.example")],
        Some(&serde_json::json!({ "path": doc }).to_string()),
    );
    assert_eq!(reply.status, 403);

    let reply = server.request("POST", "/open", &[], Some("{}"));
    assert_eq!(reply.status, 400);
    assert_eq!(reply.json()["error"], "missing path");

    let reply = server.open(&text, None);
    assert_eq!(reply.status, 400);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap()
            .contains("not a markdown file")
    );

    let reply = server.open(&root.join("missing.md"), None);
    assert_eq!(reply.status, 400);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap()
            .contains("no such file")
    );

    let reply = server.open(&root.join("sub/../doc.md"), None);
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json(), serde_json::json!({ "ok": true, "file": doc }));

    // GET is not how /open is used.
    assert_eq!(server.get("/open").status, 404);
}

#[test]
fn only_registered_paths_are_served() {
    let server = Server::start();
    let (_dir, root) = workdir();
    let doc = root.join("doc.md");
    let secret = root.join("secret.md");
    write(&doc, "hello *world*");
    write(&secret, "secret");

    for target in [
        format!("/?file={}", query(&secret)),
        format!("/?file={}", query(&root.join("nope.md"))),
        format!("/events?file={}", query(&secret)),
        format!("/image?file={}", query(&secret)),
        "/image".to_owned(),
        "/elsewhere".to_owned(),
        "/assets/nope.js".to_owned(),
    ] {
        let reply = server.get(&target);
        assert_eq!(reply.status, 404, "{target}");
        assert_eq!(reply.body, "not found", "{target}");
    }
    let viewers = server.get(&format!("/viewers?file={}", query(&secret)));
    assert_eq!(
        viewers.json(),
        serde_json::json!({ "ok": true, "count": 0 })
    );

    assert_eq!(server.open(&doc, Some(false)).status, 200);
    let page = server.get(&format!("/?file={}", query(&doc)));
    assert_eq!(page.status, 200);
    assert_eq!(
        page.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert!(page.body.contains("<title>doc.md</title>"));
    assert!(page.body.contains("hello <em>world</em>"));
    assert!(
        page.body
            .contains(&format!("\"/events?file={}&surface=tab\"", query(&doc)))
    );
}

#[test]
fn relative_links_and_images_are_served_through_the_document() {
    let server = Server::start();
    let (_dir, root) = workdir();
    std::fs::create_dir(root.join("sub")).unwrap();
    let doc = root.join("doc.md");
    let linked = root.join("sub/linked doc.md");
    let picture = root.join("sub/pic.svg");
    let unlinked = root.join("sub/unlinked.md");
    write(
        &doc,
        "# Title\n\n## Part\n\n[next](sub/linked%20doc.md#intro) \
         ![pic](sub/pic.svg) [web](https://example.com/x.md)\n",
    );
    write(&linked, "# intro");
    write(&picture, "<svg xmlns=\"http://www.w3.org/2000/svg\"/>");
    write(&unlinked, "nope");
    server.open(&doc, Some(false));

    // Nothing the document points at is served before it has been rendered.
    assert_eq!(
        server
            .get(&format!("/image?file={}", query(&picture)))
            .status,
        404
    );

    let page = server.get(&format!("/?file={}&surface=pane", query(&doc)));
    assert!(page.body.contains(&format!(
        "href=\"/?file={}&amp;surface=pane#intro\"",
        query(&linked)
    )));
    assert!(
        page.body
            .contains(&format!("src=\"/image?file={}\"", query(&picture)))
    );
    assert!(page.body.contains("href=\"https://example.com/x.md\""));
    assert!(
        page.body
            .contains(r##"<li class="toc-l2"><a href="#part">Part</a></li>"##)
    );

    let linked_page = server.get(&format!("/?file={}", query(&linked)));
    assert_eq!(linked_page.status, 200);
    let image = server.get(&format!("/image?file={}", query(&picture)));
    assert_eq!(image.status, 200);
    assert_eq!(image.header("content-type"), Some("image/svg+xml"));
    assert_eq!(image.header("cache-control"), Some("no-store"));
    assert_eq!(image.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        image.header("content-security-policy"),
        Some("default-src 'none'; sandbox")
    );
    // An image is not a page, and a page is not an image.
    assert_eq!(
        server.get(&format!("/?file={}", query(&picture))).status,
        404
    );
    assert_eq!(
        server
            .get(&format!("/image?file={}", query(&linked)))
            .status,
        404
    );
    assert_eq!(
        server.get(&format!("/?file={}", query(&unlinked))).status,
        404
    );
}

#[test]
fn bare_root_shows_the_current_file() {
    let server = Server::start();
    let page = server.get("/");
    assert_eq!(page.status, 200);
    assert!(page.body.contains("No file selected."));

    let (_dir, root) = workdir();
    let pane = root.join("pane.md");
    let tab = root.join("tab.md");
    write(&pane, "pane body");
    write(&tab, "tab body");
    server.open(&pane, None);
    server.open(&tab, Some(false));
    let page = server.get("/?surface=pane");
    assert!(page.body.contains("pane body"));
    assert!(page.body.contains("\"/events?surface=pane\""));
}

#[test]
fn every_page_gets_its_own_nonce() {
    let server = Server::start();
    let nonce = |reply: &Reply| {
        let csp = reply.header("content-security-policy").unwrap().to_owned();
        let start = csp.find("'nonce-").unwrap() + "'nonce-".len();
        let end = start + csp[start..].find('\'').unwrap();
        assert!(csp.starts_with("default-src 'none'; script-src 'nonce-"));
        csp[start..end].to_owned()
    };
    let first = server.get("/");
    let second = server.get("/");
    let (a, b) = (nonce(&first), nonce(&second));
    assert_ne!(a, b);
    assert_eq!(first.body.matches(&format!("nonce=\"{a}\"")).count(), 3);
}

#[test]
fn mermaid_runs_in_strict_mode() {
    let server = Server::start();
    assert!(server.get("/").body.contains("securityLevel: \"strict\""));
}

#[test]
fn assets_are_embedded() {
    let server = Server::start();
    for (path, content_type) in [
        ("/assets/github-markdown.css", "text/css; charset=utf-8"),
        ("/assets/hljs-light.css", "text/css; charset=utf-8"),
        ("/assets/hljs-dark.css", "text/css; charset=utf-8"),
        ("/assets/highlight.js", "text/javascript; charset=utf-8"),
        ("/assets/mermaid.min.js", "text/javascript; charset=utf-8"),
    ] {
        let reply = server.get(path);
        assert_eq!(reply.status, 200, "{path}");
        assert_eq!(reply.header("content-type"), Some(content_type), "{path}");
        assert!(!reply.body.is_empty(), "{path}");
    }
}

#[test]
fn saving_reloads_only_the_pages_showing_that_file() {
    let server = Server::start();
    let (_dir, root) = workdir();
    let a = root.join("a.md");
    let b = root.join("b.md");
    write(&a, "a");
    write(&b, "b");
    server.open(&a, Some(false));
    server.open(&b, Some(false));

    let on_a = server.events(&format!("/events?file={}", query(&a)));
    let on_b = server.events(&format!("/events?file={}", query(&b)));
    assert_eq!(on_a.status, 200);
    settle();

    write(&a, "a, edited");
    assert!(on_a.reloaded(RELOAD_WAIT), "a's page should reload");
    assert!(!on_b.reloaded(QUIET_WAIT), "b's page should not reload");
}

#[test]
fn saving_by_rename_still_reloads() {
    let server = Server::start();
    let (_dir, root) = workdir();
    let doc = root.join("doc.md");
    write(&doc, "one");
    server.open(&doc, Some(false));
    let page = server.events(&format!("/events?file={}", query(&doc)));
    settle();

    for round in ["two", "three"] {
        let temp = root.join(".doc.md.swp");
        write(&temp, round);
        std::fs::rename(&temp, &doc).unwrap();
        assert!(page.reloaded(RELOAD_WAIT), "save {round} should reload");
        // Let the debounce settle so the next round is a separate reload.
        while page.reloaded(Duration::from_millis(300)) {}
    }
}

#[test]
fn bare_root_follows_the_current_file() {
    let server = Server::start();
    let (_dir, root) = workdir();
    let first = root.join("first.md");
    let second = root.join("second.md");
    write(&first, "1");
    write(&second, "2");
    server.open(&first, None);
    let page = server.events("/events?surface=pane");
    settle();

    // A tab preview does not move a bare /.
    server.open(&second, Some(false));
    assert!(!page.reloaded(QUIET_WAIT));

    // A pane preview does, and the page follows the new file's saves.
    server.open(&second, None);
    assert!(page.reloaded(RELOAD_WAIT));
    while page.reloaded(Duration::from_millis(300)) {}
    settle();
    write(&second, "2, edited");
    assert!(page.reloaded(RELOAD_WAIT));
}

#[test]
fn opening_again_reloads_the_open_pages() {
    let server = Server::start();
    let (_dir, root) = workdir();
    let doc = root.join("doc.md");
    write(&doc, "doc");
    server.open(&doc, Some(false));
    let page = server.events(&format!("/events?file={}", query(&doc)));
    server.open(&doc, Some(false));
    assert!(page.reloaded(RELOAD_WAIT));
}

#[test]
fn viewers_count_connected_tabs_only() {
    let server = Server::start();
    let (_dir, root) = workdir();
    let doc = root.join("doc.md");
    write(&doc, "doc");
    server.open(&doc, Some(false));
    let viewers = || {
        server.get(&format!("/viewers?file={}", query(&doc))).json()["count"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(viewers(), 0);

    let tab = server.events(&format!("/events?file={}&surface=tab", query(&doc)));
    let _pane = server.events(&format!("/events?file={}&surface=pane", query(&doc)));
    let _unknown = server.events(&format!("/events?file={}&surface=what", query(&doc)));
    assert_eq!(viewers(), 2);

    // A page that goes away stops counting once the server notices.
    tab.close();
    let deadline = Instant::now() + Duration::from_secs(10);
    while viewers() != 1 {
        assert!(
            Instant::now() < deadline,
            "the closed tab was never dropped"
        );
        thread::sleep(Duration::from_millis(200));
    }

    let reply = server.request(
        "GET",
        &format!("/viewers?file={}", query(&doc)),
        &[("origin", "https://evil.example")],
        None,
    );
    assert_eq!(reply.status, 403);
}

#[test]
fn shutdown_stops_the_server() {
    let mut server = Server::start();
    let reply = server.request("POST", "/shutdown", &[("origin", "http://x")], None);
    assert_eq!(reply.status, 403);
    let reply = server.request("POST", "/shutdown", &[], None);
    assert_eq!(reply.json(), serde_json::json!({ "ok": true }));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "the server did not exit");
        thread::sleep(Duration::from_millis(50));
    }
}
