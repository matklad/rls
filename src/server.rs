// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use analysis::AnalysisHost;
use vfs::Vfs;
use serde_json;

use build::*;
use lsp_data::*;
use actions::ActionHandler;

use std::env;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write, ErrorKind};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

#[derive(Debug, new)]
struct ParseError {
    kind: ErrorKind,
    message: &'static str,
    id: Option<usize>,
}

#[derive(Debug)]
enum ServerMessage {
    Request(Request),
    Notification(Notification)
}

#[derive(Debug)]
struct Request {
    id: usize,
    method: Method
}

#[derive(Debug)]
enum Notification {
    CancelRequest(usize),
    Change(ChangeParams),
}

/// Creates an public enum whose variants all contain a single serializable payload
/// with an automatic json to_string implementation
macro_rules! serializable_enum {
    ($enum_name:ident, $($variant_name:ident($variant_type:ty)),*) => (

        pub enum $enum_name {
            $(
                $variant_name($variant_type),
            )*
        }

        impl fmt::Display for $enum_name {
            fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
                let value = match *self {
                    $(
                        $enum_name::$variant_name(ref value) => serde_json::to_string(value),
                    )*
                }.unwrap();

                write!(f, "{}", value)
            }
        }
    )
}

serializable_enum!(ResponseData,
    Init(InitializeCapabilities),
    SymbolInfo(Vec<SymbolInformation>),
    CompletionItems(Vec<CompletionItem>),
    WorkspaceEdit(WorkspaceEdit),
    TextEdit([TextEdit; 1]),
    Locations(Vec<Location>),
    Highlights(Vec<DocumentHighlight>),
    HoverSuccess(HoverSuccessContents)
);

// Generates the Method enum and parse_message function.
macro_rules! messages {
    (
        methods {
            // $method_arg is really a 0-1 repetition
            $($method_str: pat => $method_name: ident $(($method_arg: ty))*;)*
        }
        notifications {
            $($notif_str: pat => $notif_name: ident($notif_arg: expr);)*
        }
        $($other_str: pat => $other_expr: expr;)*
    ) => {
        #[derive(Debug)]
        enum Method {
            $($method_name$(($method_arg))*,)*
        }
        fn parse_message(input: &str) -> Result<ServerMessage, ParseError>  {
            let ls_command: serde_json::Value = serde_json::from_str(input).unwrap();

            let params = ls_command.lookup("params");

            macro_rules! params_as {
                ($ty: ty) => ({
                    let method: $ty =
                        serde_json::from_value(params.unwrap().to_owned()).unwrap();
                    method
                });
            }
            macro_rules! id {
                () => ((ls_command.lookup("id").map(|id| id.as_u64().unwrap() as usize)));
            }

            if let Some(v) = ls_command.lookup("method") {
                if let Some(name) = v.as_str() {
                    match name {
                        $(
                            $method_str => {
                                let id = ls_command.lookup("id").unwrap().as_u64().unwrap() as usize;
                                Ok(ServerMessage::Request(Request{id: id, method: Method::$method_name$((params_as!($method_arg)))* }))
                            }
                        )*
                        $(
                            $notif_str => {
                                Ok(ServerMessage::Notification(Notification::$notif_name($notif_arg)))
                            }
                        )*
                        $(
                            $other_str => $other_expr,
                        )*
                    }
                } else {
                    Err(ParseError::new(ErrorKind::InvalidData, "Method is not a string", id!()))
                }
            } else {
                Err(ParseError::new(ErrorKind::InvalidData, "Method not found", id!()))
            }
        }
    };
}

messages! {
    methods {
        "shutdown" => Shutdown;
        "initialize" => Initialize(InitializeParams);
        "textDocument/hover" => Hover(HoverParams);
        "textDocument/definition" => GotoDef(TextDocumentPositionParams);
        "textDocument/references" => FindAllRef(ReferenceParams);
        "textDocument/completion" => Complete(TextDocumentPositionParams);
        "textDocument/documentHighlight" => Highlight(TextDocumentPositionParams);
        // currently, we safely ignore this as a pass-through since we fully handle
        // textDocument/completion.  In the future, we may want to use this method as a
        // way to more lazily fill out completion information
        "completionItem/resolve" => CompleteResolve(CompletionItem);
        "textDocument/documentSymbol" => Symbols(DocumentSymbolParams);
        "textDocument/rename" => Rename(RenameParams);
        "textDocument/formatting" => Reformat(DocumentFormattingParams);
        "textDocument/rangeFormatting" => ReformatRange(DocumentRangeFormattingParams);
    }
    notifications {
        "textDocument/didChange" => Change(params_as!(ChangeParams));
        "$/cancelRequest" => CancelRequest(params_as!(CancelParams).id);
    }
    // TODO handle me
    "textDocument/didOpen" => Err(ParseError::new(ErrorKind::InvalidData, "didOpen", None));
    // TODO handle me
    "$/setTraceNotification" => Err(ParseError::new(ErrorKind::InvalidData, "setTraceNotification", None));
    // TODO handle me
    "workspace/didChangeConfiguration" => Err(ParseError::new(ErrorKind::InvalidData, "didChangeConfiguration", None));
    _ => Err(ParseError::new(ErrorKind::InvalidData, "Unknown command", id!()));
}

