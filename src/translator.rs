use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{Protocol, ResolvedProfile};

const SYSTEM_PROMPT: &str = r"You are a professional academic translator. Translate the following LaTeX content from English to Chinese (Simplified Chinese).

Critical rules:
1. Translate ONLY natural language text to Chinese.
2. PRESERVE exactly as-is — do NOT translate or modify:
   - All LaTeX commands (\section, \subsection, \textbf, etc.)
   - All math content ($...$, $$...$$, \[...\], \(...\), and math environments like equation, align, gather, etc.)
   - All \cite{}, \ref{}, \label{}, \url{}, \href{} commands and their arguments
   - All comments (lines starting with %)
   - Package names, file paths, identifiers
   - BibTeX keys and bibliography entries
   - Environment names in \begin{} and \end{}
3. Maintain the EXACT same LaTeX structure and formatting.
4. Use proper Chinese academic writing style (学术论文风格).
5. For well-known technical terms, use the Chinese term followed by English in parentheses on first occurrence.
6. Output ONLY the translated LaTeX content — nothing else. Do NOT add a preface, summary, or sign-off, and never write meta phrases like 'Here is the output', 'I'll translate', 'I have translated', or describe what you did. The very first and last characters of your reply must be part of the LaTeX itself.
7. If the input has no natural-language text to translate (e.g. only macro definitions, math, or comments), return it byte-for-byte UNCHANGED with no commentary.";

/// LLM provider for translation.
pub enum Provider {
    OpenAi(OpenAiProvider),
    Gemini(GeminiProvider),
    Claude(ClaudeCliProvider),
}

impl Provider {
    pub fn new(profile: &ResolvedProfile) -> Self {
        match profile.protocol {
            Protocol::OpenAi => Provider::OpenAi(OpenAiProvider {
                client: build_http_client(),
                api_key: profile.api_key.clone(),
                base_url: profile.endpoint.clone(),
                model: profile.model.clone(),
            }),
            Protocol::Gemini => Provider::Gemini(GeminiProvider {
                client: build_http_client(),
                api_key: profile.api_key.clone(),
                base_url: profile.endpoint.clone(),
                model: profile.model.clone(),
            }),
            Protocol::Claude => Provider::Claude(ClaudeCliProvider {
                bin: profile.endpoint.clone(),
                model: profile.model.clone(),
            }),
        }
    }

    pub async fn translate(&self, content: &str) -> Result<String> {
        match self {
            Provider::OpenAi(p) => p.translate(content).await,
            Provider::Gemini(p) => p.translate(content).await,
            Provider::Claude(p) => p.translate(content).await,
        }
    }
}

/// Build the shared HTTP client with sensible timeouts so a stalled
/// connection can't hang the whole pipeline forever.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_mins(3))
        .build()
        .expect("failed to build HTTP client")
}

/// Clean a raw LLM translation response.
///
/// Translation runs per-chunk and each chunk is an independent LLM call, so
/// the model can wrap *any* chunk in a fenced block or sandwich it between
/// conversational lines ("Here is the output:", "I'll translate ...") even
/// though the system prompt forbids it. Left in place, that prose lands in
/// the `.tex` file and tectonic dies with "Missing \begin{document}".
///
/// Three passes cover the real-world shapes: a bare fenced block, a bare
/// chatter preamble, and a chatter line sitting just above a fenced block.
fn clean_response(text: &str) -> String {
    let text = strip_llm_chatter(text);
    let text = strip_code_fences(&text);
    strip_llm_chatter(&text)
}

/// Strip markdown code fences if the LLM wrapped the response.
fn strip_code_fences(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let Some(newline) = trimmed.find('\n') else {
        return trimmed.to_string();
    };
    let after_open = &trimmed[newline + 1..];
    if let Some(close) = after_open.rfind("```") {
        return after_open[..close].trim_end().to_string();
    }
    after_open.to_string()
}

