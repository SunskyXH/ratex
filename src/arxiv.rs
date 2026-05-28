use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use regex::Regex;
use std::io::{Cursor, Read};
use std::path::Path;
use tar::Archive;
use url::Url;

const URL_PATH_PREFIXES: &[&str] = &["abs", "pdf", "e-print", "html", "format"];

/// Parse an arXiv paper ID from a URL or bare ID.
///
/// Accepts:
/// - Full URLs under `/abs/`, `/pdf/`, `/e-print/`, `/html/`, or `/format/`
///   (a trailing `.pdf` is tolerated; URL scheme/host are case-insensitive)
/// - New-style IDs: 2301.00001, 2301.00001v2
/// - Old-style IDs: hep-th/0601001, math.GT/0309136v1, cond-mat.mes-hall/0601001
pub fn parse_id(input: &str) -> Result<String> {
    let input = input.trim().trim_end_matches('/');

    if looks_like_url(input) {
        return parse_url(input);
    }

    if id_is_valid(input) {
        return Ok(input.to_string());
    }

    bail!(
        "Cannot parse arXiv ID from '{input}'. Expected a URL like https://arxiv.org/abs/2301.00001 or a bare ID like 2301.00001"
    )
}

/// Heuristic: anything with a `://` or starting with the arxiv hostname is
/// treated as URL input. Bare IDs (`2301.00001`, `hep-th/0601001`) fall
/// through.
fn looks_like_url(input: &str) -> bool {
    if input.contains("://") {
        return true;
    }
    let lower = input.to_ascii_lowercase();
    lower.starts_with("arxiv.org/") || lower.starts_with("www.arxiv.org/")
}

fn parse_url(input: &str) -> Result<String> {
    // `url::Url` requires a scheme. Add a default one for bare-host inputs
    // like `arxiv.org/abs/...` so the parser can do its job.
    let normalized = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };

    let url = Url::parse(&normalized).with_context(|| format!("Invalid URL: {input}"))?;

    if !matches!(url.scheme(), "http" | "https") {
        bail!(
            "Unsupported URL scheme '{}': expected http or https",
            url.scheme()
        );
    }

    // `host_str()` returns the percent-decoded, lowercased host.
    let host = url
        .host_str()
        .with_context(|| format!("URL has no host: {input}"))?;
    if host != "arxiv.org" && host != "www.arxiv.org" {
        bail!("Not an arXiv URL: host is '{host}'");
    }

    let mut segments = url
        .path_segments()
        .with_context(|| format!("URL has no path: {input}"))?
        .filter(|s| !s.is_empty());

    let kind = segments
        .next()
        .context("URL path is empty (expected /abs/, /pdf/, /e-print/, /html/, or /format/)")?;
    if !URL_PATH_PREFIXES.contains(&kind) {
        bail!(
            "Unsupported arXiv path '/{kind}/': expected one of {}",
            URL_PATH_PREFIXES.join(", ")
        );
    }

    // ID may span one or two segments (`2301.00001` vs `hep-th/0601001`).
    let rest: Vec<&str> = segments.collect();
    if rest.is_empty() {
        bail!("URL has no paper ID");
    }
    let mut candidate = rest.join("/");
    if let Some(stripped) = candidate.strip_suffix(".pdf") {
        candidate.truncate(stripped.len());
    }

    if !id_is_valid(&candidate) {
        bail!("'{candidate}' doesn't look like an arXiv ID");
    }
    Ok(candidate)
}

/// True if `s` matches one of the two arXiv ID shapes (with an optional
/// `vN` version suffix). New-style is `YYMM.NNNNN`, old-style is
/// `archive[.subject]/YYMMNNN`.
fn id_is_valid(s: &str) -> bool {
    // OnceLock would avoid recompiling, but parse_id is called once per run.
    let new_re = Regex::new(r"^\d{4}\.\d{4,5}(?:v\d+)?$").expect("valid regex");
    let old_re = Regex::new(r"^[a-z-]+(?:\.[A-Za-z-]+)?/\d{7}(?:v\d+)?$").expect("valid regex");
    new_re.is_match(s) || old_re.is_match(s)
}

/// Download and extract the arXiv e-print source into `dest`.
pub async fn download_source(arxiv_id: &str, dest: &Path) -> Result<()> {
    let url = format!("https://arxiv.org/e-print/{arxiv_id}");

    let client = reqwest::Client::builder()
        .user_agent("ratex/0.1 (academic paper translator)")
        .build()?;

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to connect to arXiv")?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("Paper '{arxiv_id}' not found on arXiv. Verify the paper ID.");
    }

    let response = response
        .error_for_status()
        .context("arXiv returned an error")?;

    let bytes = response
        .bytes()
        .await
        .context("Failed to download source from arXiv")?;

    // Decompress gzip
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .context("Failed to decompress source (may not be gzipped)")?;

    // Try to extract as tar archive
    if looks_like_tar(&decompressed) {
        let cursor = Cursor::new(&decompressed);
        let mut archive = Archive::new(cursor);
        archive
            .unpack(dest)
            .context("Failed to unpack arXiv tar source")?;
    } else {
        // Not a tar archive: arXiv sometimes returns a gzipped single .tex file.
        std::fs::write(dest.join("main.tex"), &decompressed)
            .context("Failed to write extracted .tex file")?;
    }

    Ok(())
}

