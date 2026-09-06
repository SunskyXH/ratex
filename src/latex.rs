use anyhow::{Context, Result, bail};
use regex::Regex;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::translator::Provider;

/// Hide comments without changing byte offsets used to edit the original text.
fn uncommented(content: &str) -> String {
    let mut bytes = content.as_bytes().to_vec();
    let mut comment = false;
    let mut escaped = false;
    for byte in &mut bytes {
        if *byte == b'\n' {
            comment = false;
        } else if comment || (*byte == b'%' && !escaped) {
            comment = true;
            *byte = b' ';
        }
        escaped = *byte == b'\\' && !escaped;
    }
    String::from_utf8(bytes).expect("comment masking preserves UTF-8")
}

fn group_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut closing = vec![if bytes[start] == b'[' { b']' } else { b'}' }];
    let mut escaped = false;
    for (i, &byte) in bytes.iter().enumerate().skip(start + 1) {
        if !escaped {
            if byte == b'{' {
                closing.push(b'}');
            } else if closing.last() == Some(&byte) {
                closing.pop();
                if closing.is_empty() {
                    return Some(i + 1);
                }
            }
        }
        escaped = byte == b'\\' && !escaped;
    }
    None
}

/// Find literal commands with an optional [...] and a complete {...} argument.
// ponytail: literal TeX commands only; expand macros only if real papers require it.
fn commands(content: &str, command: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let bytes = content.as_bytes();
    let mut matches = Vec::new();
    for (start, _) in content.match_indices(command) {
        if bytes[..start]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count()
            % 2
            != 0
        {
            continue;
        }
        let mut pos = start + command.len();
        while bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
            pos += 1;
        }
        if bytes.get(pos) == Some(&b'[') {
            let Some(end) = group_end(bytes, pos) else {
                continue;
            };
            pos = end;
            while bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
                pos += 1;
            }
        }
        if bytes.get(pos) == Some(&b'{')
            && let Some(end) = group_end(bytes, pos)
        {
            matches.push((start..end, pos + 1..end - 1));
        }
    }
    matches
}

/// Recursively find all .tex files in `dir`.
pub fn find_tex_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut tex_files = Vec::new();
    collect_tex_files(dir, &mut tex_files)?;
    if tex_files.is_empty() {
        bail!(
            "No .tex files found in the downloaded source. The paper may not have LaTeX source available."
        );
    }
    Ok(tex_files)
}

fn collect_tex_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".ratex-build-") || name.ends_with(".ratex-chunks") {
                continue;
            }
            collect_tex_files(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "tex") {
            files.push(path);
        }
    }
    Ok(())
}

/// Replace `\bibliography{X}` with an existing `.bbl` when its `.bib` is absent.
/// TeX input paths are relative to `source_root`, including nested main files.
///
/// arXiv source archives commonly ship `main.bbl` without the underlying
/// `.bib`. Tectonic auto-runs bibtex on every compile, which silently
/// fails on the missing `.bib` and overwrites the pre-generated `.bbl`
/// with an empty stub — leaving every `\cite` rendering as `?`.
/// Inlining the existing `.bbl` keeps `\bibdata{}` out of the `.aux`,
/// so tectonic never tries to run bibtex in the first place.
pub fn inline_missing_bibliography(main_tex: &Path, source_root: &Path) -> Result<bool> {
    let content = std::fs::read_to_string(main_tex)
        .with_context(|| format!("Failed to read {}", main_tex.display()))?;

    let active = uncommented(&content);
    let mut new_content = content.clone();
    let mut rewrote_any = false;
    for (range, args) in commands(&active, "\\bibliography").into_iter().rev() {
        let names: Vec<&str> = active[args]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();

        // Don't touch this call if every referenced .bib is present —
        // bibtex will work normally and we shouldn't second-guess it.
        let any_bib_missing = names
            .iter()
            .any(|n| !source_root.join(format!("{n}.bib")).exists());
        if !any_bib_missing {
            continue;
        }

        // Find a usable .bbl. Prefer one whose stem matches a referenced
        // name; otherwise fall back to a sibling .bbl with the same stem
        // as the main tex (arXiv's typical layout).
        let bbl = names
            .iter()
            .map(|n| source_root.join(format!("{n}.bbl")))
            .find(|p| p.exists())
            .or_else(|| {
                main_tex
                    .file_stem()
                    .map(|stem| source_root.join(format!("{}.bbl", stem.to_string_lossy())))
                    .filter(|p| p.exists())
            })
            .or_else(|| Some(main_tex.with_extension("bbl")).filter(|p| p.exists()));

        let Some(bbl_path) = bbl else { continue };
        let bbl_filename = bbl_path
            .strip_prefix(source_root)
            .context("bibliography is outside the source directory")?
            .to_string_lossy();

        let replacement = format!("\\input{{{bbl_filename}}}");
        new_content.replace_range(range, &replacement);
        rewrote_any = true;
    }

    if rewrote_any {
        let temp = tempfile::NamedTempFile::new_in(source_root)?;
        std::fs::write(temp.path(), &new_content)
            .with_context(|| format!("Failed to write {}", main_tex.display()))?;
        temp.persist(main_tex)
            .with_context(|| format!("Failed to replace {}", main_tex.display()))?;
    }
    Ok(rewrote_any)
}

