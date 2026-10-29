#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::apple_intelligence;
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::VadPolicy;
use crate::managers::audio::{recording_error_type, AudioRecordingManager};
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::policy::{live_decode, LiveDecode, ModelTraits};
use crate::managers::transcription::StreamWorkKind;
use crate::managers::transcription::{Transcription, TranscriptionManager};
use crate::settings::{get_settings, AppSettings, OverlayStyle, APPLE_INTELLIGENCE_PROVIDER_ID};
use crate::shortcut;
use crate::tray::{set_tray_state, TrayIconState};
use crate::utils::{
    self, show_processing_overlay, show_recording_overlay, show_transcribing_overlay,
};
use crate::TranscriptionCoordinator;
use log::{debug, error, warn};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Manager;
use tauri::{AppHandle, Emitter};
use tauri_specta::Event;

/// How a dictation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionOutcome {
    /// The text was typed.
    Typed,
    /// Nothing was heard, so nothing was typed.
    Empty,
    /// The microphone sent only digital silence (e.g. muted).
    Silent,
    /// Transcribing or pasting failed.
    Failed,
}

/// Emitted when a dictation finishes (not on cancel); onboarding uses it to measure speed.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct TranscriptionCompleteEvent {
    pub outcome: TranscriptionOutcome,
    pub audio_seconds: f64,
    pub wait_seconds: f64,
}

fn emit_transcription_complete(
    app: &AppHandle,
    outcome: TranscriptionOutcome,
    sample_count: usize,
    stop_time: Instant,
) {
    let event = TranscriptionCompleteEvent {
        outcome,
        audio_seconds: sample_count as f64
            / crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE as f64,
        wait_seconds: stop_time.elapsed().as_secs_f64(),
    };
    if let Err(e) = event.emit(app) {
        warn!("Failed to emit transcription complete: {e}");
    }
}

/// Nothing to type: `Silent` if the microphone sent only zeros, else `Empty`.
fn empty_outcome(input_silent: bool) -> TranscriptionOutcome {
    if input_silent {
        TranscriptionOutcome::Silent
    } else {
        TranscriptionOutcome::Empty
    }
}

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bumped per recording session, so a late paste from an earlier session
/// doesn't reset the tray or hide the overlay of a newer one.
static RECORDING_SESSION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, serde::Serialize)]
struct RecordingErrorEvent {
    error_type: String,
    detail: Option<String>,
}

/// Drop guard that finishes the transcription pipeline, including immediate
/// model unloading on early exits.
struct FinishGuard(AppHandle, Arc<TranscriptionManager>);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.1.maybe_unload_immediately("transcription session");
        if let Some(c) = self.0.try_state::<TranscriptionCoordinator>() {
            c.notify_processing_finished();
        }
        // The pipeline just freed its large transient buffers (captured PCM,
        // WAV copy, engine scratch); hand the cached pages back to the OS so
        // they don't sit in malloc arenas until they get swapped out (#1792).
        crate::memory::trim_freed_memory();
    }
}

// Shortcut Action Trait
pub trait ShortcutAction: Send + Sync {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
}

// Transcribe Action
struct TranscribeAction {
    post_process: bool,
}

/// Field name for structured output JSON schema
const TRANSCRIPTION_FIELD: &str = "transcription";

/// Strip invisible Unicode characters that some LLMs may insert
fn strip_invisible_chars(s: &str) -> String {
    s.replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'], "")
}

/// Strip a leading `<think>...</think>` block. Some endpoints can't disable
/// reasoning, and some local servers put the reasoning text into `content`
/// instead of a separate field — without this the user would get the model's
/// chain of thought pasted along with the cleaned transcription.
fn strip_think_block(s: &str) -> &str {
    if let Some(rest) = s.trim_start().strip_prefix("<think>") {
        if let Some(end) = rest.find("</think>") {
            return rest[end + "</think>".len()..].trim_start();
        }
    }
    s
}

/// Build a system prompt from the user's prompt template.
/// Removes `${output}` placeholder since the transcription is sent as the user message.
fn build_system_prompt(prompt_template: &str) -> String {
    prompt_template.replace("${output}", "").trim().to_string()
}

/// Returns `true` when a transcription has no meaningful content to
/// post-process (empty or whitespace-only). Used to skip the post-processing
/// LLM call when nothing was actually transcribed, which would otherwise make
/// the model reply with an error message such as "you need to provide the
/// transcription".
fn is_blank_transcription(transcription: &str) -> bool {
    transcription.trim().is_empty()
}

async fn complete_unless_cancelled<F, C>(operation: F, is_cancelled: C) -> Option<F::Output>
where
    F: Future,
    C: Fn() -> bool,
{
    tokio::pin!(operation);

    loop {
        if is_cancelled() {
            return None;
        }

        if let Ok(result) =
            tokio::time::timeout(CANCELLATION_POLL_INTERVAL, operation.as_mut()).await
        {
            return Some(result);
        }
    }
}

fn should_use_streaming_overlay(style: OverlayStyle, is_streaming: bool) -> bool {
    style == OverlayStyle::Live && is_streaming
}

