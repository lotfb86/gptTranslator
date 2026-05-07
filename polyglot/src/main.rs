use anyhow::{anyhow, Context, Result};
use base64::Engine;
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use futures_util::{SinkExt, StreamExt};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use serde_json::{json, Value};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const TARGET_SAMPLE_RATE: u32 = 24_000;
const VALID_TARGETS: &[&str] = &[
    "es", "pt", "fr", "ja", "ru", "zh", "de", "ko", "hi", "id", "vi", "it", "en",
];

#[derive(Parser)]
#[command(about = "Polyglot Phase 1a — pure Rust mirror of Phase 0 Node CLI")]
struct Args {
    /// ISO-639-1 target language code (e.g. en, es, fr, hi)
    #[arg(default_value = "en")]
    target: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    dotenvy::from_path("../.env").ok();
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,polyglot=info")),
        )
        .init();

    let args = Args::parse();
    let target = args.target.to_lowercase();
    if !VALID_TARGETS.contains(&target.as_str()) {
        return Err(anyhow!(
            "Invalid target language '{target}'. Valid: {:?}",
            VALID_TARGETS
        ));
    }

    let api_key = std::env::var("OPENAI_API_KEY")
        .context("Missing OPENAI_API_KEY (check .env in repo root)")?;

    println!("Polyglot Phase 1a: cpal mic -> gpt-realtime-translate (target={target}) -> cpal speakers");
    println!("Press Ctrl+C to stop.\n");

    let (mic_tx, mic_rx) = mpsc::channel::<Vec<i16>>(64);

    // ~5 seconds of 24kHz mono i16 buffer for playback
    let rb: HeapRb<i16> = HeapRb::new((TARGET_SAMPLE_RATE as usize) * 5);
    let (play_prod, play_cons) = rb.split();

    let _input_stream = setup_input_stream(mic_tx)?;
    let _output_stream = setup_output_stream(play_cons)?;

    run_session(api_key, target, mic_rx, play_prod).await?;

    Ok(())
}

// ---------- Audio I/O via cpal ----------

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

    println!(
        "[mic] device={:?} format={:?} channels={} rate={}Hz",
        device.name().unwrap_or_default(),
        sample_format,
        channels,
        in_sr
    );

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

    println!(
        "[spk] device={:?} format={:?} channels={} rate={}Hz",
        device.name().unwrap_or_default(),
        sample_format,
        channels,
        out_sr
    );

    let err_fn = |err| eprintln!("[spk] stream error: {err}");

    let stream = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            &stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                fill_output_f32(data, channels, &mut cons, out_sr);
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

// Output device sample-rate is usually 48 kHz; OpenAI sends us 24 kHz.
// We hold each 24 kHz sample for (out_sr / 24000) output frames (zero-order hold).
fn fill_output_f32(data: &mut [f32], channels: u16, cons: &mut HeapCons<i16>, out_sr: u32) {
    let ch = channels.max(1) as usize;
    let upsample_factor = (out_sr / TARGET_SAMPLE_RATE).max(1) as usize;

    let mut frame_idx_in_hold = 0usize;
    let mut current = 0i16;

    for frame in data.chunks_mut(ch) {
        if frame_idx_in_hold == 0 {
            current = cons.try_pop().unwrap_or(0);
        }
        frame_idx_in_hold = (frame_idx_in_hold + 1) % upsample_factor;

        let f = current as f32 / 32768.0;
        for ch_sample in frame.iter_mut() {
            *ch_sample = f;
        }
    }
}

// ---------- WebSocket session ----------

async fn run_session(
    api_key: String,
    target: String,
    mut mic_rx: mpsc::Receiver<Vec<i16>>,
    mut play_prod: HeapProd<i16>,
) -> Result<()> {
    let url = "wss://api.openai.com/v1/realtime/translations?model=gpt-realtime-translate";
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {api_key}").parse().unwrap(),
    );

    let (ws_stream, _) = connect_async(request).await?;
    let (mut ws_write, mut ws_read) = ws_stream.split();
    println!("[ws] connected");

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

    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            biased;

            _ = &mut ctrl_c => {
                println!("\n[{:.1}s] Ctrl+C received. Closing.", started.elapsed().as_secs_f32());
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
                    eprintln!("[ws] send failed; closing");
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
                                let model = v.pointer("/session/model").and_then(|m| m.as_str()).unwrap_or("?");
                                println!("[{:.1}s] {msg_type} | server target lang={lang} | model={model}",
                                    started.elapsed().as_secs_f32());
                            }
                            "session.output_audio.delta" | "response.output_audio.delta" | "output_audio.delta" => {
                                if let Some(b64) = v.get("delta").and_then(|d| d.as_str()) {
                                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                                        if first_audio_out.is_none() {
                                            first_audio_out = Some(Instant::now());
                                            let lat = first_audio_in
                                                .map(|t| (Instant::now() - t).as_millis())
                                                .unwrap_or(0);
                                            println!("[{:.1}s] First translated audio out ({lat}ms after first audio in)",
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
                            "session.output_transcript.delta" | "response.output_audio_transcript.delta" => {
                                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                                    use std::io::Write;
                                    print!("{d}");
                                    let _ = std::io::stdout().flush();
                                }
                            }
                            "session.output_transcript.done" | "response.output_audio_transcript.done" => {
                                println!();
                            }
                            "session.input_transcript.delta" | "conversation.item.input_audio_transcription.delta" => {
                                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                                    use std::io::Write;
                                    print!("\x1b[90m{d}\x1b[0m");
                                    let _ = std::io::stdout().flush();
                                }
                            }
                            "session.input_transcript.completed" | "conversation.item.input_audio_transcription.completed" => {
                                if let Some(lang) = v.pointer("/transcript/language").and_then(|l| l.as_str()) {
                                    println!("\n[{:.1}s] Source language detected: {lang}",
                                        started.elapsed().as_secs_f32());
                                }
                            }
                            "error" => {
                                eprintln!("\n[{:.1}s] ERROR: {}",
                                    started.elapsed().as_secs_f32(),
                                    v.get("error").map(|e| e.to_string()).unwrap_or_default());
                            }
                            _ => {
                                if std::env::var("DEBUG").is_ok() {
                                    println!("[ws] {msg_type}");
                                }
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        println!("\n[ws] server closed connection");
                        break;
                    }
                    Err(e) => {
                        eprintln!("\n[ws] error: {e}");
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