/// Find the main .tex file (the one containing \documentclass).
pub fn find_main_tex(tex_files: &[PathBuf]) -> Result<PathBuf> {
    for file in tex_files {
        if let Ok(content) = std::fs::read_to_string(file)
            && !commands(&uncommented(&content), "\\documentclass").is_empty()
        {
            return Ok(file.clone());
        }
    }
    bail!("Could not identify the main .tex file — none contain \\documentclass.");
}

/// Inject CJK support into the preamble of the main .tex file content.
/// Also removes conflicting fontenc/inputenc packages and neutralizes
/// pdfTeX-only directives that confuse hyperref's driver auto-detection
/// when the file is compiled with `XeTeX` (Tectonic / xelatex).
pub fn add_cjk_support(content: &str) -> String {
    let active = uncommented(content);
    let preamble_end = commands(&active, "\\begin")
        .into_iter()
        .find(|(_, args)| active[args.clone()].trim() == "document")
        .map_or(content.len(), |(range, _)| range.start);
    let preamble = &active[..preamble_end];
    let mut edits = Vec::new();

    if let Some((class, _)) = commands(preamble, "\\documentclass").first() {
        edits.push((
            class.end..class.end,
            concat!(
                "\n% [ratex] CJK support for Chinese translation\n",
                "\\usepackage{xeCJK}\n",
                "\\setCJKmainfont{FandolSong-Regular.otf}\n",
                "\\setCJKsansfont{FandolHei-Regular.otf}\n",
                "\\setCJKmonofont{FandolFang-Regular.otf}\n",
            )
            .to_string(),
        ));
    }

    for (range, args) in commands(preamble, "\\usepackage") {
        let packages: Vec<_> = preamble[args.clone()].split(',').map(str::trim).collect();
        let kept: Vec<_> = packages
            .iter()
            .copied()
            .filter(|name| !matches!(*name, "fontenc" | "inputenc"))
            .collect();
        if kept.len() != packages.len() {
            edits.push(if kept.is_empty() {
                (range, String::new())
            } else {
                (args, kept.join(","))
            });
        }
    }

    static PDFOUTPUT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\\pdfoutput\s*=?\s*[0-9]+").expect("valid pdfoutput regex"));
    for directive in PDFOUTPUT.find_iter(preamble) {
        edits.push((directive.range(), String::new()));
    }

    edits.sort_by_key(|(range, _)| range.start);
    let mut result = content.to_string();
    for (range, replacement) in edits.into_iter().rev() {
        result.replace_range(range, &replacement);
    }
    result
}

/// Split content into translatable chunks at section/paragraph boundaries.
///
/// The preamble (everything before \begin{document}) is NOT chunked for translation.
fn split_into_chunks(body: &str, max_bytes: usize) -> Vec<String> {
    if body.len() <= max_bytes {
        return vec![body.to_string()];
    }

    static SECTION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^(\\(?:section|subsection|subsubsection|chapter|part)\*?\{)")
            .expect("invalid regex")
    });

    // Split at section boundaries first
    let mut sections = Vec::new();
    let mut last_end = 0;

    for m in SECTION.find_iter(body) {
        if m.start() > last_end {
            sections.push(body[last_end..m.start()].to_string());
        }
        last_end = m.start();
    }
    if last_end < body.len() {
        sections.push(body[last_end..].to_string());
    }

    // Now split oversized sections at paragraph boundaries
    let mut chunks = Vec::new();
    for section in sections {
        if section.len() <= max_bytes {
            chunks.push(section);
            continue;
        }

        // Split at paragraph boundaries (double newlines)
        let mut current = String::new();
        // Keep each original delimiter so concatenating responses preserves TeX whitespace.
        // ponytail: keep oversized paragraphs intact; add TeX-aware splitting if needed.
        for paragraph in section.split_inclusive("\n\n") {
            if current.len() + paragraph.len() > max_bytes && !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            current.push_str(paragraph);
        }
        if !current.is_empty() {
            chunks.push(current);
        }
    }

    chunks
}

