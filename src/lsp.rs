use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, select, unbounded};
use eframe::egui;
use lsp_types::{
    CodeAction, CodeActionOrCommand, CodeActionResponse, Command as LspCommand, CompletionItem,
    CompletionResponse, Diagnostic, GotoDefinitionResponse, Hover, Location,
    PublishDiagnosticsParams, TextEdit, WorkspaceEdit,
};
use serde_json::{Value, json};
use url::Url;

use crate::document::{Document, Position};

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct EventSink {
    sender: Sender<LspEvent>,
    context: egui::Context,
}

impl EventSink {
    fn send(&self, event: LspEvent) {
        if self.sender.send(event).is_ok() {
            self.context.request_repaint();
        }
    }

    fn status(&self, message: impl Into<String>, error: bool) {
        self.send(LspEvent::ServerStatus {
            message: message.into(),
            error,
        });
    }
}

pub enum LspEvent {
    ServerStatus {
        message: String,
        error: bool,
    },
    Diagnostics {
        document_id: u64,
        version: Option<i32>,
        diagnostics: Vec<Diagnostic>,
    },
    Completion {
        document_id: u64,
        revision: u64,
        items: Vec<CompletionItem>,
    },
    Definition {
        document_id: u64,
        revision: u64,
        locations: Vec<Location>,
    },
    Hover {
        document_id: u64,
        revision: u64,
        position: Position,
        hover: Option<Hover>,
    },
    References {
        document_id: u64,
        revision: u64,
        locations: Vec<Location>,
    },
    Rename {
        document_id: u64,
        revision: u64,
        edit: Option<WorkspaceEdit>,
    },
    Formatting {
        document_id: u64,
        revision: u64,
        edits: Vec<TextEdit>,
    },
    CodeActions {
        document_id: u64,
        revision: u64,
        actions: Vec<CodeActionOrCommand>,
    },
    CodeActionResolved {
        document_id: u64,
        revision: u64,
        action: Box<CodeAction>,
    },
    CommandExecuted {
        document_id: u64,
    },
}

struct DocumentData {
    document_id: u64,
    uri: Url,
    language_id: String,
    revision: u64,
    text: String,
}

struct ServerCommand {
    program: &'static str,
    arguments: &'static [&'static str],
}

struct ServerConfig {
    name: &'static str,
    commands: &'static [ServerCommand],
}

const RUST_COMMANDS: &[ServerCommand] = &[ServerCommand {
    program: "rust-analyzer",
    arguments: &[],
}];
const CLANG_COMMANDS: &[ServerCommand] = &[ServerCommand {
    program: "clangd",
    arguments: &["--background-index"],
}];
const PYTHON_COMMANDS: &[ServerCommand] = &[
    ServerCommand {
        program: "pyright-langserver",
        arguments: &["--stdio"],
    },
    ServerCommand {
        program: "pylsp",
        arguments: &[],
    },
];
const TYPESCRIPT_COMMANDS: &[ServerCommand] = &[ServerCommand {
    program: "typescript-language-server",
    arguments: &["--stdio"],
}];
const GO_COMMANDS: &[ServerCommand] = &[ServerCommand {
    program: "gopls",
    arguments: &[],
}];

const RUST_CONFIG: ServerConfig = ServerConfig {
    name: "rust-analyzer",
    commands: RUST_COMMANDS,
};
const CLANG_CONFIG: ServerConfig = ServerConfig {
    name: "clangd",
    commands: CLANG_COMMANDS,
};
const PYTHON_CONFIG: ServerConfig = ServerConfig {
    name: "Python LSP",
    commands: PYTHON_COMMANDS,
};
const TYPESCRIPT_CONFIG: ServerConfig = ServerConfig {
    name: "typescript-language-server",
    commands: TYPESCRIPT_COMMANDS,
};
const GO_CONFIG: ServerConfig = ServerConfig {
    name: "gopls",
    commands: GO_COMMANDS,
};