/// Drop conversational preamble/postamble paragraphs the model sometimes adds
/// around the translation. Deliberately strict: a paragraph is removed only
/// when it carries no LaTeX syntax at all AND opens like chatter, so genuine
/// content (Chinese prose and/or LaTeX) is never touched. Strips from both
/// ends but never empties the chunk.
fn strip_llm_chatter(text: &str) -> String {
    let spans = paragraph_spans(text);
    if spans.is_empty() {
        return text.trim().to_string();
    }

    let mut lo = 0;
    while lo < spans.len() && looks_like_chatter(&text[spans[lo].0..spans[lo].1]) {
        lo += 1;
    }
    let mut hi = spans.len();
    while hi > lo && looks_like_chatter(&text[spans[hi - 1].0..spans[hi - 1].1]) {
        hi -= 1;
    }

    // Everything looked like chatter — don't silently drop the whole chunk.
    if lo >= hi {
        return text.trim().to_string();
    }
    text[spans[lo].0..spans[hi - 1].1].trim().to_string()
}

/// Byte ranges of blank-line-delimited paragraphs (runs of non-blank lines).
fn paragraph_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim().is_empty() {
            if let Some(s) = start.take() {
                spans.push((s, offset));
            }
        } else if start.is_none() {
            start = Some(offset);
        }
        offset += line.len();
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }
    spans
}

/// True when a paragraph is the model talking about the translation rather
/// than LaTeX content. Any `\`, `{`, `}`, `$`, or a leading `%` comment means
/// it is content and is kept.
fn looks_like_chatter(para: &str) -> bool {
    let trimmed = para.trim();
    if trimmed.is_empty()
        || trimmed.contains('\\')
        || trimmed.contains('{')
        || trimmed.contains('}')
        || trimmed.contains('$')
    {
        return false;
    }

    let first = trimmed.lines().next().unwrap_or("").trim();
    if first.starts_with('%') {
        return false;
    }

    let opener = regex::Regex::new(
        r"(?i)^(sure|certainly|of course|okay|ok|here'?s|here\s+(is|are)|i['’](ll|ve|m)|i\s+(will|have|am|translated|translate)|below\s+is|the\s+following|this\s+(is|content|file)|note:|以下是|翻译如下|译文如下|下面是)",
    )
    .expect("valid chatter regex")
    .is_match(first);
    if !opener {
        return false;
    }

    // An opener alone is not enough — require a handoff colon or a word that
    // ties the line to the act of translating, so an ordinary sentence that
    // merely starts with "This is ..." survives.
    let last = trimmed
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim_end();
    let handoff = last.ends_with(':') || last.ends_with('：');
    let lower = trimmed.to_lowercase();
    let meta = lower.contains("translat")
        || lower.contains("latex")
        || lower.contains("preserv")
        || lower.contains("unchanged")
        || lower.contains("comment");
    handoff || meta
}

// ─── OpenAI ──────────────────────────────────────────────────────────────────

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    temperature: f32,
    messages: Vec<OpenAiMessage>,
}

#[derive(Serialize)]
struct OpenAiMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessageResp,
}

#[derive(Deserialize)]
struct OpenAiMessageResp {
    content: String,
}

impl OpenAiProvider {
    async fn translate(&self, content: &str) -> Result<String> {
        let request = OpenAiRequest {
            model: self.model.clone(),
            temperature: 0.3,
            messages: vec![
                OpenAiMessage {
                    role: "system".to_string(),
                    content: SYSTEM_PROMPT.to_string(),
                },
                OpenAiMessage {
                    role: "user".to_string(),
                    content: content.to_string(),
                },
            ],
        };

        let url = format!("{}/chat/completions", self.base_url);
        let response = retry_request(|| {
            self.client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .json(&request)
                .send()
        })
        .await
        .context("OpenAI API request failed")?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!(
                "Invalid OpenAI API key. Check your --api-key or OPENAI_API_KEY environment variable."
            );
        }

        let body = response.text().await?;
        if !status.is_success() {
            bail!("OpenAI API error ({status}): {body}");
        }

        let resp: OpenAiResponse =
            serde_json::from_str(&body).context("Failed to parse OpenAI response")?;

        let text = resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        Ok(clean_response(&text))
    }
}

// ─── Gemini ──────────────────────────────────────────────────────────────────

pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    system_instruction: GeminiContent,
    contents: Vec<GeminiContent>,
    generation_config: GeminiGenConfig,
}

