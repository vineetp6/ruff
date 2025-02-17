//! Scheduling, I/O, and API endpoints.

use std::num::NonZeroUsize;

use lsp::Connection;
use lsp_server as lsp;
use lsp_types as types;
use types::ClientCapabilities;
use types::CodeActionKind;
use types::CodeActionOptions;
use types::DiagnosticOptions;
use types::DidChangeWatchedFilesRegistrationOptions;
use types::FileSystemWatcher;
use types::OneOf;
use types::TextDocumentSyncCapability;
use types::TextDocumentSyncKind;
use types::TextDocumentSyncOptions;
use types::WorkDoneProgressOptions;
use types::WorkspaceFoldersServerCapabilities;

use self::schedule::event_loop_thread;
use self::schedule::Scheduler;
use self::schedule::Task;
use crate::session::Session;
use crate::PositionEncoding;

mod api;
mod client;
mod schedule;

pub(crate) type Result<T> = std::result::Result<T, api::Error>;

pub struct Server {
    conn: lsp::Connection,
    client_capabilities: ClientCapabilities,
    threads: lsp::IoThreads,
    worker_threads: NonZeroUsize,
    session: Session,
}

impl Server {
    pub fn new(worker_threads: NonZeroUsize) -> crate::Result<Self> {
        let (conn, threads) = lsp::Connection::stdio();

        let (id, params) = conn.initialize_start()?;

        let init_params: types::InitializeParams = serde_json::from_value(params)?;

        let client_capabilities = init_params.capabilities;
        let server_capabilities = Self::server_capabilities(&client_capabilities);

        let workspaces = init_params
            .workspace_folders
            .map(|folders| folders.into_iter().map(|folder| folder.uri).collect())
            .or_else(|| {
                tracing::debug!("No workspace(s) were provided during initialization. Using the current working directory as a default workspace...");
                Some(vec![types::Url::from_file_path(std::env::current_dir().ok()?).ok()?])
            })
            .ok_or_else(|| {
                anyhow::anyhow!("Failed to get the current working directory while creating a default workspace.")
            })?;

        let initialize_data = serde_json::json!({
            "capabilities": server_capabilities,
            "serverInfo": {
                "name": crate::SERVER_NAME,
                "version": crate::version()
            }
        });

        conn.initialize_finish(id, initialize_data)?;

        Ok(Self {
            conn,
            threads,
            worker_threads,
            session: Session::new(&client_capabilities, &server_capabilities, &workspaces)?,
            client_capabilities,
        })
    }

    pub fn run(self) -> crate::Result<()> {
        let result = event_loop_thread(move || {
            Self::event_loop(
                &self.conn,
                &self.client_capabilities,
                self.session,
                self.worker_threads,
            )
        })?
        .join();
        self.threads.join()?;
        result
    }

    #[allow(clippy::needless_pass_by_value)] // this is because we aren't using `next_request_id` yet.
    fn event_loop(
        connection: &Connection,
        client_capabilities: &ClientCapabilities,
        mut session: Session,
        worker_threads: NonZeroUsize,
    ) -> crate::Result<()> {
        let mut scheduler =
            schedule::Scheduler::new(&mut session, worker_threads, &connection.sender);

        Self::try_register_capabilities(client_capabilities, &mut scheduler);
        for msg in &connection.receiver {
            let task = match msg {
                lsp::Message::Request(req) => {
                    if connection.handle_shutdown(&req)? {
                        return Ok(());
                    }
                    api::request(req)
                }
                lsp::Message::Notification(notification) => api::notification(notification),
                lsp::Message::Response(response) => scheduler.response(response),
            };
            scheduler.dispatch(task);
        }
        Ok(())
    }