fn server_config(path: &Path) -> Option<&'static ServerConfig> {
    match path.extension()?.to_str()? {
        "rs" => Some(&RUST_CONFIG),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(&CLANG_CONFIG),
        "py" | "pyi" | "pyw" => Some(&PYTHON_CONFIG),
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx" => Some(&TYPESCRIPT_CONFIG),
        "go" => Some(&GO_CONFIG),
        _ => None,
    }
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => "rust",
        Some("c" | "h") => "c",
        Some("cc" | "cpp" | "cxx" | "hpp" | "hh") => "cpp",
        Some("py" | "pyi" | "pyw") => "python",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("jsx") => "javascriptreact",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("go") => "go",
        _ => "plaintext",
    }
}

fn document_uri(document: &Document) -> Option<Url> {
    Url::from_file_path(&document.path).ok()
}

fn document_data(document: &Document) -> Option<DocumentData> {
    Some(DocumentData {
        document_id: document.id,
        uri: document_uri(document)?,
        language_id: language_id(&document.path).to_owned(),
        revision: document.revision,
        text: document.lsp_text()?,
    })
}

fn version(revision: u64) -> i32 {
    (revision % i32::MAX as u64) as i32 + 1
}

enum ManagerCommand {
    Open {
        data: DocumentData,
        config: &'static ServerConfig,
        root: PathBuf,
    },
    Change(DocumentData),
    Save {
        document_id: u64,
        uri: Url,
    },
    Close {
        document_id: u64,
    },
    Completion {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
    },
    Definition {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
    },
    Hover {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
        editor_position: Position,
    },
    References {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
    },
    Rename {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
        new_name: String,
    },
    Formatting {
        document_id: u64,
        revision: u64,
        uri: Url,
        tab_size: u32,
        insert_spaces: bool,
    },
    CodeActions {
        document_id: u64,
        revision: u64,
        uri: Url,
        range: lsp_types::Range,
        diagnostics: Vec<Diagnostic>,
    },
    ResolveCodeAction {
        document_id: u64,
        revision: u64,
        action: CodeAction,
    },
    ExecuteCommand {
        document_id: u64,
        command: LspCommand,
    },
    Shutdown,
}