async fn post_process_transcription(
    settings: &AppSettings,
    transcription: &str,
    strict: bool,
) -> Option<String> {
    if is_blank_transcription(transcription) {
        debug!("Post-processing skipped because the transcription is empty");
        return None;
    }

    let provider = match settings.active_post_process_provider().cloned() {
        Some(provider) => provider,
        None => {
            debug!("Post-processing enabled but no provider is selected");
            return None;
        }
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    if model.trim().is_empty() {
        debug!(
            "Post-processing skipped because provider '{}' has no model configured",
            provider.id
        );
        return None;
    }

    let selected_prompt_id = match &settings.post_process_selected_prompt_id {
        Some(id) => id.clone(),
        None => {
            debug!("Post-processing skipped because no prompt is selected");
            return None;
        }
    };

    let prompt = match settings
        .post_process_prompts
        .iter()
        .find(|prompt| prompt.id == selected_prompt_id)
    {
        Some(prompt) => prompt.prompt.clone(),
        None => {
            debug!(
                "Post-processing skipped because prompt '{}' was not found",
                selected_prompt_id
            );
            return None;
        }
    };

    if prompt.trim().is_empty() {
        debug!("Post-processing skipped because the selected prompt is empty");
        return None;
    }

    debug!(
        "Starting LLM post-processing with provider '{}' (model: {})",
        provider.id, model
    );

    let api_key = settings
        .post_process_api_keys
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    // Ask these providers to skip reasoning/thinking — post-processing rarely
    // benefits from it and it adds seconds of latency. llm_client picks the
    // field the endpoint understands and retries without it if rejected.
    let disable_reasoning = matches!(provider.id.as_str(), "custom" | "openrouter");

    if provider.supports_structured_output {
        debug!("Using structured outputs for provider '{}'", provider.id);

        let system_prompt = build_system_prompt(&prompt);
        let user_content = transcription.to_string();

        // Handle Apple Intelligence separately since it uses native Swift APIs
        if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                if !apple_intelligence::check_apple_intelligence_availability() {
                    debug!(
                        "Apple Intelligence selected but not currently available on this device"
                    );
                    return None;
                }

                let token_limit = model.trim().parse::<i32>().unwrap_or(0);
                let apple_result = tokio::task::spawn_blocking(move || {
                    apple_intelligence::process_text_with_system_prompt(
                        &system_prompt,
                        &user_content,
                        token_limit,
                    )
                })
                .await;
                return match apple_result {
                    Ok(Ok(result)) => {
                        if result.trim().is_empty() {
                            debug!("Apple Intelligence returned an empty response");
                            None
                        } else {
                            let result = strip_invisible_chars(&result);
                            debug!(
                                "Apple Intelligence post-processing succeeded. Output length: {} chars",
                                result.len()
                            );
                            Some(result)
                        }
                    }
                    Ok(Err(err)) => {
                        error!("Apple Intelligence post-processing failed: {}", err);
                        None
                    }
                    Err(err) => {
                        error!("Apple Intelligence task failed: {}", err);
                        None
                    }
                };
            }

            #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
            {
                debug!("Apple Intelligence provider selected on unsupported platform");
                return None;
            }
        }

        // Define JSON schema for transcription output
        let json_schema = serde_json::json!({
            "type": "object",
            "properties": {
                (TRANSCRIPTION_FIELD): {
                    "type": "string",
                    "description": "The cleaned and processed transcription text"
                }
            },
            "required": [TRANSCRIPTION_FIELD],
            "additionalProperties": false
        });

        match crate::llm_client::send_chat_completion_with_schema(
            &provider,
            api_key.clone(),
            &model,
            user_content,
            Some(system_prompt),
            Some(json_schema),
            disable_reasoning,
        )
        .await
        {
            Ok(Some(content)) => {
                // Parse the JSON response to extract the transcription field
                let content = strip_think_block(&content);
                match serde_json::from_str::<serde_json::Value>(content) {
                    Ok(json) => {
                        if let Some(transcription_value) =
                            json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str())
                        {
                            let result = strip_invisible_chars(transcription_value);
                            debug!(
                                "Structured output post-processing succeeded for provider '{}'. Output length: {} chars",
                                provider.id,
                                result.len()
                            );
                            return if strict && result.trim().is_empty() {
                                None
                            } else {
                                Some(result)
                            };
                        } else {
                            error!("Structured output response missing 'transcription' field");
                            return if strict {
                                None
                            } else {
                                Some(strip_invisible_chars(content))
                            };
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to parse structured output JSON: {}. Returning raw content.",
                            e
                        );
                        return if strict {
                            None
                        } else {
                            Some(strip_invisible_chars(content))
                        };
                    }
                }
            }
            Ok(None) => {
                error!("LLM API response has no content");
                return None;
            }
            Err(e) => {
                if strict {
                    warn!(
                        "Structured style output failed for provider '{}': {}",
                        provider.id, e
                    );
                    return None;
                }
                warn!(
                    "Structured output failed for provider '{}': {}. Falling back to legacy mode.",
                    provider.id, e
                );
                // Fall through to legacy mode below
            }
        }
    }

    // Style prompts use a separate system message whenever the endpoint supports it.
    if strict {
        return match crate::llm_client::send_chat_completion_with_schema(
            &provider,
            api_key,
            &model,
            transcription.to_string(),
            Some(build_system_prompt(&prompt)),
            None,
            disable_reasoning,
        )
        .await
        {
            Ok(Some(content)) => {
                let content = strip_invisible_chars(strip_think_block(&content));
                if content.trim().is_empty() {
                    None
                } else {
                    Some(content)
                }
            }
            _ => None,
        };
    }

    // Legacy mode: Replace ${output} variable in the prompt with the actual text
    let processed_prompt = prompt.replace("${output}", transcription);
    debug!("Processed prompt length: {} chars", processed_prompt.len());

    match crate::llm_client::send_chat_completion(
        &provider,
        api_key,
        &model,
        processed_prompt,
        disable_reasoning,
    )
    .await
    {
        Ok(Some(content)) => {
            let content = strip_invisible_chars(strip_think_block(&content));
            debug!(
                "LLM post-processing succeeded for provider '{}'. Output length: {} chars",
                provider.id,
                content.len()
            );
            if strict && content.trim().is_empty() {
                None
            } else {
                Some(content)
            }
        }
        Ok(None) => {
            error!("LLM API response has no content");
            None
        }
        Err(e) => {
            error!(
                "LLM post-processing failed for provider '{}': {}. Falling back to original transcription.",
                provider.id,
                e
            );
            None
        }
    }
}

pub(crate) struct ProcessedTranscription {
    pub final_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
}

pub(crate) async fn process_transcription_output(
    app: &AppHandle,
    transcription: &str,
    post_process: bool,
) -> ProcessedTranscription {
    let settings = get_settings(app);
    let mut final_text = transcription.to_string();
    let mut post_processed_text: Option<String> = None;
    let mut post_process_prompt: Option<String> = None;

    if post_process {
        if let Some(processed_text) =
            post_process_transcription(&settings, &final_text, false).await
        {
            post_processed_text = Some(processed_text.clone());
            final_text = processed_text;

            if let Some(prompt_id) = &settings.post_process_selected_prompt_id {
                if let Some(prompt) = settings
                    .post_process_prompts
                    .iter()
                    .find(|prompt| &prompt.id == prompt_id)
                {
                    post_process_prompt = Some(prompt.prompt.clone());
                }
            }
        }
    }

    ProcessedTranscription {
        final_text,
        post_processed_text,
        post_process_prompt,
    }
}

