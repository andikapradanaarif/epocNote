//! End-to-end MCP checks against the actual CLI and pinned SDK's stdio transport.

use epocnote::db::{Index, SCHEMA_VERSION};
use epocnote::root::db_path;
use rmcp::model::ErrorCode;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";

fn binary() -> PathBuf {
    std::env::var_os("EPOCNOTE_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_epocnote")))
}

struct Client {
    child: Child,
    responses: Receiver<String>,
    id: u64,
}

impl Client {
    fn start(root: &Path, reuse: &str) -> Self {
        let mut child = Command::new(binary())
            .arg("serve")
            .arg("--root")
            .arg(root)
            .args(["--scan-reuse", reuse])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            responses,
            id: 0,
        }
    }

    fn send(&mut self, request: Value) {
        let input = self.child.stdin.as_mut().unwrap();
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
    }

    fn start_request(&mut self, method: &str, params: Value) -> u64 {
        self.id += 1;
        self.send(json!({"jsonrpc":"2.0", "id":self.id, "method":method, "params":params}));
        self.id
    }

    fn receive(&self) -> Value {
        let line = self
            .responses
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("non-protocol stdout: {line}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.start_request(method, params);
        let reply = self.receive();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    fn modern(&mut self, method: &str, mut params: Value) -> Value {
        params["_meta"] = metadata(MODERN);
        self.request(method, params)
    }

    fn legacy(&mut self, version: &str) -> Value {
        let result = self.request(
            "initialize",
            json!({"protocolVersion":version, "clientInfo":client_info(), "capabilities":{}}),
        );
        assert_eq!(result["result"]["protocolVersion"], LEGACY, "{result}");
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        result
    }

    fn finish(mut self) {
        self.child.stdin.take();
        assert_eq!(
            self.responses.recv_timeout(Duration::from_secs(15)),
            Err(RecvTimeoutError::Disconnected)
        );
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.child.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn client_info() -> Value {
    json!({"name":"epocnote-acceptance", "version":"1"})
}

fn metadata(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion":version,
        "io.modelcontextprotocol/clientInfo":client_info(),
        "io.modelcontextprotocol/clientCapabilities":{}
    })
}

fn note_args(title: &str) -> Value {
    json!({"name":"epocnote_note","arguments":{
        "title":title, "body":"Session revocation uses the session_store.", "impacts":["src/session/"]
    }})
}

fn search_args() -> Value {
    json!({"name":"epocnote_search","arguments":{"query":"session_store","path_filter":"src/session/store.rs"}})
}

fn successful(reply: &Value, modern: bool) -> &Value {
    let result = &reply["result"];
    assert_eq!(result["isError"], false, "{reply}");
    if modern {
        assert_eq!(result["resultType"], "complete", "{result}");
    } else {
        assert!(result.get("resultType").is_none(), "{result}");
    }
    assert!(serde_json::to_vec(result).unwrap().len() <= 48 * 1024);
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        result["structuredContent"]
    );
    &result["structuredContent"]
}

fn app_error(reply: &Value, code: &str, note: bool) -> Value {
    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    assert!(result.get("structuredContent").is_none(), "{reply}");
    assert!(serde_json::to_vec(result).unwrap().len() <= 4 * 1024);
    let payload: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["code"], code, "{reply}");
    assert!(payload["message"].as_str().unwrap().chars().count() <= 512);
    assert!(payload["retryable"].is_boolean());
    if note {
        assert_eq!(payload["saved"], false);
    } else {
        assert!(payload.get("saved").is_none());
    }
    payload
}

