use anyhow::{bail, Context, Result};
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
        // Show last meaningful error lines
        let error_output: String = stderr
            .lines()
            .chain(stdout.lines())
            .filter(|l| l.contains("error") || l.contains("Error") || l.starts_with('!'))
            .take(15)
            .collect::<Vec<_>>()
            .join("\n");

        if !error_output.is_empty() {
            bail!("tectonic compilation failed:\n{}", error_output);
        }
        bail!(
            "tectonic compilation failed:\n{}\n{}",
            stdout.chars().take(2000).collect::<String>(),
            stderr.chars().take(2000).collect::<String>()
        );
    }

    let pdf_path = work_dir.join(format!("{}.pdf", stem));
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
    let aux_path = work_dir.join(format!("{}.aux", stem));
    let needs_bibtex = if aux_path.exists() {
        let aux_content = std::fs::read_to_string(&aux_path).unwrap_or_default();
        aux_content.contains("\\citation") || aux_content.contains("\\bibdata")
    } else {
        false
    };

    if needs_bibtex {
        eprintln!("  [bibtex] Running bibtex...");
        let _ = Command::new("bibtex")
            .arg(stem.as_ref())
            .current_dir(work_dir)
            .output();
    }

    // Second + third xelatex passes
    eprintln!("  [2/3] Running xelatex (second pass)...");
    run_xelatex(work_dir, &tex_filename)?;

    eprintln!("  [3/3] Running xelatex (third pass)...");
    run_xelatex(work_dir, &tex_filename)?;

    let pdf_path = work_dir.join(format!("{}.pdf", stem));
    if !pdf_path.exists() {
        let log_path = work_dir.join(format!("{}.log", stem));
        if log_path.exists() {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            let last_lines: String = log.lines().rev().take(30).collect::<Vec<_>>().join("\n");
            bail!(
                "PDF was not generated. Last lines of xelatex log:\n{}",
                last_lines
            );
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
        let error_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with('!') || l.contains("Error"))
            .take(10)
            .collect();

        if !error_lines.is_empty() {
            eprintln!("  xelatex errors:");
            for line in &error_lines {
                eprintln!("    {}", line);
            }
        }
    }

    Ok(())
}