/// Resolve only adjacent, explicit clock-time corrections before editing. Other
/// corrections are left to the model because their intent is ambiguous.
fn resolve_explicit_time_corrections(text: &str) -> String {
    static TIME_CORRECTION: Lazy<regex::Regex> = Lazy::new(|| {
        regex::Regex::new(
        r"(?i)\b(?P<old>\d{1,2}(?:[:.]\d{2})?\s*[ap]\.?m\b\.?)\s*(?P<separator>[,.;!?—-])?\s*(?:sorry|actually|no)\s*,?\s*(?:it(?:'|’)s\s+|it\s+is\s+)?(?P<new>\d{1,2}(?:[:.]\d{2})?\s*[ap]\.?m\b\.?)"
    ).expect("valid time correction regex")
    });
    fn valid_time(time: &str) -> bool {
        let number = time
            .trim()
            .split(|c: char| c.is_ascii_alphabetic())
            .next()
            .unwrap_or("")
            .trim();
        let mut parts = number.split([':', '.']);
        let hour = parts.next().and_then(|part| part.parse::<u32>().ok());
        let minute = parts.next().map(|part| part.parse::<u32>().ok());
        matches!(hour, Some(1..=12))
            && minute.is_none_or(|m| matches!(m, Some(0..=59)))
            && parts.next().is_none()
    }
    TIME_CORRECTION
        .replace_all(text, |caps: &regex::Captures<'_>| {
            if valid_time(&caps["old"]) && valid_time(&caps["new"]) {
                let mut result = caps["new"].to_string();
                if let Some(separator) = caps.name("separator") {
                    if matches!(separator.as_str(), "?" | "!") {
                        // A period after the corrected time ends the correction,
                        // while the original question/exclamation carries its intent.
                        if result.ends_with('.') {
                            result.pop();
                        }
                        result.push_str(separator.as_str());
                    }
                }
                result
            } else {
                caps[0].to_string()
            }
        })
        .into_owned()
}

/// Preserve numeric details in the prepared transcript when accepting an edit.
fn preserves_numbers(input: &str, output: &str) -> bool {
    static CLOCK_VALUES: Lazy<regex::Regex> = Lazy::new(|| {
        regex::Regex::new(r"(?i)\b(\d{1,2})(?:[:.](\d{2}))?\s*([ap])\.?m\b")
            .expect("valid clock value regex")
    });
    static CLOCK: Lazy<regex::Regex> = Lazy::new(|| {
        regex::Regex::new(r"(?i)\b(\d{1,2})[.:](\d{2})\s*([ap])\.?m\b").expect("valid clock regex")
    });
    static NUMBERS: Lazy<regex::Regex> =
        Lazy::new(|| regex::Regex::new(r"[0-9]+(?:[.:,][0-9]+)*").expect("valid number regex"));
    let clocks = |text: &str| {
        CLOCK_VALUES
            .captures_iter(text)
            .map(|c| {
                (
                    c[1].parse::<u32>().ok(),
                    c.get(2).map(|m| m.as_str()).unwrap_or("00").to_string(),
                    c[3].to_ascii_lowercase(),
                )
            })
            .collect::<Vec<_>>()
    };
    if clocks(input) != clocks(output) {
        return false;
    }
    let input = CLOCK.replace_all(input, "${1}:${2} ${3}m");
    let output = CLOCK.replace_all(output, "${1}:${2} ${3}m");
    NUMBERS
        .find_iter(&input)
        .map(|m| m.as_str())
        .eq(NUMBERS.find_iter(&output).map(|m| m.as_str()))
}

/// Identify tokens whose casing or punctuation can carry technical meaning.
fn is_technical_token(token: &str) -> bool {
    if token.chars().any(|c| {
        matches!(
            c,
            '/' | '\\'
                | '_'
                | '@'
                | '#'
                | '='
                | '`'
                | '"'
                | '<'
                | '>'
                | '$'
                | '&'
                | '|'
                | '+'
                | '*'
                | '%'
                | '~'
                | '^'
        )
    }) || token.contains("--")
        || token.contains("::")
    {
        return true;
    }
    let letters: Vec<char> = token.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() > 1 && letters.iter().all(|c| c.is_uppercase()) {
        return true;
    }
    if letters.iter().skip(1).any(|c| c.is_uppercase()) {
        return true;
    }
    let chars: Vec<char> = token.chars().collect();
    chars.windows(3).any(|w| {
        w[1] == '.'
            && w[0].is_alphanumeric()
            && w[2].is_alphanumeric()
            && !(w[0].is_ascii_digit() && w[2].is_ascii_digit())
    })
}

fn preserves_technical_tokens(input: &str, output: &str) -> bool {
    input
        .split_whitespace()
        .filter(|word| is_technical_token(word))
        .all(|word| {
            let token = word
                .trim_matches(|c: char| matches!(c, ',' | ';' | '!' | '?' | '(' | ')' | '[' | ']'));
            token.is_empty() || output.contains(token)
        })
}

/// Avoid changing plain-looking command words when dictating into a terminal.
fn likely_technical_text(text: &str) -> bool {
    let first = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let command = [
        "git", "cargo", "npm", "npx", "bun", "node", "python", "python3", "pip", "cd", "ls", "pwd",
        "echo", "sudo", "curl", "docker", "kubectl", "make", "grep", "rg", "cat", "rm", "mkdir",
        "touch", "export",
    ]
    .contains(&first.as_str());
    command || text.split_whitespace().any(is_technical_token)
}

/// Lowercase ordinary text while preserving the model's light punctuation.
fn very_casual_format(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for token in text.split_inclusive(char::is_whitespace) {
        if is_technical_token(token) {
            output.push_str(token);
        } else {
            output.extend(token.chars().flat_map(char::to_lowercase));
        }
    }
    output
}