/// Translate every `.tex` file in `tex_files` in parallel, in place.
///
/// Each file becomes its own task, all sharing the chunk-level
/// `Arc<Semaphore>` so total in-flight API calls stay bounded by the
/// configured concurrency. Empty files are skipped. On the first task error every other task is aborted and
/// the error is propagated (fail-fast, same as chunk-level).
pub async fn translate_all(
    tex_files: Vec<PathBuf>,
    main_tex: &Path,
    provider: Arc<Provider>,
    semaphore: Arc<Semaphore>,
) -> Result<()> {
    let total = tex_files.len();
    eprintln!("  Translating ({total} files in parallel)...");

    let mut set: JoinSet<Result<Option<String>>> = JoinSet::new();
    for tex_file in tex_files {
        let is_main = tex_file.as_path() == main_tex;
        let provider = Arc::clone(&provider);
        let semaphore = Arc::clone(&semaphore);
        set.spawn(async move {
            let filename = tex_file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            let content = tokio::fs::read_to_string(&tex_file)
                .await
                .with_context(|| format!("Failed to read {}", tex_file.display()))?;
            if content.trim().is_empty() {
                return Ok(None);
            }

            let label = if is_main {
                format!("{filename} (main)")
            } else {
                filename.clone()
            };
            let progress_dir = tex_file.with_extension("ratex-chunks");
            tokio::fs::create_dir(&progress_dir)
                .await
                .with_context(|| format!("Failed to create {}", progress_dir.display()))?;
            let translated = translate_tex_file(
                &content,
                is_main,
                &provider,
                &semaphore,
                &label,
                &progress_dir,
            )
            .await?;
            write_atomic(&tex_file, &translated).await?;
            if let Err(e) = tokio::fs::remove_dir_all(&progress_dir).await {
                eprintln!(
                    "  Warning: could not remove {}: {e}",
                    progress_dir.display()
                );
            }
            Ok(Some(filename))
        });
    }

    let mut completed = 0usize;
    while let Some(joined) = set.join_next().await {
        let filename = joined.context("file translation task failed")??;
        completed += 1;
        if let Some(filename) = filename {
            eprintln!("  [{completed}/{total}] {filename}");
        }
    }
    Ok(())
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
    progress_dir: &Path,
) -> Result<String> {
    let active = uncommented(content);
    let doc_begin = commands(&active, "\\begin")
        .into_iter()
        .find(|(_, args)| active[args.clone()].trim() == "document")
        .map(|(range, _)| range);

    if is_main && let Some(begin) = doc_begin {
        let preamble = &content[..begin.start];

        // Add CJK support to preamble
        let new_preamble = add_cjk_support(preamble);

        // Extract the body between \begin{document} and \end{document}
        let doc_end = commands(&active, "\\end")
            .into_iter()
            .rev()
            .find(|(range, args)| {
                range.start >= begin.end && active[args.clone()].trim() == "document"
            })
            .map(|(range, _)| range.start);
        let body_content = &content[begin.end..doc_end.unwrap_or(content.len())];

        // Translate body in chunks
        let translated_body =
            translate_chunks(body_content, provider, semaphore, label, progress_dir).await?;

        let mut result = new_preamble;
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&content[begin]);
        result.push_str(&translated_body);
        if let Some(end) = doc_end {
            if !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&content[end..]);
        }
        return Ok(result);
    }

    // For non-main files or files without \begin{document}, translate everything
    translate_chunks(content, provider, semaphore, label, progress_dir).await
}

async fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let temp = tempfile::NamedTempFile::new_in(path.parent().context("output has no parent")?)
        .with_context(|| format!("Failed to create temporary file for {}", path.display()))?;
    tokio::fs::write(temp.path(), content)
        .await
        .with_context(|| format!("Failed to write {}", path.display()))?;
    temp.persist(path)
        .with_context(|| format!("Failed to replace {}", path.display()))?;
    Ok(())
}

