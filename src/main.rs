mod arxiv;
mod compiler;
mod config;
mod latex;
mod translator;
mod utils;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;

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
    #[arg(long)]
    profile: Option<String>,

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

    // Pre-flight: detect the compiler now, before translation, so a missing CLI
    // doesn't waste the LLM bill. Missing → save translated .tex for manual compile.
    let no_compile = cli.no_compile || {
        match compiler::check_available() {
            Ok(()) => false,
            Err(e) => {
                eprintln!("{}", e);
                eprintln!();
                eprintln!("Continuing in --no-compile mode (translated .tex will be saved).");
                eprintln!("You can upload it to Overleaf, or install a compiler and rerun.");
                eprintln!();
                true
            }
        }
    };

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
            profile: cli.profile,
            model: cli.model,
            base_url: cli.base_url,
            api_key: cli.api_key,
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

    latex::translate_all(&tex_files, &main_tex, provider, semaphore).await?;
    eprintln!("  Translation complete!");

    // 5. Compile or copy output
    let sanitized_id = arxiv_id.replace('/', "_");

    if no_compile {
        let output_dir = cli
            .output
            .unwrap_or_else(|| PathBuf::from(format!("{}_zh_tex", sanitized_id)));
        utils::copy_dir_recursive(work_dir.path(), &output_dir)?;
        eprintln!(
            "[5/5] Translated .tex files saved to: {}",
            output_dir.display()
        );
        return Ok(());
    }

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
    let compile_err = match compiler::compile(&main_tex) {
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
            return Ok(());
        }
        Err(e) => e,
    };

    // Compilation failed — preserve the translated source so the
    // user can recompile manually without re-paying for translation.
    let fallback_dir = PathBuf::from(format!("{}_zh_tex", sanitized_id));
    if let Err(save_err) = utils::copy_dir_recursive(work_dir.path(), &fallback_dir) {
        eprintln!(
            "  Warning: also failed to save translated .tex to {}: {}",
            fallback_dir.display(),
            save_err,
        );
        return Err(compile_err);
    }

    let main_name = main_tex
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<main>.tex".into());
    eprintln!();
    eprintln!(
        "  Translated .tex saved to: {} (so you don't have to re-translate)",
        fallback_dir.display(),
    );
    eprintln!("  After fixing the source you can recompile manually, e.g.:");
    eprintln!(
        "    cd {} && tectonic {}",
        fallback_dir.display(),
        main_name
    );
    Err(compile_err)
}