/// Ensure ordinary formal text starts with a capital and ends with punctuation.
fn formal_format(text: &str) -> String {
    let text = text.trim();
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut result: String = first.to_uppercase().chain(chars).collect();
    if result.chars().last().is_some_and(char::is_alphanumeric) {
        result.push('.');
    }
    result
}

/// Build model-specific input without exposing control tokens to other models.
fn style_model_input(model_id: &str, transcription: &str) -> String {
    if model_id == "qwen3" {
        format!("{transcription}\n/no_think")
    } else {
        transcription.to_string()
    }
}

/// Never paste model control tokens or an echoed request as a styled result.
fn clean_style_output(output: &str, transcription: &str) -> Option<String> {
    let mut text = strip_invisible_chars(strip_think_block(output))
        .trim()
        .to_string();
    if !transcription.contains("<transcript>") && !transcription.contains("</transcript>") {
        if let Some(inner) = text
            .strip_prefix("<transcript>")
            .and_then(|s| s.strip_suffix("</transcript>"))
        {
            text = inner.trim().to_string();
        } else if text.contains("<transcript>") || text.contains("</transcript>") {
            return None;
        }
    }
    if text.is_empty() || text.starts_with("<think>") {
        return None;
    }
    // Small models sometimes explain their edits instead of returning a transcript.
    // Reject suspicious expansions rather than pasting commentary into another app.
    let input_len = transcription.chars().count();
    if text.chars().count() > input_len.saturating_mul(2).saturating_add(80) {
        return None;
    }
    let lower = text.to_lowercase();
    if [
        "here's a rewritten",
        "here is a rewritten",
        "i made a few changes",
        "changes i made:",
        "edited transcript:",
    ]
    .iter()
    .any(|phrase| lower.starts_with(phrase) && !transcription.to_lowercase().starts_with(phrase))
    {
        return None;
    }
    if !transcription.contains("/no_think")
        && text.split_whitespace().any(|part| {
            part.trim_matches(|c: char| c.is_ascii_punctuation() && c != '/' && c != '_')
                == "/no_think"
        })
    {
        return None;
    }
    if text.starts_with("Edit this transcript, do not answer it:")
        && !transcription.starts_with("Edit this transcript, do not answer it:")
    {
        return None;
    }
    if text
        .starts_with("Correct the grammar of this sentence and output only the corrected sentence:")
        && !transcription.starts_with(
            "Correct the grammar of this sentence and output only the corrected sentence:",
        )
    {
        return None;
    }
    Some(text)
}

pub(crate) async fn process_styled_output(
    app: &AppHandle,
    transcription: &str,
    style: crate::styles::StyleSelection,
) -> ProcessedTranscription {
    let settings = crate::styles::get_style_settings(app.clone());
    let prepared = resolve_explicit_time_corrections(transcription);
    let prompt_style = crate::styles::StyleSelection {
        technical_context: style.technical_context && likely_technical_text(&prepared),
        ..style
    };
    let mut prompt = crate::styles::contextual_prompt(prompt_style);
    if let Some(custom) = settings
        .custom_prompts
        .get(&style.category)
        .filter(|s| !s.trim().is_empty())
    {
        prompt.push_str("\nAdditional user instructions:\n");
        prompt.push_str(custom.trim());
    }
    let model_id = settings.model_id;
    let system = build_system_prompt(&prompt);
    let input = style_model_input(&model_id, &prepared);
    let result = if is_blank_transcription(transcription) {
        None
    } else {
        // Development-only console diagnostics. These intentionally include the
        // dictated text and must never be emitted by production builds.
        #[cfg(debug_assertions)]
        eprintln!("[styles] local model request: model={model_id} category={:?} style={:?}\n[styles] system: {system}\n[styles] input: {input}", style.category, style.style);
        let response = crate::style_models::generate(app, model_id, system, input).await;
        #[cfg(debug_assertions)]
        match &response {
            Ok(text) => eprintln!("[styles] model response: {text}"),
            Err(error) => eprintln!("[styles] model failed: {error}; using original transcript"),
        }
        response
            .map_err(|error| warn!("Local style processing failed: {error}"))
            .ok()
            .and_then(|text| clean_style_output(&text, transcription))
            .filter(|text| {
                let numbers_ok = preserves_numbers(&prepared, text);
                let technical_ok = preserves_technical_tokens(&prepared, text);
                if !numbers_ok || !technical_ok {
                    debug!("Style output rejected: numbers_preserved={numbers_ok}, technical_tokens_preserved={technical_ok}");
                    #[cfg(debug_assertions)]
                    eprintln!("[styles] rejected: numbers_preserved={numbers_ok} technical_tokens_preserved={technical_ok}");
                }
                numbers_ok && technical_ok
            })
            .map(|text| {
                if style.technical_context && likely_technical_text(&prepared) {
                    return text;
                }
                match style.style {
                    crate::styles::WritingStyle::VeryCasual => very_casual_format(&text),
                    crate::styles::WritingStyle::Formal if !likely_technical_text(&prepared) => formal_format(&text),
                    _ => text,
                }
            })
            .filter(|text| !text.trim().is_empty())
    };
    #[cfg(debug_assertions)]
    if let Some(text) = &result {
        eprintln!("[styles] final styled output: {text}");
    }
    if result.is_none() && !is_blank_transcription(transcription) {
        #[cfg(debug_assertions)]
        eprintln!("[styles] no usable styled output; using original transcript");
        warn!("Style processing failed; using original transcription");
        let _ = app.emit("style-processing-skipped", ());
    }
    ProcessedTranscription {
        final_text: result.clone().unwrap_or_else(|| transcription.to_string()),
        post_processed_text: result.clone(),
        post_process_prompt: Some(crate::styles::history_selection(style, &prompt)),
    }
}