#[derive(Serialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiPart {
    text: String,
}

#[derive(Serialize)]
struct GeminiGenConfig {
    temperature: f32,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiContentResp,
}

#[derive(Deserialize)]
struct GeminiContentResp {
    parts: Vec<GeminiPartResp>,
}

#[derive(Deserialize)]
struct GeminiPartResp {
    text: String,
}

impl GeminiProvider {
    async fn translate(&self, content: &str) -> Result<String> {
        let request = GeminiRequest {
            system_instruction: GeminiContent {
                parts: vec![GeminiPart {
                    text: SYSTEM_PROMPT.to_string(),
                }],
            },
            contents: vec![GeminiContent {
                parts: vec![GeminiPart {
                    text: content.to_string(),
                }],
            }],
            generation_config: GeminiGenConfig { temperature: 0.3 },
        };

        let url = format!(
            "{}/v1beta/models/{}:generateContent?key={}",
            self.base_url, self.model, self.api_key
        );

        let response = retry_request(|| self.client.post(&url).json(&request).send())
            .await
            .context("Gemini API request failed")?;

        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            bail!("Gemini API error ({status}): {body}");
        }

        let resp: GeminiResponse =
            serde_json::from_str(&body).context("Failed to parse Gemini response")?;

        let text = resp
            .candidates
            .and_then(|mut c| c.pop())
            .map(|c| {
                c.content
                    .parts
                    .into_iter()
                    .map(|p| p.text)
                    .collect::<String>()
            })
            .unwrap_or_default();

        Ok(clean_response(&text))
    }
}

// ─── Claude CLI ──────────────────────────────────────────────────────────────

/// Shells out to the Claude Code CLI (`claude -p`). Auth comes from the user's
/// local `claude` setup — no API key threaded through this codebase.
pub struct ClaudeCliProvider {
    /// Path to the `claude` binary, or just `"claude"` to use PATH.
    bin: String,
    /// Optional model override. Empty → don't pass `--model`.
    model: String,
}

impl ClaudeCliProvider {
    async fn translate(&self, content: &str) -> Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--print")
            .arg("--append-system-prompt")
            .arg(SYSTEM_PROMPT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Make sure a timeout (which drops the wait future and hence the
            // Child) actually kills the subprocess. Tokio's default is to
            // leave it running.
            .kill_on_drop(true);

        if !self.model.is_empty() {
            cmd.args(["--model", &self.model]);
        }

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn '{}'. Is the Claude CLI installed and on PATH?",
                self.bin
            )
        })?;

        let mut stdin = child
            .stdin
            .take()
            .context("Claude CLI stdin pipe was not opened")?;
        stdin
            .write_all(content.as_bytes())
            .await
            .context("Failed to write content to Claude CLI stdin")?;
        // Close stdin so claude sees EOF and starts processing.
        drop(stdin);

        let output = tokio::time::timeout(Duration::from_mins(5), child.wait_with_output())
            .await
            .context("Claude CLI timed out after 5 minutes")?
            .context("Failed to wait for Claude CLI")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "Claude CLI exited with status {}: {}",
                output.status,
                stderr.trim()
            );
        }

        let text =
            String::from_utf8(output.stdout).context("Claude CLI returned non-UTF-8 output")?;
        Ok(clean_response(&text))
    }
}

// ─── Retry helper ────────────────────────────────────────────────────────────

