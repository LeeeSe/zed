use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::{
    display_map::{BlockContext, BlockDisposition, BlockProperties, BlockStyle, RenderBlock},
    Editor,
};
use futures::{channel::mpsc, SinkExt as _, StreamExt as _};
use gpui::{actions, AppContext, Context, EntityId, Global, Model};
use gpui::{Entity, View};
use language::Point;
use outputs::ExecutionView;
use runtimelib::{JupyterMessage, JupyterMessageContent};
use settings::Settings as _;
use std::path::PathBuf;
use std::sync::Arc;
use ui::prelude::*;
use util::ResultExt;
use workspace::Workspace;

mod kernelspecs;
mod outputs;
mod stdio;

use theme::{ActiveTheme, ThemeSettings};

actions!(runtimes, [Run]);

#[derive(Clone)]
pub struct RuntimeGlobal(Model<RuntimeManager>);

impl Global for RuntimeGlobal {}

/** On startup, we will look for all available kernels, or so I expect */

pub fn init(cx: &mut AppContext) {
    let runtime = cx.new_model(|cx| RuntimeManager::new(cx));
    RuntimeManager::set_global(runtime.clone(), cx);

    cx.observe_new_views(
        |workspace: &mut Workspace, _: &mut ViewContext<Workspace>| {
            // Note: this will have to both start a kernel if not already running, and run code selections
            workspace.register_action(RuntimeManager::run);
        },
    )
    .detach();
}

#[derive(Debug)]
pub struct ExecutionRequest {
    execution_id: ExecutionId,
    request: runtimelib::ExecuteRequest,
    response_sender: mpsc::UnboundedSender<ExecutionUpdate>,
}

pub struct Runtime {
    execution_request_tx: mpsc::UnboundedSender<ExecutionRequest>,
    _runtime_handle: std::thread::JoinHandle<()>,
}

pub struct RuntimeManager {
    runtimes: HashMap<EntityId, Runtime>,
}

static HARDCODED_DENO_KERNEL: &str =
    "/Users/kylekelley/Library/Jupyter/runtime/kernel-c4579ffe-82a7-4a82-bf70-41af60dc39ec.json";

static HARDCODED_PYTHON_KERNEL: &str =
    "/Users/kylekelley/Library/Jupyter/runtime/kernel-890afee2-2367-4dc2-b6f6-1dede4493f82.json";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ExecutionId(String);

impl ExecutionId {
    fn new() -> Self {
        ExecutionId(uuid::Uuid::new_v4().to_string())
    }
}

impl From<String> for ExecutionId {
    fn from(id: String) -> Self {
        ExecutionId(id)
    }
}

#[derive(Debug)]
struct ExecutionUpdate {
    #[allow(dead_code)]
    execution_id: ExecutionId,
    update: JupyterMessageContent,
}

// Associates execution IDs with outputs and other messages
struct DocumentClient {
    iopub_handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    shell_handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    _executions:
        Arc<tokio::sync::Mutex<HashMap<ExecutionId, mpsc::UnboundedSender<ExecutionUpdate>>>>,
}

impl DocumentClient {
    async fn new(
        kernel_path: &PathBuf,
        mut execution_request_rx: mpsc::UnboundedReceiver<ExecutionRequest>,
    ) -> Result<Self> {
        let connection_info = runtimelib::ConnectionInfo::from_path(kernel_path).await?;

        let mut iopub = connection_info.create_client_iopub_connection("").await?;
        let mut shell = connection_info.create_client_shell_connection().await?;

        let executions: Arc<
            tokio::sync::Mutex<HashMap<ExecutionId, mpsc::UnboundedSender<ExecutionUpdate>>>,
        > = Default::default();

        let iopub_handle = tokio::spawn({
            let executions = executions.clone();
            async move {
                loop {
                    let message = iopub.read().await?;

                    if let Some(parent_header) = message.parent_header {
                        let execution_id = ExecutionId::from(parent_header.msg_id);

                        if let Some(mut execution) = executions.lock().await.get(&execution_id) {
                            execution
                                .send(ExecutionUpdate {
                                    execution_id,
                                    update: message.content,
                                })
                                .await
                                .ok();
                        }
                    }
                }
            }
        });

        let shell_handle = tokio::spawn({
            let executions = executions.clone();
            async move {
                while let Some(execution) = execution_request_rx.next().await {
                    let mut message: JupyterMessage = execution.request.into();
                    message.header.msg_id.clone_from(&execution.execution_id.0);

                    executions
                        .lock()
                        .await
                        .insert(execution.execution_id, execution.response_sender);

                    shell
                        .send(message)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to send execute request: {e:?}"))?;
                }
                anyhow::Ok(())
            }
        });

        let document_client = Self {
            iopub_handle,
            shell_handle,
            _executions: executions,
        };

        Ok(document_client)
    }
}