    fn try_register_capabilities(
        client_capabilities: &ClientCapabilities,
        scheduler: &mut Scheduler,
    ) {
        let dynamic_registration = client_capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files)
            .and_then(|watched_files| watched_files.dynamic_registration)
            .unwrap_or_default();
        if dynamic_registration {
            // Register all dynamic capabilities here

            // `workspace/didChangeWatchedFiles`
            // (this registers the configuration file watcher)
            let params = lsp_types::RegistrationParams {
                registrations: vec![lsp_types::Registration {
                    id: "ruff-server-watch".into(),
                    method: "workspace/didChangeWatchedFiles".into(),
                    register_options: Some(
                        serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                            watchers: vec![
                                FileSystemWatcher {
                                    glob_pattern: types::GlobPattern::String(
                                        "**/.?ruff.toml".into(),
                                    ),
                                    kind: None,
                                },
                                FileSystemWatcher {
                                    glob_pattern: types::GlobPattern::String(
                                        "**/pyproject.toml".into(),
                                    ),
                                    kind: None,
                                },
                            ],
                        })
                        .unwrap(),
                    ),
                }],
            };

            let response_handler = |()| {
                tracing::info!("Configuration file watcher successfully registered");
                Task::nothing()
            };

            if let Err(err) = scheduler
                .request::<lsp_types::request::RegisterCapability>(params, response_handler)
            {
                tracing::error!("An error occurred when trying to register the configuration file watcher: {err}");
            }
        } else {
            tracing::warn!("LSP client does not support dynamic capability registration - automatic configuration reloading will not be available.");
        }
    }

    fn server_capabilities(client_capabilities: &ClientCapabilities) -> types::ServerCapabilities {
        let position_encoding = client_capabilities
            .general
            .as_ref()
            .and_then(|general_capabilities| general_capabilities.position_encodings.as_ref())
            .and_then(|encodings| {
                encodings
                    .iter()
                    .filter_map(|encoding| PositionEncoding::try_from(encoding).ok())
                    .max() // this selects the highest priority position encoding
            })
            .unwrap_or_default();
        types::ServerCapabilities {
            position_encoding: Some(position_encoding.into()),
            code_action_provider: Some(types::CodeActionProviderCapability::Options(
                CodeActionOptions {
                    code_action_kinds: Some(
                        SupportedCodeAction::all()
                            .flat_map(|action| action.kinds().into_iter())
                            .collect(),
                    ),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                    resolve_provider: Some(true),
                },
            )),
            workspace: Some(types::WorkspaceServerCapabilities {
                workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                    supported: Some(true),
                    change_notifications: Some(OneOf::Left(true)),
                }),
                file_operations: None,
            }),
            document_formatting_provider: Some(OneOf::Left(true)),
            document_range_formatting_provider: Some(OneOf::Left(true)),
            diagnostic_provider: Some(types::DiagnosticServerCapabilities::Options(
                DiagnosticOptions {
                    identifier: Some(crate::DIAGNOSTIC_NAME.into()),
                    // multi-file analysis could change this
                    inter_file_dependencies: false,
                    workspace_diagnostics: false,
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                },
            )),
            text_document_sync: Some(TextDocumentSyncCapability::Options(
                TextDocumentSyncOptions {
                    open_close: Some(true),
                    change: Some(TextDocumentSyncKind::INCREMENTAL),
                    will_save: Some(false),
                    will_save_wait_until: Some(false),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }
}

/// The code actions we support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SupportedCodeAction {
    /// Maps to the `quickfix` code action kind. Quick fix code actions are shown under
    /// their respective diagnostics. Quick fixes are only created where the fix applicability is
    /// at least [`ruff_diagnostics::Applicability::Unsafe`].
    QuickFix,
    /// Maps to the `source.fixAll` and `source.fixAll.ruff` code action kinds.
    /// This is a source action that applies all safe fixes to the currently open document.
    SourceFixAll,
    /// Maps to `source.organizeImports` and `source.organizeImports.ruff` code action kinds.
    /// This is a source action that applies import sorting fixes to the currently open document.
    #[allow(dead_code)] // TODO: remove
    SourceOrganizeImports,
}

impl SupportedCodeAction {
    /// Returns the possible LSP code action kind(s) that map to this code action.
    fn kinds(self) -> Vec<CodeActionKind> {
        match self {
            Self::QuickFix => vec![CodeActionKind::QUICKFIX],
            Self::SourceFixAll => vec![CodeActionKind::SOURCE_FIX_ALL, crate::SOURCE_FIX_ALL_RUFF],
            Self::SourceOrganizeImports => vec![
                CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                crate::SOURCE_ORGANIZE_IMPORTS_RUFF,
            ],
        }
    }

    /// Returns all code actions kinds that the server currently supports.
    fn all() -> impl Iterator<Item = Self> {
        [
            Self::QuickFix,
            Self::SourceFixAll,
            Self::SourceOrganizeImports,
        ]
        .into_iter()
    }
}

impl TryFrom<CodeActionKind> for SupportedCodeAction {
    type Error = ();

    fn try_from(kind: CodeActionKind) -> std::result::Result<Self, Self::Error> {
        for supported_kind in Self::all() {
            if supported_kind.kinds().contains(&kind) {
                return Ok(supported_kind);
            }
        }
        Err(())
    }
}
