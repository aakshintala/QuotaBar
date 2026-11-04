use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const FEED: &[u8] = br#"{"schemaVersion":1,"generatedAt":"2027-01-15T08:00:00Z","disabledProviderIds":[],"providers":[{"id":"claude","name":"Claude","tier":null,"capturedAt":null,"ageSeconds":null,"freshness":"unknown","errorCode":null,"status":"unknown","quotas":[]}] }"#;
static NEXT_HOME: AtomicUsize = AtomicUsize::new(0);

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "quotabard-cli-binary-{}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Request {
    line: String,
    headers: String,
    body: Vec<u8>,
}

fn accept(listener: &TcpListener) -> io::Result<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "loopback client timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let request_line = line.trim_end().to_owned();
    let mut headers = String::new();
    let mut content_length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().unwrap();
        }
        headers.push_str(&line);
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    Ok(Request {
        line: request_line,
        headers,
        body,
    })
}

fn reply(stream: &mut TcpStream, body: &[u8]) -> io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn child(args: &[&str], home: &Path, feed_url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quotabard"))
        .args(args)
        .env("HOME", home)
        .env("QUOTA_FEED", feed_url)
        .output()
        .unwrap()
}

#[test]
fn accepted_nonblocking_listener_stream_waits_for_a_delayed_request() {
    use std::sync::mpsc;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener).unwrap();
        accepted_tx.send(()).unwrap();
        read_request(&mut stream)
    });

    let mut client = TcpStream::connect(address).unwrap();
    accepted_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    thread::sleep(Duration::from_millis(50));
    client
        .write_all(b"GET /delayed HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();

    let request = server.join().unwrap().unwrap();
    assert_eq!(request.line, "GET /delayed HTTP/1.1");
}

#[test]
fn real_binary_client_uses_custom_loopback_feed_without_opening_daemon_state() {
    // This is the only test in this integration target. It spawns client-mode
    // subprocesses only; it does not open RuntimeState or test lock reopening.
    let home = Home::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let feed_url = format!("http://127.0.0.1:{}/quotas", address.port());
    let server = thread::spawn(move || {
        let mut stream = accept(&listener).unwrap();
        let get = read_request(&mut stream).unwrap();
        reply(&mut stream, FEED).unwrap();

        let mut stream = accept(&listener).unwrap();
        let put = read_request(&mut stream).unwrap();
        reply(&mut stream, br#"{"id":"cursor","enabled":true}"#).unwrap();
        (get, put)
    });

    let get = child(&["quota", "--json"], home.path(), &feed_url);
    assert!(
        get.status.success(),
        "{}",
        String::from_utf8_lossy(&get.stderr)
    );
    assert_eq!(get.stdout, FEED);
    assert!(!home.path().join(".quotabar").exists());

    let daemon_dir = home.path().join(".quotabar/daemon");
    fs::create_dir_all(&daemon_dir).unwrap();
    let token_path = daemon_dir.join("control-token");
    fs::write(&token_path, [vec![b'a'; 64], vec![b'\n']].concat()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let put = child(&["quota", "enable", "cursor"], home.path(), &feed_url);
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    assert_eq!(put.stdout, b"Provider cursor enabled.\n");
    assert!(!String::from_utf8_lossy(&put.stderr).contains(&"a".repeat(64)));
    assert!(!daemon_dir.join("instance.lock").exists());
    assert!(!home.path().join(".quotabar/daemon.json").exists());

    let (get_request, put_request) = server.join().unwrap();
    assert_eq!(get_request.line, "GET /quotas HTTP/1.1");
    assert!(
        !get_request
            .headers
            .to_ascii_lowercase()
            .contains("x-quotabar-control-token")
    );
    assert_eq!(put_request.line, "PUT /providers/cursor/enabled HTTP/1.1");
    assert!(put_request.headers.contains(&"a".repeat(64)));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&put_request.body).unwrap(),
        serde_json::json!({"enabled":true})
    );
}
