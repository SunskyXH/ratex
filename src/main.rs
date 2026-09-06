mod arxiv;
mod compiler;
mod config;
mod latex;
mod translator;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

use config::ResolveInputs;

#[derive(Parser)]
#[command(
    name = "ratex",
    version,
    about = "Translate arXiv papers from English to Chinese"
)]
struct Cli {
    /// arXiv paper URL or ID (e.g., <https://arxiv.org/abs/2301.00001> or 2301.00001)
    #[arg(
        required_unless_present = "compile_only",
        conflicts_with = "compile_only"
    )]
    url: Option<String>,

    /// Compile an existing translated source directory, without calling an LLM
    #[arg(long, conflicts_with = "no_compile")]
    compile_only: Option<PathBuf>,

    /// Directory to keep source and completed translations (default: {paper_id}_zh_tex)
    #[arg(long, conflicts_with = "compile_only")]
    source_dir: Option<PathBuf>,

    /// TeX backend to use
    #[arg(long, value_enum, default_value = "auto")]
    compiler: compiler::Compiler,

    /// Path to config file (default: ~/.config/ratex/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Use a named profile from the config file
    #[arg(long)]
    profile: Option<String>,

    /// API key (overrides profile's `api_key_env`)
    #[arg(long)]
    api_key: Option<String>,

    /// Model name (overrides profile's model)
    #[arg(long, short)]
    model: Option<String>,

    /// API base URL (overrides profile's endpoint)
    #[arg(long)]
    base_url: Option<String>,

    /// Output file path (default: `{paper_id}_zh.pdf`)
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
    run(cli).await
}

async fn run(cli: Cli) -> Result<()> {
    if let Some(source_dir) = &cli.compile_only {
        let source_dir = source_dir
            .canonicalize()
            .context("Cannot open source directory")?;
        if std::fs::symlink_metadata(source_dir.join(".ratex-incomplete")).is_ok() {
            bail!(
                "Translation is incomplete in {}. Recover the saved .ratex-chunks files and remove .ratex-incomplete after finishing the source.",
                source_dir.display()
            );
        }
        let main_tex = latex::find_main_tex(&latex::find_tex_files(&source_dir)?)?;
        let compiler = compiler::check_available(cli.compiler)?;
        let name = source_dir
            .file_name()
            .context("Source directory must have a name")?
            .to_string_lossy();
        let default_output = source_dir.with_file_name(format!(
            "{}.pdf",
            name.strip_suffix("_tex").unwrap_or(&name)
        ));
        return compile_pdf(
            &source_dir,
            &main_tex,
            cli.output.as_deref().unwrap_or(&default_output),
            compiler,
        );
    }

    let Cli {
        url,
        config,
        profile,
        api_key,
        model,
        base_url,
        output,
        no_compile,
        concurrency,
        source_dir,
        compiler,
        ..
    } = cli;

    // 1. Parse arXiv ID
    let arxiv_id = arxiv::parse_id(url.as_deref().context("An arXiv ID is required")?)?;
    let sanitized_id = arxiv_id.replace('/', "_");
    if no_compile && output.is_some() && source_dir.is_some() {
        bail!(
            "With --no-compile, choose either --output or --source-dir for the source directory."
        );
    }
    let source_dir = source_dir
        .or_else(|| output.clone().filter(|_| no_compile))
        .unwrap_or_else(|| PathBuf::from(format!("{sanitized_id}_zh_tex")));
    eprintln!("[1/5] Paper ID: {arxiv_id}");

    // 2. Load config, resolve profile, create provider
    let resolved = resolve_profile(
        config.as_deref(),
        ResolveInputs {
            profile,
            model,
            base_url,
            api_key,
            concurrency,
        },
    )?;
    let provider = Arc::new(translator::Provider::new(&resolved));
    let semaphore = Arc::new(Semaphore::new(resolved.concurrency));
    let compiler = if no_compile {
        None
    } else {
        match compiler::check_available(compiler) {
            Ok(compiler) => Some(compiler),
            Err(e) => {
                eprintln!(
                    "{e:#}\nContinuing with source output only. Use --compile-only after fixing the TeX environment."
                );
                None
            }
        }
    };
    eprintln!(
        "[2/5] LLM: {} (model: {}, concurrency: {})",
        resolved.protocol.as_str(),
        resolved.model,
        resolved.concurrency,
    );

    // 3. Download and extract source
    // Own a new persistent directory before spending on translation. Never merge
    // a new download into an existing result or a user's manually fixed source.
    create_source_dir(&source_dir)?;
    let source_dir = source_dir.canonicalize()?;
    eprintln!("[3/5] Downloading source from arXiv...");
    eprintln!(
        "  Source and completed work will remain in: {}",
        source_dir.display()
    );
    let incomplete = source_dir.join(".ratex-incomplete");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&incomplete)?
        .write_all(
            b"Translation has not completed. Completed chunks are in *.ratex-chunks directories.\n",
        )?;
    arxiv::download_source(&arxiv_id, &source_dir).await?;

    // 4. Find and translate .tex files
    let tex_files = latex::find_tex_files(&source_dir)?;
    let main_tex = latex::find_main_tex(&tex_files)?;
    eprintln!(
        "[4/5] Found {} .tex file(s), main: {}",
        tex_files.len(),
        main_tex.file_name().unwrap_or_default().to_string_lossy()
    );

    latex::translate_all(tex_files, &main_tex, provider, semaphore)
        .await
        .with_context(|| {
            format!(
                "Translation failed. Source and completed chunks remain in {}",
                source_dir.display()
            )
        })?;
    std::fs::remove_file(incomplete)?;
    eprintln!(
        "  Translation complete! Source kept in: {}",
        source_dir.display()
    );

    // 5. Compile or copy output
    let Some(compiler) = compiler else {
        return Ok(());
    };

    let output = output.unwrap_or_else(|| PathBuf::from(format!("{sanitized_id}_zh.pdf")));
    compile_pdf(&source_dir, &main_tex, &output, compiler)
}