enum ServerMessage {
    Open(DocumentData),
    Change(DocumentData),
    Save {
        uri: Url,
    },
    Close {
        uri: Url,
    },
    Completion {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
    },
    Definition {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
    },
    Hover {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
        editor_position: Position,
    },
    References {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
    },
    Rename {
        document_id: u64,
        revision: u64,
        uri: Url,
        position: lsp_types::Position,
        new_name: String,
    },
    Formatting {
        document_id: u64,
        revision: u64,
        uri: Url,
        tab_size: u32,
        insert_spaces: bool,
    },
    CodeActions {
        document_id: u64,
        revision: u64,
        uri: Url,
        range: lsp_types::Range,
        diagnostics: Vec<Diagnostic>,
    },
    ResolveCodeAction {
        document_id: u64,
        revision: u64,
        action: CodeAction,
    },
    ExecuteCommand {
        document_id: u64,
        command: LspCommand,
    },
    Shutdown,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct ServerKey {
    name: &'static str,
    root: PathBuf,
}

pub struct LspWorker {
    command_tx: Sender<ManagerCommand>,
    event_rx: Receiver<LspEvent>,
}

impl LspWorker {
    pub fn new(context: &egui::Context) -> Self {
        let (command_tx, command_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        let sink = EventSink {
            sender: event_tx,
            context: context.clone(),
        };
        thread::Builder::new()
            .name("fce-lsp-manager".into())
            .spawn(move || run_manager(command_rx, sink))
            .expect("failed to start LSP manager");
        Self {
            command_tx,
            event_rx,
        }
    }

    pub fn open(&self, document: &Document, root: Option<PathBuf>) {
        let Some(config) = server_config(&document.path) else {
            return;
        };
        let Some(data) = document_data(document) else {
            return;
        };
        let root = root
            .or_else(|| discover_root(&document.path))
            .unwrap_or_else(|| PathBuf::from("."));
        let _ = self
            .command_tx
            .send(ManagerCommand::Open { data, config, root });
    }

    pub fn change(&self, document: &Document) {
        if server_config(&document.path).is_none() {
            return;
        }
        if let Some(data) = document_data(document) {
            let _ = self.command_tx.send(ManagerCommand::Change(data));
        }
    }

    pub fn save(&self, document: &Document) {
        if let Some(uri) = document_uri(document) {
            let _ = self.command_tx.send(ManagerCommand::Save {
                document_id: document.id,
                uri,
            });
        }
    }

    pub fn close(&self, document_id: u64) {
        let _ = self.command_tx.send(ManagerCommand::Close { document_id });
    }

    pub fn request_completion(&self, document: &Document, position: Position) {
        if let Some(uri) = document_uri(document) {
            let _ = self.command_tx.send(ManagerCommand::Completion {
                document_id: document.id,
                revision: document.revision,
                uri,
                position: document.to_lsp_position(position),
            });
        }
    }

    pub fn request_definition(&self, document: &Document, position: Position) {
        if let Some(uri) = document_uri(document) {
            let _ = self.command_tx.send(ManagerCommand::Definition {
                document_id: document.id,
                revision: document.revision,
                uri,
                position: document.to_lsp_position(position),
            });
        }
    }

    pub fn request_hover(&self, document: &Document, position: Position) {
        if let Some(uri) = document_uri(document) {
            let _ = self.command_tx.send(ManagerCommand::Hover {
                document_id: document.id,
                revision: document.revision,
                uri,
                position: document.to_lsp_position(position),
                editor_position: position,
            });
        }
    }

    pub fn request_references(&self, document: &Document, position: Position) {
        if let Some(uri) = document_uri(document) {
            let _ = self.command_tx.send(ManagerCommand::References {
                document_id: document.id,
                revision: document.revision,
                uri,
                position: document.to_lsp_position(position),
            });
        }
    }

    pub fn request_rename(&self, document: &Document, position: Position, new_name: String) {
        if let Some(uri) = document_uri(document) {
            let _ = self.command_tx.send(ManagerCommand::Rename {
                document_id: document.id,
                revision: document.revision,
                uri,
                position: document.to_lsp_position(position),
                new_name,
            });
        }
    }

    pub fn request_formatting(&self, document: &Document) {
        if let Some(uri) = document_uri(document) {
            let _ = self.command_tx.send(ManagerCommand::Formatting {
                document_id: document.id,
                revision: document.revision,
                uri,
                tab_size: 4,
                insert_spaces: true,
            });
        }
    }

    pub fn request_code_actions(
        &self,
        document: &Document,
        start: Position,
        end: Position,
        diagnostics: Vec<Diagnostic>,
    ) {
        if let Some(uri) = document_uri(document) {
            let _ = self.command_tx.send(ManagerCommand::CodeActions {
                document_id: document.id,
                revision: document.revision,
                uri,
                range: lsp_types::Range {
                    start: document.to_lsp_position(start),
                    end: document.to_lsp_position(end),
                },
                diagnostics,
            });
        }
    }

    pub fn resolve_code_action(&self, document: &Document, action: CodeAction) {
        let _ = self.command_tx.send(ManagerCommand::ResolveCodeAction {
            document_id: document.id,
            revision: document.revision,
            action,
        });
    }

    pub fn execute_command(&self, document_id: u64, command: LspCommand) {
        let _ = self.command_tx.send(ManagerCommand::ExecuteCommand {
            document_id,
            command,
        });
    }

    pub fn try_recv(&self) -> Option<LspEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for LspWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(ManagerCommand::Shutdown);
    }
}

fn discover_root(path: &Path) -> Option<PathBuf> {
    const MARKERS: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "setup.py",
        "go.mod",
        "compile_commands.json",
        ".git",
    ];
    path.parent().and_then(|parent| {
        parent
            .ancestors()
            .find(|ancestor| MARKERS.iter().any(|marker| ancestor.join(marker).exists()))
            .or(Some(parent))
            .map(Path::to_path_buf)
    })
}

