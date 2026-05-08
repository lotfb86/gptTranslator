use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use futures_util::{SinkExt, StreamExt};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use tauri::{AppHandle, Emitter, State};
use tokio::io::AsyncReadExt;
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
    StartOutgoing {
        target: String,
        output_device: Option<String>,
        response: oneshot::Sender<Result<(), String>>,
    },
    StopOutgoing {
        response: oneshot::Sender<()>,
    },
    StartIncoming {
        output_device: Option<String>,
        response: oneshot::Sender<Result<(), String>>,
    },
    StopIncoming {
        response: oneshot::Sender<()>,
    },
}

/// Held in Tauri State<>. Only carries the channel sender; everything Send-unsafe
/// (cpal::Stream, WebSocket sink/stream) lives on the coordinator thread.
pub struct AppState {
    pub audio_tx: mpsc::Sender<AudioCommand>,
    pub outgoing_running: Mutex<bool>,
    pub incoming_running: Mutex<bool>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatusUpdate {
    pub direction: String, // "outgoing" | "incoming"
    pub running: bool,
    pub target: Option<String>,
    pub message: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptUpdate {
    pub direction: String, // "outgoing" | "incoming"
    pub kind: String,      // "source" | "translated"
    pub delta: String,
    pub done: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
}

// ---------- Tauri commands ----------

#[tauri::command]
async fn start_outgoing(
    target: String,
    output_device: Option<String>,
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
        .send(AudioCommand::StartOutgoing {
            target,
            output_device,
            response: response_tx,
        })
        .await
        .map_err(|_| "Audio coordinator unavailable".to_string())?;

    response_rx
        .await
        .map_err(|_| "Audio coordinator dropped response".to_string())?
}

#[tauri::command]
async fn stop_outgoing(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let (response_tx, response_rx) = oneshot::channel();
    state
        .audio_tx
        .send(AudioCommand::StopOutgoing {
            response: response_tx,
        })
        .await
        .map_err(|_| "Audio coordinator unavailable".to_string())?;
    let _ = response_rx.await;
    Ok(())
}

#[tauri::command]
async fn start_incoming(
    output_device: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let (response_tx, response_rx) = oneshot::channel();
    state
        .audio_tx
        .send(AudioCommand::StartIncoming {
            output_device,
            response: response_tx,
        })
        .await
        .map_err(|_| "Audio coordinator unavailable".to_string())?;
    response_rx
        .await
        .map_err(|_| "Audio coordinator dropped response".to_string())?
}

#[tauri::command]
async fn stop_incoming(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let (response_tx, response_rx) = oneshot::channel();
    state
        .audio_tx
        .send(AudioCommand::StopIncoming {
            response: response_tx,
        })
        .await
        .map_err(|_| "Audio coordinator unavailable".to_string())?;
    let _ = response_rx.await;
    Ok(())
}

#[tauri::command]
fn get_output_devices() -> Result<Vec<DeviceInfo>, String> {
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.name().ok());

    let devices = host
        .output_devices()
        .map_err(|e| e.to_string())?
        .filter_map(|d| {
            let name = d.name().ok()?;
            let is_default = default_name.as_ref() == Some(&name);
            Some(DeviceInfo { name, is_default })
        })
        .collect();
    Ok(devices)
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatusSnapshot {
    pub outgoing: bool,
    pub incoming: bool,
}

#[tauri::command]
async fn get_status(state: State<'_, Arc<AppState>>) -> Result<StatusSnapshot, String> {
    Ok(StatusSnapshot {
        outgoing: *state.outgoing_running.lock().await,
        incoming: *state.incoming_running.lock().await,
    })
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
        outgoing_running: Mutex::new(false),
        incoming_running: Mutex::new(false),
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
            start_outgoing,
            stop_outgoing,
            start_incoming,
            stop_incoming,
            get_status,
            get_output_devices
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
        let mut outgoing: Option<RunningSession> = None;
        let mut incoming: Option<RunningSession> = None;

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                AudioCommand::StartOutgoing { target, output_device, response } => {
                    if outgoing.is_some() {
                        let _ = response.send(Err("Outgoing session already running".into()));
                        continue;
                    }
                    match start_outgoing_session(target.clone(), output_device, app.clone()).await {
                        Ok(sess) => {
                            outgoing = Some(sess);
                            *state.outgoing_running.lock().await = true;
                            let _ = app.emit("session-status", StatusUpdate {
                                direction: "outgoing".into(),
                                running: true,
                                target: Some(target),
                                message: None,
                            });
                            let _ = response.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = app.emit("session-status", StatusUpdate {
                                direction: "outgoing".into(),
                                running: false,
                                target: None,
                                message: Some(format!("Failed to start outgoing: {e}")),
                            });
                            let _ = response.send(Err(e.to_string()));
                        }
                    }
                }
                AudioCommand::StopOutgoing { response } => {
                    if let Some(sess) = outgoing.take() {
                        sess.stop().await;
                    }
                    *state.outgoing_running.lock().await = false;
                    let _ = app.emit("session-status", StatusUpdate {
                        direction: "outgoing".into(),
                        running: false,
                        target: None,
                        message: None,
                    });
                    let _ = response.send(());
                }
                AudioCommand::StartIncoming { output_device, response } => {
                    if incoming.is_some() {
                        let _ = response.send(Err("Incoming session already running".into()));
                        continue;
                    }
                    match start_incoming_session(output_device, app.clone()).await {
                        Ok(sess) => {
                            incoming = Some(sess);
                            *state.incoming_running.lock().await = true;
                            let _ = app.emit("session-status", StatusUpdate {
                                direction: "incoming".into(),
                                running: true,
                                target: Some("en".into()),
                                message: None,
                            });
                            let _ = response.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = app.emit("session-status", StatusUpdate {
                                direction: "incoming".into(),
                                running: false,
                                target: None,
                                message: Some(format!("Failed to start incoming: {e}")),
                            });
                            let _ = response.send(Err(e.to_string()));
                        }
                    }
                }
                AudioCommand::StopIncoming { response } => {
                    if let Some(sess) = incoming.take() {
                        sess.stop().await;
                    }
                    *state.incoming_running.lock().await = false;
                    let _ = app.emit("session-status", StatusUpdate {
                        direction: "incoming".into(),
                        running: false,
                        target: None,
                        message: None,
                    });
                    let _ = response.send(());
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// Owns the cpal streams + spawned WebSocket task for one running direction.
struct RunningSession {
    _input_stream: Option<Stream>,  // Some for outgoing (mic), None for incoming
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
    }
}

async fn start_outgoing_session(
    target: String,
    output_device: Option<String>,
    app: AppHandle,
) -> Result<RunningSession> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .context("Missing OPENAI_API_KEY (set in .env at repo root)")?;

    let (mic_tx, mic_rx) = mpsc::channel::<Vec<i16>>(64);
    let rb: HeapRb<i16> = HeapRb::new((TARGET_SAMPLE_RATE as usize) * 5);
    let (play_prod, play_cons) = rb.split();

    let input_stream = setup_input_stream(mic_tx)?;
    let output_stream = setup_output_stream(play_cons, output_device)?;

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let target_for_task = target.clone();
    let app_for_task = app.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = run_session(api_key, target_for_task, mic_rx, play_prod, cancel_rx, app_for_task.clone()).await {
            let _ = app_for_task.emit("session-status", StatusUpdate {
                direction: "outgoing".into(),
                running: false,
                target: None,
                message: Some(format!("Session error: {e}")),
            });
        }
    });