impl ShortcutAction for TranscribeAction {
    fn start(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        let start_time = Instant::now();
        debug!("TranscribeAction::start called for binding: {}", binding_id);

        // Load model in the background
        let tm = app.state::<Arc<TranscriptionManager>>();
        let rm = app.state::<Arc<AudioRecordingManager>>();

        // Load ASR model and VAD model in parallel
        let kickoff_started = Instant::now();
        tm.initiate_model_load();
        let rm_clone = Arc::clone(&rm);
        std::thread::spawn(move || {
            if let Err(e) = rm_clone.preload_vad() {
                debug!("VAD pre-load failed: {}", e);
            }
        });
        let kickoff_elapsed = kickoff_started.elapsed();

        // Don't open the mic if nothing can transcribe the recording; the load
        // kicked off above fails and reports why.
        if !tm.is_model_loaded() {
            let selected_model = get_settings(app).selected_model;
            if let Err(e) = app
                .state::<Arc<ModelManager>>()
                .get_model_path(&selected_model)
            {
                warn!("Not starting recording: no model can transcribe it ({})", e);
                return;
            }
        }

        crate::styles::capture(app, self.post_process);
        let binding_id = binding_id.to_string();
        RECORDING_SESSION.fetch_add(1, Ordering::SeqCst);
        let tray_started = Instant::now();
        set_tray_state(app, TrayIconState::Recording);
        let tray_elapsed = tray_started.elapsed();

        // Get the microphone mode to determine audio feedback timing
        let plan_started = Instant::now();
        let settings = get_settings(app);
        let is_always_on = settings.always_on_microphone;

        let selected_model_info = app
            .state::<Arc<ModelManager>>()
            .get_model_info(&settings.selected_model);

        // Use the app-facing model capability as the single pre-recording source
        // for live streaming decisions. Unknown support is represented as false
        // until the model registry is updated by discovery or runtime load.
        let model_supports_streaming = selected_model_info
            .as_ref()
            .map(|m| m.supports_streaming)
            .unwrap_or(false);
        let vad_policy = if !settings.vad_enabled {
            VadPolicy::Disabled
        } else if model_supports_streaming {
            VadPolicy::Streaming
        } else {
            VadPolicy::Offline
        };
        // Before the model loads, whether it keeps its accuracy on long audio
        // is unknown, so assume not: the worker decides again with the loaded
        // model, and a wrong guess costs only an idle worker.
        let expected_model = ModelTraits {
            streaming: model_supports_streaming,
            long_form: false,
        };
        if live_decode(expected_model, settings.long_form_chunking) != LiveDecode::None {
            tm.start_stream();
        }
        let plan_elapsed = plan_started.elapsed();

        // Sizing the overlay follows the same advertised capability. A model that
        // doesn't stream (or whose capability is not known yet) gets the compact
        // pill instead of an oversized transparent live window.
        let overlay_started = Instant::now();
        match settings.overlay_style {
            OverlayStyle::Live if model_supports_streaming => utils::show_streaming_overlay(app),
            OverlayStyle::Live | OverlayStyle::Minimal => show_recording_overlay(app),
            OverlayStyle::None => {} // show_overlay_state no-ops on None anyway
        }
        // Everything above runs before capture can begin, so each span here is
        // added keypress->capture latency.
        debug!(
            "start-path pre-recording steps: model_kickoff={:?} tray={:?} settings+stream_plan={:?} overlay={:?}",
            kickoff_elapsed,
            tray_elapsed,
            plan_elapsed,
            overlay_started.elapsed()
        );
        debug!("Microphone mode - always_on: {}", is_always_on);

        let mut recording_error: Option<anyhow::Error> = None;
        let recording_start_time = Instant::now();
        match rm.try_start_recording(&binding_id, vad_policy) {
            Ok(readiness) => {
                debug!(
                    "Recording request accepted in {:?}; waiting for first microphone samples",
                    recording_start_time.elapsed()
                );
                let generation = readiness.generation();
                let app_clone = app.clone();
                let rm_clone = Arc::clone(&rm);
                std::thread::spawn(move || {
                    if !readiness.wait() {
                        debug!("Microphone readiness wait ended without receiving samples");
                        return;
                    }

                    // Development-only preview hook for evaluating the brief
                    // arming animation on hardware that normally starts too fast
                    // to make it visible.
                    #[cfg(debug_assertions)]
                    if let Ok(delay_ms) = std::env::var("HANDY_DEBUG_MIC_READY_DELAY_MS")
                        .unwrap_or_default()
                        .parse::<u64>()
                    {
                        let delay_ms = delay_ms.min(10_000);
                        if delay_ms > 0 {
                            debug!("Delaying microphone-ready cue by {delay_ms}ms for UI preview");
                            std::thread::sleep(Duration::from_millis(delay_ms));
                        }
                    }

                    if !rm_clone.is_recording_readiness_current(generation) {
                        debug!("Microphone became ready for an inactive recording");
                        return;
                    }

                    debug!("Microphone is receiving samples; recording is ready");
                    utils::emit_recording_ready(&app_clone);

                    // The start chime is a readiness cue, so it must follow the
                    // first real input callback rather than Stream::play() or a
                    // fixed delay. The helper returns immediately when feedback
                    // is disabled; mute still follows the same readiness point.
                    if rm_clone.is_recording_readiness_current(generation) {
                        play_feedback_sound_blocking(&app_clone, SoundType::Start);
                    }
                    if rm_clone.is_recording_readiness_current(generation) {
                        rm_clone.apply_mute();
                    }
                });
            }
            Err(e) => {
                debug!("Failed to start recording: {}", e);
                recording_error = Some(e);
            }
        }

        if recording_error.is_none() {
            // Dynamically register the cancel shortcut in a separate task to avoid deadlock
            shortcut::register_cancel_shortcut(app);
        } else {
            // Starting failed (for example due to blocked microphone permissions).
            // Revert UI state so we don't stay stuck in the recording overlay.
            tm.cancel_stream();
            utils::hide_recording_overlay(app);
            set_tray_state(app, TrayIconState::Idle);
            if let Some(err) = recording_error {
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: recording_error_type(&err).to_string(),
                        detail: Some(format!("{err:#}")),
                    },
                );
            }
        }

        debug!(
            "TranscribeAction::start completed in {:?}",
            start_time.elapsed()
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        // Prevent a slow microphone from emitting a ready event or start chime
        // after the user has already requested stop.
        app.state::<Arc<AudioRecordingManager>>()
            .invalidate_recording_readiness();

        // Unregister the cancel shortcut when transcription stops
        shortcut::unregister_cancel_shortcut(app);

        let stop_time = Instant::now();
        debug!("TranscribeAction::stop called for binding: {}", binding_id);

        let ah = app.clone();
        let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
        let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
        let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());

        set_tray_state(app, TrayIconState::Transcribing);
        // Stop should give immediate visual feedback. Live streaming can keep
        // the larger panel, but it still switches from listening to a working
        // spinner while the stream finalizes. Non-streaming paths use the
        // compact transcribing pill (None no-ops in show_*).
        let style = get_settings(app).overlay_style;
        // Capture this before finalizing the stream so every later working state
        // targets the same overlay that was shown for this transcription.
        let use_streaming_overlay = should_use_streaming_overlay(style, tm.is_streaming());
        if use_streaming_overlay {
            tm.emit_stream_working(StreamWorkKind::Transcribing);
        } else {
            show_transcribing_overlay(app);
        }

        // Unmute before playing audio feedback so the stop sound is audible
        rm.remove_mute();

        // Play audio feedback for recording stop
        play_feedback_sound(app, SoundType::Stop);

        let binding_id = binding_id.to_string(); // Clone binding_id for the async task
        let post_process = self.post_process;
        let writing_style = crate::styles::take();
        let cancel_generation = rm.cancel_generation();

        tauri::async_runtime::spawn(async move {
            let _guard = FinishGuard(ah.clone(), Arc::clone(&tm));
            debug!(
                "Starting async transcription task for binding: {}",
                binding_id
            );

            let stop_recording_time = Instant::now();
            if let Some(samples) = rm.stop_recording(&binding_id, cancel_generation) {
                // Read now: by the time transcribing ends, another take may
                // have replaced it.
                let input_silent = rm.last_input_silent();
                debug!(
                    "Recording stopped and samples retrieved in {:?}, sample count: {}",
                    stop_recording_time.elapsed(),
                    samples.len()
                );

                if rm.was_cancelled_since(cancel_generation) {
                    debug!("Transcription operation cancelled after recording stop");
                    tm.cancel_stream();
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                    return;
                }

                if samples.is_empty() {
                    debug!("Recording produced no audio samples; skipping persistence");
                    // Tear down any streaming worker so its channel doesn't leak
                    // and block the next start_stream.
                    tm.cancel_stream();
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                    emit_transcription_complete(&ah, empty_outcome(input_silent), 0, stop_time);
                } else {
                    // Save WAV concurrently with transcription
                    let sample_count = samples.len();
                    let file_name = format!("handy-{}.wav", chrono::Utc::now().timestamp());
                    let wav_path = hm.recordings_dir().join(&file_name);
                    let wav_path_for_verify = wav_path.clone();
                    let samples_for_wav = samples.clone();
                    let wav_handle = tauri::async_runtime::spawn_blocking(move || {
                        crate::audio_toolkit::save_wav_file(&wav_path, &samples_for_wav)
                    });

                    // Transcribe concurrently with WAV save: the live decode's
                    // result when it has one (all audio was already fed to it),
                    // otherwise a batch run of the samples. A cancel is surfaced
                    // as an error, so it never starts a batch run.
                    let transcription_time = Instant::now();
                    let transcription_result = tm.transcribe_recording(samples);

                    // Await WAV save and verify
                    let wav_saved = match wav_handle.await {
                        Ok(Ok(())) => {
                            match crate::audio_toolkit::verify_wav_file(
                                &wav_path_for_verify,
                                sample_count,
                            ) {
                                Ok(()) => true,
                                Err(e) => {
                                    error!("WAV verification failed: {}", e);
                                    false
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            error!("Failed to save WAV file: {}", e);
                            false
                        }
                        Err(e) => {
                            error!("WAV save task panicked: {}", e);
                            false
                        }
                    };

                    if rm.was_cancelled_since(cancel_generation) {
                        debug!("Transcription operation cancelled before output handling");
                        utils::hide_recording_overlay(&ah);
                        set_tray_state(&ah, TrayIconState::Idle);
                        return;
                    }

                    match transcription_result {
                        Ok(Transcription {
                            text: transcription,
                            language,
                        }) => {
                            debug!(
                                "Transcription completed in {:?}: '{}'",
                                transcription_time.elapsed(),
                                utils::redact_text(&transcription)
                            );

                            if post_process || writing_style.is_some() {
                                if use_streaming_overlay {
                                    tm.emit_stream_working(StreamWorkKind::Polishing);
                                } else {
                                    show_processing_overlay(&ah);
                                }
                            }
                            let Some(processed) = complete_unless_cancelled(
                                async {
                                    if let Some(style) = writing_style {
                                        process_styled_output(&ah, &transcription, style).await
                                    } else {
                                        process_transcription_output(
                                            &ah,
                                            &transcription,
                                            post_process,
                                        )
                                        .await
                                    }
                                },
                                || rm.was_cancelled_since(cancel_generation),
                            )
                            .await
                            else {
                                debug!("Transcription operation cancelled during output handling");
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            };

                            if rm.was_cancelled_since(cancel_generation) {
                                debug!("Transcription operation cancelled before paste");
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            }

                            // Save to history if WAV was saved
                            if wav_saved {
                                if let Err(err) = hm.save_entry(
                                    file_name,
                                    transcription,
                                    post_process || writing_style.is_some(),
                                    processed.post_processed_text.clone(),
                                    processed.post_process_prompt.clone(),
                                    language,
                                ) {
                                    error!("Failed to save history entry: {}", err);
                                }
                            }

                            if processed.final_text.is_empty() {
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                emit_transcription_complete(
                                    &ah,
                                    empty_outcome(input_silent),
                                    sample_count,
                                    stop_time,
                                );
                            } else {
                                let ah_clone = ah.clone();
                                let paste_time = Instant::now();
                                let final_text = processed.final_text;
                                let rm_for_paste = Arc::clone(&rm);
                                // Taken before the paste thread exists, and so
                                // before the next session can start.
                                let session = RECORDING_SESSION.load(Ordering::SeqCst);
                                // Off the main thread: paste sleeps between steps, and a
                                // paste into Handy's own windows needs the main thread free.
                                std::thread::Builder::new()
                                    .name("paste".into())
                                    .spawn(move || {
                                        if rm_for_paste.was_cancelled_since(cancel_generation) {
                                            debug!(
                                                "Transcription operation cancelled before paste"
                                            );
                                            utils::hide_recording_overlay(&ah_clone);
                                            set_tray_state(&ah_clone, TrayIconState::Idle);
                                            return;
                                        }

                                        let outcome =
                                            match utils::paste(final_text, ah_clone.clone()) {
                                                Ok(()) => {
                                                    debug!(
                                                        "Text pasted successfully in {:?}",
                                                        paste_time.elapsed()
                                                    );
                                                    TranscriptionOutcome::Typed
                                                }
                                                Err(e) => {
                                                    error!("Failed to paste transcription: {}", e);
                                                    let _ = ah_clone.emit("paste-error", ());
                                                    TranscriptionOutcome::Failed
                                                }
                                            };
                                        // A new session may have started while
                                        // this paste ran; the UI is now its.
                                        if RECORDING_SESSION.load(Ordering::SeqCst) == session {
                                            utils::hide_recording_overlay(&ah_clone);
                                            set_tray_state(&ah_clone, TrayIconState::Idle);
                                        }
                                        emit_transcription_complete(
                                            &ah_clone,
                                            outcome,
                                            sample_count,
                                            stop_time,
                                        );
                                    })
                                    .map(|_| ())
                                    .unwrap_or_else(|e| {
                                        error!("Failed to start the paste thread: {:?}", e);
                                        utils::hide_recording_overlay(&ah);
                                        set_tray_state(&ah, TrayIconState::Idle);
                                        emit_transcription_complete(
                                            &ah,
                                            TranscriptionOutcome::Failed,
                                            sample_count,
                                            stop_time,
                                        );
                                    });
                            }
                        }
                        Err(err) => {
                            if rm.was_cancelled_since(cancel_generation) {
                                debug!(
                                    "Transcription operation cancelled after transcription error"
                                );
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            }

                            error!("Transcription failed: {}", err);
                            // Surface the failure to the UI (toast). The full
                            // message is also in handy.log via the line above.
                            let _ = ah.emit("transcription-error", err.to_string());
                            // Save entry with empty text so user can retry
                            if wav_saved {
                                if let Err(save_err) = hm.save_entry(
                                    file_name,
                                    String::new(),
                                    post_process,
                                    None,
                                    None,
                                    None,
                                ) {
                                    error!("Failed to save failed history entry: {}", save_err);
                                }
                            }
                            utils::hide_recording_overlay(&ah);
                            set_tray_state(&ah, TrayIconState::Idle);
                            emit_transcription_complete(
                                &ah,
                                TranscriptionOutcome::Failed,
                                sample_count,
                                stop_time,
                            );
                        }
                    }
                }
            } else {
                debug!("No samples retrieved from recording stop");
                // Tear down any streaming worker so its channel doesn't leak.
                tm.cancel_stream();
                utils::hide_recording_overlay(&ah);
                set_tray_state(&ah, TrayIconState::Idle);
                if !rm.was_cancelled_since(cancel_generation) {
                    emit_transcription_complete(&ah, TranscriptionOutcome::Empty, 0, stop_time);
                }
            }
        });

        debug!(
            "TranscribeAction::stop completed in {:?}",
            stop_time.elapsed()
        );
    }
}