fn run_manager(receiver: Receiver<ManagerCommand>, sink: EventSink) {
    let mut servers: HashMap<ServerKey, Sender<ServerMessage>> = HashMap::new();
    let mut routes: HashMap<u64, ServerKey> = HashMap::new();
    let mut document_uris: HashMap<u64, Url> = HashMap::new();
    while let Ok(command) = receiver.recv() {
        match command {
            ManagerCommand::Open { data, config, root } => {
                let root = std::fs::canonicalize(&root).unwrap_or(root);
                let key = ServerKey {
                    name: config.name,
                    root: root.clone(),
                };
                let document_id = data.document_id;
                let uri = data.uri.clone();
                let sender = servers
                    .entry(key.clone())
                    .or_insert_with(|| spawn_server(config, root, sink.clone()));
                if sender.send(ServerMessage::Open(data)).is_ok() {
                    routes.insert(document_id, key);
                    document_uris.insert(document_id, uri);
                }
            }
            ManagerCommand::Change(data) => {
                if let Some(sender) = route_sender(&routes, &servers, data.document_id) {
                    let _ = sender.send(ServerMessage::Change(data));
                }
            }
            ManagerCommand::Save { document_id, uri } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::Save { uri });
                }
            }
            ManagerCommand::Close { document_id } => {
                if let Some(uri) = document_uris.remove(&document_id)
                    && let Some(key) = routes.remove(&document_id)
                    && let Some(sender) = servers.get(&key)
                {
                    let _ = sender.send(ServerMessage::Close { uri });
                }
            }
            ManagerCommand::Completion {
                document_id,
                revision,
                uri,
                position,
            } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::Completion {
                        document_id,
                        revision,
                        uri,
                        position,
                    });
                }
            }
            ManagerCommand::Definition {
                document_id,
                revision,
                uri,
                position,
            } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::Definition {
                        document_id,
                        revision,
                        uri,
                        position,
                    });
                }
            }
            ManagerCommand::Hover {
                document_id,
                revision,
                uri,
                position,
                editor_position,
            } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::Hover {
                        document_id,
                        revision,
                        uri,
                        position,
                        editor_position,
                    });
                }
            }
            ManagerCommand::References {
                document_id,
                revision,
                uri,
                position,
            } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::References {
                        document_id,
                        revision,
                        uri,
                        position,
                    });
                }
            }
            ManagerCommand::Rename {
                document_id,
                revision,
                uri,
                position,
                new_name,
            } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::Rename {
                        document_id,
                        revision,
                        uri,
                        position,
                        new_name,
                    });
                }
            }
            ManagerCommand::Formatting {
                document_id,
                revision,
                uri,
                tab_size,
                insert_spaces,
            } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::Formatting {
                        document_id,
                        revision,
                        uri,
                        tab_size,
                        insert_spaces,
                    });
                }
            }
            ManagerCommand::CodeActions {
                document_id,
                revision,
                uri,
                range,
                diagnostics,
            } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::CodeActions {
                        document_id,
                        revision,
                        uri,
                        range,
                        diagnostics,
                    });
                }
            }
            ManagerCommand::ResolveCodeAction {
                document_id,
                revision,
                action,
            } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::ResolveCodeAction {
                        document_id,
                        revision,
                        action,
                    });
                }
            }
            ManagerCommand::ExecuteCommand {
                document_id,
                command,
            } => {
                if let Some(sender) = route_sender(&routes, &servers, document_id) {
                    let _ = sender.send(ServerMessage::ExecuteCommand {
                        document_id,
                        command,
                    });
                }
            }
            ManagerCommand::Shutdown => {
                for sender in servers.values() {
                    let _ = sender.send(ServerMessage::Shutdown);
                }
                break;
            }
        }
    }
}

fn route_sender(
    routes: &HashMap<u64, ServerKey>,
    servers: &HashMap<ServerKey, Sender<ServerMessage>>,
    document_id: u64,
) -> Option<Sender<ServerMessage>> {
    let key = routes.get(&document_id)?;
    servers.get(key).cloned()
}