    Ok(RunningSession {
        _input_stream: Some(input_stream),
        _output_stream: output_stream,
        cancel_tx: Some(cancel_tx),
        handle: Some(handle),
    })
}

// ---------- Incoming side: audiotee subprocess captures Chrome audio ----------

/// Locates the audiotee sidecar binary in dev or bundled mode.
fn audiotee_path() -> Result<PathBuf> {
    let triple = "audiotee-aarch64-apple-darwin";
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let candidates = vec![
        cwd.join("binaries").join(triple),
        cwd.join("polyglot-app/src-tauri/binaries").join(triple),
        cwd.parent().map(|p| p.join("src-tauri/binaries").join(triple)).unwrap_or_default(),
    ];
    for path in &candidates {
        if path.exists() {
            return Ok(path.clone());
        }
    }
    bail!("audiotee binary not found. Expected one of: {candidates:?}")
}

/// Enumerates audio-service helper PIDs across all Chromium-based browsers
/// (Chrome, Comet, Edge, Brave, Arc, Vivaldi, Opera, etc). They all run a
/// dedicated utility process launched with
/// `--utility-sub-type=audio.mojom.AudioService` for their audio output.
fn find_browser_audio_pids() -> Result<Vec<u32>> {
    let out = std::process::Command::new("ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
        .context("Failed to run `ps`")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut pids = Vec::new();
    for line in stdout.lines() {
        if line.contains("--utility-sub-type=audio.mojom.AudioService") {
            if let Some(pid_str) = line.trim().split_whitespace().next() {
                if let Ok(pid) = pid_str.parse::<u32>() {
                    pids.push(pid);
                }
            }
        }
    }
    if pids.is_empty() {
        bail!("No browser audio process found. Is Chrome, Comet, or another Chromium-based browser running with audio playing?");
    }
    Ok(pids)
}

async fn start_incoming_session(
    output_device: Option<String>,
    app: AppHandle,
) -> Result<RunningSession> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .context("Missing OPENAI_API_KEY (set in .env at repo root)")?;
    // Best-effort: still try to log which browser audio helpers we'd have used,
    // but don't fail the session if none found — we tap-all anyway.
    let pids = find_browser_audio_pids().unwrap_or_default();
    tracing::info!("[incoming] (info only) browser audio helper PIDs: {pids:?}");
    let audiotee = audiotee_path()?;

    let rb: HeapRb<i16> = HeapRb::new((TARGET_SAMPLE_RATE as usize) * 5);
    let (play_prod, play_cons) = rb.split();
    let output_stream = setup_output_stream(play_cons, output_device)?;

    let (cancel_tx, cancel_rx) = oneshot::channel();
    let app_for_task = app.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = run_incoming_session(api_key, audiotee, pids, play_prod, cancel_rx, app_for_task.clone()).await {
            let _ = app_for_task.emit("session-status", StatusUpdate {
                direction: "incoming".into(),
                running: false,
                target: None,
                message: Some(format!("Incoming session error: {e}")),
            });
        }
    });

    Ok(RunningSession {
        _input_stream: None,
        _output_stream: output_stream,
        cancel_tx: Some(cancel_tx),
        handle: Some(handle),
    })
}

