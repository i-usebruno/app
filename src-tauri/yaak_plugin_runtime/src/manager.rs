use crate::error::Error::{ClientNotInitializedErr, PluginErr, PluginNotFoundErr, UnknownEventErr};
use crate::error::Result;
use crate::events::{
    BootRequest, CallHttpRequestActionRequest, CallTemplateFunctionArgs,
    CallTemplateFunctionRequest, CallTemplateFunctionResponse, FilterRequest, FilterResponse,
    GetHttpRequestActionsRequest, GetHttpRequestActionsResponse, GetTemplateFunctionsResponse,
    ImportRequest, ImportResponse, InternalEvent, InternalEventPayload, RenderPurpose,
};
use crate::nodejs::start_nodejs_plugin_runtime;
use crate::plugin_handle::PluginHandle;
use crate::server::plugin_runtime::plugin_runtime_server::PluginRuntimeServer;
use crate::server::PluginRuntimeServerImpl;
use log::{info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager, Runtime};
use tokio::fs::read_dir;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tonic::codegen::tokio_stream;
use tonic::transport::Server;
use yaak_models::queries::{generate_id, list_plugins};

#[derive(Clone)]
pub struct PluginManager {
    subscribers: Arc<Mutex<HashMap<String, mpsc::Sender<InternalEvent>>>>,
    plugins: Arc<Mutex<Vec<PluginHandle>>>,
    kill_tx: tokio::sync::watch::Sender<bool>,
    server: Arc<PluginRuntimeServerImpl>,
}