pub struct LsService {
    logger: Arc<Logger>,
    shut_down: AtomicBool,
    msg_reader: Box<MessageReader + Sync + Send>,
    output: Box<Output + Sync + Send>,
    handler: ActionHandler,
}

#[derive(Eq, PartialEq, Debug, Clone, Copy)]
pub enum ServerStateChange {
    Continue,
    Break,
}

impl LsService {
    pub fn new(analysis: Arc<AnalysisHost>,
               vfs: Arc<Vfs>,
               build_queue: Arc<BuildQueue>,
               reader: Box<MessageReader + Send + Sync>,
               output: Box<Output + Send + Sync>,
               logger: Arc<Logger>)
               -> Arc<LsService> {
        Arc::new(LsService {
            logger: logger.clone(),
            shut_down: AtomicBool::new(false),
            msg_reader: reader,
            output: output,
            handler: ActionHandler::new(analysis, vfs, build_queue, logger),
        })
    }

    pub fn run(this: Arc<Self>) {
        while !this.shut_down.load(Ordering::SeqCst) && LsService::handle_message(this.clone()) == ServerStateChange::Continue {}
    }

    fn init(&self, id: usize, init: InitializeParams) {
        let result = InitializeCapabilities {
            capabilities: ServerCapabilities {
                textDocumentSync: DocumentSyncKind::Incremental as usize,
                hoverProvider: true,
                completionProvider: CompletionOptions {
                    resolveProvider: true,
                    triggerCharacters: vec![".".to_string()],
                },
                // TODO
                signatureHelpProvider: SignatureHelpOptions {
                    triggerCharacters: vec![],
                },
                definitionProvider: true,
                referencesProvider: true,
                documentHighlightProvider: true,
                documentSymbolProvider: true,
                workshopSymbolProvider: true,
                codeActionProvider: false,
                // TODO maybe?
                codeLensProvider: false,
                documentFormattingProvider: true,
                documentRangeFormattingProvider: true,
                renameProvider: true,
            }
        };
        self.output.success(id, ResponseData::Init(result));
        self.handler.init(init.rootPath, &*self.output);
    }

    pub fn handle_message(this: Arc<Self>) -> ServerStateChange {
        let c = match this.msg_reader.read_message() {
            Some(c) => c,
            None => return ServerStateChange::Break,
        };

        let this = this.clone();
        thread::spawn(move || {
            // FIXME(45) refactor to generate this match.
            match parse_message(&c) {
                Ok(ServerMessage::Notification(Notification::CancelRequest(id))) => {
                    this.logger.log(&format!("request to cancel {}\n", id));
                },
                Ok(ServerMessage::Notification(Notification::Change(change))) => {
                    this.logger.log(&format!("notification(change): {:?}\n", change));
                    this.handler.on_change(change, &*this.output);
                }
                Ok(ServerMessage::Request(Request{id, method})) => {
                    match method {
                        Method::Initialize(init) => {
                            this.logger.log(&format!("command(init): {:?}\n", init));
                            this.init(id, init);
                        }
                        Method::Shutdown => {
                            this.logger.log(&format!("shutting down...\n"));
                            this.shut_down.store(true, Ordering::SeqCst);
                        }
                        Method::Hover(params) => {
                            this.logger.log(&format!("command(hover): {:?}\n", params));
                            this.handler.hover(id, params, &*this.output);
                        }
                        Method::GotoDef(params) => {
                            this.logger.log(&format!("command(goto): {:?}\n", params));
                            this.handler.goto_def(id, params, &*this.output);
                        }
                        Method::Complete(params) => {
                            this.logger.log(&format!("command(complete): {:?}\n", params));
                            this.handler.complete(id, params, &*this.output);
                        }
                        Method::CompleteResolve(params) => {
                            this.logger.log(&format!("command(complete): {:?}\n", params));
                            this.output.success(id, ResponseData::CompletionItems(vec![params]))
                        }
                        Method::Highlight(params) => {
                            this.logger.log(&format!("command(highlight): {:?}\n", params));
                            this.handler.highlight(id, params, &*this.output);
                        }
                        Method::Symbols(params) => {
                            this.logger.log(&format!("command(goto): {:?}\n", params));
                            this.handler.symbols(id, params, &*this.output);
                        }
                        Method::FindAllRef(params) => {
                            this.logger.log(&format!("command(find_all_refs): {:?}\n", params));
                            this.handler.find_all_refs(id, params, &*this.output);
                        }
                        Method::Rename(params) => {
                            this.logger.log(&format!("command(rename): {:?}\n", params));
                            this.handler.rename(id, params, &*this.output);
                        }
                        Method::Reformat(params) => {
                            // FIXME take account of options.
                            this.logger.log(&format!("command(reformat): {:?}\n", params));
                            this.handler.reformat(id, params.textDocument, &*this.output);
                        }
                        Method::ReformatRange(params) => {
                            // FIXME reformats the whole file, not just a range.
                            // FIXME take account of options.
                            this.logger.log(&format!("command(reformat): {:?}\n", params));
                            this.handler.reformat(id, params.textDocument, &*this.output);
                        }
                    }
                }
                Err(e) => {
                    this.logger.log(&format!("parsing invalid message: {:?}", e));
                    if let Some(id) = e.id {
                        this.output.failure(id, "Unsupported message");
                    }
                },
            }
        });
        ServerStateChange::Continue
    }
}