fn init(root: &Path) {
    assert!(
        Command::new(binary())
            .arg("init")
            .arg("--root")
            .arg(root)
            .arg("--target")
            .arg("codex")
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn modern_discovery_recovers_from_version_probe_and_initialization() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("project with spaces");
    fs::create_dir(&root).unwrap();
    let mut client = Client::start(&root, "off");
    let unsupported = client.request("server/discover", json!({"_meta":metadata("2099-01-01")}));
    assert_eq!(
        unsupported["error"]["code"],
        ErrorCode::UNSUPPORTED_PROTOCOL_VERSION.0
    );
    let discover = client.modern("server/discover", json!({}));
    assert_eq!(discover["result"]["supportedVersions"], json!([MODERN]));
    assert!(
        discover["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("verify")
    );
    let reply = client.modern("tools/list", json!({}));
    let tools = reply["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["limit"]["maximum"],
        i32::MAX
    );
    assert_eq!(tools[1]["annotations"]["idempotentHint"], false);
    assert!(
        tools
            .iter()
            .all(|tool| tool["outputSchema"]["properties"].is_object())
    );
    let uninitialized = client.modern("tools/call", search_args());
    let error = app_error(&uninitialized, "NOT_INITIALIZED", false);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("epocnote init --root")
    );
    assert!(!root.join(".context").exists());
    init(&root);

    let new_note = client.modern("tools/call", note_args("Session revocation"));
    let note = successful(&new_note, true);
    assert_eq!(note["indexed"], true);
    let path = note["path"].as_str().unwrap();
    assert!(root.join(path).is_file());
    let reply = client.modern("tools/call", search_args());
    assert_eq!(successful(&reply, true)["results"][0]["path"], path);
    let legacy_without_metadata = client.request("tools/list", json!({}));
    assert_eq!(
        legacy_without_metadata["error"]["code"],
        ErrorCode::INVALID_PARAMS.0
    );
    client.finish();
}

#[test]
fn legacy_round_trip_and_modern_direct_call() {
    let root = tempfile::tempdir().unwrap();
    init(root.path());
    let mut legacy = Client::start(root.path(), "auto");
    let initialize = legacy.legacy(MODERN);
    let reply = legacy.request("tools/list", json!({}));
    assert_eq!(reply["result"]["tools"].as_array().unwrap().len(), 2);
    let reply = legacy.request("tools/call", note_args("Legacy setup"));
    let path = successful(&reply, false)["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply = legacy.request("tools/call", search_args());
    assert_eq!(successful(&reply, false)["results"][0]["path"], path);
    legacy.finish();

    let mut direct = Client::start(root.path(), "auto");
    let reply = direct.modern("tools/call", search_args());
    assert_eq!(successful(&reply, true)["results"][0]["path"], path);
    direct.finish();

    let mut fallback = Client::start(root.path(), "off");
    let invalid_probe = fallback.request("server/discover", json!({}));
    assert_eq!(invalid_probe["error"]["code"], ErrorCode::INVALID_PARAMS.0);
    let legacy_discovery = fallback.legacy(LEGACY);
    let mut discover = Client::start(root.path(), "off");
    let modern_discovery = discover.modern("server/discover", json!({}));
    assert_eq!(
        modern_discovery["result"]["instructions"],
        initialize["result"]["instructions"]
    );
    assert_eq!(
        legacy_discovery["result"]["instructions"],
        initialize["result"]["instructions"]
    );
    fallback.finish();
    discover.finish();
}

#[test]
fn invalid_calls_leave_files_and_index_untouched() {
    let root = tempfile::tempdir().unwrap();
    init(root.path());
    let mut client = Client::start(root.path(), "auto");
    for args in [
        json!({"limit":null,"query":"hello"}),
        json!({"limit":4.5,"query":"hello"}),
        json!({"query":null,"path_filter":"src"}),
        json!({"query":"___"}),
        json!({"path_filter":"../foo"}),
        json!({"query":"hello","unknown":true}),
    ] {
        let reply = client.modern(
            "tools/call",
            json!({"name":"epocnote_search","arguments":args}),
        );
        app_error(&reply, "INVALID_ARGUMENT", false);
    }
    let long = "a/".repeat(3000);
    let invalid = client.modern(
        "tools/call",
        json!({"name":"epocnote_note","arguments":{
            "title":"Current","body":"Sensitive body marker","impacts":[],
            "supersedes":[format!(".context/{long}missing.md")]
        }}),
    );
    let payload = app_error(&invalid, "INVALID_ARGUMENT", true);
    assert!(
        !payload["message"]
            .as_str()
            .unwrap()
            .contains("Sensitive body marker")
    );
    let missing = client.modern(
        "tools/call",
        json!({"name":"epocnote_note","arguments":{
            "title":"Current","body":"Valid body","impacts":[],"supersedes":[".context/missing.md"]
        }}),
    );
    app_error(&missing, "INVALID_ARGUMENT", true);
    let unknown = client.modern("tools/call", json!({"name":"unknown","arguments":{}}));
    assert_eq!(unknown["error"]["code"], ErrorCode::INVALID_PARAMS.0);
    assert!(!db_path(root.path()).exists());
    assert_eq!(
        fs::read_dir(root.path().join(".context/notes"))
            .unwrap()
            .count(),
        0
    );
    client.finish();
}

#[test]
fn saved_note_survives_index_failure_and_is_repaired_by_rebuild() {
    let root = tempfile::tempdir().unwrap();
    init(root.path());
    let index = Index::open(root.path()).unwrap();
    index
        .conn()
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    let mut client = Client::start(root.path(), "off");
    client.legacy(LEGACY);
    let reply = client.request("tools/call", search_args());
    app_error(&reply, "SCHEMA_TOO_NEW", false);

    let reply = client.request("tools/call", note_args("Publication survives"));
    let note = successful(&reply, false);
    assert_eq!(note["saved"], true);
    assert_eq!(note["indexed"], false);
    assert_eq!(note["warnings"][0]["code"], "INDEXING_FAILED");
    let path = note["path"].as_str().unwrap().to_owned();
    assert!(root.path().join(&path).is_file());

    index
        .conn()
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .unwrap();
    epocnote::sync::rebuild(root.path()).unwrap();
    let reply = client.request("tools/call", search_args());
    assert_eq!(successful(&reply, false)["results"][0]["path"], path);
    client.finish();
}

#[test]
fn cancellation_after_publication_leaves_exactly_one_complete_note() {
    let root = tempfile::tempdir().unwrap();
    init(root.path());
    let mut index = Index::open(root.path()).unwrap();
    let lock = index
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let mut client = Client::start(root.path(), "off");
    client.legacy(LEGACY);
    let id = client.start_request("tools/call", note_args("Cancellation boundary"));
    let notes_dir = root.path().join(".context/notes");
    let by = Instant::now() + Duration::from_secs(5);
    let saved = loop {
        if let Some(path) = fs::read_dir(&notes_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "md"))
        {
            break path;
        }
        assert!(Instant::now() < by, "note was never published");
        std::thread::sleep(Duration::from_millis(10));
    };
    client.send(
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}}),
    );
    let ping_id = client.start_request("ping", json!({}));
    loop {
        if client.receive()["id"] == ping_id {
            break;
        }
    }
    lock.rollback().unwrap();

    let text = fs::read_to_string(&saved).unwrap();
    assert!(text.contains("Session revocation uses the session_store."));
    let search_id = client.start_request("tools/call", search_args());
    loop {
        let reply = client.receive();
        if reply["id"] == search_id {
            assert_eq!(
                successful(&reply, false)["results"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            break;
        }
    }
    assert_eq!(fs::read_dir(notes_dir).unwrap().count(), 1);
    client.finish();
}

#[test]
fn complete_wire_budget_fits_ascii_and_truncates_multibyte_results() {
    let root = tempfile::tempdir().unwrap();
    init(root.path());
    fs::remove_file(root.path().join(".context/architecture.md")).unwrap();
    for (label, character) in [("ascii", "a"), ("unicode", "🧭")] {
        for number in 0..10 {
            fs::write(
                root.path()
                    .join(format!(".context/notes/{label}-{number}.md")),
                format!(
                    "---\ntitle: Note {number}\nimpacts: [src/{label}]\n---\n\n{}\n",
                    character.repeat(1500)
                ),
            )
            .unwrap();
        }
    }
    let mut client = Client::start(root.path(), "off");
    let args = |label| {
        json!({"name":"epocnote_search","arguments":{
            "path_filter":format!("src/{label}"),"limit":i32::MAX
        }})
    };
    let reply = client.modern("tools/call", args("ascii"));
    let data = successful(&reply, true);
    assert_eq!(data["results"].as_array().unwrap().len(), 10);
    assert_eq!(data["truncated"], false);
    let reply = client.modern("tools/call", args("unicode"));
    let data = successful(&reply, true);
    assert_eq!(data["truncated"], true);
    assert!((1..10).contains(&data["results"].as_array().unwrap().len()));
    assert!(data["results"].as_array().unwrap().iter().all(|entry| {
        entry["body"]
            .as_str()
            .is_some_and(|body| body.chars().count() == 1500)
    }));
    client.finish();
}

#[test]
fn reconciliation_contention_returns_a_partial_search_and_setup_contention_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    init(root.path());
    let mut index = Index::open(root.path()).unwrap();
    let mut client = Client::start(root.path(), "off");
    client.legacy(LEGACY);

    let lock = index
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let result = client.request("tools/call", search_args());
    let data = successful(&result, false);
    assert_eq!(data["partial_index"], true);
    assert_eq!(data["warnings"][0]["code"], "INDEX_CATCHING_UP");
    assert!(
        data["warnings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("1 document")
    );
    lock.rollback().unwrap();

    // Let the prior reconciliation finish. A schema upgrade also requires a
    // write lock, but it occurs during setup and is an application error.
    let _ = client.request("tools/call", search_args());
    let lock = index
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let result = client.request("tools/call", search_args());
    assert_eq!(successful(&result, false)["partial_index"], false);
    lock.rollback().unwrap();
    index.conn().pragma_update(None, "user_version", 0).unwrap();
    let lock = index
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let result = client.request("tools/call", search_args());
    assert_eq!(app_error(&result, "INDEX_BUSY", false)["retryable"], true);
    lock.rollback().unwrap();
    client.finish();
}

#[cfg(unix)]
#[test]
fn live_server_follows_context_reinitialization_without_reusing_the_old_index() {
    let root = tempfile::tempdir().unwrap();
    init(root.path());
    let mut client = Client::start(root.path(), "auto");
    client.modern("tools/call", note_args("Old context"));
    let reply = client.modern("tools/call", search_args());
    assert_eq!(
        successful(&reply, true)["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    fs::rename(
        root.path().join(".context"),
        root.path().join("old context"),
    )
    .unwrap();
    let reply = client.modern("tools/call", search_args());
    app_error(&reply, "NOT_INITIALIZED", false);
    fs::write(root.path().join(".context"), "invalid context").unwrap();
    let reply = client.modern("tools/call", search_args());
    app_error(&reply, "UNSAFE_PATH", false);
    fs::remove_file(root.path().join(".context")).unwrap();

    init(root.path());
    let reply = client.modern("tools/call", search_args());
    assert!(
        successful(&reply, true)["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    client.finish();
}

#[test]
fn linked_worktrees_search_only_their_own_markdown() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let primary = temporary.path().join("primary worktree");
    let linked = temporary.path().join("linked worktree");
    fs::create_dir(&primary).unwrap();
    let git = |args: &[&str]| {
        let result = Command::new("git")
            .arg("-C")
            .arg(&primary)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=EpocNote",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-q",
        "-m",
        "Fixture",
    ]);
    git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        linked.to_str().unwrap(),
    ]);
    assert!(linked.join(".git").is_file());

    for (directory, own, foreign) in [
        (&primary, "primarymarker", "linkedmarker"),
        (&linked, "linkedmarker", "primarymarker"),
    ] {
        init(directory);
        let mut client = Client::start(directory, "off");
        let note = client.modern(
            "tools/call",
            json!({"name":"epocnote_note","arguments":{
                "title":own, "body":own, "impacts":[]
            }}),
        );
        assert_eq!(successful(&note, true)["indexed"], true);
        for (query, hits) in [(own, 1), (foreign, 0)] {
            let result = client.modern(
                "tools/call",
                json!({"name":"epocnote_search","arguments":{"query":query}}),
            );
            assert_eq!(
                successful(&result, true)["results"]
                    .as_array()
                    .unwrap()
                    .len(),
                hits
            );
        }
        client.finish();
    }
    assert!(db_path(&primary).is_file());
    assert!(db_path(&linked).is_file());
}