fn looks_like_tar(bytes: &[u8]) -> bool {
    const BLOCK_SIZE: usize = 512;
    const CHECKSUM_START: usize = 148;
    const CHECKSUM_END: usize = 156;

    if bytes.len() < BLOCK_SIZE {
        return false;
    }

    let header = &bytes[..BLOCK_SIZE];
    if header.iter().all(|&b| b == 0) {
        return true;
    }

    let Some(stored_checksum) = parse_tar_checksum(&header[CHECKSUM_START..CHECKSUM_END]) else {
        return false;
    };

    // Checksum is computed as if the checksum field itself were ASCII spaces.
    let computed_checksum: u32 = header[..CHECKSUM_START]
        .iter()
        .chain(&header[CHECKSUM_END..])
        .chain(std::iter::repeat_n(&b' ', CHECKSUM_END - CHECKSUM_START))
        .map(|&b| u32::from(b))
        .sum();

    stored_checksum == computed_checksum
}

fn parse_tar_checksum(field: &[u8]) -> Option<u32> {
    let value = std::str::from_utf8(field).ok()?;
    let value = value.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if value.is_empty() {
        return None;
    }

    u32::from_str_radix(value, 8).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_abs_url() {
        assert_eq!(
            parse_id("https://arxiv.org/abs/2301.00001").unwrap(),
            "2301.00001"
        );
    }

    #[test]
    fn parses_pdf_url() {
        assert_eq!(
            parse_id("https://arxiv.org/pdf/2602.21340").unwrap(),
            "2602.21340"
        );
    }

    #[test]
    fn parses_pdf_url_with_pdf_extension() {
        assert_eq!(
            parse_id("https://arxiv.org/pdf/2602.21340.pdf").unwrap(),
            "2602.21340"
        );
    }

    #[test]
    fn parses_html_url_with_version() {
        assert_eq!(
            parse_id("https://arxiv.org/html/2510.26912v1").unwrap(),
            "2510.26912v1"
        );
    }

    #[test]
    fn parses_e_print_url() {
        assert_eq!(
            parse_id("https://arxiv.org/e-print/2301.00001v3").unwrap(),
            "2301.00001v3"
        );
    }

    #[test]
    fn parses_old_style_url() {
        assert_eq!(
            parse_id("https://arxiv.org/abs/hep-th/0601001v2").unwrap(),
            "hep-th/0601001v2"
        );
    }

    #[test]
    fn parses_old_style_with_subject_class() {
        assert_eq!(
            parse_id("https://arxiv.org/abs/math.GT/0309136").unwrap(),
            "math.GT/0309136"
        );
        assert_eq!(
            parse_id("https://arxiv.org/abs/cond-mat.mes-hall/0601001v2").unwrap(),
            "cond-mat.mes-hall/0601001v2"
        );
        assert_eq!(parse_id("math.GT/0309136").unwrap(), "math.GT/0309136");
    }

    #[test]
    fn rejects_id_with_trailing_junk() {
        // Without the boundary anchor, these would silently truncate to
        // 2602.21340 / 2301.00001.
        assert!(parse_id("https://arxiv.org/pdf/2602.21340vfoo").is_err());
        assert!(parse_id("https://arxiv.org/abs/2301.00001extra").is_err());
    }

    #[test]
    fn url_with_query_or_fragment_still_parses() {
        assert_eq!(
            parse_id("https://arxiv.org/abs/2301.00001?context=foo").unwrap(),
            "2301.00001"
        );
        assert_eq!(
            parse_id("https://arxiv.org/html/2510.26912v1#section.2").unwrap(),
            "2510.26912v1"
        );
    }

    #[test]
    fn parses_bare_new_style_id() {
        assert_eq!(parse_id("2510.26912").unwrap(), "2510.26912");
        assert_eq!(parse_id("2510.26912v1").unwrap(), "2510.26912v1");
    }

    #[test]
    fn parses_bare_old_style_id() {
        assert_eq!(parse_id("hep-th/0601001").unwrap(), "hep-th/0601001");
    }

    #[test]
    fn url_with_trailing_slash() {
        assert_eq!(
            parse_id("https://arxiv.org/html/2510.26912v1/").unwrap(),
            "2510.26912v1"
        );
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(parse_id("not-an-arxiv-id").is_err());
        assert!(parse_id("https://example.com/abs/2301.00001").is_err());
    }

    #[test]
    fn rejects_look_alike_hosts() {
        assert!(parse_id("https://notarxiv.org/abs/2301.00001").is_err());
        assert!(parse_id("xhttps://arxiv.org/abs/2301.00001").is_err());
        assert!(parse_id("evil.com/arxiv.org/abs/2301.00001").is_err());
    }

    #[test]
    fn accepts_protocol_and_www_variants() {
        assert_eq!(
            parse_id("http://arxiv.org/abs/2301.00001").unwrap(),
            "2301.00001"
        );
        assert_eq!(
            parse_id("https://www.arxiv.org/abs/2301.00001").unwrap(),
            "2301.00001"
        );
        assert_eq!(parse_id("arxiv.org/abs/2301.00001").unwrap(), "2301.00001");
    }

    #[test]
    fn scheme_and_host_are_case_insensitive() {
        // `arXiv.org` is a real-world citation form.
        assert_eq!(
            parse_id("https://arXiv.org/abs/2301.00001").unwrap(),
            "2301.00001"
        );
        assert_eq!(
            parse_id("HTTP://ARXIV.ORG/abs/2301.00001").unwrap(),
            "2301.00001"
        );
        assert_eq!(
            parse_id("https://WWW.arxiv.org/html/2510.26912v1").unwrap(),
            "2510.26912v1"
        );
    }

    #[test]
    fn plain_tex_is_not_detected_as_tar() {
        let tex =
            "\\documentclass{article}\n\\begin{document}\nhello\n\\end{document}\n".repeat(50);
        assert!(!looks_like_tar(tex.as_bytes()));
    }

    #[test]
    fn valid_tar_header_is_detected() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        let content = b"hello";
        header.set_size(content.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "main.tex", &content[..])
            .unwrap();
        let archive = builder.into_inner().unwrap();

        assert!(looks_like_tar(&archive));
    }
}