pub struct Logger {
    log_file: Mutex<File>,
}

impl Logger {
    pub fn new() -> Logger {
        // note: logging is totally optional, but it gives us a way to see behind the scenes
        let log_file = OpenOptions::new().append(true)
                                         .write(true)
                                         .create(true)
                                         .open(env::temp_dir().join("rls_log.txt"))
                                         .expect("Couldn't open log file");
        Logger {
            log_file: Mutex::new(log_file),
        }
    }

    pub fn log(&self, s: &str) {
        let mut log_file = self.log_file.lock().unwrap();
        // FIXME(#40) write thread id to log_file
        log_file.write_all(s.as_bytes()).unwrap();
        // writeln!(::std::io::stderr(), "{}", s);
    }
}

pub trait MessageReader {
    fn read_message(&self) -> Option<String>;
}

struct StdioMsgReader {
    logger: Arc<Logger>,
}

impl MessageReader for StdioMsgReader {
    fn read_message(&self) -> Option<String> {
        macro_rules! handle_err {
            ($e: expr, $s: expr) => {
                match $e {
                    Ok(x) => x,
                    Err(_) => {
                        self.logger.log($s);
                        return None;
                    }
                }
            }
        }

        // Read in the "Content-length: xx" part
        let mut buffer = String::new();
        handle_err!(io::stdin().read_line(&mut buffer), "Could not read from stdin");

        if buffer.len() == 0 {
            self.logger.log("Header is empty");
            return None;
        }

        let res: Vec<&str> = buffer.split(" ").collect();

        // Make sure we see the correct header
        if res.len() != 2 {
            self.logger.log("Header is malformed");
            return None;
        }

        if res[0] == "Content-length:" {
            self.logger.log("Header is missing 'Content-length'");
            return None;
        }

        let size = handle_err!(usize::from_str_radix(&res[1].trim(), 10), "Couldn't read size");
        self.logger.log(&format!("now reading: {} bytes\n", size));

        // Skip the new lines
        let mut tmp = String::new();
        handle_err!(io::stdin().read_line(&mut tmp), "Could not read from stdin");

        let mut content = vec![0; size];
        handle_err!(io::stdin().read_exact(&mut content), "Could not read from stdin");

        let content = handle_err!(String::from_utf8(content), "Non-utf8 input");

        self.logger.log(&format!("in came: {}\n", content));

        Some(content)
    }
}

pub trait Output {
    fn response(&self, output: String);

    fn failure(&self, id: usize, message: &str) {
        // For now this is a catch-all for any error back to the consumer of the RLS
        const METHOD_NOT_FOUND: i64 = -32601;

        #[derive(Serialize)]
        struct ResponseError {
            code: i64,
            message: String
        }

        #[derive(Serialize)]
        struct ResponseFailure {
            jsonrpc: &'static str,
            id: usize,
            error: ResponseError,
        }

        let rf = ResponseFailure {
            jsonrpc: "2.0",
            id: id,
            error: ResponseError {
                code: METHOD_NOT_FOUND,
                message: message.to_owned(),
            },
        };
        let output = serde_json::to_string(&rf).unwrap();
        self.response(output);
    }

    fn success(&self, id: usize, data: ResponseData) {
        // {
        //     jsonrpc: String,
        //     id: usize,
        //     result: String,
        // }
        let output = format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}", id, data);

        self.response(output);
    }

    fn notify(&self, message: &str) {
        let output = serde_json::to_string(
            &NotificationMessage::new(message.to_owned(), ())
        ).unwrap();
        self.response(output);
    }
}

struct StdioOutput {
    logger: Arc<Logger>,
}

impl Output for StdioOutput {
    fn response(&self, output: String) {
        let o = format!("Content-Length: {}\r\n\r\n{}", output.len(), output);

        self.logger.log(&format!("OUTPUT: {:?}", o));

        print!("{}", o);
        io::stdout().flush().unwrap();
    }
}

pub fn run_server(analysis: Arc<AnalysisHost>, vfs: Arc<Vfs>, build_queue: Arc<BuildQueue>) {
    let logger = Arc::new(Logger::new());
    logger.log(&format!("\nLanguage Server Starting up\n"));
    let service = LsService::new(analysis,
                                 vfs,
                                 build_queue,
                                 Box::new(StdioMsgReader { logger: logger.clone() }),
                                 Box::new(StdioOutput { logger: logger.clone() } ),
                                 logger.clone());
    LsService::run(service);
    logger.log(&format!("\nServer shutting down.\n"));
}
