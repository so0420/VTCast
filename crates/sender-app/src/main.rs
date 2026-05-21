//! VTCast Tauri desktop GUI. Drives the same `vtcast_sender::Pipeline`
//! the CLI uses; exposes start/stop/list commands and forwards pipeline
//! events as Tauri events the webview listens to.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use tauri::{Emitter, Manager, State};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;
use vtcast_sender::{Config, Pipeline, PipelineEvent, StopHandle};

struct AppState {
    pipeline: Arc<Mutex<Option<RunningPipeline>>>,
}

struct RunningPipeline {
    stop: StopHandle,
    listener_task: tokio::task::JoinHandle<()>,
}

#[tauri::command]
async fn list_senders() -> Result<Vec<String>, String> {
    vtcast_sender::list_spout_senders().map_err(|e| format!("{:#}", e))
}

#[tauri::command]
async fn list_windows() -> Result<Vec<String>, String> {
    vtcast_sender::list_capture_windows().map_err(|e| format!("{:#}", e))
}

#[tauri::command]
async fn list_displays() -> Result<Vec<(usize, String)>, String> {
    vtcast_sender::list_capture_displays().map_err(|e| format!("{:#}", e))
}

#[tauri::command]
async fn start_pipeline(
    config: Config,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut guard = state.pipeline.lock().await;
    // Reap a stale entry left over from a previous pipeline that self-stopped
    // (e.g. unrecoverable relay error). The listener task ends right after
    // emitting Stopped, so a finished task means the pipeline is dead even
    // if the guard still holds a RunningPipeline.
    if let Some(rp) = guard.as_ref() {
        if rp.listener_task.is_finished() {
            *guard = None;
        }
    }
    if guard.is_some() {
        return Err("pipeline already running".into());
    }

    let mut pipeline = Pipeline::start(config)
        .await
        .map_err(|e| format!("{:#}", e))?;
    let stop = pipeline.stop_handle();

    let app_for_listener = app.clone();
    let listener_task = tokio::spawn(async move {
        while let Some(ev) = pipeline.next_event().await {
            let is_stop = matches!(ev, PipelineEvent::Stopped);
            let summary = match &ev {
                PipelineEvent::Started { room, .. } => format!("Started(room={})", room),
                PipelineEvent::PublisherConnected { attempt } => format!("PublisherConnected({})", attempt),
                PipelineEvent::PublisherDisconnected { reason, will_retry } => {
                    format!("PublisherDisconnected(retry={}, {})", will_retry, reason)
                }
                PipelineEvent::Publishing { aus_sent } => format!("Publishing({})", aus_sent),
                PipelineEvent::Error { detail } => format!("Error({})", detail),
                PipelineEvent::Stopped => "Stopped".into(),
            };
            tracing::debug!(event = %summary, "emit pipeline_event");
            if let Err(e) = app_for_listener.emit("pipeline_event", &ev) {
                tracing::warn!(error = ?e, "emit pipeline_event failed");
            }
            if is_stop {
                break;
            }
        }
        tracing::debug!("event listener task ended");
    });

    *guard = Some(RunningPipeline {
        stop,
        listener_task,
    });
    Ok(())
}

#[tauri::command]
async fn stop_pipeline(state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.pipeline.lock().await;
    if let Some(rp) = guard.take() {
        rp.stop.stop();
        // The Pipeline's orchestrator will emit Stopped after draining;
        // dropping the listener task here would cut that off, so we await.
        let _ = rp.listener_task.await;
    }
    Ok(())
}

#[tauri::command]
async fn pipeline_running(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state.pipeline.lock().await.is_some())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,vtcast_sender=debug,vtcast_sender_app=debug")),
        )
        .init();

    tauri::Builder::default()
        .setup(|app| {
            app.manage(AppState {
                pipeline: Arc::new(Mutex::new(None)),
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window while a pipeline is running used to leave
            // ffmpeg + WebRTC orphaned. Trap the close, signal shutdown,
            // wait briefly for the orchestrator to wind down, then let the
            // window close.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                let pipeline = Arc::clone(&state.pipeline);
                let mut guard = match pipeline.try_lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                if let Some(rp) = guard.take() {
                    rp.stop.stop();
                    api.prevent_close();
                    let win = window.clone();
                    tokio::spawn(async move {
                        let _ = rp.listener_task.await;
                        let _ = win.destroy();
                    });
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_senders,
            list_windows,
            list_displays,
            start_pipeline,
            stop_pipeline,
            pipeline_running
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
