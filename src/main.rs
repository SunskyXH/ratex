mod arxiv;
mod compiler;
mod config;
mod latex;
mod translator;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Parser)]
#[command(
    name = "ratex",
    version,
    about = "Translate arXiv papers from English to Chinese"
)]
struct Cli {
    /// arXiv paper URL or ID (e.g., https://arxiv.org/abs/2301.00001 or 2301.00001)
    url: String,

    /// Path to config file (default: ~/.config/ratex/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Use a named profile from the config file
    #[arg(long, conflicts_with = "provider")]
    profile: Option<String>,

    /// LLM protocol [deprecated: use --profile or a config file]
    #[arg(long, value_parser = ["openai", "gemini"])]
    provider: Option<String>,

    /// API key (overrides profile's api_key_env)
    #[arg(long)]
    api_key: Option<String>,

    /// Model name (overrides profile's model)
    #[arg(long, short)]
    model: Option<String>,

    /// API base URL (overrides profile's endpoint)
    #[arg(long)]
    base_url: Option<String>,

    /// Output file path (default: {paper_id}_zh.pdf)
    #[arg(long, short)]
    output: Option<PathBuf>,

    /// Skip PDF compilation, output translated .tex only
    #[arg(long)]
    no_compile: bool,

    /// Max concurrent translation requests (overrides profile/config)
    #[arg(long)]
    concurrency: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file (silently ignore if not found)
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // 1. Parse arXiv ID
    let arxiv_id = arxiv::parse_id(&cli.url)?;
    eprintln!("[1/5] Paper ID: {}", arxiv_id);

    // 2. Load config, resolve profile, create provider
    let config_file = match cli.config.as_ref() {
        Some(path) => Some(config::load_required(path)?),
        None => config::load_optional(&config::default_config_path()?)?,
    };
    let resolved = config::resolve(
        config_file.as_ref(),
        config::ResolveInputs {
            profile: cli.profile.as_deref(),
            provider: cli.provider.as_deref(),
            model: cli.model.as_deref(),
            base_url: cli.base_url.as_deref(),
            api_key: cli.api_key.as_deref(),
            concurrency: cli.concurrency,
        },
    )?;
    let provider = Arc::new(translator::Provider::new(&resolved));
    let semaphore = Arc::new(Semaphore::new(resolved.concurrency));
    eprintln!(
        "[2/5] LLM: {} (model: {}, concurrency: {})",
        resolved.protocol.as_str(),
        resolved.model,
        resolved.concurrency,
    );

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

    let total_files = tex_files.len();
    eprintln!("  Translating ({} files in parallel)...", total_files);

    let main_tex_arc = Arc::new(main_tex.clone());
    let mut file_set: JoinSet<Result<Option<String>>> = JoinSet::new();
    for tex_file in tex_files.clone() {
        let provider = Arc::clone(&provider);
        let semaphore = Arc::clone(&semaphore);
        let main_tex = Arc::clone(&main_tex_arc);
        file_set.spawn(async move {
            let filename = tex_file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            let content = match std::fs::read_to_string(&tex_file) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("  Skipping {} (cannot read: {})", filename, e);
                    return Ok(None);
                }
            };
            if content.trim().is_empty() {
                return Ok(None);
            }

            let is_main = tex_file == *main_tex;
            let label = if is_main {
                format!("{} (main)", filename)
            } else {
                filename.clone()
            };
            let translated =
                latex::translate_tex_file(&content, is_main, &provider, &semaphore, &label).await?;
            std::fs::write(&tex_file, translated)
                .with_context(|| format!("Failed to write translated {}", filename))?;
            Ok(Some(filename))
        });
    }

    let mut completed = 0usize;
    while let Some(joined) = file_set.join_next().await {
        match joined {
            Ok(Ok(Some(filename))) => {
                completed += 1;
                eprintln!("  [{}/{}] {}", completed, total_files, filename);
            }
            Ok(Ok(None)) => {
                completed += 1;
            }
            Ok(Err(e)) => {
                file_set.abort_all();
                return Err(e);
            }
            Err(e) => {
                file_set.abort_all();
                return Err(anyhow!("file translation task panicked: {}", e));
            }
        }
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
        // Patch the main tex if its \bibliography{} points at a .bib that
        // isn't in the source archive — without this tectonic clobbers any
        // pre-generated .bbl when it tries to run bibtex.
        match latex::inline_missing_bibliography(&main_tex) {
            Ok(true) => eprintln!(
                "  Inlined pre-generated .bbl (no .bib in source) so bibtex won't clobber it."
            ),
            Ok(false) => {}
            Err(e) => eprintln!("  Warning: bibliography pre-check failed: {}", e),
        }

        eprintln!("[5/5] Compiling PDF with xelatex...");
        match compiler::compile(&main_tex) {
            Ok(pdf_path) => {
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
            Err(compile_err) => {
                // Compilation failed — preserve the translated source so the
                // user can recompile manually without re-paying for translation.
                let fallback_dir = PathBuf::from(format!("{}_zh_tex", sanitized_id));
                if let Err(save_err) = copy_dir_recursive(work_dir.path(), &fallback_dir) {
                    eprintln!(
                        "  Warning: also failed to save translated .tex to {}: {}",
                        fallback_dir.display(),
                        save_err,
                    );
                } else {
                    let main_name = main_tex
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "<main>.tex".into());
                    eprintln!();
                    eprintln!(
                        "  Translated .tex saved to: {} (so you don't have to re-translate)",
                        fallback_dir.display(),
                    );
                    eprintln!(
                        "  After fixing the source you can recompile manually, e.g.:"
                    );
                    eprintln!("    cd {} && tectonic {}", fallback_dir.display(), main_name);
                }
                return Err(compile_err);
            }
        }
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