fn spawn_server(
    config: &'static ServerConfig,
    root: PathBuf,
    sink: EventSink,
) -> Sender<ServerMessage> {
    let (sender, receiver) = unbounded();
    thread::Builder::new()
        .name(format!("fce-lsp-{}", config.name))
        .spawn(move || {
            if let Err(error) = run_server(config, root, receiver, sink.clone()) {
                sink.status(format!("LSP {}: {error:#}", config.name), true);
            }
        })
        .expect("failed to start language server thread");
    sender
}

fn run_server(
    config: &'static ServerConfig,
    root: PathBuf,
    receiver: Receiver<ServerMessage>,
    sink: EventSink,
) -> Result<()> {
    let (mut child, mut stdin, stdout) = start_process(config)?;
    let mut stdout = BufReader::new(stdout);
    let root_uri = Url::from_file_path(&root)
        .map_err(|_| anyhow::anyhow!("Некорректный путь корня {}", root.display()))?;
    let initialize_result = initialize(&mut stdin, &mut stdout, &root_uri);
    if let Err(error) = initialize_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    sink.status(format!("LSP {} подключён", config.name), false);

    let (incoming_tx, incoming_rx) = unbounded();
    thread::Builder::new()
        .name(format!("fce-lsp-read-{}", config.name))
        .spawn(move || {
            while let Ok(Some(message)) = read_message(&mut stdout) {
                if incoming_tx.send(message).is_err() {
                    break;
                }
            }
        })
        .expect("failed to start language server reader");

    let result = server_loop(
        &mut child,
        &mut stdin,
        receiver,
        incoming_rx,
        root_uri,
        sink,
    );
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn start_process(config: &ServerConfig) -> Result<(Child, ChildStdin, ChildStdout)> {
    let mut last_error = None;
    for command in config.commands {
        match Command::new(command.program)
            .args(command.arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let stdin = child.stdin.take().context("LSP stdin недоступен")?;
                let stdout = child.stdout.take().context("LSP stdout недоступен")?;
                return Ok((child, stdin, stdout));
            }
            Err(error) => last_error = Some(error),
        }
    }
    bail!(
        "языковой сервер не найден ({})",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "нет команд".to_owned())
    )
}

fn initialize(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    root_uri: &Url,
) -> Result<()> {
    let root_name = root_uri
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("workspace");
    write_message(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{"uri": root_uri, "name": root_name}],
                "capabilities": {
                    "workspace": {
                        "workspaceFolders": true,
                        "configuration": true,
                        "workspaceEdit": {"documentChanges": true}
                    },
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "completion": {"completionItem": {"snippetSupport": false}},
                        "definition": {"linkSupport": true},
                        "publishDiagnostics": {"versionSupport": true},
                        "formatting": {},
                        "codeAction": {
                            "codeActionLiteralSupport": {
                                "codeActionKind": {
                                    "valueSet": [
                                        "", "quickfix", "refactor", "refactor.extract",
                                        "refactor.inline", "refactor.rewrite", "source",
                                        "source.organizeImports", "source.fixAll"
                                    ]
                                }
                            },
                            "isPreferredSupport": true,
                            "disabledSupport": true,
                            "dataSupport": true,
                            "resolveSupport": {"properties": ["edit"]}
                        }
                    }
                }
            }
        }),
    )?;
    loop {
        let Some(message) = read_message(stdout)? else {
            bail!("сервер завершился во время initialize");
        };
        if message.get("id").and_then(Value::as_u64) == Some(0) && message.get("method").is_none() {
            if let Some(error) = message.get("error") {
                bail!("initialize отклонён: {error}");
            }
            break;
        }
        if message.get("method").is_some() && message.get("id").is_some() {
            respond_to_server_request(stdin, &message, root_uri)?;
        }
    }
    write_message(
        stdin,
        &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    )?;
    Ok(())
}