// Cancel Action
struct CancelAction;

impl ShortcutAction for CancelAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        utils::cancel_current_operation(app);
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // Nothing to do on stop for cancel
    }
}

// Test Action
struct TestAction;

impl ShortcutAction for TestAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Started - {} (App: {})", // Changed "Pressed" to "Started" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Stopped - {} (App: {})", // Changed "Released" to "Stopped" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }
}

// Static Action Map
pub static ACTION_MAP: Lazy<HashMap<String, Arc<dyn ShortcutAction>>> = Lazy::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "transcribe".to_string(),
        Arc::new(TranscribeAction {
            post_process: false,
        }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "transcribe_with_post_process".to_string(),
        Arc::new(TranscribeAction { post_process: true }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "cancel".to_string(),
        Arc::new(CancelAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "test".to_string(),
        Arc::new(TestAction) as Arc<dyn ShortcutAction>,
    );
    map
});

#[cfg(test)]
mod tests {
    use super::{
        clean_style_output, complete_unless_cancelled, formal_format, is_blank_transcription,
        likely_technical_text, preserves_numbers, preserves_technical_tokens,
        resolve_explicit_time_corrections, should_use_streaming_overlay, strip_think_block,
        style_model_input, very_casual_format,
    };
    use crate::settings::OverlayStyle;
    use std::future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn removes_model_wrapper_without_destroying_literal_markup() {
        assert_eq!(
            clean_style_output("<transcript>Hello.</transcript>", "Hello.").as_deref(),
            Some("Hello.")
        );
        assert_eq!(clean_style_output("<transcript>Hello", "Hello"), None);
        assert_eq!(
            clean_style_output(
                "<transcript>Hello</transcript>",
                "<transcript>Hello</transcript>"
            )
            .as_deref(),
            Some("<transcript>Hello</transcript>")
        );
    }

