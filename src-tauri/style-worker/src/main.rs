//! Isolated local text generation worker. Standard output is reserved for the
//! newline-delimited JSON protocol; native library diagnostics go to stderr.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, LlamaChatMessage, LlamaModel},
    sampling::LlamaSampler,
};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};
use std::num::NonZeroU32;

#[derive(Deserialize)]
struct Request {
    system: String,
    text: String,
}
#[derive(Serialize)]
struct Response {
    text: Option<String>,
    error: Option<String>,
}

fn generate(
    backend: &LlamaBackend,
    model: &LlamaModel,
    request: Request,
) -> Result<String, String> {
    let messages = [
        LlamaChatMessage::new("system".into(), request.system).map_err(|e| e.to_string())?,
        LlamaChatMessage::new("user".into(), request.text).map_err(|e| e.to_string())?,
    ];
    let template = model.chat_template(None).map_err(|e| e.to_string())?;
    let prompt = model
        .apply_chat_template(&template, &messages, true)
        .map_err(|e| e.to_string())?;
    let tokens = model.vocab().tokenize(prompt.as_bytes(), true, true);
    const CONTEXT: usize = 4096;
    if tokens.is_empty() || tokens.len() >= CONTEXT / 2 {
        return Err("Transcript exceeds local model context".into());
    }
    let limit = (tokens.len() * 2 + 128).min(CONTEXT - tokens.len());
    let mut context = model
        .new_context(
            backend,
            LlamaContextParams::default()
                .with_n_ctx(NonZeroU32::new(CONTEXT as u32))
                .with_n_batch(CONTEXT as u32),
        )
        .map_err(|e| e.to_string())?;
    let mut batch = LlamaBatch::new(CONTEXT, 1);
    for (i, token) in tokens.iter().enumerate() {
        batch
            .add(*token, i as i32, &[0], i + 1 == tokens.len())
            .map_err(|e| e.to_string())?;
    }
    context.decode(&mut batch).map_err(|e| e.to_string())?;
    let mut sampler = LlamaSampler::greedy();
    let mut output = Vec::new();
    let mut ended = false;
    for _ in 0..limit {
        let token = sampler.sample(&context, -1);
        if model.vocab().is_eog(token) {
            ended = true;
            break;
        }
        sampler.accept(token);
        output.push(token);
        batch.clear();
        batch
            .add(token, (tokens.len() + output.len() - 1) as i32, &[0], true)
            .map_err(|e| e.to_string())?;
        context.decode(&mut batch).map_err(|e| e.to_string())?;
    }
    if !ended {
        return Err("Local model output was truncated".into());
    }
    let text = String::from_utf8(model.vocab().detokenize(&output, false, false))
        .map_err(|e| e.to_string())?;
    if text.trim().is_empty() {
        return Err("Local model returned no text".into());
    }
    Ok(text)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    llama_cpp_2::send_logs_to_tracing(llama_cpp_2::LogOptions::default().with_logs_enabled(false));
    let path = std::env::args_os().nth(1).ok_or("missing model path")?;
    let backend = LlamaBackend::init()?;
    let model = LlamaModel::load_from_file(&backend, path, &LlamaModelParams::default())?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let result = line
            .map_err(|e| e.to_string())
            .and_then(|line| serde_json::from_str::<Request>(&line).map_err(|e| e.to_string()))
            .and_then(|req| generate(&backend, &model, req));
        let response = match result {
            Ok(text) => Response {
                text: Some(text),
                error: None,
            },
            Err(error) => Response {
                text: None,
                error: Some(error),
            },
        };
        serde_json::to_writer(&mut stdout, &response)?;
        writeln!(stdout)?;
        stdout.flush()?;
    }
    Ok(())
}
