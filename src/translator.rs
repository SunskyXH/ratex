use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::{Protocol, ResolvedProfile};

const SYSTEM_PROMPT: &str = r#"You are a professional academic translator. Translate the following LaTeX content from English to Chinese (Simplified Chinese).

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
6. Output ONLY the translated LaTeX content. No explanations, no markdown code fences, no extra text."#;

/// LLM provider for translation.
pub enum Provider {
    OpenAi(OpenAiProvider),
    Gemini(GeminiProvider),
}

impl Provider {
    pub fn new(profile: &ResolvedProfile) -> Self {
        match profile.protocol {
            Protocol::OpenAi => Provider::OpenAi(OpenAiProvider {
                client: reqwest::Client::new(),
                api_key: profile.api_key.clone(),
                base_url: profile.endpoint.clone(),
                model: profile.model.clone(),
            }),
            Protocol::Gemini => Provider::Gemini(GeminiProvider {
                client: reqwest::Client::new(),
                api_key: profile.api_key.clone(),
                base_url: profile.endpoint.clone(),
                model: profile.model.clone(),
            }),
        }
    }

    pub async fn translate(&self, content: &str) -> Result<String> {
        match self {
            Provider::OpenAi(p) => p.translate(content).await,
            Provider::Gemini(p) => p.translate(content).await,
        }
    }
}

/// Strip markdown code fences if the LLM wrapped the response.
fn strip_code_fences(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with("```") {
        // Remove opening fence (```latex or ```)
        let after_open = if let Some(pos) = trimmed.find('\n') {
            &trimmed[pos + 1..]
        } else {
            return trimmed.to_string();
        };
        // Remove closing fence
        if let Some(pos) = after_open.rfind("```") {
            return after_open[..pos].trim_end().to_string();
        }
        return after_open.to_string();
    }
    trimmed.to_string()
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
            bail!("Invalid OpenAI API key. Check your --api-key or OPENAI_API_KEY environment variable.");
        }

        let body = response.text().await?;
        if !status.is_success() {
            bail!("OpenAI API error ({}): {}", status, body);
        }

        let resp: OpenAiResponse =
            serde_json::from_str(&body).context("Failed to parse OpenAI response")?;

        let text = resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        Ok(strip_code_fences(&text))
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
            bail!("Gemini API error ({}): {}", status, body);
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
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        Ok(strip_code_fences(&text))
    }
}

// ─── Retry helper ────────────────────────────────────────────────────────────

async fn retry_request<F, Fut>(make_request: F) -> Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut last_err = None;
    for attempt in 0..3 {
        match make_request().await {
            Ok(resp) => {
                let status = resp.status();
                // Retry on rate limit or server errors
                if (status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status.is_server_error())
                    && attempt < 2
                {
                    let delay = Duration::from_secs(2u64.pow(attempt as u32 + 1));
                    eprintln!(
                        "  API returned {}, retrying in {}s...",
                        status,
                        delay.as_secs()
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt < 2 {
                    let delay = Duration::from_secs(2u64.pow(attempt as u32 + 1));
                    eprintln!(
                        "  Request failed ({}), retrying in {}s...",
                        e,
                        delay.as_secs()
                    );
                    tokio::time::sleep(delay).await;
                    last_err = Some(e);
                    continue;
                }
                return Err(e.into());
            }
        }
    }
    Err(last_err.unwrap().into())
}
