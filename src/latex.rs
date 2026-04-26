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

/// Replace `\bibliography{X}` in `main_tex` with `\input{X.bbl}` when no
/// `X.bib` is available next to it but a pre-generated `.bbl` exists.
///
/// arXiv source archives commonly ship `main.bbl` without the underlying
/// `.bib`. Tectonic auto-runs bibtex on every compile, which silently
/// fails on the missing `.bib` and overwrites the pre-generated `.bbl`
/// with an empty stub — leaving every `\cite` rendering as `?`.
/// Inlining the existing `.bbl` keeps `\bibdata{}` out of the `.aux`,
/// so tectonic never tries to run bibtex in the first place.
pub fn inline_missing_bibliography(main_tex: &Path) -> Result<bool> {
    let dir = main_tex
        .parent()
        .ok_or_else(|| anyhow!("main tex has no parent directory"))?;
    let content = std::fs::read_to_string(main_tex)
        .with_context(|| format!("Failed to read {}", main_tex.display()))?;

    let bib_re = Regex::new(r"(?m)^([ \t]*)\\bibliography\{([^}]+)\}[ \t]*$")
        .expect("invalid regex");

    let mut new_content = content.clone();
    let mut rewrote_any = false;
    for cap in bib_re.captures_iter(&content) {
        let full = cap.get(0).expect("regex match").as_str().to_string();
        let names: Vec<String> = cap[2]
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Don't touch this call if every referenced .bib is present —
        // bibtex will work normally and we shouldn't second-guess it.
        let any_bib_missing = names
            .iter()
            .any(|n| !dir.join(format!("{}.bib", n)).exists());
        if !any_bib_missing {
            continue;
        }

        // Find a usable .bbl. Prefer one whose stem matches a referenced
        // name; otherwise fall back to a sibling .bbl with the same stem
        // as the main tex (arXiv's typical layout).
        let bbl = names
            .iter()
            .map(|n| dir.join(format!("{}.bbl", n)))
            .find(|p| p.exists())
            .or_else(|| {
                main_tex
                    .file_stem()
                    .map(|stem| dir.join(format!("{}.bbl", stem.to_string_lossy())))
                    .filter(|p| p.exists())
            });

        let Some(bbl_path) = bbl else { continue };
        let bbl_filename = bbl_path
            .file_name()
            .expect("bbl path has filename")
            .to_string_lossy();

        let replacement = format!(
            "% [ratex] no .bib found beside the source — inline pre-generated .bbl\n\
             \\input{{{}}}",
            bbl_filename
        );
        new_content = new_content.replace(&full, &replacement);
        rewrote_any = true;
    }

    if rewrote_any {
        std::fs::write(main_tex, &new_content)
            .with_context(|| format!("Failed to write {}", main_tex.display()))?;
    }
    Ok(rewrote_any)
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
/// Also removes conflicting fontenc/inputenc packages and neutralizes
/// pdfTeX-only directives that confuse hyperref's driver auto-detection
/// when the file is compiled with XeTeX (Tectonic / xelatex).
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

        // \pdfoutput=1 (arXiv's pdfTeX hint) misleads hyperref into loading
        // hpdftex.def under XeTeX, which then fails on pdfTeX-only primitives.
        if !trimmed.starts_with('%') && trimmed.starts_with("\\pdfoutput") {
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
    label: &str,
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
            let translated_body =
                translate_chunks(body_content, provider, semaphore, label).await?;

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
    translate_chunks(content, provider, semaphore, label).await
}

async fn translate_chunks(
    content: &str,
    provider: &Arc<Provider>,
    semaphore: &Arc<Semaphore>,
    label: &str,
) -> Result<String> {
    let chunks = split_into_chunks(content, 8000);
    let total = chunks.len();

    if total == 0 {
        return Ok(content.to_string());
    }

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
                if total > 1 {
                    eprintln!("  {}: chunk {}/{} done", label, completed, total);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_cjk_support_neutralizes_pdfoutput() {
        let src = "\\pdfoutput=1\n\\documentclass{article}\n\\usepackage{hyperref}\n\\begin{document}\nhi\n\\end{document}\n";
        let out = add_cjk_support(src);
        // Original line is no longer an active directive — it has been
        // commented out so XeTeX doesn't see \pdfoutput=1.
        assert!(!out.lines().any(|l| l.trim_start().starts_with("\\pdfoutput")),
            "uncommented \\pdfoutput remained:\n{out}");
        assert!(out.contains("[ratex] removed:"), "expected removal marker, got:\n{out}");
        assert!(out.contains("\\usepackage{xeCJK}"), "CJK package missing:\n{out}");
    }

    #[test]
    fn add_cjk_support_still_removes_fontenc_inputenc() {
        let src = "\\documentclass{article}\n\\usepackage[T1]{fontenc}\n\\usepackage[utf8]{inputenc}\n\\begin{document}\n\\end{document}\n";
        let out = add_cjk_support(src);
        let active_pkgs: Vec<&str> = out
            .lines()
            .filter(|l| !l.trim_start().starts_with('%') && l.contains("\\usepackage"))
            .collect();
        assert!(!active_pkgs.iter().any(|l| l.contains("fontenc")),
            "fontenc still active in:\n{}", active_pkgs.join("\n"));
        assert!(!active_pkgs.iter().any(|l| l.contains("inputenc")),
            "inputenc still active in:\n{}", active_pkgs.join("\n"));
    }

    fn make_tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    #[test]
    fn inline_bbl_when_bib_missing_and_matching_bbl_exists() {
        let dir = make_tempdir();
        let main = dir.path().join("main.tex");
        std::fs::write(&main, "before\n\\bibliography{custom}\nafter\n").unwrap();
        std::fs::write(dir.path().join("main.bbl"), "% bbl content").unwrap();
        // No custom.bib, no custom.bbl — fall back to <main_stem>.bbl.

        let changed = inline_missing_bibliography(&main).unwrap();
        assert!(changed);
        let out = std::fs::read_to_string(&main).unwrap();
        assert!(out.contains("\\input{main.bbl}"), "got:\n{out}");
        assert!(!out.contains("\\bibliography{custom}"), "still has original call:\n{out}");
    }

    #[test]
    fn inline_bbl_prefers_bbl_with_referenced_name() {
        let dir = make_tempdir();
        let main = dir.path().join("main.tex");
        std::fs::write(&main, "\\bibliography{refs}\n").unwrap();
        std::fs::write(dir.path().join("refs.bbl"), "named bbl").unwrap();
        std::fs::write(dir.path().join("main.bbl"), "stem bbl").unwrap();

        inline_missing_bibliography(&main).unwrap();
        let out = std::fs::read_to_string(&main).unwrap();
        assert!(out.contains("\\input{refs.bbl}"), "expected refs.bbl, got:\n{out}");
    }

    #[test]
    fn inline_bbl_skips_when_bib_present() {
        let dir = make_tempdir();
        let main = dir.path().join("main.tex");
        let original = "\\bibliography{custom}\n";
        std::fs::write(&main, original).unwrap();
        std::fs::write(dir.path().join("custom.bib"), "@article{...}").unwrap();
        std::fs::write(dir.path().join("custom.bbl"), "stale").unwrap();

        let changed = inline_missing_bibliography(&main).unwrap();
        assert!(!changed, "should not rewrite when .bib is present");
        assert_eq!(std::fs::read_to_string(&main).unwrap(), original);
    }

    #[test]
    fn inline_bbl_noop_when_no_bbl_available() {
        let dir = make_tempdir();
        let main = dir.path().join("main.tex");
        let original = "\\bibliography{custom}\n";
        std::fs::write(&main, original).unwrap();
        // No .bib, no .bbl anywhere — leave the file alone.

        let changed = inline_missing_bibliography(&main).unwrap();
        assert!(!changed);
        assert_eq!(std::fs::read_to_string(&main).unwrap(), original);
    }
}
