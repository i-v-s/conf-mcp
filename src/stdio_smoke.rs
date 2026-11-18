use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start(home: &std::path::Path) -> Self {
        let mut child = command(home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    async fn send(&mut self, value: Value) {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.send(
            json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}),
        )
        .await;
        let mut line = String::new();
        let bytes = timeout(Duration::from_secs(10), self.stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes > 0, "Сервер закрыл stdout до ответа");
        let response: Value =
            serde_json::from_str(&line).expect("stdout должен содержать только JSON-RPC");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], self.next_id);
        response
    }

    async fn finish(self) {
        let Self {
            mut child,
            stdin,
            mut stdout,
            ..
        } = self;
        drop(stdin);
        let status = timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("Сервер должен завершиться по EOF")
            .unwrap();
        assert!(status.success());
        let mut trailing = String::new();
        stdout.read_to_string(&mut trailing).await.unwrap();
        assert!(
            trailing.is_empty(),
            "Посторонние данные в stdout: {trailing}"
        );
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .await
            .unwrap();
        assert!(stderr.is_empty(), "Неожиданная диагностика: {stderr}");
    }
}

fn command(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_conf-mcp"));
    command
        .env("HOME", home)
        .env_remove("CONFLUENCE_BASE_URL")
        .env_remove("CONFLUENCE_TOKEN");
    command
}

#[tokio::test]
async fn stdio_initialization_tools_errors_and_eof() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".config")).unwrap();
    std::fs::write(
        home.path().join(".config/conf-mcp.toml"),
        "base_url = 'http://127.0.0.1:9/confluence'\ntoken = 'stdio-test-secret'\n",
    )
    .unwrap();
    for version in ["2024-11-05", "2025-11-25"] {
        let mut session = Session::start(home.path());
        let response = session
            .request(
                "initialize",
                json!({
                    "protocolVersion": version, "capabilities": {},
                    "clientInfo": {"name": "conf-mcp-smoke", "version": "1.0"}
                }),
            )
            .await;
        assert_eq!(response["result"]["protocolVersion"], version);
        assert_eq!(response["result"]["serverInfo"]["name"], "conf-mcp");
        assert!(response["result"]["capabilities"].get("tools").is_some());
        session
            .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        let response = session.request("tools/list", json!({})).await;
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        for name in ["get_page", "get_image", "search_pages"] {
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        let image_tool = tools
            .iter()
            .find(|tool| tool["name"] == "get_image")
            .unwrap();
        assert_eq!(image_tool["inputSchema"]["required"], json!(["url"]));
        assert_eq!(
            image_tool["inputSchema"]["properties"]["url"]["type"],
            "string"
        );
        assert_eq!(image_tool["inputSchema"]["additionalProperties"], false);
        for (name, arguments) in [
            (
                "get_page",
                json!({"page": "https://another.test/pages/viewpage.action?pageId=42"}),
            ),
            ("get_page", json!({"page": "unsupported"})),
            (
                "search_pages",
                json!({"query": "Rust", "cql": "space = DEV"}),
            ),
            ("search_pages", json!({"query": "Rust", "limit": 0})),
            ("search_pages", json!({"cql": "space = DEV ORDER BY "})),
            (
                "get_image",
                json!({"url": "https://another.test/image.png"}),
            ),
            ("get_image", json!({"url": "image.png"})),
            (
                "get_image",
                json!({"url": "http://127.0.0.1:9/outside/image.png"}),
            ),
            ("get_image", json!({})),
            ("get_image", json!({"url": 42})),
            ("get_image", json!({"url": "image.png", "unexpected": true})),
        ] {
            let response = session
                .request("tools/call", json!({"name": name, "arguments": arguments}))
                .await;
            assert_eq!(response["result"]["isError"], true, "{response}");
            assert!(!response.to_string().contains("stdio-test-secret"));
        }
        let response = session
            .request("tools/call", json!({"name": "get_page", "arguments": {}}))
            .await;
        assert_eq!(response["result"]["isError"], true, "{response}");
        let response = session.request("ping", json!({})).await;
        assert!(
            response.get("result").is_some(),
            "Сервер должен работать после ошибок"
        );
        session.finish().await;
    }
}

#[tokio::test]
async fn missing_or_invalid_configuration_fails_on_stderr_only() {
    let home = tempfile::tempdir().unwrap();
    let output = timeout(Duration::from_secs(5), command(home.path()).output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("CONFLUENCE_BASE_URL"));
    std::fs::create_dir(home.path().join(".config")).unwrap();
    std::fs::write(
        home.path().join(".config/conf-mcp.toml"),
        "token = 'stdio-test-secret' invalid",
    )
    .unwrap();
    let output = timeout(Duration::from_secs(5), command(home.path()).output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("TOML"));
    assert!(!stderr.contains("stdio-test-secret"));
}
