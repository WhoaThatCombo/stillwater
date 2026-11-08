/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Compare with Edge: shows how the current page differs from the same page in Microsoft Edge,
//! for when something looks slightly off. Stillwater measures and screenshots the page
//! (`compare-collect.js`), a hidden Edge (headless, muted, on 127.0.0.1, with a throwaway
//! profile) loads the same address twice at the same size and colour scheme, and a report opens
//! in a new tab: a slider between the two, the differences outlined, and a list in words. What
//! changed between Edge's two loads (ads, carousels) is ignored.
//!
//! Nothing leaves the computer except Edge loading the page itself. Reports are kept in
//! `<config dir>/compare/` (the last ten).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use servo::{JSValue, Theme, WebView};
use winit::event_loop::EventLoopProxy;

use crate::desktop::event_loop::AppEvent;

pub(crate) const COLLECT_JS: &str = include_str!("compare-collect.js");
const REPORT_TEMPLATE: &str = include_str!("compare-report.html");
const EDGE_PATHS: &[&str] = &[
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
];
/// How long to let a page settle after its load event before measuring it.
const SETTLE: Duration = Duration::from_secs(3);

/// Progress and results, sent to the main thread.
#[derive(Debug)]
pub(crate) enum CompareEvent {
    Status(String),
    /// The report to open in a new tab.
    Done(url::Url),
    Failed(String),
}

fn find_edge() -> Option<&'static str> {
    EDGE_PATHS.iter().copied().find(|path| Path::new(path).exists())
}

/// Start comparing `webview`'s page. Progress arrives as `AppEvent::Compare`.
pub(crate) fn start(webview: &WebView, theme: Theme, proxy: EventLoopProxy<AppEvent>) {
    let send = move |event: CompareEvent| {
        let _ = proxy.send_event(AppEvent::Compare(event));
    };
    let Some(url) = webview.url().filter(|url| matches!(url.scheme(), "http" | "https")) else {
        send(CompareEvent::Failed("Compare with Edge works on web pages (http and https).".into()));
        return;
    };
    if find_edge().is_none() {
        send(CompareEvent::Failed("Microsoft Edge isn't installed, so there's nothing to compare with.".into()));
        return;
    }
    let scale = webview.hidpi_scale_factor().get().max(0.5);
    let size = webview.size();
    let css_size = ((size.width / scale).round() as u32, (size.height / scale).round() as u32);
    let title = webview.page_title().unwrap_or_default();
    let scheme = match theme {
        Theme::Dark => "dark",
        Theme::Light => "light",
    };
    send(CompareEvent::Status("Measuring the page…".into()));
    let screenshot_webview = webview.clone();
    webview.evaluate_javascript(COLLECT_JS.to_owned(), move |result| {
        let measure = match result {
            Ok(JSValue::String(json)) => serde_json::from_str::<Value>(&json).unwrap_or(Value::Null),
            _ => Value::Null,
        };
        screenshot_webview.take_screenshot(None, move |image| {
            let shot = image.ok().and_then(|image| {
                crate::desktop::sidebar::png_data_url(image.width(), image.height(), image.into_raw())
            });
            let Some(shot) = shot else {
                send(CompareEvent::Failed("Couldn't take a screenshot of the page.".into()));
                return;
            };
            send(CompareEvent::Status("Loading the page in Edge…".into()));
            let job = Job {
                url: url.clone(),
                title,
                css_size,
                scale,
                scheme,
                stillwater: json!({ "measure": measure, "shot": shot }),
            };
            std::thread::Builder::new()
                .name("CompareWithEdge".into())
                .spawn(move || match job.run(&send) {
                    Ok(report) => send(CompareEvent::Done(report)),
                    Err(error) => send(CompareEvent::Failed(error)),
                })
                .ok();
        });
    });
}

struct Job {
    url: url::Url,
    title: String,
    css_size: (u32, u32),
    scale: f32,
    scheme: &'static str,
    stillwater: Value,
}

impl Job {
    fn run(self, send: &impl Fn(CompareEvent)) -> Result<url::Url, String> {
        let mut edge = Edge::start().map_err(|error| format!("Edge didn't start: {error}"))?;
        let first = edge.load(&self, 1);
        send(CompareEvent::Status("Loading it in Edge again, to ignore changing content…".into()));
        let second = first.as_ref().ok().and_then(|_| edge.load(&self, 2).ok());
        edge.stop();
        let first = first.map_err(|error| format!("Edge couldn't load the page: {error}"))?;
        let data = json!({
            "url": self.url.as_str(),
            "title": self.title,
            "scheme": self.scheme,
            "viewport": [self.css_size.0, self.css_size.1],
            "scale": self.scale,
            "stillwater": self.stillwater,
            "edge": first,
            "edge2": second,
            "created": SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_secs()),
        });
        write_report(&data)
    }
}

