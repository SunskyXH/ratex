use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use regex::Regex;
use std::io::{Cursor, Read};
use std::path::Path;
use tar::Archive;

/// Parse an arXiv paper ID from a URL or bare ID.
///
/// Accepts:
/// - Full URLs: <https://arxiv.org/abs/2301.00001>, <https://arxiv.org/pdf/2301.00001>
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
        "Cannot parse arXiv ID from '{input}'. Expected a URL like https://arxiv.org/abs/2301.00001 or a bare ID like 2301.00001"
    )
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

    let computed_checksum = header
        .iter()
        .enumerate()
        .map(|(idx, byte)| {
            if (CHECKSUM_START..CHECKSUM_END).contains(&idx) {
                u32::from(b' ')
            } else {
                u32::from(*byte)
            }
        })
        .sum::<u32>();

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