    #[test]
    fn corrects_only_clear_adjacent_clock_repairs() {
        let input = "Plan the call around 7pm. No, it's 8pm. Discuss with Jane.";
        assert_eq!(
            resolve_explicit_time_corrections(input),
            "Plan the call around 8pm. Discuss with Jane."
        );
        assert_eq!(
            resolve_explicit_time_corrections("Meet at 7pm or 8pm."),
            "Meet at 7pm or 8pm."
        );
        assert_eq!(
            resolve_explicit_time_corrections("Meet at 27pm. No, it's 8pm."),
            "Meet at 27pm. No, it's 8pm."
        );
        assert!(preserves_numbers("Meet at 8pm.", "meet at 8 pm"));
        assert!(!preserves_numbers("Meet at 8pm.", "meet at 7pm"));
        let reported = "Hey Zahangir, can we have a quick call by today evening 7pm? Sorry, 7.30pm";
        let prepared = resolve_explicit_time_corrections(reported);
        assert_eq!(
            prepared,
            "Hey Zahangir, can we have a quick call by today evening 7.30pm?"
        );
        let model = "Hey Zahangir, can we have a quick call by today evening at 7:30 PM?";
        assert!(preserves_numbers(&prepared, model));
        assert_eq!(
            resolve_explicit_time_corrections(
                "Hey guys, can we have a quick call around 7.30pm? Sorry, 8pm."
            ),
            "Hey guys, can we have a quick call around 8pm?"
        );
        assert!(!preserves_numbers(&prepared, "Meet at 7:45 PM?"));
        assert!(!preserves_numbers(&prepared, "Meet at 7:30 AM?"));
        assert!(!preserves_numbers("Version 7.30", "Version 7:30"));
    }