/// Save the report page and return its file URL.
fn write_report(data: &Value) -> Result<url::Url, String> {
    let dir = crate::desktop::settings::config_dir()
        .map(|dir| dir.join("compare"))
        .ok_or("There's no folder to save the report in.")?;
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let stamp = data["created"].as_u64().unwrap_or(0);
    let path = dir.join(format!("compare-{stamp}.html"));
    // `</script>` can't appear inside the inline script that holds the data.
    let json = data.to_string().replace("</", "<\\/");
    std::fs::write(&path, REPORT_TEMPLATE.replace("__COMPARE_DATA__", &json)).map_err(|error| error.to_string())?;
    prune_reports(&dir);
    url::Url::from_file_path(&path).map_err(|_| "The report's path isn't valid.".to_owned())
}

/// Keep the ten newest reports.
fn prune_reports(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut reports: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "html"))
        .collect();
    reports.sort();
    let excess = reports.len().saturating_sub(10);
    for old in reports.into_iter().take(excess) {
        let _ = std::fs::remove_file(old);
    }
}

/// A hidden Edge driven over the Chrome DevTools Protocol.
struct Edge {
    child: Option<Child>,
    profile: PathBuf,
    socket: tungstenite::WebSocket<TcpStream>,
    next_id: u64,
}

impl Edge {
    fn start() -> Result<Self, String> {
        let exe = find_edge().ok_or("not installed")?;
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_millis());
        let profile = std::env::temp_dir().join(format!("stillwater-compare-{}-{millis}", std::process::id()));
        let mut command = Command::new(exe);
        command
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--mute-audio",
                "--hide-scrollbars",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-sync",
                "--disable-background-networking",
                "about:blank",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        // Edge (and the processes it starts) ends when Stillwater does, however it exits.
        #[cfg(windows)]
        kill_with_stillwater(&child);
        let port_file = profile.join("DevToolsActivePort");
        let deadline = Instant::now() + Duration::from_secs(30);
        let port = loop {
            if let Ok(text) = std::fs::read_to_string(&port_file) &&
                let Some(port) = text.lines().next().and_then(|line| line.trim().parse::<u16>().ok())
            {
                break port;
            }
            if Instant::now() > deadline || child.try_wait().ok().flatten().is_some() {
                let _ = child.kill();
                let _ = std::fs::remove_dir_all(&profile);
                return Err("it didn't open its debugging port".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let connect = || -> Result<tungstenite::WebSocket<TcpStream>, String> {
            let version = http_get_json(port, "/json/version")?;
            let ws_url = version["webSocketDebuggerUrl"].as_str().ok_or("no debugger address")?.to_owned();
            let stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
            stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
            let (socket, _) = tungstenite::client(ws_url.as_str(), stream).map_err(|error| error.to_string())?;
            Ok(socket)
        };
        let socket = match connect() {
            Ok(socket) => socket,
            Err(error) => {
                // Never leave a hidden Edge running.
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&profile);
                return Err(error);
            },
        };
        Ok(Self {
            child: Some(child),
            profile,
            socket,
            next_id: 0,
        })
    }

    /// Send a command and wait for its result. With `event`, also wait for that event from the
    /// same session (other events are dropped).
    fn send(&mut self, method: &str, params: Value, session: Option<&str>, timeout: Duration, event: Option<&str>) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.socket
            .send(tungstenite::Message::Text(message.to_string().into()))
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + timeout;
        let mut result = None;
        let mut event_seen = event.is_none();
        while result.is_none() || !event_seen {
            if Instant::now() > deadline {
                return Err(format!("{method} timed out"));
            }
            let text = match self.socket.read() {
                Ok(tungstenite::Message::Text(text)) => text.to_string(),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock || error.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(error) => return Err(error.to_string()),
            };
            let Ok(reply) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if reply["id"].as_u64() == Some(id) {
                if let Some(error) = reply.get("error") {
                    return Err(format!("{method}: {}", error["message"].as_str().unwrap_or("error")));
                }
                result = Some(reply["result"].clone());
            } else if event.is_some_and(|event| reply["method"] == event) && session.is_some_and(|session| reply["sessionId"] == session) {
                event_seen = true;
            }
        }
        Ok(result.unwrap_or(Value::Null))
    }