async fn run_incoming_session(
    api_key: String,
    audiotee_path: PathBuf,
    _pids: Vec<u32>,
    mut play_prod: HeapProd<i16>,
    mut cancel_rx: oneshot::Receiver<()>,
    app: AppHandle,
) -> Result<()> {
    // Tap ALL processes (not just specific browser PIDs). This catches audio
    // from any source — Comet, Chrome, Spotify, Slack, Apple Music, etc. —
    // without depending on the specific audio architecture each app uses.
    // Exclude our own PID so the translation playback we generate doesn't
    // feed back into the capture loop.
    let mut cmd = tokio::process::Command::new(&audiotee_path);
    let own_pid = std::process::id();
    cmd.args(["--exclude-processes", &own_pid.to_string()]);
    cmd.args(["--sample-rate", "24000", "--mute"]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().context("Failed to spawn audiotee")?;
    let mut stdout = child.stdout.take().context("audiotee stdout missing")?;

    // Drain audiotee's stderr (JSON lifecycle messages) into tracing at info
    // level so we can diagnose tap setup issues without changing the env filter.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!("[audiotee] {line}");
            }
        });
    }

    tracing::info!("[incoming] spawning audiotee (tap-all, exclude self pid {own_pid})");

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
                "output": { "language": "en" }
            }
        }
    });
    ws_write.send(Message::Text(session_update.to_string())).await?;

    let started = Instant::now();
    let mut first_audio_in: Option<Instant> = None;
    let mut first_audio_out: Option<Instant> = None;
    let mut audio_buf = vec![0u8; 4096];
    let mut bytes_in: u64 = 0;
    let mut bytes_out: u64 = 0;
    let mut peak_amp_in: u32 = 0;
    let mut peak_amp_out: u32 = 0;
    let mut last_summary = Instant::now();

    loop {
        tokio::select! {
            biased;

            _ = &mut cancel_rx => {
                let _ = ws_write.send(Message::Close(None)).await;
                break;
            }

            res = stdout.read(&mut audio_buf) => {
                match res {
                    Ok(0) => {
                        tracing::warn!("audiotee stdout EOF");
                        break;
                    }
                    Ok(n) => {
                        if first_audio_in.is_none() { first_audio_in = Some(Instant::now()); }
                        bytes_in += n as u64;
                        let pcm = &audio_buf[..n];
                        // Sample peak amplitude of input audio (PCM16 LE) so we can
                        // see whether audiotee is capturing real audio or zeros.
                        for c in pcm.chunks_exact(2) {
                            let s = i16::from_le_bytes([c[0], c[1]]);
                            peak_amp_in = peak_amp_in.max(s.unsigned_abs() as u32);
                        }
                        let b64 = base64::engine::general_purpose::STANDARD.encode(pcm);
                        let msg = json!({
                            "type": "session.input_audio_buffer.append",
                            "audio": b64,
                        });
                        if ws_write.send(Message::Text(msg.to_string())).await.is_err() {
                            break;
                        }
                        if last_summary.elapsed().as_secs() >= 5 {
                            tracing::info!("[incoming/{:.1}s] audio in={}KB peak={} | out={}KB peak={}",
                                started.elapsed().as_secs_f32(),
                                bytes_in / 1024, peak_amp_in,
                                bytes_out / 1024, peak_amp_out);
                            peak_amp_in = 0;
                            peak_amp_out = 0;
                            last_summary = Instant::now();
                        }
                    }
                    Err(e) => {
                        tracing::error!("audiotee read error: {e}");
                        break;
                    }
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
                                tracing::info!("[incoming/{:.1}s] {msg_type} | server target lang={lang}",
                                    started.elapsed().as_secs_f32());
                            }
                            "session.output_audio.delta"
                            | "response.output_audio.delta"
                            | "output_audio.delta" => {
                                if let Some(b64) = v.get("delta").and_then(|d| d.as_str()) {
                                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                                        bytes_out += bytes.len() as u64;
                                        if first_audio_out.is_none() {
                                            first_audio_out = Some(Instant::now());
                                            let lat = first_audio_in.map(|t| (Instant::now() - t).as_millis()).unwrap_or(0);
                                            tracing::info!("[incoming/{:.1}s] First translated audio out ({lat}ms)",
                                                started.elapsed().as_secs_f32());
                                        }
                                        let samples: Vec<i16> = bytes.chunks_exact(2)
                                            .map(|c| i16::from_le_bytes([c[0], c[1]]))
                                            .collect();
                                        let max_abs = samples.iter().map(|s| s.unsigned_abs() as u32).max().unwrap_or(0);
                                        peak_amp_out = peak_amp_out.max(max_abs);
                                        let pushed = play_prod.push_slice(&samples);
                                        if pushed < samples.len() {
                                            tracing::warn!("[incoming] play_prod overflow: dropped {} samples",
                                                samples.len() - pushed);
                                        }
                                    }
                                }
                            }
                            "session.output_transcript.delta"
                            | "response.output_audio_transcript.delta" => {
                                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                                    let _ = app.emit("transcript", TranscriptUpdate {
                                        direction: "incoming".into(),
                                        kind: "translated".into(),
                                        delta: d.to_string(),
                                        done: false,
                                    });
                                }
                            }
                            "session.output_transcript.done"
                            | "response.output_audio_transcript.done" => {
                                let _ = app.emit("transcript", TranscriptUpdate {
                                    direction: "incoming".into(),
                                    kind: "translated".into(),
                                    delta: String::new(),
                                    done: true,
                                });
                            }
                            "session.input_transcript.delta"
                            | "conversation.item.input_audio_transcription.delta" => {
                                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                                    let _ = app.emit("transcript", TranscriptUpdate {
                                        direction: "incoming".into(),
                                        kind: "source".into(),
                                        delta: d.to_string(),
                                        done: false,
                                    });
                                }
                            }
                            "session.input_transcript.completed"
                            | "conversation.item.input_audio_transcription.completed" => {
                                let _ = app.emit("transcript", TranscriptUpdate {
                                    direction: "incoming".into(),
                                    kind: "source".into(),
                                    delta: String::new(),
                                    done: true,
                                });
                            }
                            "error" => {
                                let err_str = v.get("error").map(|e| e.to_string()).unwrap_or_default();
                                tracing::error!("[incoming/{:.1}s] ERROR: {err_str}",
                                    started.elapsed().as_secs_f32());
                            }
                            other => {
                                // Temporary: log all unhandled events at info to find
                                // missing transcript event names from the new model.
                                tracing::info!("[incoming/{:.1}s] unhandled event: {other}",
                                    started.elapsed().as_secs_f32());
                            }
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Err(_) => break,
                    _ => {}
                }
            }
        }
    }

    tracing::info!("[incoming] final stats: in={}KB out={}KB", bytes_in / 1024, bytes_out / 1024);
    let _ = child.kill().await;
    Ok(())
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
    callbacks: u64,
    underruns: u64,
    last_log: Option<Instant>,
}

