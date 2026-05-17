use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which TeX compiler to use.
enum Compiler {
    Tectonic,
    Xelatex,
}

/// Detect the best available compiler: tectonic first, then xelatex.
fn detect_compiler() -> Result<Compiler> {
    // Prefer tectonic: Rust-based, auto-downloads packages, handles multi-pass
    if Command::new("tectonic")
        .arg("--help")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return Ok(Compiler::Tectonic);
    }

    if Command::new("xelatex")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return Ok(Compiler::Xelatex);
    }

    bail!(
        "No TeX compiler found. Install one of:\n\
         \n\
         - tectonic (recommended, auto-downloads packages):\n\
         \x20\x20 brew install tectonic\n\
         \x20\x20 cargo install tectonic\n\
         \n\
         - TeX Live / MacTeX (full distribution):\n\
         \x20\x20 brew install --cask mactex"
    )
}

/// Returns Ok(()) if a TeX compiler is available, Err with install instructions otherwise.
pub fn check_available() -> Result<()> {
    detect_compiler().map(|_| ())
}

/// Compile the translated LaTeX to PDF.
/// Returns the path to the generated PDF.
pub fn compile(main_tex: &Path) -> Result<PathBuf> {
    let compiler = detect_compiler()?;

    match compiler {
        Compiler::Tectonic => compile_tectonic(main_tex),
        Compiler::Xelatex => compile_xelatex(main_tex),
    }
}

// ─── Tectonic ────────────────────────────────────────────────────────────────

fn compile_tectonic(main_tex: &Path) -> Result<PathBuf> {
    let work_dir = main_tex
        .parent()
        .context("Cannot determine working directory")?;
    let tex_filename = main_tex
        .file_name()
        .context("Invalid tex file path")?
        .to_string_lossy();
    let stem = main_tex
        .file_stem()
        .context("Invalid tex file path")?
        .to_string_lossy();

    eprintln!("  Using tectonic (auto-downloads missing packages)...");

    let output = Command::new("tectonic")
        .arg(&*tex_filename)
        .current_dir(work_dir)
        .output()
        .context("Failed to run tectonic")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let error_output = tex_error_summary(&stdout, &stderr, 15);

        if !error_output.is_empty() {
            bail!("tectonic compilation failed:\n{error_output}");
        }
        let stdout = truncate(&stdout, 2000);
        let stderr = truncate(&stderr, 2000);
        bail!("tectonic compilation failed:\n{stdout}\n{stderr}");
    }

    let pdf_path = work_dir.join(format!("{stem}.pdf"));
    if !pdf_path.exists() {
        bail!("tectonic ran successfully but no PDF was generated.");
    }

    Ok(pdf_path)
}

// ─── XeLaTeX ─────────────────────────────────────────────────────────────────

fn compile_xelatex(main_tex: &Path) -> Result<PathBuf> {
    let work_dir = main_tex
        .parent()
        .context("Cannot determine working directory")?;
    let tex_filename = main_tex
        .file_name()
        .context("Invalid tex file path")?
        .to_string_lossy();
    let stem = main_tex
        .file_stem()
        .context("Invalid tex file path")?
        .to_string_lossy();

    eprintln!("  Using xelatex (requires full TeX Live installation)...");

    // First xelatex pass
    eprintln!("  [1/3] Running xelatex (first pass)...");
    run_xelatex(work_dir, &tex_filename)?;

    // Run bibtex if .aux contains citations
    let aux_path = work_dir.join(format!("{stem}.aux"));
    let needs_bibtex = if aux_path.exists() {
        let aux_content = std::fs::read_to_string(&aux_path).unwrap_or_default();
        aux_content.contains("\\citation") || aux_content.contains("\\bibdata")
    } else {
        false
    };

    if needs_bibtex {
        eprintln!("  [bibtex] Running bibtex...");
        let output = Command::new("bibtex")
            .arg(stem.as_ref())
            .current_dir(work_dir)
            .output()
            .context("Failed to run bibtex")?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = truncate(&stdout, 2000);
            let stderr = truncate(&stderr, 2000);
            bail!("bibtex failed:\n{stdout}\n{stderr}");
        }
    }

    // Second + third xelatex passes
    eprintln!("  [2/3] Running xelatex (second pass)...");
    run_xelatex(work_dir, &tex_filename)?;

    eprintln!("  [3/3] Running xelatex (third pass)...");
    run_xelatex(work_dir, &tex_filename)?;

    let pdf_path = work_dir.join(format!("{stem}.pdf"));
    if !pdf_path.exists() {
        let log_path = work_dir.join(format!("{stem}.log"));
        if log_path.exists() {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            let last_lines: String = log.lines().rev().take(30).collect::<Vec<_>>().join("\n");
            bail!("PDF was not generated. Last lines of xelatex log:\n{last_lines}");
        }
        bail!("PDF was not generated and no log file found.");
    }

    Ok(pdf_path)
}

fn run_xelatex(work_dir: &Path, tex_filename: &str) -> Result<()> {
    let output = Command::new("xelatex")
        .args(["-interaction=nonstopmode", "-halt-on-error", tex_filename])
        .current_dir(work_dir)
        .output()
        .context("Failed to run xelatex")?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let error_output = tex_error_summary(&stdout, &stderr, 10);
        if !error_output.is_empty() {
            bail!("xelatex failed:\n{error_output}");
        }
        let stdout = truncate(&stdout, 2000);
        let stderr = truncate(&stderr, 2000);
        bail!("xelatex failed:\n{stdout}\n{stderr}");
    }

    Ok(())
}

fn tex_error_summary(stdout: &str, stderr: &str, max_lines: usize) -> String {
    stderr
        .lines()
        .chain(stdout.lines())
        .filter(|line| line.starts_with('!') || line.contains("Error") || line.contains("error"))
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}