impl RuntimeManager {
    pub fn new(_cx: &mut AppContext) -> Self {
        Self {
            runtimes: Default::default(),
        }
    }

    pub fn spawn_kernel(
        &mut self,
        language_name: &Arc<str>,
        entity_id: EntityId,
    ) -> Option<mpsc::UnboundedSender<ExecutionRequest>> {
        let kernel_path = match language_name.as_ref() {
            "python" => HARDCODED_PYTHON_KERNEL,
            "typescript" => HARDCODED_DENO_KERNEL,
            // todo!(): don't run any kernel if the language is not supported
            _ => HARDCODED_PYTHON_KERNEL,
        };

        let maybe_runtime = self.runtimes.get(&entity_id);
        if let Some(runtime) = maybe_runtime {
            return Some(runtime.execution_request_tx.clone());
        }

        let (execution_request_tx, execution_request_rx) = mpsc::unbounded::<ExecutionRequest>();

        let kernel_path = std::path::PathBuf::from(kernel_path);

        let _runtime_handle = std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio failed to start");

            // TODO: Will need a signal handler to shutdown the runtime
            runtime
                .block_on(async move {
                    // Set up the kernel client here as our prototype
                    let kernel_path = kernel_path.clone();
                    let document_client =
                        DocumentClient::new(&kernel_path, execution_request_rx).await?;

                    let join_fut = futures::future::try_join(
                        document_client.iopub_handle,
                        document_client.shell_handle,
                    );

                    let results = join_fut.await?;

                    if let Err(e) = results.0 {
                        log::error!("iopub error: {e:?}");
                    }
                    if let Err(e) = results.1 {
                        log::error!("shell error: {e:?}");
                    }

                    anyhow::Ok(())
                })
                .log_err();
        });

        let runtime = Runtime {
            execution_request_tx: execution_request_tx.clone(),
            _runtime_handle,
        };

        self.runtimes.insert(entity_id, runtime);

        Some(execution_request_tx)
    }

    fn execute_code(
        &mut self,
        entity_id: EntityId,
        language_name: &Arc<str>,
        execution_id: ExecutionId,
        code: String,
    ) -> Result<mpsc::UnboundedReceiver<ExecutionUpdate>> {
        let (tx, rx) = mpsc::unbounded();

        let execution_request_tx = match self.spawn_kernel(language_name, entity_id) {
            Some(execution_request_tx) => execution_request_tx,
            None => return Err(anyhow::anyhow!("Could not spawn kernel")),
        };

        execution_request_tx
            .unbounded_send(ExecutionRequest {
                execution_id,
                request: runtimelib::ExecuteRequest {
                    code,
                    allow_stdin: false,
                    silent: false,
                    store_history: true,
                    user_expressions: None,
                    stop_on_error: false,
                    // TODO(runtimelib): set up Default::default() for the rest of the fields
                    // ..Default::default()
                },
                response_sender: tx,
            })
            .expect("Failed to send execution request");

        Ok(rx)
    }

    pub fn global(cx: &AppContext) -> Option<Model<Self>> {
        cx.try_global::<RuntimeGlobal>()
            .map(|model| model.0.clone())
    }

    pub fn set_global(runtime: Model<Self>, cx: &mut AppContext) {
        cx.set_global(RuntimeGlobal(runtime));
    }

    pub fn run(workspace: &mut Workspace, _: &Run, cx: &mut ViewContext<Workspace>) {
        let code_snippet = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            .map(|editor_view| {
                let editor = editor_view.read(cx);
                let selection = editor.selections.newest::<usize>(cx);
                let buffer = editor.buffer().read(cx).snapshot(cx);

                let range = if selection.is_empty() {
                    let cursor = selection.head();

                    let line_start = buffer.offset_to_point(cursor).row;
                    let mut start_offset = buffer.point_to_offset(Point::new(line_start, 0));

                    // Iterate backwards to find the start of the line
                    while start_offset > 0 {
                        let ch = buffer.chars_at(start_offset - 1).next().unwrap_or('\0');
                        if ch == '\n' {
                            break;
                        }
                        start_offset -= 1;
                    }

                    let mut end_offset = cursor;

                    // Iterate forwards to find the end of the line
                    while end_offset < buffer.len() {
                        let ch = buffer.chars_at(end_offset).next().unwrap_or('\0');
                        if ch == '\n' {
                            break;
                        }
                        end_offset += 1;
                    }

                    // Create a range from the start to the end of the line
                    start_offset..end_offset
                } else {
                    selection.range()
                };

                let anchor = buffer.anchor_after(range.end);

                let selected_text = buffer.text_for_range(range.clone()).collect::<String>();

                let start_language = buffer.language_at(range.start);
                let end_language = buffer.language_at(range.end);
                let language_name = if start_language == end_language {
                    start_language.map(|language| language.code_fence_block_name())
                } else {
                    None
                };

                (selected_text, language_name, anchor, editor_view)
            });

        if let Some((code, language_name, anchor, editor)) = code_snippet {
            let language_name = if let Some(language_name) = language_name {
                language_name
            } else {
                return;
            };

            if language_name.as_ref() == "markdown" {
                return;
            }

            if let Some(model) = RuntimeManager::global(cx) {
                let entity_id = editor.entity_id();
                let execution_id = ExecutionId::new();

                let receiver = model.update(cx, |model, _| {
                    model.execute_code(
                        entity_id,
                        &language_name,
                        execution_id.clone(),
                        code.clone(),
                    )
                });

                let mut receiver = match receiver {
                    Ok(receiver) => receiver,
                    Err(e) => {
                        log::error!("Failed to execute code: {e:?}");
                        return;
                    }
                };

                let execution_view = cx.new_view(|cx| ExecutionView::new(execution_id.clone(), cx));

                // Since we don't know the height, in editor terms, we have to calculate it over time
                // and just create a new block, replacing the old. It would be better if we could
                // just rely on the view updating and for the height to be calculated automatically.
                //
                // We will just handle text for the moment to keep this accurate.
                // Plots and other images will have to wait.

                let mut block_id = editor.update(cx, |editor, cx| {
                    let block = BlockProperties {
                        position: anchor,
                        height: 1,
                        style: BlockStyle::Sticky,
                        render: create_output_area_render(execution_view.clone()),
                        disposition: BlockDisposition::Below,
                    };

                    editor.insert_blocks([block], None, cx)[0]
                });

                cx.spawn(|_this, mut cx| async move {
                    let execution_view = execution_view.clone();
                    while let Some(update) = receiver.next().await {
                        execution_view.update(&mut cx, |execution_view, cx| {
                            execution_view.push_message(&update.update, cx)
                        })?;

                        editor.update(&mut cx, |editor, cx| {
                            let mut blocks_to_remove = HashSet::default();
                            blocks_to_remove.insert(block_id);

                            editor.remove_blocks(blocks_to_remove, None, cx);

                            let block = BlockProperties {
                                position: anchor,
                                height: 1 + execution_view.read(cx).execution.read(cx).num_lines(),
                                style: BlockStyle::Sticky,
                                render: create_output_area_render(execution_view.clone()),
                                disposition: BlockDisposition::Below,
                            };

                            block_id = editor.insert_blocks([block], None, cx)[0];
                        })?;
                    }
                    anyhow::Ok(())
                })
                .detach();
            }
        }
    }
}

fn create_output_area_render(execution_view: View<ExecutionView>) -> RenderBlock {
    let render = move |cx: &mut BlockContext| {
        let execution_view = execution_view.clone();
        let text_font = ThemeSettings::get_global(cx).buffer_font.family.clone();
        // Note: we'll want to use `cx.anchor_x` when someone runs something with no output -- just show a checkmark and not make the full block below the line

        let gutter_width = cx.gutter_dimensions.width;

        h_flex()
            .w_full()
            .bg(cx.theme().colors().background)
            .border_y_1()
            .border_color(cx.theme().colors().border)
            .pl(gutter_width)
            .child(
                div()
                    .font_family(text_font)
                    // .ml(gutter_width)
                    .mx_1()
                    .my_2()
                    .h_full()
                    .w_full()
                    .mr(gutter_width)
                    .child(execution_view),
            )
            .into_any_element()
    };

    Box::new(render)
}