    /// Load the page in a new tab at Stillwater's size, scale and colour scheme; measure it and
    /// screenshot it.
    fn load(&mut self, job: &Job, attempt: u32) -> Result<Value, String> {
        let long = Duration::from_secs(40);
        let short = Duration::from_secs(15);
        let target = self.send("Target.createTarget", json!({ "url": "about:blank" }), None, short, None)?["targetId"]
            .as_str()
            .ok_or("no tab")?
            .to_owned();
        let session = self.send("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None, short, None)?
            ["sessionId"]
            .as_str()
            .ok_or("no session")?
            .to_owned();
        let session = Some(session.as_str());
        let result = (|| {
            self.send(
                "Emulation.setDeviceMetricsOverride",
                json!({ "width": job.css_size.0, "height": job.css_size.1, "deviceScaleFactor": job.scale, "mobile": false }),
                session,
                short,
                None,
            )?;
            self.send(
                "Emulation.setEmulatedMedia",
                json!({ "features": [{ "name": "prefers-color-scheme", "value": job.scheme }] }),
                session,
                short,
                None,
            )?;
            self.send("Page.enable", json!({}), session, short, None)?;
            let loaded = self
                .send("Page.navigate", json!({ "url": job.url.as_str() }), session, long, Some("Page.loadEventFired"))
                .is_ok();
            std::thread::sleep(SETTLE);
            let measure = self.send(
                "Runtime.evaluate",
                json!({ "expression": COLLECT_JS, "returnByValue": true }),
                session,
                short,
                None,
            )?;
            let measure = measure["result"]["value"]
                .as_str()
                .and_then(|json| serde_json::from_str::<Value>(json).ok())
                .unwrap_or(Value::Null);
            let shot = self.send("Page.captureScreenshot", json!({ "format": "png" }), session, long, None)?;
            let shot = shot["data"].as_str().ok_or("no screenshot")?;
            Ok(json!({
                "measure": measure,
                "shot": format!("data:image/png;base64,{shot}"),
                "loaded": loaded,
                "attempt": attempt,
            }))
        })();
        let _ = self.send("Target.closeTarget", json!({ "targetId": target }), None, short, None);
        result
    }

    /// Close Edge: politely, then (only the process started here) by force.
    fn stop(&mut self) {
        let _ = self.send("Browser.close", json!({}), None, Duration::from_secs(5), None);
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                std::thread::sleep(Duration::from_millis(100));
            }
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

impl Drop for Edge {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.stop();
        }
    }
}

fn http_get_json(port: u16, path: &str) -> Result<Value, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").map_err(|error| error.to_string())?;
    // Read the headers, then exactly Content-Length bytes: Edge keeps the connection open.
    let mut response = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = stream.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&response);
        if let Some((headers, body)) = text.split_once("\r\n\r\n") {
            let length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.trim().eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            });
            if length.is_some_and(|length| body.len() >= length) {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&response);
    let body = text.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    serde_json::from_str(body.trim()).map_err(|error| error.to_string())
}

/// Put `child` in a job object that ends it, and every process it starts, when Stillwater exits:
/// the job's only handle is closed by Windows when Stillwater's process ends.
#[cfg(windows)]
#[expect(unsafe_code)]
fn kill_with_stillwater(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        minimum_working_set: usize,
        maximum_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const std::ffi::c_void, name: *const u16) -> *mut std::ffi::c_void;
        fn SetInformationJobObject(job: *mut std::ffi::c_void, class: i32, info: *const std::ffi::c_void, length: u32) -> i32;
        fn AssignProcessToJobObject(job: *mut std::ffi::c_void, process: *mut std::ffi::c_void) -> i32;
    }
    const EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const KILL_ON_JOB_CLOSE: u32 = 0x2000;

    // One job for all of Stillwater's Edges, kept open (never closed) for the process's life.
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| {
        // SAFETY: plain Win32 calls with valid pointers to local, correctly laid-out structs.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut limits = ExtendedLimits::default();
            limits.basic.limit_flags = KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                EXTENDED_LIMIT_INFORMATION,
                &limits as *const ExtendedLimits as *const std::ffi::c_void,
                std::mem::size_of::<ExtendedLimits>() as u32,
            );
            job as usize
        }
    });
    if job != 0 {
        // SAFETY: the child's handle is valid while `child` lives; the job handle is never closed.
        unsafe {
            AssignProcessToJobObject(job as *mut std::ffi::c_void, child.as_raw_handle() as *mut std::ffi::c_void);
        }
    }
}