async fn translate_chunks(
    content: &str,
    provider: &Arc<Provider>,
    semaphore: &Arc<Semaphore>,
    label: &str,
    progress_dir: &Path,
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
        let chunk_path = progress_dir.join(format!("{:05}.tex", i + 1));
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
            write_atomic(&chunk_path, &result).await?;
            Ok((i, result))
        });
    }

    let mut results: Vec<Option<String>> = (0..total).map(|_| None).collect();
    let mut completed = 0usize;
    while let Some(joined) = set.join_next().await {
        let (i, text) = joined.context("translation task failed")??;
        completed += 1;
        if total > 1 {
            eprintln!("  {label}: chunk {completed}/{total} done");
        }
        results[i] = Some(text);
    }

    Ok(results
        .into_iter()
        .map(|o| o.expect("chunk index missing — JoinSet returned fewer results than spawned"))
        .collect::<Vec<_>>()
        .concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_cjk_support_neutralizes_pdfoutput() {
        let src = "\\pdfoutput=1\n\\documentclass{article}\n\\usepackage{hyperref}\n\\begin{document}\nhi\n\\end{document}\n";
        let out = add_cjk_support(src);
        // XeTeX must not see the pdfTeX-only assignment.
        assert!(
            !out.lines()
                .any(|l| l.trim_start().starts_with("\\pdfoutput")),
            "uncommented \\pdfoutput remained:\n{out}"
        );
        assert!(
            out.contains("\\usepackage{xeCJK}"),
            "CJK package missing:\n{out}"
        );
    }

    #[test]
    fn add_cjk_support_still_removes_fontenc_inputenc() {
        let src = "\\documentclass{article}\n\\usepackage[T1]{fontenc}\n\\usepackage[utf8]{inputenc}\n\\begin{document}\n\\end{document}\n";
        let out = add_cjk_support(src);
        let active_pkgs: Vec<&str> = out
            .lines()
            .filter(|l| !l.trim_start().starts_with('%') && l.contains("\\usepackage"))
            .collect();
        assert!(
            !active_pkgs.iter().any(|l| l.contains("fontenc")),
            "fontenc still active in:\n{}",
            active_pkgs.join("\n")
        );
        assert!(
            !active_pkgs.iter().any(|l| l.contains("inputenc")),
            "inputenc still active in:\n{}",
            active_pkgs.join("\n")
        );
    }

    #[test]
    fn cjk_edits_preserve_comments_packages_and_multiline_class() {
        let src = concat!(
            "% \\documentclass{ignored}\n",
            "\\documentclass[\n11pt,foo={]} % closing ] inside braces/comment\n]{article}% class note\n",
            "\\usepackage[T1]{fontenc} \\usepackage{amsmath}% keep this\n",
            "\\usepackage{inputenc,graphicx}\n",
            "\\usepackage{hyperref} % fontenc is mentioned only in a comment\n",
            "% end preamble\n",
        );
        let out = add_cjk_support(src);
        assert!(out.starts_with("% \\documentclass{ignored}\n\\documentclass["));
        assert!(out.find("]{article}").unwrap() < out.find("\\usepackage{xeCJK}").unwrap());
        assert!(out.contains(" \\usepackage{amsmath}% keep this\n"));
        assert!(out.contains("\\usepackage{graphicx}\n"));
        assert!(out.contains("\\usepackage{hyperref} % fontenc is mentioned only in a comment\n"));
        assert!(out.ends_with("% end preamble\n"));
        assert_eq!(
            uncommented("\\% kept % 隐藏\n\\\\% hidden\n"),
            "\\% kept         \n\\\\        \n"
        );
    }

    #[test]
    fn chunking_retains_original_separators() {
        let src = "\n\n\\section{One}\nText.% comment\n\n\n\\section{Two}\nMore text.\n\n";
        assert_eq!(split_into_chunks(src, 24).concat(), src);
        assert_eq!(split_into_chunks("\n\n\n\n", 1).concat(), "\n\n\n\n");
    }

    #[cfg(unix)]
    fn mock_cli(dir: &Path) -> Arc<Provider> {
        use crate::config::{Protocol, ResolvedProfile};
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("mock-claude");
        std::fs::write(&bin, "#!/bin/sh\ninput=$(/bin/cat)\ncase \"$input\" in *FAIL*) exit 1;; esac\nprintf '%s' \"$input\"\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        Arc::new(Provider::new(&ResolvedProfile {
            protocol: Protocol::Claude,
            endpoint: bin.to_string_lossy().into_owned(),
            model: String::new(),
            api_key: String::new(),
            concurrency: 1,
        }))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn translated_document_boundaries_survive_comments() {
        let dir = make_tempdir();
        let original = concat!(
            "\\documentclass[\n11pt\n]{article}\n",
            "% fake \\begin{document}\n% end preamble\n",
            "\\begin{document}\nHello\n",
            "\\begin{verbatim}\n\\end{document}\n\\end{verbatim}\nAfter code sample.\n% end body\n",
            "\\end{document}\n% keep trailing comments\n",
        );
        let out = translate_tex_file(
            original,
            true,
            &mock_cli(dir.path()),
            &Arc::new(Semaphore::new(1)),
            "main",
            dir.path(),
        )
        .await
        .unwrap();
        assert!(out.contains("% end preamble\n\\begin{document}\n"));
        assert!(out.contains("% end body\n\\end{document}\n% keep trailing comments\n"));
        assert!(
            std::fs::read_to_string(dir.path().join("00001.tex"))
                .unwrap()
                .contains("After code sample.")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_translation_keeps_original_and_completed_chunks() {
        let dir = make_tempdir();
        let main = dir.path().join("main.tex");
        let first = format!("{}\n\n", "A".repeat(7997));
        let original = format!(
            "\\documentclass{{article}}\n\\begin{{document}}{first}FAIL\\end{{document}}\n"
        );
        std::fs::write(&main, &original).unwrap();
        let error = translate_all(
            vec![main.clone()],
            &main,
            mock_cli(dir.path()),
            Arc::new(Semaphore::new(1)),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("chunk 2/2"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&main).unwrap(), original);
        assert_eq!(
            std::fs::read_to_string(main.with_extension("ratex-chunks").join("00001.tex")).unwrap(),
            first
        );
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

        let changed = inline_missing_bibliography(&main, dir.path()).unwrap();
        assert!(changed);
        let out = std::fs::read_to_string(&main).unwrap();
        assert!(out.contains("\\input{main.bbl}"), "got:\n{out}");
        assert!(
            !out.contains("\\bibliography{custom}"),
            "still has original call:\n{out}"
        );
    }

    #[test]
    fn inline_bbl_prefers_bbl_with_referenced_name() {
        let dir = make_tempdir();
        let main = dir.path().join("main.tex");
        std::fs::write(&main, "\\bibliography{refs}\n").unwrap();
        std::fs::write(dir.path().join("refs.bbl"), "named bbl").unwrap();
        std::fs::write(dir.path().join("main.bbl"), "stem bbl").unwrap();

        inline_missing_bibliography(&main, dir.path()).unwrap();
        let out = std::fs::read_to_string(&main).unwrap();
        assert!(
            out.contains("\\input{refs.bbl}"),
            "expected refs.bbl, got:\n{out}"
        );
    }

    #[test]
    fn inline_bbl_skips_when_bib_present() {
        let dir = make_tempdir();
        let main = dir.path().join("main.tex");
        let original = "\\bibliography{custom}\n";
        std::fs::write(&main, original).unwrap();
        std::fs::write(dir.path().join("custom.bib"), "@article{...}").unwrap();
        std::fs::write(dir.path().join("custom.bbl"), "stale").unwrap();

        let changed = inline_missing_bibliography(&main, dir.path()).unwrap();
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

        let changed = inline_missing_bibliography(&main, dir.path()).unwrap();
        assert!(!changed);
        assert_eq!(std::fs::read_to_string(&main).unwrap(), original);
    }

    #[test]
    fn inline_bbl_keeps_paths_relative_to_source_root() {
        let dir = make_tempdir();
        std::fs::create_dir(dir.path().join("paper")).unwrap();
        std::fs::create_dir(dir.path().join("refs")).unwrap();
        let main = dir.path().join("paper/main.tex");
        std::fs::write(
            &main,
            "% \\bibliography{ignored}\n\\bibliography{refs/custom}% trailing comment\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("refs/custom.bbl"), "existing bibliography").unwrap();
        assert!(inline_missing_bibliography(&main, dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(main).unwrap(),
            "% \\bibliography{ignored}\n\\input{refs/custom.bbl}% trailing comment\n"
        );
    }
}
