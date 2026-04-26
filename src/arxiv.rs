use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use regex::Regex;
use std::io::Read;
use std::path::Path;
use tar::Archive;

/// Parse an arXiv paper ID from a URL or bare ID.
///
/// Accepts:
/// - Full URLs: https://arxiv.org/abs/2301.00001, https://arxiv.org/pdf/2301.00001
/// - New-style IDs: 2301.00001, 2301.00001v2
/// - Old-style IDs: hep-th/0601001, hep-th/0601001v2
pub fn parse_id(input: &str) -> Result<String> {
    let input = input.trim().trim_end_matches('/');

    // Handle full URLs
    let url_re = Regex::new(r"arxiv\.org/(?:abs|pdf|e-print)/([^\s?#]+)")?;
    if let Some(caps) = url_re.captures(input) {
        return Ok(caps[1].to_string());
    }

    // Handle bare new-style IDs: 2301.00001 or 2301.00001v2
    let new_re = Regex::new(r"^(\d{4}\.\d{4,5})(v\d+)?$")?;
    if new_re.is_match(input) {
        return Ok(input.to_string());
    }

    // Handle bare old-style IDs: hep-th/0601001 or hep-th/0601001v2
    let old_re = Regex::new(r"^([a-z-]+/\d{7})(v\d+)?$")?;
    if old_re.is_match(input) {
        return Ok(input.to_string());
    }

    bail!(
        "Cannot parse arXiv ID from '{}'. Expected a URL like https://arxiv.org/abs/2301.00001 or a bare ID like 2301.00001",
        input
    )
}

/// Download and extract the arXiv e-print source into `dest`.
pub async fn download_source(arxiv_id: &str, dest: &Path) -> Result<()> {
    let url = format!("https://arxiv.org/e-print/{}", arxiv_id);

    let client = reqwest::Client::builder()
        .user_agent("ratex/0.1 (academic paper translator)")
        .build()?;

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to connect to arXiv")?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "Paper '{}' not found on arXiv. Verify the paper ID.",
            arxiv_id
        );
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
    let cursor = std::io::Cursor::new(&decompressed);
    let mut archive = Archive::new(cursor);

    match archive.unpack(dest) {
        Ok(_) => Ok(()),
        Err(_) => {
            // Not a tar archive — treat as a single .tex file
            std::fs::write(dest.join("main.tex"), &decompressed)
                .context("Failed to write extracted .tex file")?;
            Ok(())
        }
    }
}