enum PendingRequest {
    Completion {
        document_id: u64,
        revision: u64,
    },
    Definition {
        document_id: u64,
        revision: u64,
    },
    Hover {
        document_id: u64,
        revision: u64,
        position: Position,
    },
    References {
        document_id: u64,
        revision: u64,
    },
    Rename {
        document_id: u64,
        revision: u64,
    },
    Formatting {
        document_id: u64,
        revision: u64,
    },
    CodeActions {
        document_id: u64,
        revision: u64,
    },
    ResolveCodeAction {
        document_id: u64,
        revision: u64,
    },
    ExecuteCommand {
        document_id: u64,
    },
}

fn server_loop(
    child: &mut Child,
    stdin: &mut ChildStdin,
    receiver: Receiver<ServerMessage>,
    incoming: Receiver<Value>,
    root_uri: Url,
    sink: EventSink,
) -> Result<()> {
    let mut next_request_id = 1_u64;
    let mut pending = HashMap::new();
    let mut documents = HashMap::new();
    loop {
        select! {
            recv(receiver) -> command => {
                let Ok(command) = command else { break; };
                match command {
                    ServerMessage::Open(data) => {
                        documents.insert(data.uri.clone(), data.document_id);
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/didOpen",
                            "params": {"textDocument": {
                                "uri": data.uri,
                                "languageId": data.language_id,
                                "version": version(data.revision),
                                "text": data.text
                            }}
                        }))?;
                    }
                    ServerMessage::Change(data) => {
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/didChange",
                            "params": {
                                "textDocument": {"uri": data.uri, "version": version(data.revision)},
                                "contentChanges": [{"text": data.text}]
                            }
                        }))?;
                    }
                    ServerMessage::Save { uri, .. } => {
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/didSave",
                            "params": {"textDocument": {"uri": uri}}
                        }))?;
                    }
                    ServerMessage::Close { uri, .. } => {
                        documents.remove(&uri);
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/didClose",
                            "params": {"textDocument": {"uri": uri}}
                        }))?;
                    }
                    ServerMessage::Completion { document_id, revision, uri, position } => {
                        let id = next_request_id;
                        next_request_id += 1;
                        pending.insert(id, PendingRequest::Completion { document_id, revision });
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0", "id": id, "method": "textDocument/completion",
                            "params": {
                                "textDocument": {"uri": uri}, "position": position,
                                "context": {"triggerKind": 1}
                            }
                        }))?;
                    }
                    ServerMessage::Definition { document_id, revision, uri, position } => {
                        let id = next_request_id;
                        next_request_id += 1;
                        pending.insert(id, PendingRequest::Definition { document_id, revision });
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0", "id": id, "method": "textDocument/definition",
                            "params": {"textDocument": {"uri": uri}, "position": position}
                        }))?;
                    }
                    ServerMessage::Hover { document_id, revision, uri, position, editor_position } => {
                        let id = next_request_id;
                        next_request_id += 1;
                        pending.insert(id, PendingRequest::Hover {
                            document_id,
                            revision,
                            position: editor_position,
                        });
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0", "id": id, "method": "textDocument/hover",
                            "params": {"textDocument": {"uri": uri}, "position": position}
                        }))?;
                    }
                    ServerMessage::References { document_id, revision, uri, position } => {
                        let id = next_request_id;
                        next_request_id += 1;
                        pending.insert(id, PendingRequest::References { document_id, revision });
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0", "id": id, "method": "textDocument/references",
                            "params": {
                                "textDocument": {"uri": uri}, "position": position,
                                "context": {"includeDeclaration": true}
                            }
                        }))?;
                    }
                    ServerMessage::Rename { document_id, revision, uri, position, new_name } => {
                        let id = next_request_id;
                        next_request_id += 1;
                        pending.insert(id, PendingRequest::Rename { document_id, revision });
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0", "id": id, "method": "textDocument/rename",
                            "params": {
                                "textDocument": {"uri": uri}, "position": position,
                                "newName": new_name
                            }
                        }))?;
                    }
                    ServerMessage::Formatting { document_id, revision, uri, tab_size, insert_spaces } => {
                        let id = next_request_id;
                        next_request_id += 1;
                        pending.insert(id, PendingRequest::Formatting { document_id, revision });
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0", "id": id, "method": "textDocument/formatting",
                            "params": {
                                "textDocument": {"uri": uri},
                                "options": {"tabSize": tab_size, "insertSpaces": insert_spaces}
                            }
                        }))?;
                    }
                    ServerMessage::CodeActions { document_id, revision, uri, range, diagnostics } => {
                        let id = next_request_id;
                        next_request_id += 1;
                        pending.insert(id, PendingRequest::CodeActions { document_id, revision });
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0", "id": id, "method": "textDocument/codeAction",
                            "params": {
                                "textDocument": {"uri": uri}, "range": range,
                                "context": {"diagnostics": diagnostics}
                            }
                        }))?;
                    }
                    ServerMessage::ResolveCodeAction { document_id, revision, action } => {
                        let id = next_request_id;
                        next_request_id += 1;
                        pending.insert(id, PendingRequest::ResolveCodeAction { document_id, revision });
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0", "id": id, "method": "codeAction/resolve",
                            "params": action
                        }))?;
                    }
                    ServerMessage::ExecuteCommand { document_id, command } => {
                        let id = next_request_id;
                        next_request_id += 1;
                        pending.insert(id, PendingRequest::ExecuteCommand { document_id });
                        write_message(stdin, &json!({
                            "jsonrpc": "2.0", "id": id, "method": "workspace/executeCommand",
                            "params": {
                                "command": command.command,
                                "arguments": command.arguments.unwrap_or_default()
                            }
                        }))?;
                    }
                    ServerMessage::Shutdown => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(());
                    }
                }
            }
            recv(incoming) -> message => {
                let Ok(message) = message else {
                    bail!("соединение с сервером закрыто");
                };
                handle_incoming(
                    message,
                    stdin,
                    &mut pending,
                    &documents,
                    &root_uri,
                    &sink,
                )?;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn handle_incoming(
    message: Value,
    stdin: &mut ChildStdin,
    pending: &mut HashMap<u64, PendingRequest>,
    documents: &HashMap<Url, u64>,
    root_uri: &Url,
    sink: &EventSink,
) -> Result<()> {
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        if message.get("id").is_some() {
            return respond_to_server_request(stdin, &message, root_uri);
        }
        if method == "textDocument/publishDiagnostics" {
            let params: PublishDiagnosticsParams =
                serde_json::from_value(message["params"].clone())?;
            if let Some(&document_id) = documents.get(&params.uri) {
                sink.send(LspEvent::Diagnostics {
                    document_id,
                    version: params.version,
                    diagnostics: params.diagnostics,
                });
            }
        }
        return Ok(());
    }

    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        return Ok(());
    };
    let Some(request) = pending.remove(&id) else {
        return Ok(());
    };
    if let Some(error) = message.get("error") {
        sink.status(format!("Ошибка LSP-запроса: {error}"), true);
        return Ok(());
    }
    let result = message["result"].clone();
    match request {
        PendingRequest::Completion {
            document_id,
            revision,
        } => {
            let response: Option<CompletionResponse> = serde_json::from_value(result)?;
            let items = match response {
                Some(CompletionResponse::Array(items)) => items,
                Some(CompletionResponse::List(list)) => list.items,
                None => Vec::new(),
            };
            sink.send(LspEvent::Completion {
                document_id,
                revision,
                items,
            });
        }
        PendingRequest::Definition {
            document_id,
            revision,
        } => {
            let response: Option<GotoDefinitionResponse> = serde_json::from_value(result)?;
            let locations = definition_locations(response);
            sink.send(LspEvent::Definition {
                document_id,
                revision,
                locations,
            });
        }
        PendingRequest::Hover {
            document_id,
            revision,
            position,
        } => {
            let hover: Option<Hover> = serde_json::from_value(result)?;
            sink.send(LspEvent::Hover {
                document_id,
                revision,
                position,
                hover,
            });
        }
        PendingRequest::References {
            document_id,
            revision,
        } => {
            let locations: Option<Vec<Location>> = serde_json::from_value(result)?;
            sink.send(LspEvent::References {
                document_id,
                revision,
                locations: locations.unwrap_or_default(),
            });
        }
        PendingRequest::Rename {
            document_id,
            revision,
        } => {
            let edit: Option<WorkspaceEdit> = serde_json::from_value(result)?;
            sink.send(LspEvent::Rename {
                document_id,
                revision,
                edit,
            });
        }
        PendingRequest::Formatting {
            document_id,
            revision,
        } => {
            let edits: Option<Vec<TextEdit>> = serde_json::from_value(result)?;
            sink.send(LspEvent::Formatting {
                document_id,
                revision,
                edits: edits.unwrap_or_default(),
            });
        }
        PendingRequest::CodeActions {
            document_id,
            revision,
        } => {
            let actions: Option<CodeActionResponse> = serde_json::from_value(result)?;
            sink.send(LspEvent::CodeActions {
                document_id,
                revision,
                actions: actions.unwrap_or_default(),
            });
        }
        PendingRequest::ResolveCodeAction {
            document_id,
            revision,
        } => {
            let action: CodeAction = serde_json::from_value(result)?;
            sink.send(LspEvent::CodeActionResolved {
                document_id,
                revision,
                action: Box::new(action),
            });
        }
        PendingRequest::ExecuteCommand { document_id } => {
            sink.send(LspEvent::CommandExecuted { document_id });
        }
    }
    Ok(())
}

