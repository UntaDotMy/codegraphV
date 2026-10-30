//! A file with `%` in its name opens as itself (upstream 2afee00c, #2933): the request path and the
//! query are percent-decoded exactly once, so `100%.ts` is not refused and `a%20b.ts` is not read as
//! `a b.ts` while the index lookup uses the name as given.
use codegraph_ui_server::{serve, Options};
use std::{
    io::{Read, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

fn get(port: u16, target: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// Serves a project holding `src/a%20b.ts`, `src/a b.ts` and `src/100%.ts`, each with its own text.
fn with_viewer(check: impl FnOnce(u16)) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src").join("a%20b.ts"), "export const percentTwenty = 1;\n").unwrap();
    std::fs::write(root.join("src").join("a b.ts"), "export const withSpace = 2;\n").unwrap();
    std::fs::write(root.join("src").join("100%.ts"), "export const hundred = 3;\n").unwrap();
    let cg = codegraph_core::CodeGraph::init_and_index(root.to_str().unwrap(), None).unwrap();
    cg.close();
    let viewer = dir.path().join("viewer");
    std::fs::create_dir(&viewer).unwrap();
    std::fs::write(viewer.join("index.html"), "<main>viewer</main>").unwrap();
    let options = Options { root: root.to_string_lossy().into_owned(), viewer, port: 0, port_fallback: false, read_only: true };
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    let (sender, receiver) = mpsc::channel();
    let server = std::thread::spawn(move || serve(options, &signal, |port| sender.send(port).unwrap()).map_err(|e| e.to_string()));
    let port = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    check(port);
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap().unwrap();
}

#[test]
fn a_file_named_with_a_percent_is_served_as_itself_over_api_source() {
    with_viewer(|port| {
        // The query a browser sends for `src/100%.ts`: the `%` is itself encoded once.
        let hundred = get(port, "/api/source?file=src%2F100%25.ts&ondrift=current");
        assert!(hundred.contains("200 OK"), "{hundred}");
        assert!(hundred.contains("hundred"), "{hundred}");
        assert!(hundred.contains("\"drift\":false"), "{hundred}");

        let twenty = get(port, "/api/source?file=src%2Fa%2520b.ts&ondrift=current");
        assert!(twenty.contains("200 OK"), "{twenty}");
        assert!(twenty.contains("percentTwenty") && !twenty.contains("withSpace"), "{twenty}");
        assert!(twenty.contains("\"drift\":false"), "{twenty}");

        let spaced = get(port, "/api/source?file=src%2Fa%20b.ts&ondrift=current");
        assert!(spaced.contains("200 OK"), "{spaced}");
        assert!(spaced.contains("withSpace") && !spaced.contains("percentTwenty"), "{spaced}");
        assert!(spaced.contains("\"drift\":false"), "{spaced}");
    });
}

#[test]
fn api_file_resolves_a_percent_in_the_path_once() {
    with_viewer(|port| {
        let hundred = get(port, "/api/file/src/100%25.ts");
        assert!(hundred.contains("200 OK"), "{hundred}");
        assert!(hundred.contains("hundred"), "{hundred}");

        let twenty = get(port, "/api/file/src/a%2520b.ts");
        assert!(twenty.contains("200 OK"), "{twenty}");
        assert!(twenty.contains("percentTwenty") && !twenty.contains("withSpace"), "{twenty}");
    });
}

#[test]
fn an_encoded_traversal_is_still_refused() {
    with_viewer(|port| {
        assert!(get(port, "/api/source?file=src%2F..%2F..%2Fsecret.ts").contains("403"));
        assert!(get(port, "/api/file/src/..%2f..%2fsecret.ts").contains("403"));
        assert!(get(port, "/api/source?file=src%2Fa%00.ts").contains("403"));
    });
}