async fn retry_request<F, Fut>(make_request: F) -> Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    const MAX_ATTEMPTS: u32 = 3;

    for attempt in 0..MAX_ATTEMPTS {
        let can_retry = attempt + 1 < MAX_ATTEMPTS;
        match make_request().await {
            Ok(resp) => {
                let status = resp.status();
                // Retry on rate limit or server errors
                if (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                    && can_retry
                {
                    let delay = retry_delay(attempt);
                    eprintln!(
                        "  API returned {status}, retrying in {}s...",
                        delay.as_secs()
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if can_retry {
                    let delay = retry_delay(attempt);
                    eprintln!(
                        "  Request failed ({}), retrying in {}s...",
                        redact_secrets(&e.to_string()),
                        delay.as_secs()
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(e.into());
            }
        }
    }
    unreachable!("retry loop has a non-zero attempt count")
}

fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(2u64.pow(attempt + 1))
}

/// Strip query parameters that look like API keys (e.g. Gemini's `?key=...`)
/// from error messages so they don't end up in logs.
fn redact_secrets(msg: &str) -> String {
    // Replace `key=...` query value up to the next `&` or end of token.
    let re = regex::Regex::new(r"([?&]key=)[^&\s\)]+").expect("valid regex");
    re.replace_all(msg, "${1}REDACTED").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_gemini_key_query_param() {
        let msg = "error sending request for url (https://x/v1beta?key=AIza-secret-123)";
        let r = redact_secrets(msg);
        assert!(!r.contains("AIza-secret-123"), "got: {r}");
        assert!(r.contains("key=REDACTED"), "got: {r}");
    }

    #[test]
    fn redacts_key_in_middle_of_query_string() {
        let msg = "url (https://x?foo=1&key=topsecret&bar=2)";
        let r = redact_secrets(msg);
        assert_eq!(r, "url (https://x?foo=1&key=REDACTED&bar=2)");
    }

    #[test]
    fn leaves_unrelated_text_alone() {
        let msg = "no secrets here";
        assert_eq!(redact_secrets(msg), msg);
    }

    // ─── response cleaning ────────────────────────────────────────────────

    #[test]
    fn strips_the_real_world_preamble() {
        // The exact failure that broke 2503.20783: chatter prepended to a
        // chunk of math_commands.tex (all macros) → tectonic "Missing
        // \begin{document}".
        let raw = "I'll translate the LaTeX content following your rules. This content is entirely LaTeX command definitions (macros) and comments — there is no natural language body text to translate except the comments. Per your rules, comments (lines starting with %) must be preserved exactly as-is.\n\nHere is the output:\n\n% Tensor\n\\def\\tA{{\\tens{A}}}\n";
        let cleaned = clean_response(raw);
        assert_eq!(cleaned, "% Tensor\n\\def\\tA{{\\tens{A}}}");
    }

    #[test]
    fn leaves_clean_macro_chunk_untouched() {
        let raw = "% Tensor\n\\def\\tA{{\\tens{A}}}\n\\def\\tB{{\\tens{B}}}";
        assert_eq!(clean_response(raw), raw);
    }

    #[test]
    fn keeps_chinese_body_prose() {
        // Real translated content must survive even with no LaTeX syntax.
        let raw = "我们提出了一种新的方法来解决这个问题。\n\n实验结果表明该方法是有效的。";
        assert_eq!(clean_response(raw), raw);
    }

    #[test]
    fn keeps_paragraph_that_mentions_translation_but_has_latex() {
        // "translat" keyword present, but it's genuine content (has a command).
        let raw = "我们翻译了 \\cite{smith2020} 中的定义。";
        assert_eq!(clean_response(raw), raw);
    }

    #[test]
    fn keeps_plain_sentence_starting_with_this_is() {
        // Opener matches but there's no handoff colon or meta keyword.
        let raw = "This is a normal sentence that should not be removed.";
        assert_eq!(clean_response(raw), raw);
    }

    #[test]
    fn strips_trailing_postamble() {
        let raw = "\\section{方法}\n\n本节介绍方法。\n\nNote: I kept all LaTeX commands unchanged as requested.";
        assert_eq!(clean_response(raw), "\\section{方法}\n\n本节介绍方法。");
    }

    #[test]
    fn strips_chatter_line_above_a_fenced_block() {
        let raw = "Here is the translated output:\n\n```latex\n\\section{引言}\n```";
        assert_eq!(clean_response(raw), "\\section{引言}");
    }

    #[test]
    fn plain_fenced_block_still_unwraps() {
        let raw = "```latex\n\\section{引言}\n```";
        assert_eq!(clean_response(raw), "\\section{引言}");
    }

    #[test]
    fn never_empties_an_all_chatter_chunk() {
        // Degenerate: nothing but chatter. Better to keep it than to silently
        // drop the chunk and lose real content on a false positive.
        let raw = "Here is the output:";
        assert_eq!(clean_response(raw), "Here is the output:");
    }
}
