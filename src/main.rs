#[macro_use] extern crate failure;
#[macro_use] extern crate serde_derive;

mod models;
mod utils;

use self::models::*;

use failure::Error;
use rnix::{
    parser::AST,
    Error as NixError
};
use std::{
    collections::HashMap,
    fmt,
    fs::File,
    io::{self, prelude::*}
};

fn main() -> Result<(), Error> {
    let mut log = File::create("/tmp/nix-lsp.log")?;

    let stdout = io::stdout();
    let mut app = App {
        files: HashMap::new(),
        log: &mut log,
        stdout: stdout.lock()
    };
    if let Err(err) = app.main() {
        writeln!(log, "{:?}", err);
        return Err(err);
    }

    Ok(())
}

struct App<'a, W: io::Write> {
    files: HashMap<String, (Option<AST<'static>>, String)>,
    log: &'a mut File,
    stdout: W
}
impl<'a, W: io::Write> App<'a, W> {
    fn main(&mut self) -> Result<(), Error> {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();

        loop {
            let mut length = None;
            let mut line = String::new();

            loop {
                stdin.read_line(&mut line)?;

                let mut parts = line.split(':');
                match (parts.next().map(str::trim), parts.next().map(str::trim)) {
                    (Some("Content-Length"), Some(x)) => length = Some(x.parse()?),
                    _ => ()
                }

                if line.is_empty() || line.ends_with("\r\n\r\n") {
                    break;
                }
            }

            let length = length.ok_or_else(|| format_err!("missing Content-Length in request"))?;

            let mut body = vec![0; length];
            stdin.read_exact(&mut body)?;

            writeln!(self.log, "Raw: {:?}", std::str::from_utf8(&body).unwrap_or_default())?;
            let req: Result<Request, _> = serde_json::from_slice(&body);
            writeln!(self.log, "{:#?}", req)?;

            let req = match req {
                Ok(req) => req,
                Err(err) => {
                    writeln!(self.log, "{:?}", err);
                    self.send(&Response::error(None, err))?;
                    continue;
                }
            };

            let id = req.id;
            if let Err(err) = self.handle_request(req) {
                writeln!(self.log, "{:?}", err);
                self.send(&Response::error(id, err))?;
            }
        }
    }
    fn send<T: serde::Serialize + fmt::Debug>(&mut self, msg: &T) -> Result<(), Error> {
        writeln!(self.log, "Sending: {:?}", msg)?;
        let bytes = serde_json::to_vec(msg)?;
        write!(self.stdout, "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n")?;
        write!(self.stdout, "Content-Length: {}\r\n", bytes.len())?;
        write!(self.stdout, "\r\n")?;

        self.stdout.write_all(&bytes)?;
        self.stdout.flush()?;
        Ok(())
    }
    fn handle_request(&mut self, req: Request) -> Result<(), Error> {
        match &*req.method {
            "initialize" => self.send(&Response::success(req.id, Some(
                InitializeResult {
                    capabilities: ServerCapabilities {
                        definition_provider: true,
                        completion_provider: CompletionOptions {
                            resolve_provider: true
                        }
                    }
                }
            )))?,
            "textDocument/didOpen" => {
                let params: DidOpen = serde_json::from_value(req.params)?;
                let text = params.text_document.text.ok_or_else(|| format_err!("missing text in request"))?;
                let parsed = rnix::parse(&text);
                self.send_diagnostics(params.text_document.uri.clone(), &text, &parsed)?;
                self.files.insert(params.text_document.uri, (parsed.ok(), text));
            },
            "textDocument/didChange" => {
                let params: DidChange = serde_json::from_value(req.params)?;
                if let Some(change) = params.content_changes.into_iter().last() {
                    //writeln!(self.log, "PARSED: {:?}", rnix::parse(&change.text).map(|_| ()))?;
                    let parsed = rnix::parse(&change.text);
                    self.send_diagnostics(params.text_document.uri.clone(), &change.text, &parsed)?;
                    self.files.insert(params.text_document.uri, (parsed.ok(), change.text));
                }
            },
            "textDocument/definition" => {
                let params: Definition = serde_json::from_value(req.params)?;
                if let Some((Some(ast), code)) = self.files.get(&params.text_document.uri) {
                    let offset = utils::lookup_pos(code, params.position)?;

                    let mut scope = Vec::new();
                    let (name, _) = utils::ident_at(code, offset);
                    let def = utils::lookup_var(
                        &ast.arena,
                        &ast.root,
                        &mut scope,
                        offset as u32,
                        &mut |scopes, _span| {
                            scopes.iter().rev()
                                .filter_map(|scope| scope.get(&*name).cloned())
                                .next()
                        });
                    //writeln!(self.log, "LOOKUP DEFINITION {:?} {:?} {:?}", offset, scope, def)?;

                    self.send(&Response::success(req.id, if let Some(Some(span)) = def {
                        Some(Location {
                            uri: params.text_document.uri,
                            range: utils::span_to_range(code, span)
                        })
                    } else {
                        None
                    }))?;
                } else {
                    self.send(&Response::empty(req.id))?;
                }
            },
            "textDocument/completion" => {
                let params: Definition = serde_json::from_value(req.params)?;
                if let Some((Some(ast), code)) = self.files.get(&params.text_document.uri) {
                    let offset = utils::lookup_pos(code, params.position)?;

                    let mut scopes = Vec::new();
                    let (name, span) = utils::ident_at(code, offset);
                    let def = utils::lookup_var(
                        &ast.arena,
                        &ast.root,
                        &mut scopes,
                        offset as u32,
                        &mut |_, _| ()
                    );

                    let mut completions = Vec::new();

                    if let Some(()) = def {
                        for scope in scopes {
                            for (var, _) in scope {
                                if var.starts_with(&name) {
                                    completions.push(CompletionItem {
                                        label: var.clone(),
                                        edit: TextEdit {
                                            range: utils::span_to_range(code, span),
                                            new_text: var
                                        }
                                    });
                                }
                            }
                        }
                    }

                    self.send(&Response::success(req.id, completions))?;
                }
            },
            _ => ()
        }
        Ok(())
    }
    fn send_diagnostics(&mut self, uri: String, code: &str, ast: &Result<AST, NixError>) -> Result<(), Error> {
        let errors = match ast {
            Ok(ast) => {
                ast.errors()
                    .map(|(span, err)| (*span, err.to_string()))
                    .collect()
            },
            Err(err) => match *err {
                NixError::TokenizeError(span, ref err) => vec![(Some(span), err.to_string())],
                NixError::ParseError(span, ref err) => vec![(span, err.to_string())],
            }
        };
        let mut diagnostics = Vec::with_capacity(errors.len());
        for (span, error) in errors {
            if let Some(span) = span {
                diagnostics.push(Diagnostic {
                    range: utils::span_to_range(code, span),
                    severity: ERROR,
                    message: error
                });
            }
        }
        self.send(&Notification {
            method: "textDocument/publishDiagnostics".into(),
            params: DiagnosticParams {
                uri,
                diagnostics
            }
        })
    }
}