impl PluginManager {
    pub fn new<R: Runtime>(app_handle: AppHandle<R>) -> PluginManager {
        let (events_tx, mut events_rx) = mpsc::channel(128);
        let (kill_server_tx, kill_server_rx) = tokio::sync::watch::channel(false);

        let (client_disconnect_tx, mut client_disconnect_rx) = mpsc::channel(128);
        let (client_connect_tx, mut client_connect_rx) = tokio::sync::watch::channel(false);
        let server =
            PluginRuntimeServerImpl::new(events_tx, client_disconnect_tx, client_connect_tx);

        let plugin_manager = PluginManager {
            plugins: Arc::new(Mutex::new(Vec::new())),
            subscribers: Arc::new(Mutex::new(HashMap::new())),
            server: Arc::new(server.clone()),
            kill_tx: kill_server_tx,
        };

        // Forward events to subscribers
        let subscribers = plugin_manager.subscribers.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                for (tx_id, tx) in subscribers.lock().await.iter_mut() {
                    if let Err(e) = tx.try_send(event.clone()) {
                        warn!("Failed to send event to subscriber {tx_id} {e:?}");
                    }
                }
            }
        });

        // Handle when client plugin runtime disconnects
        tauri::async_runtime::spawn(async move {
            while let Some(_) = client_disconnect_rx.recv().await {
                info!("Plugin runtime client disconnected! TODO: Handle this case");
            }
        });

        info!("Starting plugin server");

        let svc = PluginRuntimeServer::new(server.to_owned());
        let listen_addr = match option_env!("PORT") {
            None => "localhost:0".to_string(),
            Some(port) => format!("localhost:{port}"),
        };
        let listener = tauri::async_runtime::block_on(async move {
            TcpListener::bind(listen_addr)
                .await
                .expect("Failed to bind TCP listener")
        });
        let addr = listener.local_addr().expect("Failed to get local address");

        // 1. Reload all plugins when the Node.js runtime connects
        {
            let plugin_manager = plugin_manager.clone();
            let app_handle = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                match client_connect_rx.changed().await {
                    Ok(_) => {
                        info!("Plugin runtime client connected!");
                        plugin_manager
                            .initialize_all_plugins(&app_handle)
                            .await
                            .expect("Failed to reload plugins");
                    }
                    Err(e) => {
                        warn!("Failed to receive from client connection rx {e:?}");
                    }
                }
            });
        };

        // 1. Spawn server in the background
        info!("Starting gRPC plugin server on {addr}");
        tauri::async_runtime::spawn(async move {
            Server::builder()
                .timeout(Duration::from_secs(10))
                .add_service(svc)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .expect("grpc plugin runtime server failed to start");
        });

        // 2. Start Node.js runtime and initialize plugins
        tauri::async_runtime::block_on(async move {
            start_nodejs_plugin_runtime(&app_handle, addr, &kill_server_rx)
                .await
                .unwrap();
        });

        plugin_manager
    }

    pub async fn list_plugin_dirs<R: Runtime>(&self, app_handle: &AppHandle<R>) -> Vec<String> {
        let plugins_dir = app_handle
            .path()
            .resolve("plugins", BaseDirectory::Resource)
            .expect("failed to resolve plugin directory resource");

        let bundled_plugin_dirs = read_plugins_dir(&plugins_dir)
            .await
            .expect(format!("Failed to read plugins dir: {:?}", plugins_dir).as_str());

        let plugins = list_plugins(app_handle).await.unwrap_or_default();
        let installed_plugin_dirs = plugins
            .iter()
            .map(|p| p.directory.to_owned())
            .collect::<Vec<String>>();

        let plugin_dirs = [bundled_plugin_dirs, installed_plugin_dirs].concat();
        plugin_dirs
    }

    pub async fn uninstall(&self, dir: &str) -> Result<()> {
        let plugin = self
            .get_plugin_by_dir(dir)
            .await
            .ok_or(PluginNotFoundErr(dir.to_string()))?;
        self.remove_plugin(&plugin).await
    }

    async fn remove_plugin(&self, plugin: &PluginHandle) -> Result<()> {
        let mut plugins = self.plugins.lock().await;

        // Terminate the plugin
        plugin.terminate().await?;

        // Remove the plugin from the list
        let pos = plugins.iter().position(|p| p.ref_id == plugin.ref_id);
        if let Some(pos) = pos {
            plugins.remove(pos);
        }

        Ok(())
    }

    pub async fn add_plugin_by_dir(&self, dir: &str) -> Result<()> {
        info!("Adding plugin by dir {dir}");
        let maybe_tx = self.server.app_to_plugin_events_tx.lock().await;
        let tx = match &*maybe_tx {
            None => return Err(ClientNotInitializedErr),
            Some(tx) => tx,
        };
        let ph = PluginHandle::new(dir, tx.clone());
        self.plugins.lock().await.push(ph.clone());
        let plugin = self
            .get_plugin_by_dir(dir)
            .await
            .ok_or(PluginNotFoundErr(dir.to_string()))?;

        // Boot the plugin
        let event = self
            .send_to_plugin_and_wait(
                &plugin,
                &InternalEventPayload::BootRequest(BootRequest {
                    dir: dir.to_string(),
                }),
            )
            .await?;

        let resp = match event.payload {
            InternalEventPayload::BootResponse(resp) => resp,
            _ => return Err(UnknownEventErr),
        };

        plugin.set_boot_response(&resp).await;

        Ok(())
    }

    pub async fn initialize_all_plugins<R: Runtime>(
        &self,
        app_handle: &AppHandle<R>,
    ) -> Result<()> {
        for dir in self.list_plugin_dirs(app_handle).await {
            // First remove the plugin if it exists
            if let Some(plugin) = self.get_plugin_by_dir(dir.as_str()).await {
                if let Err(e) = self.remove_plugin(&plugin).await {
                    warn!("Failed to remove plugin {dir} {e:?}");
                }
            }
            if let Err(e) = self.add_plugin_by_dir(dir.as_str()).await {
                warn!("Failed to add plugin {dir} {e:?}");
            }
        }

        Ok(())
    }

    pub async fn subscribe(&self) -> (String, mpsc::Receiver<InternalEvent>) {
        let (tx, rx) = mpsc::channel(128);
        let rx_id = generate_id();
        self.subscribers.lock().await.insert(rx_id.clone(), tx);
        (rx_id, rx)
    }

    pub async fn unsubscribe(&self, rx_id: &str) {
        self.subscribers.lock().await.remove(rx_id);
    }

    pub async fn terminate(&self) {
        self.kill_tx.send_replace(true);

        // Give it a bit of time to kill
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    pub async fn reply(
        &self,
        source_event: &InternalEvent,
        payload: &InternalEventPayload,
    ) -> Result<()> {
        let reply_id = Some(source_event.clone().id);
        let plugin = self
            .get_plugin_by_ref_id(source_event.plugin_ref_id.as_str())
            .await
            .ok_or(PluginNotFoundErr(source_event.plugin_ref_id.to_string()))?;
        let event = plugin.build_event_to_send(&payload, reply_id);
        plugin.send(&event).await
    }

    pub async fn get_plugin_by_ref_id(&self, ref_id: &str) -> Option<PluginHandle> {
        self.plugins
            .lock()
            .await
            .iter()
            .find(|p| p.ref_id == ref_id)
            .cloned()
    }

    pub async fn get_plugin_by_dir(&self, dir: &str) -> Option<PluginHandle> {
        self.plugins
            .lock()
            .await
            .iter()
            .find(|p| p.dir == dir)
            .cloned()
    }

    pub async fn get_plugin_by_name(&self, name: &str) -> Option<PluginHandle> {
        for plugin in self.plugins.lock().await.iter().cloned() {
            let info = plugin.info().await?;
            if info.name == name {
                return Some(plugin);
            }
        }
        None
    }

    async fn send_to_plugin_and_wait(
        &self,
        plugin: &PluginHandle,
        payload: &InternalEventPayload,
    ) -> Result<InternalEvent> {
        let events = self
            .send_to_plugins_and_wait(payload, vec![plugin.to_owned()])
            .await?;
        Ok(events.first().unwrap().to_owned())
    }

    async fn send_and_wait(&self, payload: &InternalEventPayload) -> Result<Vec<InternalEvent>> {
        self.send_to_plugins_and_wait(payload, self.plugins.lock().await.clone())
            .await
    }

    async fn send_to_plugins_and_wait(
        &self,
        payload: &InternalEventPayload,
        plugins: Vec<PluginHandle>,
    ) -> Result<Vec<InternalEvent>> {
        let (rx_id, mut rx) = self.subscribe().await;

        // 1. Build the events with IDs and everything
        let events_to_send = plugins
            .iter()
            .map(|p| p.build_event_to_send(payload, None))
            .collect::<Vec<InternalEvent>>();

        // 2. Spawn thread to subscribe to incoming events and check reply ids
        let send_events_fut = {
            let events_to_send = events_to_send.clone();

            tokio::spawn(async move {
                let mut found_events = Vec::new();

                while let Some(event) = rx.recv().await {
                    if events_to_send
                        .iter()
                        .find(|e| Some(e.id.to_owned()) == event.reply_id)
                        .is_some()
                    {
                        found_events.push(event.clone());
                    };
                    if found_events.len() == events_to_send.len() {
                        break;
                    }
                }

                found_events
            })
        };

        // 3. Send the events
        for event in events_to_send {
            let plugin = plugins
                .iter()
                .find(|p| p.ref_id == event.plugin_ref_id)
                .expect("Didn't find plugin in list");
            plugin.send(&event).await?
        }

        // 4. Join on the spawned thread
        let events = send_events_fut.await.expect("Thread didn't succeed");

        // 5. Unsubscribe
        self.unsubscribe(rx_id.as_str()).await;

        Ok(events)
    }

    pub async fn get_http_request_actions(&self) -> Result<Vec<GetHttpRequestActionsResponse>> {
        let reply_events = self
            .send_and_wait(&InternalEventPayload::GetHttpRequestActionsRequest(
                GetHttpRequestActionsRequest {},
            ))
            .await?;

        let mut all_actions = Vec::new();
        for event in reply_events {
            if let InternalEventPayload::GetHttpRequestActionsResponse(resp) = event.payload {
                all_actions.push(resp.clone());
            }
        }

        Ok(all_actions)
    }

    pub async fn get_template_functions(&self) -> Result<Vec<GetTemplateFunctionsResponse>> {
        let reply_events = self
            .send_and_wait(&InternalEventPayload::GetTemplateFunctionsRequest)
            .await?;

        let mut all_actions = Vec::new();
        for event in reply_events {
            if let InternalEventPayload::GetTemplateFunctionsResponse(resp) = event.payload {
                all_actions.push(resp.clone());
            }
        }

        Ok(all_actions)
    }

    pub async fn call_http_request_action(&self, req: CallHttpRequestActionRequest) -> Result<()> {
        let ref_id = req.plugin_ref_id.clone();
        let plugin = self
            .get_plugin_by_ref_id(ref_id.as_str())
            .await
            .ok_or(PluginNotFoundErr(ref_id))?;
        let event = plugin.build_event_to_send(
            &InternalEventPayload::CallHttpRequestActionRequest(req),
            None,
        );
        plugin.send(&event).await?;
        Ok(())
    }

    pub async fn call_template_function(
        &self,
        fn_name: &str,
        args: HashMap<String, String>,
        purpose: RenderPurpose,
    ) -> Result<Option<String>> {
        let req = CallTemplateFunctionRequest {
            name: fn_name.to_string(),
            args: CallTemplateFunctionArgs {
                purpose,
                values: args,
            },
        };

        let events = self
            .send_and_wait(&InternalEventPayload::CallTemplateFunctionRequest(req))
            .await?;

        let value = events.into_iter().find_map(|e| match e.payload {
            InternalEventPayload::CallTemplateFunctionResponse(CallTemplateFunctionResponse {
                value,
            }) => value,
            _ => None,
        });

        Ok(value)
    }

    pub async fn import_data(&self, content: &str) -> Result<(ImportResponse, String)> {
        let reply_events = self
            .send_and_wait(&InternalEventPayload::ImportRequest(ImportRequest {
                content: content.to_string(),
            }))
            .await?;

        // TODO: Don't just return the first valid response
        let result = reply_events.into_iter().find_map(|e| match e.payload {
            InternalEventPayload::ImportResponse(resp) => Some((resp, e.plugin_ref_id)),
            _ => None,
        });

        match result {
            None => Err(PluginErr(
                "No importers found for file contents".to_string(),
            )),
            Some((resp, ref_id)) => {
                let plugin = self
                    .get_plugin_by_ref_id(ref_id.as_str())
                    .await
                    .ok_or(PluginNotFoundErr(ref_id))?;
                let info = plugin.info().await.unwrap();
                Ok((resp, info.name))
            }
        }
    }

    pub async fn filter_data(
        &self,
        filter: &str,
        content: &str,
        content_type: &str,
    ) -> Result<FilterResponse> {
        let plugin_name = if content_type.to_lowercase().contains("json") {
            "filter-jsonpath"
        } else {
            "filter-xpath"
        };

        let plugin = self
            .get_plugin_by_dir(plugin_name)
            .await
            .ok_or(PluginNotFoundErr(plugin_name.to_string()))?;

        let event = self
            .send_to_plugin_and_wait(
                &plugin,
                &InternalEventPayload::FilterRequest(FilterRequest {
                    filter: filter.to_string(),
                    content: content.to_string(),
                }),
            )
            .await?;

        match event.payload {
            InternalEventPayload::FilterResponse(resp) => Ok(resp),
            InternalEventPayload::EmptyResponse => {
                Err(PluginErr("Filter returned empty".to_string()))
            }
            e => Err(PluginErr(format!("Export returned invalid event {:?}", e))),
        }
    }
}

async fn read_plugins_dir(dir: &PathBuf) -> Result<Vec<String>> {
    let mut result = read_dir(dir).await?;
    let mut dirs: Vec<String> = vec![];
    while let Ok(Some(entry)) = result.next_entry().await {
        if entry.path().is_dir() {
            #[cfg(target_os = "windows")]
            dirs.push(fix_windows_paths(&entry.path()));
            #[cfg(not(target_os = "windows"))]
            dirs.push(entry.path().to_string_lossy().to_string());
        }
    }
    Ok(dirs)
}

#[cfg(target_os = "windows")]
fn fix_windows_paths(p: &PathBuf) -> String {
    use dunce;
    use path_slash::PathBufExt;
    use regex::Regex;

    // 1. Remove UNC prefix for Windows paths to pass to sidecar
    let safe_path = dunce::simplified(p.as_path()).to_string_lossy().to_string();

    // 2. Remove the drive letter
    let safe_path = Regex::new("^[a-zA-Z]:")
        .unwrap()
        .replace(safe_path.as_str(), "");

    // 3. Convert backslashes to forward
    let safe_path = PathBuf::from(safe_path.to_string())
        .to_slash_lossy()
        .to_string();

    safe_path
}