impl PlaybackState {
    fn new() -> Self {
        Self {
            playing: false,
            pos: 0.0,
            last_idx: -1,
            current_sample: 0,
            callbacks: 0,
            underruns: 0,
            last_log: None,
        }
    }
}

fn setup_output_stream(mut cons: HeapCons<i16>, device_name: Option<String>) -> Result<Stream> {
    let host = cpal::default_host();
    let device = match device_name.as_deref() {
        Some(name) => host
            .output_devices()?
            .find(|d| d.name().ok().as_deref() == Some(name))
            .with_context(|| format!("Output device '{name}' not found"))?,
        None => host
            .default_output_device()
            .context("No default output device")?,
    };
    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let channels = supported.channels();
    let out_sr = supported.sample_rate().0;
    let resolved_name = device.name().unwrap_or_else(|_| "<unknown>".into());
    tracing::info!(
        "[output] device='{resolved_name}' format={sample_format:?} channels={channels} rate={out_sr}Hz"
    );
    let stream_config: StreamConfig = supported.into();

    let err_fn = |err| tracing::error!("[output] stream error: {err}");
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

    let was_playing = state.playing;
    if !state.playing && buffered >= start_threshold {
        state.playing = true;
        state.pos = 0.0;
        state.last_idx = -1;
    } else if state.playing && buffered < pause_threshold {
        state.playing = false;
        state.underruns += 1;
    }
    state.callbacks += 1;
    if was_playing != state.playing {
        tracing::info!(
            "[output] playback {} (buffered={} samples, {}ms)",
            if state.playing { "STARTED" } else { "PAUSED" },
            buffered,
            buffered * 1000 / TARGET_SAMPLE_RATE as usize,
        );
    }
    let now = Instant::now();
    let should_log = state
        .last_log
        .map(|t| now.duration_since(t).as_secs() >= 5)
        .unwrap_or(true);
    if should_log {
        tracing::info!(
            "[output] callbacks={} underruns={} buffered={}ms playing={}",
            state.callbacks,
            state.underruns,
            buffered * 1000 / TARGET_SAMPLE_RATE as usize,
            state.playing,
        );
        state.last_log = Some(now);
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
                                        direction: "outgoing".into(),
                                        kind: "translated".into(),
                                        delta: d.to_string(),
                                        done: false,
                                    });
                                }
                            }
                            "session.output_transcript.done"
                            | "response.output_audio_transcript.done" => {
                                let _ = app.emit("transcript", TranscriptUpdate {
                                    direction: "outgoing".into(),
                                    kind: "translated".into(),
                                    delta: String::new(),
                                    done: true,
                                });
                            }
                            "session.input_transcript.delta"
                            | "conversation.item.input_audio_transcription.delta" => {
                                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                                    let _ = app.emit("transcript", TranscriptUpdate {
                                        direction: "outgoing".into(),
                                        kind: "source".into(),
                                        delta: d.to_string(),
                                        done: false,
                                    });
                                }
                            }
                            "session.input_transcript.completed"
                            | "conversation.item.input_audio_transcription.completed" => {
                                let _ = app.emit("transcript", TranscriptUpdate {
                                    direction: "outgoing".into(),
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
                                    direction: "outgoing".into(),
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