    #[test]
    fn very_casual_preserves_technical_tokens() {
        assert_eq!(
            very_casual_format("Today, meet Jane at 8pm. Is it ready?"),
            "today, meet jane at 8pm. is it ready?"
        );
        assert_eq!(
            very_casual_format("Use API_KEY and https://example.com/Foo. at 8:30, version 3.14."),
            "use API_KEY and https://example.com/Foo. at 8:30, version 3.14."
        );
        assert_eq!(
            very_casual_format("Run FooBar and API now!"),
            "run FooBar and API now!"
        );
        assert_eq!(
            very_casual_format("Use C++ and FooBar!"),
            "use C++ and FooBar!"
        );
        assert!(preserves_technical_tokens("Use API_KEY", "use API_KEY"));
        assert!(!preserves_technical_tokens("Use API_KEY", "use api_key"));
        assert!(likely_technical_text("git status;"));
        assert!(likely_technical_text("run FooBar"));
        assert!(!likely_technical_text("Today I need to meet Jane at 8pm."));
    }

    #[test]
    fn formal_style_capitalizes_and_punctuates_plain_text() {
        assert_eq!(formal_format("hello everyone"), "Hello everyone.");
        assert_eq!(formal_format("can we meet?"), "Can we meet?");
    }

    #[test]
    fn repairs_reported_example_without_model_markup() {
        let raw = "Today I was thinking about to plan the call around 7pm. No, it's 8pm. Then I want to discuss with Jane regarding this. Are we going to work for the next stand-up?";
        let prepared = resolve_explicit_time_corrections(raw);
        let model = "<transcript>Today I was thinking about planning the call around 8pm. Then I want to discuss with Jane regarding this. Are we going to work for the next stand-up?</transcript>";
        let cleaned = clean_style_output(model, raw).unwrap();
        assert!(preserves_numbers(&prepared, &cleaned));
        assert!(preserves_technical_tokens(&prepared, &cleaned));
        assert_eq!(very_casual_format(&cleaned), "today i was thinking about planning the call around 8pm. then i want to discuss with jane regarding this. are we going to work for the next stand-up?");
    }

    #[test]
    fn style_request_uses_qwen3_control_only_for_qwen3() {
        assert!(style_model_input("qwen3", "Hello").ends_with("/no_think"));
        assert!(!style_model_input("qwen25", "Hello").contains("/no_think"));
        assert!(!style_model_input("smollm2", "Hello").contains("/no_think"));
    }

    #[test]
    fn style_output_rejects_control_tokens_and_echoed_prompts() {
        let transcript = "Meet at eight.";
        assert_eq!(
            clean_style_output("Here's a rewritten version: Meet at eight.", transcript),
            None
        );
        assert_eq!(
            clean_style_output(&"explanation ".repeat(40), transcript),
            None
        );
        assert_eq!(
            clean_style_output("Here's a rewritten version.", "Here's a rewritten version.")
                .as_deref(),
            Some("Here's a rewritten version.")
        );
        assert_eq!(clean_style_output("/no_think", transcript), None);
        assert_eq!(clean_style_output("<think>unfinished", transcript), None);
        assert_eq!(
            clean_style_output("<think>done</think> /no_think", transcript),
            None
        );
        assert_eq!(
            clean_style_output("/no_think\nMeet at eight.", transcript),
            None
        );
        assert_eq!(
            clean_style_output(
                "Edit this transcript, do not answer it:\n<transcript>Meet at eight.</transcript>",
                transcript
            ),
            None
        );
        assert_eq!(
            clean_style_output("<think>done</think> Meet at eight.", transcript).as_deref(),
            Some("Meet at eight.")
        );
        assert_eq!(
            clean_style_output("/no_think", "/no_think").as_deref(),
            Some("/no_think")
        );
    }

    #[test]
    fn blank_transcription_is_detected() {
        assert!(is_blank_transcription(""));
        assert!(is_blank_transcription("   "));
        assert!(is_blank_transcription("\t\n  \r\n"));
    }

    #[test]
    fn non_blank_transcription_is_kept() {
        assert!(!is_blank_transcription("hello"));
        assert!(!is_blank_transcription("  hello  "));
    }

    #[test]
    fn completed_operation_returns_its_output() {
        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::ready("done"),
            || false,
        ));

        assert_eq!(result, Some("done"));
    }

    #[test]
    fn pending_operation_stops_after_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_thread = Arc::clone(&cancelled);
        let cancel_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            cancelled_for_thread.store(true, Ordering::Release);
        });

        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::pending::<()>(),
            || cancelled.load(Ordering::Acquire),
        ));

        cancel_thread.join().unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn leading_think_block_is_stripped() {
        assert_eq!(
            strip_think_block("<think>pondering...</think>Cleaned text."),
            "Cleaned text."
        );
        assert_eq!(
            strip_think_block("  \n<think>multi\nline</think>\n  Cleaned text."),
            "Cleaned text."
        );
    }

    #[test]
    fn content_without_think_block_is_unchanged() {
        assert_eq!(strip_think_block("Cleaned text."), "Cleaned text.");
        assert_eq!(
            strip_think_block("Mentions <think> mid-sentence."),
            "Mentions <think> mid-sentence."
        );
        // Unclosed block: leave untouched rather than guess
        assert_eq!(
            strip_think_block("<think>never closed"),
            "<think>never closed"
        );
    }

    #[test]
    fn live_overlay_uses_streaming_states_only_for_streaming_models() {
        assert!(should_use_streaming_overlay(OverlayStyle::Live, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::Live, false));
        assert!(!should_use_streaming_overlay(OverlayStyle::Minimal, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::None, true));
    }
}
