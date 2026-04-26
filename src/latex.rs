use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::translator::Provider;

/// Recursively find all .tex files in `dir`.
pub fn find_tex_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut tex_files = Vec::new();
    collect_tex_files(dir, &mut tex_files)?;
    if tex_files.is_empty() {
        bail!("No .tex files found in the downloaded source. The paper may not have LaTeX source available.");
    }
    Ok(tex_files)
}

fn collect_tex_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_tex_files(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "tex") {
            files.push(path);
        }
    }
    Ok(())
}

/// Find the main .tex file (the one containing \documentclass).
pub fn find_main_tex(tex_files: &[PathBuf]) -> Result<PathBuf> {
    for file in tex_files {
        if let Ok(content) = std::fs::read_to_string(file) {
            // Check for \documentclass not inside a comment
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.starts_with('%') && trimmed.contains("\\documentclass") {
                    return Ok(file.clone());
                }
            }
        }
    }
    bail!("Could not identify the main .tex file — none contain \\documentclass.");
}

/// Inject CJK support into the preamble of the main .tex file content.
/// Also removes conflicting fontenc/inputenc packages.
pub fn add_cjk_support(content: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut insert_pos = None;
    let mut removals = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Find \documentclass line to know where to insert after
        if !trimmed.starts_with('%') && trimmed.contains("\\documentclass") && insert_pos.is_none()
        {
            insert_pos = Some(i + 1);
        }

        // Mark fontenc and inputenc for removal (they conflict with xelatex)
        if !trimmed.starts_with('%')
            && trimmed.contains("\\usepackage")
            && (trimmed.contains("fontenc") || trimmed.contains("inputenc"))
        {
            removals.push(i);
        }

        // Stop at \begin{document}
        if trimmed.starts_with("\\begin{document}") {
            break;
        }
    }

    // Remove conflicting packages (reverse order to preserve indices)
    for &idx in removals.iter().rev() {
        lines[idx] = format!("% [ratex] removed: {}", lines[idx]);
    }

    // Insert CJK support after \documentclass
    if let Some(pos) = insert_pos {
        let cjk_lines = vec![
            String::new(),
            "% [ratex] CJK support for Chinese translation".to_string(),
            "\\usepackage{xeCJK}".to_string(),
            "\\setCJKmainfont{FandolSong-Regular.otf}".to_string(),
            "\\setCJKsansfont{FandolHei-Regular.otf}".to_string(),
            "\\setCJKmonofont{FandolFang-Regular.otf}".to_string(),
        ];
        for (j, line) in cjk_lines.into_iter().enumerate() {
            lines.insert(pos + j, line);
        }
    }

    lines.join("\n")
}

/// Split content into translatable chunks at section/paragraph boundaries.
///
/// The preamble (everything before \begin{document}) is NOT chunked for translation.
fn split_into_chunks(body: &str, max_chars: usize) -> Vec<String> {
    if body.len() <= max_chars {
        return vec![body.to_string()];
    }

    let section_re = Regex::new(r"(?m)^(\\(?:section|subsection|subsubsection|chapter|part)\*?\{)")
        .expect("invalid regex");

    // Split at section boundaries first
    let mut sections = Vec::new();
    let mut last_end = 0;

    for m in section_re.find_iter(body) {
        if m.start() > last_end {
            let piece = &body[last_end..m.start()];
            if !piece.trim().is_empty() {
                sections.push(piece.to_string());
            }
        }
        last_end = m.start();
    }
    if last_end < body.len() {
        let piece = &body[last_end..];
        if !piece.trim().is_empty() {
            sections.push(piece.to_string());
        }
    }

    // Now split oversized sections at paragraph boundaries
    let mut chunks = Vec::new();
    for section in sections {
        if section.len() <= max_chars {
            chunks.push(section);
        } else {
            // Split at paragraph boundaries (double newlines)
            let mut current = String::new();
            for paragraph in section.split("\n\n") {
                if current.len() + paragraph.len() + 2 > max_chars && !current.is_empty() {
                    chunks.push(current.clone());
                    current.clear();
                }
                if !current.is_empty() {
                    current.push_str("\n\n");
                }
                current.push_str(paragraph);
            }
            if !current.is_empty() {
                chunks.push(current);
            }
        }
    }

    chunks
}

/// Translate a single .tex file content.
///
/// If `is_main` is true, CJK support is injected into the preamble and only
/// the document body is sent for translation.
pub async fn translate_tex_file(
    content: &str,
    is_main: bool,
    provider: &Arc<Provider>,
    semaphore: &Arc<Semaphore>,
) -> Result<String> {
    // Find \begin{document} and \end{document}
    let doc_begin = content.find("\\begin{document}");
    let doc_end = content.rfind("\\end{document}");

    if is_main {
        if let Some(begin_pos) = doc_begin {
            let preamble = &content[..begin_pos];
            let after_begin = &content[begin_pos..];

            // Add CJK support to preamble
            let new_preamble = add_cjk_support(preamble);

            // Extract the body between \begin{document} and \end{document}
            let body_start = "\\begin{document}".len();
            let body_content = if let Some(end_pos) = after_begin.rfind("\\end{document}") {
                &after_begin[body_start..end_pos]
            } else {
                &after_begin[body_start..]
            };

            // Translate body in chunks
            let translated_body = translate_chunks(body_content, provider, semaphore).await?;

            let mut result = new_preamble;
            result.push_str("\\begin{document}");
            result.push_str(&translated_body);
            if doc_end.is_some() {
                result.push_str("\\end{document}\n");
            }
            return Ok(result);
        }
    }

    // For non-main files or files without \begin{document}, translate everything
    translate_chunks(content, provider, semaphore).await
}

async fn translate_chunks(
    content: &str,
    provider: &Arc<Provider>,
    semaphore: &Arc<Semaphore>,
) -> Result<String> {
    let chunks = split_into_chunks(content, 8000);
    let total = chunks.len();

    if total == 0 {
        return Ok(content.to_string());
    }

    eprintln!("  Translating {} chunk(s)...", total);

    let mut set: JoinSet<Result<(usize, String)>> = JoinSet::new();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let provider = Arc::clone(provider);
        let semaphore = Arc::clone(semaphore);
        set.spawn(async move {
            // Acquire happens inside the task so all chunks are queued without
            // serializing the spawning loop on permit availability.
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("semaphore closed unexpectedly");
            let result = provider
                .translate(&chunk)
                .await
                .with_context(|| format!("Failed to translate chunk {}/{}", i + 1, total))?;
            Ok((i, result))
        });
    }

    let mut results: Vec<Option<String>> = (0..total).map(|_| None).collect();
    let mut completed = 0usize;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok((i, text))) => {
                completed += 1;
                eprintln!("  [{}/{}] chunk done", completed, total);
                results[i] = Some(text);
            }
            Ok(Err(e)) => {
                set.abort_all();
                return Err(e);
            }
            Err(e) => {
                set.abort_all();
                return Err(anyhow!("translation task panicked: {e}"));
            }
        }
    }

    Ok(results
        .into_iter()
        .map(|o| o.expect("chunk index missing — JoinSet returned fewer results than spawned"))
        .collect::<Vec<_>>()
        .join("\n\n"))
}
