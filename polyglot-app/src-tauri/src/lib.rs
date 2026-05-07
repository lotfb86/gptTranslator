use anyhow::{anyhow, Context, Result};
use base64::Engine;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use futures_util::{SinkExt, StreamExt};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const TARGET_SAMPLE_RATE: u32 = 24_000;
const VALID_TARGETS: &[&str] = &[
    "es", "pt", "fr", "ja", "ru", "zh", "de", "ko", "hi", "id", "vi", "it", "en",
];

// ---------- Tauri-side state ----------

/// Sent from Tauri commands -> audio coordinator thread.
pub enum AudioCommand {
    Start {
        target: String,
        response: oneshot::Sender<Result<(), String>>,
    },
    Stop {
        response: oneshot::Sender<()>,
    },
}

/// Held in Tauri State<>. Only carries the channel sender; everything Send-unsafe
/// (cpal::Stream, WebSocket sink/stream) lives on the coordinator thread.
pub struct AppState {
    pub audio_tx: mpsc::Sender<AudioCommand>,
    pub running: Mutex<bool>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatusUpdate {
    pub running: bool,
    pub target: Option<String>,
    pub message: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptUpdate {
    pub kind: String, // "source" | "translated"
    pub delta: String,
    pub done: bool,
}

// ---------- Tauri commands ----------

#[tauri::command]
async fn start_session(
    target: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let target = target.to_lowercase();
    if !VALID_TARGETS.contains(&target.as_str()) {
        return Err(format!(
            "Invalid target language '{target}'. Valid: {VALID_TARGETS:?}"
        ));
    }

    let (response_tx, response_rx) = oneshot::channel();
    state
        .audio_tx
        .send(AudioCommand::Start {
            target,
            response: response_tx,
        })
        .await
        .map_err(|_| "Audio coordinator unavailable".to_string())?;

    response_rx
        .await
        .map_err(|_| "Audio coordinator dropped response".to_string())?
}

#[tauri::command]
async fn stop_session(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let (response_tx, response_rx) = oneshot::channel();
    state
        .audio_tx
        .send(AudioCommand::Stop {
            response: response_tx,
        })
        .await
        .map_err(|_| "Audio coordinator unavailable".to_string())?;
    let _ = response_rx.await;
    Ok(())
}

#[tauri::command]
async fn get_status(state: State<'_, Arc<AppState>>) -> Result<bool, String> {
    Ok(*state.running.lock().await)
}

// ---------- App entry ----------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Load .env from common candidate locations so users can run from anywhere.
    for candidate in [".env", "../.env", "../../.env"] {
        if dotenvy::from_path(candidate).is_ok() {
            break;
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,polyglot_app_lib=info")),
        )
        .try_init();

    let (audio_tx, audio_rx) = mpsc::channel::<AudioCommand>(8);

    let app_state = Arc::new(AppState {
        audio_tx,
        running: Mutex::new(false),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(app_state.clone())
        .setup(move |app| {
            let app_handle = app.handle().clone();
            let state_for_thread = app_state.clone();
            std::thread::spawn(move || {
                if let Err(e) = audio_coordinator_thread(audio_rx, app_handle, state_for_thread) {
                    eprintln!("Audio coordinator thread exited with error: {e}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_session,
            stop_session,
            get_status
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ---------- Audio coordinator (lives on its own OS thread) ----------

/// Owns cpal Streams and the WebSocket session so the !Send types never cross
/// thread boundaries. Receives commands from Tauri, broadcasts events back.
fn audio_coordinator_thread(
    mut cmd_rx: mpsc::Receiver<AudioCommand>,
    app: AppHandle,
    state: Arc<AppState>,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        // Holds the active session, if any.
        let mut active: Option<RunningSession> = None;

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                AudioCommand::Start { target, response } => {
                    if active.is_some() {
                        let _ = response.send(Err("Session already running".into()));
                        continue;
                    }
                    match start_running_session(target.clone(), app.clone()).await {
                        Ok(sess) => {
                            active = Some(sess);
                            *state.running.lock().await = true;
                            let _ = app.emit(
                                "session-status",
                                StatusUpdate {
                                    running: true,
                                    target: Some(target),
                                    message: None,
                                },
                            );
                            let _ = response.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = app.emit(
                                "session-status",
                                StatusUpdate {
                                    running: false,
                                    target: None,
                                    message: Some(format!("Failed to start: {e}")),
                                },
                            );
                            let _ = response.send(Err(e.to_string()));
                        }
                    }
                }
                AudioCommand::Stop { response } => {
                    if let Some(sess) = active.take() {
                        sess.stop().await;
                    }
                    *state.running.lock().await = false;
                    let _ = app.emit(
                        "session-status",
                        StatusUpdate {
                            running: false,
                            target: None,
                            message: None,
                        },
                    );
                    let _ = response.send(());
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// Owns the cpal streams + spawned WebSocket task for one running session.
struct RunningSession {
    _input_stream: Stream,
    _output_stream: Stream,
    cancel_tx: Option<oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl RunningSession {
    async fn stop(mut self) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
        // Streams drop here, releasing devices.
    }
}

async fn start_running_session(target: String, app: AppHandle) -> Result<RunningSession> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .context("Missing OPENAI_API_KEY (set in .env at repo root)")?;

    let (mic_tx, mic_rx) = mpsc::channel::<Vec<i16>>(64);
    let rb: HeapRb<i16> = HeapRb::new((TARGET_SAMPLE_RATE as usize) * 5);
    let (play_prod, play_cons) = rb.split();

    let input_stream = setup_input_stream(mic_tx)?;
    let output_stream = setup_output_stream(play_cons)?;

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let target_for_task = target.clone();
    let app_for_task = app.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = run_session(api_key, target_for_task, mic_rx, play_prod, cancel_rx, app_for_task.clone()).await {
            let _ = app_for_task.emit(
                "session-status",
                StatusUpdate {
                    running: false,
                    target: None,
                    message: Some(format!("Session error: {e}")),
                },
            );
        }
    });

    Ok(RunningSession {
        _input_stream: input_stream,
        _output_stream: output_stream,
        cancel_tx: Some(cancel_tx),
        handle: Some(handle),
    })
}

// ---------- cpal audio I/O (lifted from polyglot/) ----------

fn setup_input_stream(tx: mpsc::Sender<Vec<i16>>) -> Result<Stream> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("No default input device")?;
    let supported = device.default_input_config()?;
    let sample_format = supported.sample_format();
    let channels = supported.channels();
    let in_sr = supported.sample_rate().0;
    let stream_config: StreamConfig = supported.into();

    let err_fn = |err| eprintln!("[mic] stream error: {err}");

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let samples = downsample_f32_to_i16(data, channels, in_sr);
                if !samples.is_empty() {
                    let _ = tx.try_send(samples);
                }
            },
            err_fn,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let f32_data: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                let samples = downsample_f32_to_i16(&f32_data, channels, in_sr);
                if !samples.is_empty() {
                    let _ = tx.try_send(samples);
                }
            },
            err_fn,
            None,
        )?,
        SampleFormat::I32 => device.build_input_stream(
            &stream_config,
            move |data: &[i32], _: &cpal::InputCallbackInfo| {
                let f32_data: Vec<f32> = data.iter().map(|&s| (s >> 16) as f32 / 32768.0).collect();
                let samples = downsample_f32_to_i16(&f32_data, channels, in_sr);
                if !samples.is_empty() {
                    let _ = tx.try_send(samples);
                }
            },
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("Unsupported input sample format: {other:?}")),
    };

    stream.play()?;
    Ok(stream)
}

struct PlaybackState {
    playing: bool,
    pos: f64,
    last_idx: i64,
    current_sample: i16,
}

impl PlaybackState {
    fn new() -> Self {
        Self {
            playing: false,
            pos: 0.0,
            last_idx: -1,
            current_sample: 0,
        }
    }
}

fn setup_output_stream(mut cons: HeapCons<i16>) -> Result<Stream> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("No default output device")?;
    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let channels = supported.channels();
    let out_sr = supported.sample_rate().0;
    let stream_config: StreamConfig = supported.into();

    let err_fn = |err| eprintln!("[spk] stream error: {err}");
    let mut state = PlaybackState::new();

    let stream = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            &stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                fill_output_f32(data, channels, &mut cons, out_sr, &mut state);
            },
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("Unsupported output sample format: {other:?}")),
    };

    stream.play()?;
    Ok(stream)
}

fn downsample_f32_to_i16(data: &[f32], channels: u16, in_sr: u32) -> Vec<i16> {
    let ch = channels.max(1) as usize;
    let mono: Vec<f32> = if ch == 1 {
        data.to_vec()
    } else {
        data.chunks(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    };
    let ratio = in_sr as f32 / TARGET_SAMPLE_RATE as f32;
    let target_len = (mono.len() as f32 / ratio) as usize;
    let mut out = Vec::with_capacity(target_len);
    for i in 0..target_len {
        let src_idx = (i as f32 * ratio) as usize;
        let s = mono.get(src_idx).copied().unwrap_or(0.0).clamp(-1.0, 1.0);
        out.push((s * 32767.0) as i16);
    }
    out
}

// Jitter-buffered playback. State persists across cpal callbacks.
// Once started, plays continuously until buffer drops below pause_threshold,
// then waits to re-fill to start_threshold before resuming. Avoids click-y
// underruns from chunk-boundary gaps in the model's 200ms output cadence.
fn fill_output_f32(
    data: &mut [f32],
    channels: u16,
    cons: &mut HeapCons<i16>,
    out_sr: u32,
    state: &mut PlaybackState,
) {
    const START_THRESHOLD_MS: usize = 250;
    const PAUSE_THRESHOLD_MS: usize = 60;

    let ch = channels.max(1) as usize;
    let step = TARGET_SAMPLE_RATE as f64 / out_sr as f64;

    let buffered = cons.occupied_len();
    let start_threshold = (TARGET_SAMPLE_RATE as usize) * START_THRESHOLD_MS / 1000;
    let pause_threshold = (TARGET_SAMPLE_RATE as usize) * PAUSE_THRESHOLD_MS / 1000;

    if !state.playing && buffered >= start_threshold {
        state.playing = true;
        state.pos = 0.0;
        state.last_idx = -1;
    } else if state.playing && buffered < pause_threshold {
        state.playing = false;
    }

    if !state.playing {
        for sample in data.iter_mut() {
            *sample = 0.0;
        }
        return;
    }

    for frame in data.chunks_mut(ch) {
        let idx = state.pos.floor() as i64;
        while state.last_idx < idx {
            state.current_sample = cons.try_pop().unwrap_or(0);
            state.last_idx += 1;
        }
        let f = state.current_sample as f32 / 32768.0;
        for ch_sample in frame.iter_mut() {
            *ch_sample = f;
        }
        state.pos += step;
    }
}

// ---------- WebSocket session ----------

async fn run_session(
    api_key: String,
    target: String,
    mut mic_rx: mpsc::Receiver<Vec<i16>>,
    mut play_prod: HeapProd<i16>,
    mut cancel_rx: oneshot::Receiver<()>,
    app: AppHandle,
) -> Result<()> {
    let url = "wss://api.openai.com/v1/realtime/translations?model=gpt-realtime-translate";
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {api_key}").parse().unwrap(),
    );

    let (ws_stream, _) = connect_async(request).await?;
    let (mut ws_write, mut ws_read) = ws_stream.split();

    let session_update = json!({
        "type": "session.update",
        "session": {
            "audio": {
                "input": {
                    "transcription": { "model": "gpt-realtime-whisper" },
                    "noise_reduction": { "type": "near_field" }
                },
                "output": { "language": target }
            }
        }
    });
    ws_write
        .send(Message::Text(session_update.to_string()))
        .await?;

    let started = Instant::now();
    let mut first_audio_in: Option<Instant> = None;
    let mut first_audio_out: Option<Instant> = None;

    loop {
        tokio::select! {
            biased;

            _ = &mut cancel_rx => {
                tracing::info!("Cancellation received, closing WebSocket");
                let _ = ws_write.send(Message::Close(None)).await;
                break;
            }

            Some(samples) = mic_rx.recv() => {
                if first_audio_in.is_none() { first_audio_in = Some(Instant::now()); }
                let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let msg = json!({
                    "type": "session.input_audio_buffer.append",
                    "audio": b64,
                });
                if ws_write.send(Message::Text(msg.to_string())).await.is_err() {
                    break;
                }
            }

            Some(msg) = ws_read.next() => {
                match msg {
                    Ok(Message::Text(text)) => {
                        let v: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match msg_type {
                            "session.created" | "session.updated" => {
                                let lang = v.pointer("/session/audio/output/language").and_then(|l| l.as_str()).unwrap_or("?");
                                tracing::info!("[{:.1}s] {msg_type} | server target lang={lang}",
                                    started.elapsed().as_secs_f32());
                            }
                            "session.output_audio.delta"
                            | "response.output_audio.delta"
                            | "output_audio.delta" => {
                                if let Some(b64) = v.get("delta").and_then(|d| d.as_str()) {
                                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                                        if first_audio_out.is_none() {
                                            first_audio_out = Some(Instant::now());
                                            let lat = first_audio_in
                                                .map(|t| (Instant::now() - t).as_millis())
                                                .unwrap_or(0);
                                            tracing::info!("[{:.1}s] First translated audio out ({lat}ms after first audio in)",
                                                started.elapsed().as_secs_f32());
                                        }
                                        let samples: Vec<i16> = bytes
                                            .chunks_exact(2)
                                            .map(|c| i16::from_le_bytes([c[0], c[1]]))
                                            .collect();
                                        let _ = play_prod.push_slice(&samples);
                                    }
                                }
                            }
                            "session.output_transcript.delta"
                            | "response.output_audio_transcript.delta" => {
                                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                                    let _ = app.emit("transcript", TranscriptUpdate {
                                        kind: "translated".into(),
                                        delta: d.to_string(),
                                        done: false,
                                    });
                                }
                            }
                            "session.output_transcript.done"
                            | "response.output_audio_transcript.done" => {
                                let _ = app.emit("transcript", TranscriptUpdate {
                                    kind: "translated".into(),
                                    delta: String::new(),
                                    done: true,
                                });
                            }
                            "session.input_transcript.delta"
                            | "conversation.item.input_audio_transcription.delta" => {
                                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                                    let _ = app.emit("transcript", TranscriptUpdate {
                                        kind: "source".into(),
                                        delta: d.to_string(),
                                        done: false,
                                    });
                                }
                            }
                            "session.input_transcript.completed"
                            | "conversation.item.input_audio_transcription.completed" => {
                                let _ = app.emit("transcript", TranscriptUpdate {
                                    kind: "source".into(),
                                    delta: String::new(),
                                    done: true,
                                });
                            }
                            "error" => {
                                let err_str = v.get("error").map(|e| e.to_string()).unwrap_or_default();
                                tracing::error!("[{:.1}s] ERROR: {err_str}",
                                    started.elapsed().as_secs_f32());
                                let _ = app.emit("session-status", StatusUpdate {
                                    running: true,
                                    target: Some(target.clone()),
                                    message: Some(format!("Server error: {err_str}")),
                                });
                            }
                            _ => {}
                        }
                    }
                    Ok(Message::Close(_)) => {
                        tracing::info!("WebSocket closed by server");
                        break;
                    }
                    Err(e) => {
                        tracing::error!("WebSocket error: {e}");
                        break;
                    }
                    _ => {}
                }
            }

            else => {
                break;
            }
        }
    }

    Ok(())
}
