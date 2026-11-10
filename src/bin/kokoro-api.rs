
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::Response,
    routing::get,
    Json, Router,
};
use kokoro_en::{KokoroTts, Voice};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs,
    net::SocketAddr,
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::Semaphore,
};

struct AppState {
    tts: Arc<KokoroTts>,
    voices: Vec<String>,
    synthesis_slot: Arc<Semaphore>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct VoicesResponse {
    voices: Vec<String>,
}

#[derive(Deserialize)]
struct TtsRequest {
    text: String,
    voice: Option<String>,
    speed: Option<f32>,
    format: Option<String>,
}

fn audio_format(
    format: &str,
) -> Option<(&'static str, &'static str, &'static [&'static str])> {
    match format {
        "wav" => Some(("audio/wav", "wav", &[])),
        "mp3" => Some((
            "audio/mpeg",
            "mp3",
            &["-c:a", "libmp3lame", "-q:a", "4", "-f", "mp3", "pipe:1"],
        )),
        "ogg" => Some((
            "audio/ogg",
            "ogg",
            &["-c:a", "libvorbis", "-q:a", "4", "-f", "ogg", "pipe:1"],
        )),
        "flac" => Some((
            "audio/flac",
            "flac",
            &["-c:a", "flac", "-f", "flac", "pipe:1"],
        )),
        "aac" => Some((
            "audio/aac",
            "aac",
            &["-c:a", "aac", "-b:a", "128k", "-f", "adts", "pipe:1"],
        )),
        _ => None,
    }
}

async fn convert_audio(
    wav: Vec<u8>,
    format: &str,
) -> Result<Vec<u8>, String> {
    let (_, _, encoder_args) = audio_format(format)
        .ok_or_else(|| "Unsupported audio format".to_string())?;

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "wav",
            "-i",
            "pipe:0",
            "-vn",
        ])
        .args(encoder_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start FFmpeg: {}", e))?;

    let mut stdin = child.stdin.take()
        .ok_or_else(|| "Could not open FFmpeg input".to_string())?;

    // Write the WAV input concurrently so FFmpeg's output
    // pipes can be drained without blocking the conversion.
    let writer = tokio::spawn(async move {
        stdin.write_all(&wav).await
    });

    let output = child.wait_with_output().await
        .map_err(|e| format!("FFmpeg execution failed: {}", e))?;

    writer.await
        .map_err(|e| format!("FFmpeg input task failed: {}", e))?
        .map_err(|e| format!("Could not send audio to FFmpeg: {}", e))?;

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!("FFmpeg conversion failed: {}", error));
    }

    if output.stdout.is_empty() {
        return Err("FFmpeg produced empty audio output".into());
    }

    Ok(output.stdout)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn voices(
    State(state): State<Arc<AppState>>,
) -> Json<VoicesResponse> {
    Json(VoicesResponse {
        voices: state.voices.clone(),
    })
}

fn create_wav(samples: &[f32]) -> Vec<u8> {
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);

    let sample_rate = 24000u32;
    let channels = 1u16;
    let bits_per_sample = 16u16;
    let data_size = (samples.len() * 2) as u32;

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());

    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());

    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        wav.extend_from_slice(&value.to_le_bytes());
    }

    wav
}

async fn synthesize(
    State(state): State<Arc<AppState>>,
    Json(request): Json<TtsRequest>,
) -> Result<Response, (StatusCode, String)> {
    let text = request.text.trim();

    if text.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Text cannot be empty".into(),
        ));
    }

    if text.chars().count() > 2000 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Text cannot exceed 2000 characters".into(),
        ));
    }

    let voice = request.voice
        .unwrap_or_else(|| "af_heart".to_string());

    if !state.voices.contains(&voice) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Unknown voice: {}", voice),
        ));
    }

    let speed = request.speed.unwrap_or(1.0);

    if !speed.is_finite() || !(0.5..=2.0).contains(&speed) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Speed must be between 0.5 and 2.0".into(),
        ));
    }

    let format = request.format
        .unwrap_or_else(|| "wav".to_string())
        .to_ascii_lowercase();

    let (content_type, extension, _) = audio_format(&format)
        .ok_or((
            StatusCode::BAD_REQUEST,
            "Unsupported format. Use wav, mp3, ogg, flac or aac.".into(),
        ))?;

    let _permit = state.synthesis_slot
        .clone()
        .try_acquire_owned()
        .map_err(|_| (
            StatusCode::TOO_MANY_REQUESTS,
            "Another synthesis request is currently running".into(),
        ))?;

    let (samples, duration) = state.tts
        .synth(text, Voice::new(voice).with_speed(speed))
        .await
        .map_err(|error| (
            StatusCode::UNPROCESSABLE_ENTITY,
            error.to_string(),
        ))?;

    let wav = create_wav(&samples);

    let audio = if format == "wav" {
        wav
    } else {
        convert_audio(wav, &format)
            .await
            .map_err(|error| (
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
            ))?
    };

    let filename = format!("kokoro-speech.{}", extension);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .header(
            header::CONTENT_LENGTH,
            audio.len().to_string(),
        )
        .header(
            "x-synthesis-ms",
            duration.as_millis().to_string(),
        )
        .header("x-audio-format", format)
        .body(axum::body::Body::from(audio))
        .map_err(|error| (
            StatusCode::INTERNAL_SERVER_ERROR,
            error.to_string(),
        ))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("Loading Kokoro model...");

    let tts = KokoroTts::new(
        "models/model_quantized.onnx",
        "voices",
    ).await?;

    println!("Kokoro model loaded!");

    let mut available_voices = Vec::new();

    for entry in fs::read_dir("voices")? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|ext| ext.to_str()) == Some("bin") {
            if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                available_voices.push(name.to_string());
            }
        }
    }

    if available_voices.is_empty() {
        return Err("No voice files found in voices/".into());
    }

    println!("Available voices: {:?}", available_voices);

    let state = Arc::new(AppState {
        tts: Arc::new(tts),
        voices: available_voices,
        synthesis_slot: Arc::new(Semaphore::new(1)),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/voices", get(voices))
        .route("/tts", axum::routing::post(synthesize))
        .with_state(state);

    let address: SocketAddr = "0.0.0.0:8080".parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;

    println!("Kokoro API listening on {}", address);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;

    Ok(())
}
