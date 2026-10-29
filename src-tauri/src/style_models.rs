//! Downloads and runs the local text models used for dictation styles.
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specta::Type;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

#[derive(Clone, Serialize, Type)]
pub struct StyleModel {
    id: &'static str,
    name: &'static str,
    size: u64,
    downloaded: bool,
}
struct ModelSpec {
    id: &'static str,
    name: &'static str,
    filename: &'static str,
    url: &'static str,
    size: u64,
    sha256: &'static str,
}
const MODELS: &[ModelSpec] = &[
    ModelSpec { id: "qwen3", name: "Qwen3 0.6B", filename: "Qwen3-0.6B-Q4_0.gguf", url: "https://huggingface.co/ggml-org/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q4_0.gguf", size: 428970080, sha256: "da2572f16c06133561ce56accaa822216f2391ef4d37fba427801cd6736417d4" },
    ModelSpec { id: "qwen25", name: "Qwen2.5 0.5B Instruct", filename: "qwen2.5-0.5b-instruct-q4_k_m.gguf", url: "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf", size: 491400032, sha256: "74a4da8c9fdbcd15bd1f6d01d621410d31c6fc00986f5eb687824e7b93d7a9db" },
];
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static CANCEL_DOWNLOAD: AtomicBool = AtomicBool::new(false);
struct DownloadGuard;
struct PartialDownload(PathBuf);
impl Drop for PartialDownload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
impl Drop for DownloadGuard {
    fn drop(&mut self) {
        DOWNLOADING.store(false, Ordering::Release);
    }
}
fn model(id: &str) -> Result<&'static ModelSpec, String> {
    MODELS
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| "Unknown style model".to_string())
}
fn directory(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(crate::portable::app_data_dir(app)
        .map_err(|e| e.to_string())?
        .join("style-models"))
}
fn path(app: &AppHandle, spec: &ModelSpec) -> Result<PathBuf, String> {
    Ok(directory(app)?.join(spec.filename))
}
#[tauri::command]
#[specta::specta]
pub fn list_style_models(app: AppHandle) -> Result<Vec<StyleModel>, String> {
    MODELS
        .iter()
        .map(|m| {
            Ok(StyleModel {
                id: m.id,
                name: m.name,
                size: m.size,
                downloaded: path(&app, m)?.is_file(),
            })
        })
        .collect()
}
#[tauri::command]
#[specta::specta]
pub fn cancel_style_model_download() {
    if DOWNLOADING.load(Ordering::Acquire) {
        CANCEL_DOWNLOAD.store(true, Ordering::Release);
    }
}

async fn download_cancelled() {
    while !CANCEL_DOWNLOAD.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
#[tauri::command]
#[specta::specta]
pub async fn download_style_model(app: AppHandle, id: String) -> Result<(), String> {
    let spec = model(&id)?;
    // Avoid concurrent writers to the same partial file.
    DOWNLOADING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "Another style model is downloading")?;
    let _guard = DownloadGuard;
    CANCEL_DOWNLOAD.store(false, Ordering::Release);
    let folder = directory(&app)?;
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let target = path(&app, spec)?;
    if target.is_file() {
        return Ok(());
    }
    let partial = folder.join(format!("{}.partial", spec.filename));
    let _partial_guard = PartialDownload(partial.clone());
    let client = reqwest::Client::new();
    let response = tokio::select! {
        result = client.get(spec.url).send() => result.map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?,
        _ = download_cancelled() => return Err("cancelled".into()),
    };
    let mut stream = response.bytes_stream();
    let mut file = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut received = 0u64;
    let mut last_progress = Instant::now();
    loop {
        let chunk = tokio::select! {
            next = stream.next() => match next { Some(chunk) => chunk.map_err(|e| e.to_string())?, None => break },
            _ = download_cancelled() => return Err("cancelled".into()),
        };
        received += chunk.len() as u64;
        if received > spec.size {
            return Err("Model download exceeded expected size".into());
        }
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        hash.update(&chunk);
        if last_progress.elapsed() >= Duration::from_millis(200) || received == spec.size {
            let _ = app.emit(
                "style-model-progress",
                serde_json::json!({"id": id, "received": received, "total": spec.size}),
            );
            last_progress = Instant::now();
        }
    }
    if CANCEL_DOWNLOAD.load(Ordering::Acquire) {
        return Err("cancelled".into());
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    if received != spec.size || format!("{:x}", hash.finalize()) != spec.sha256 {
        return Err("Model download failed integrity verification".into());
    }
    std::fs::rename(partial, target).map_err(|e| e.to_string())?;
    Ok(())
}
#[tauri::command]
#[specta::specta]
pub fn delete_style_model(app: AppHandle, id: String) -> Result<(), String> {
    let path = path(&app, model(&id)?)?;
    if path.exists() {
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}
#[derive(Deserialize)]
struct WorkerResponse {
    text: Option<String>,
    error: Option<String>,
}
fn worker_path() -> Result<PathBuf, String> {
    let name = if cfg!(windows) {
        "handy-style-worker.exe"
    } else {
        "handy-style-worker"
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let sibling = exe.parent().ok_or("No executable directory")?.join(name);
    if sibling.is_file() {
        return Ok(sibling);
    }
    Err("Handy's local style worker is unavailable".into())
}
pub async fn generate(
    app: &AppHandle,
    id: String,
    system: String,
    text: String,
) -> Result<String, String> {
    let model_path = path(app, model(&id)?)?;
    if !model_path.is_file() {
        return Err("Style model is not downloaded".into());
    }
    let executable = worker_path()?;
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new(executable)
            .arg(model_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut stdin = child.stdin.take().ok_or("Missing worker input")?;
        let stdout = child.stdout.take().ok_or("Missing worker output")?;
        let request = serde_json::json!({"system": system, "text": text});
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> Result<String, String> {
                writeln!(stdin, "{request}").map_err(|e| e.to_string())?;
                drop(stdin);
                let mut line = String::new();
                BufReader::new(stdout)
                    .read_line(&mut line)
                    .map_err(|e| e.to_string())?;
                Ok(line)
            })();
            let _ = tx.send(result);
        });
        let response = rx.recv_timeout(Duration::from_secs(30));
        if !matches!(response, Ok(Ok(_))) {
            let _ = child.kill();
            let _ = child.wait();
            return match response {
                Ok(Err(error)) => Err(error),
                _ => Err("Local style model timed out".into()),
            };
        }
        let line = response.map_err(|e| e.to_string())??;
        // The response is complete; terminate the one-shot worker so a stuck
        // native teardown cannot delay pasting the result.
        let _ = child.kill();
        let _ = child.wait();
        let response: WorkerResponse = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        response
            .text
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                response
                    .error
                    .unwrap_or_else(|| "Empty local model output".into())
            })
    })
    .await
    .map_err(|e| e.to_string())?
}