fn definition_locations(response: Option<GotoDefinitionResponse>) -> Vec<Location> {
    match response {
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
        Some(GotoDefinitionResponse::Array(locations)) => locations,
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| Location {
                uri: link.target_uri,
                range: link.target_range,
            })
            .collect(),
        None => Vec::new(),
    }
}

fn respond_to_server_request(
    stdin: &mut ChildStdin,
    message: &Value,
    root_uri: &Url,
) -> Result<()> {
    let id = message["id"].clone();
    let method = message["method"].as_str().unwrap_or_default();
    let result = match method {
        "workspace/configuration" => {
            let count = message["params"]["items"].as_array().map_or(0, Vec::len);
            Value::Array(vec![Value::Null; count])
        }
        "workspace/workspaceFolders" => {
            let name = root_uri
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .unwrap_or("workspace");
            json!([{"uri": root_uri, "name": name}])
        }
        _ => Value::Null,
    };
    write_message(
        stdin,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn write_message(writer: &mut impl Write, message: &Value) -> Result<()> {
    if std::env::var_os("FCE_LSP_DEBUG").is_some() {
        eprintln!("LSP -> {message}");
    }
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = content_length.context("Content-Length отсутствует")?;
    if length > MAX_MESSAGE_BYTES {
        bail!("LSP-сообщение слишком большое: {length} байт");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let message = serde_json::from_slice(&body)?;
    if std::env::var_os("FCE_LSP_DEBUG").is_some() {
        eprintln!("LSP <- {message}");
    }
    Ok(Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn json_rpc_frames_round_trip() {
        let message = json!({"jsonrpc": "2.0", "id": 7, "result": [{"label": "Привет"}]});
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).unwrap();
        let decoded = read_message(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn language_servers_cover_supported_extensions() {
        assert_eq!(
            server_config(Path::new("main.rs")).unwrap().name,
            "rust-analyzer"
        );
        assert_eq!(language_id(Path::new("component.tsx")), "typescriptreact");
        assert_eq!(language_id(Path::new("main.c")), "c");
        assert!(server_config(Path::new("README.md")).is_none());
    }

    #[test]
    fn location_links_use_selection_target() {
        let uri = Url::parse("file:///tmp/main.rs").unwrap();
        let response = Some(GotoDefinitionResponse::Link(vec![
            lsp_types::LocationLink {
                origin_selection_range: None,
                target_uri: uri.clone(),
                target_range: lsp_types::Range::default(),
                target_selection_range: lsp_types::Range::default(),
            },
        ]));
        assert_eq!(definition_locations(response)[0].uri, uri);
    }
}