fn resolve_profile(
    config_path: Option<&Path>,
    inputs: ResolveInputs,
) -> Result<config::ResolvedProfile> {
    let config_file = match config_path {
        Some(path) => Some(config::load_required(path)?),
        None => config::load_optional(&config::default_config_path()?)?,
    };

    config::resolve(config_file.as_ref(), inputs)
}

fn create_source_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(path).with_context(|| {
        format!("Cannot create source directory {}. Existing results are never overwritten; choose a new --source-dir, or use --compile-only to rebuild existing translations.", path.display())
    })
}

fn compile_pdf(
    source_dir: &Path,
    main_tex: &Path,
    output: &Path,
    compiler: compiler::Compiler,
) -> Result<()> {
    eprintln!("[5/5] Compiling PDF...");
    let pdf = compiler::compile(source_dir, main_tex, compiler).with_context(|| {
        format!(
            "Compilation failed. Source remains in {}. Retry with --compile-only.",
            source_dir.display()
        )
    })?;
    export_pdf(&pdf, output)?;
    eprintln!("Output: {}", output.display());
    Ok(())
}

fn export_pdf(pdf: &Path, output: &Path) -> Result<()> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Replace only after the complete copy succeeds; the compiled PDF remains
    // in the persistent source directory even if export fails.
    (|| -> Result<()> {
        std::fs::create_dir_all(parent)?;
        let staged = tempfile::NamedTempFile::new_in(parent)?;
        std::fs::copy(pdf, staged.path())?;
        staged.persist(output)?;
        Ok(())
    })()
    .with_context(|| {
        format!(
            "Failed to export PDF to {}. The compiled PDF is kept at {}",
            output.display(),
            pdf.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_supports_compile_without_profile_or_paper() {
        let cli = Cli::try_parse_from(["ratex", "--compile-only", "paper_zh_tex"]).unwrap();
        assert!(cli.url.is_none());
        assert!(Cli::try_parse_from(["ratex"]).is_err());
        assert!(Cli::try_parse_from(["ratex", "2406.06608", "--compile-only", "paper"]).is_err());
        assert!(Cli::try_parse_from(["ratex", "--compile-only", "paper", "--no-compile"]).is_err());
    }

    #[test]
    fn persistent_source_and_pdf_survive_export_failure() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        create_source_dir(&source).unwrap();
        let tex = source.join("main.tex");
        let pdf = source.join("main.pdf");
        std::fs::write(&tex, "translated source").unwrap();
        std::fs::write(&pdf, b"%PDF-1.4 test").unwrap();
        assert!(create_source_dir(&source).is_err());
        let blocked_parent = temp.path().join("file");
        std::fs::write(&blocked_parent, "keep me").unwrap();
        assert!(export_pdf(&pdf, &blocked_parent.join("out.pdf")).is_err());
        assert_eq!(std::fs::read_to_string(&tex).unwrap(), "translated source");
        assert_eq!(std::fs::read(&pdf).unwrap(), b"%PDF-1.4 test");
        assert_eq!(std::fs::read_to_string(&blocked_parent).unwrap(), "keep me");
        let output = temp.path().join("nested/out.pdf");
        export_pdf(&pdf, &output).unwrap();
        assert_eq!(std::fs::read(output).unwrap(), b"%PDF-1.4 test");
    }

    #[tokio::test]
    async fn compile_only_refuses_incomplete_translation() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join(".ratex-incomplete"), "incomplete").unwrap();
        let cli = Cli::try_parse_from(["ratex", "--compile-only", source.path().to_str().unwrap()])
            .unwrap();
        assert!(
            run(cli)
                .await
                .unwrap_err()
                .to_string()
                .contains("Translation is incomplete")
        );
    }
}
