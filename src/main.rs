mod arxiv;
mod compiler;
mod latex;
mod translator;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "ratex",
    version,
    about = "Translate arXiv papers from English to Chinese"
)]
struct Cli {
    /// arXiv paper URL or ID (e.g., https://arxiv.org/abs/2301.00001 or 2301.00001)
    url: String,

    /// LLM provider
    #[arg(long, default_value = "gemini", value_parser = ["openai", "gemini"])]
    provider: String,

    /// API key (overrides OPENAI_API_KEY / GEMINI_API_KEY env var)
    #[arg(long)]
    api_key: Option<String>,

    /// Model name (default: gemini-3-flash-preview for gemini, gpt-4o for openai)
    #[arg(long, short)]
    model: Option<String>,

    /// API base URL (overrides default endpoint)
    #[arg(long)]
    base_url: Option<String>,

    /// Output file path (default: {paper_id}_zh.pdf)
    #[arg(long, short)]
    output: Option<PathBuf>,

    /// Skip PDF compilation, output translated .tex only
    #[arg(long)]
    no_compile: bool,
}

impl Cli {
    fn resolve_api_key(&self) -> Result<String> {
        if let Some(ref key) = self.api_key {
            return Ok(key.clone());
        }

        let env_var = match self.provider.as_str() {
            "openai" => "OPENAI_API_KEY",
            "gemini" => "GEMINI_API_KEY",
            _ => unreachable!(),
        };

        std::env::var(env_var).with_context(|| {
            format!(
                "No API key provided. Use --api-key or set the {} environment variable.",
                env_var
            )
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file (silently ignore if not found)
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // 1. Parse arXiv ID
    let arxiv_id = arxiv::parse_id(&cli.url)?;
    eprintln!("[1/5] Paper ID: {}", arxiv_id);

    // 2. Resolve API key and create provider
    let api_key = cli.resolve_api_key()?;
    let provider = translator::Provider::new(
        &cli.provider,
        &api_key,
        cli.model.as_deref(),
        cli.base_url.as_deref(),
    )?;
    eprintln!("[2/5] LLM provider: {}", cli.provider);

    // 3. Download and extract source
    let work_dir = tempfile::tempdir().context("Failed to create temp directory")?;
    eprintln!("[3/5] Downloading source from arXiv...");
    arxiv::download_source(&arxiv_id, work_dir.path()).await?;
    eprintln!("  Source extracted to: {}", work_dir.path().display());

    // 4. Find and translate .tex files
    let tex_files = latex::find_tex_files(work_dir.path())?;
    let main_tex = latex::find_main_tex(&tex_files)?;
    eprintln!(
        "[4/5] Found {} .tex file(s), main: {}",
        tex_files.len(),
        main_tex.file_name().unwrap_or_default().to_string_lossy()
    );

    eprintln!("  Translating...");
    for tex_file in &tex_files {
        let filename = tex_file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let content = match std::fs::read_to_string(tex_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  Skipping {} (cannot read: {})", filename, e);
                continue;
            }
        };

        if content.trim().is_empty() {
            continue;
        }

        let is_main = *tex_file == main_tex;
        eprintln!("  Processing: {}{}", filename, if is_main { " (main)" } else { "" });

        let translated = latex::translate_tex_file(&content, is_main, &provider).await?;
        std::fs::write(tex_file, translated)
            .with_context(|| format!("Failed to write translated {}", filename))?;
    }
    eprintln!("  Translation complete!");

    // 5. Compile or copy output
    let sanitized_id = arxiv_id.replace('/', "_");

    if cli.no_compile {
        let output_dir = cli
            .output
            .unwrap_or_else(|| PathBuf::from(format!("{}_zh_tex", sanitized_id)));
        copy_dir_recursive(work_dir.path(), &output_dir)?;
        eprintln!("[5/5] Translated .tex files saved to: {}", output_dir.display());
    } else {
        eprintln!("[5/5] Compiling PDF with xelatex...");
        let pdf_path = compiler::compile(&main_tex)?;

        let output_path = cli
            .output
            .unwrap_or_else(|| PathBuf::from(format!("{}_zh.pdf", sanitized_id)));
        std::fs::copy(&pdf_path, &output_path).with_context(|| {
            format!(
                "Failed to copy PDF from {} to {}",
                pdf_path.display(),
                output_path.display()
            )
        })?;
        eprintln!("Output: {}", output_path.display());
    }

    Ok(())
}

/// Recursively copy all files and directories from `src` to `dst`.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else {
            std::fs::copy(&path, &dest_path)?;
        }
    }
    Ok(())
}
