use anyhow::{Context, Result, bail};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// Versioned format-33 bundle verified for Tectonic 0.17.0; downloaded on demand.
const TECTONIC_BUNDLE: &str = "https://data1b.fullyjustified.net/tlextras-2022.0r0.tar";
const COMPILE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compiler {
    #[default]
    Auto,
    Tectonic,
    Latexmk,
}

/// Resolve the backend and compile the fonts/packages translation actually uses.
pub fn check_available(requested: Compiler) -> Result<Compiler> {
    let candidates: &[Compiler] = match requested {
        Compiler::Auto => &[Compiler::Tectonic, Compiler::Latexmk],
        Compiler::Tectonic => &[Compiler::Tectonic],
        Compiler::Latexmk => &[Compiler::Latexmk],
    };
    let mut errors = Vec::new();
    for &compiler in candidates {
        let dir = tempfile::Builder::new()
            .prefix("ratex-preflight-")
            .tempdir()?;
        let main = dir.path().join("ratex-preflight.tex");
        std::fs::write(
            &main,
            crate::latex::add_cjk_support(
                "\\documentclass{article}\n\\begin{document}\n中文 {\\sffamily 中文} {\\ttfamily 中文}\n\\end{document}\n",
            ),
        )?;
        match compile(dir.path(), &main, compiler) {
            Ok(_) => return Ok(compiler),
            Err(error) => {
                let logs = dir.keep();
                errors.push(format!(
                    "{compiler:?}: {error:#} (preflight: {})",
                    logs.display()
                ));
            }
        }
    }
    bail!(
        "No working Chinese TeX compiler. Install Tectonic on PATH, or TeX Live/MacTeX with latexmk, XeLaTeX, xeCJK and Fandol.\n{}",
        errors.join("\n")
    )
}

/// Outputs and complete logs stay in a fresh build directory inside the source tree.
pub fn compile(source_root: &Path, main_tex: &Path, compiler: Compiler) -> Result<PathBuf> {
    if compiler == Compiler::Auto {
        bail!("Resolve the compiler with check_available before compilation");
    }
    let source_root = source_root.canonicalize().context("Invalid source root")?;
    let main_tex = main_tex.canonicalize().context("Invalid main TeX file")?;
    let relative_main = main_tex
        .strip_prefix(&source_root)
        .context("Main TeX file must be inside the source root")?;
    // Both engines use a fresh output directory, so make an existing source .bbl
    // explicit rather than relying on the jobname's generated-file lookup.
    crate::latex::inline_missing_bibliography(&main_tex, &source_root)?;
    let stem = main_tex
        .file_stem()
        .and_then(|s| s.to_str())
        .context("Main TeX filename must be UTF-8")?;
    let output_dir = tempfile::Builder::new()
        .prefix(".ratex-build-")
        .tempdir_in(&source_root)?
        .keep();
    let log_path = output_dir.join("ratex-compiler.log");
    let mut cmd = Command::new(match compiler {
        Compiler::Tectonic => "tectonic",
        Compiler::Latexmk => "latexmk",
        Compiler::Auto => unreachable!(),
    });
    cmd.current_dir(&source_root);
    // Keep the workspace and its links alive until Tectonic has finished.
    let workspace;
    let pdf_path = match compiler {
        Compiler::Tectonic => {
            workspace = tectonic_workspace(&source_root, relative_main, stem, &output_dir)?;
            cmd.current_dir(workspace.path()).args([
                "-X",
                "build",
                "--untrusted",
                "--keep-logs",
                "--print",
            ]);
            output_dir
                .join(format!("{stem}.tex"))
                .join(format!("{stem}.pdf"))
        }
        Compiler::Latexmk => {
            cmd.args([
                "-norc",
                "-xelatex",
                "-cd-",
                "-bibtex-cond",
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-no-shell-escape",
            ])
            .arg(format!("-outdir={}", output_dir.display()))
            .arg(Path::new(".").join(relative_main));
            output_dir.join(format!("{stem}.pdf"))
        }
        Compiler::Auto => unreachable!(),
    };
    eprintln!("  Using {compiler:?}; full log: {}", log_path.display());
    run_command(&mut cmd, &log_path, COMPILE_TIMEOUT)?;
    if !pdf_path.is_file() {
        bail!(
            "Compiler succeeded without a PDF; full log: {}",
            log_path.display()
        );
    }
    Ok(pdf_path)
}

#[cfg(unix)]
fn tectonic_workspace(
    source_root: &Path,
    main: &Path,
    stem: &str,
    output_dir: &Path,
) -> Result<tempfile::TempDir> {
    // V1 searches input.parent(), and --untrusted disables extra search paths.
    // V2 separates the source root, entry path and jobname without rewriting TeX.
    let workspace = tempfile::Builder::new()
        .prefix("ratex-tectonic-")
        .tempdir()?;
    std::os::unix::fs::symlink(source_root, workspace.path().join("src"))?;
    std::os::unix::fs::symlink(output_dir, workspace.path().join("build"))?;
    let quote = |value: &str| toml::Value::String(value.into()).to_string();
    let config = format!(
        "[doc]\nname = {}\nbundle = {}\n[[output]]\nname = {}\ntype = \"pdf\"\ninputs = {}\n",
        quote(stem),
        quote(TECTONIC_BUNDLE),
        quote(&format!("{stem}.tex")),
        quote(main.to_str().context("Main TeX path must be UTF-8")?),
    );
    std::fs::write(workspace.path().join("Tectonic.toml"), config)?;
    Ok(workspace)
}

#[cfg(not(unix))]
fn tectonic_workspace(_: &Path, _: &Path, _: &str, _: &Path) -> Result<tempfile::TempDir> {
    bail!("The Tectonic backend currently supports macOS/Linux; use --compiler latexmk")
}

fn run_command(cmd: &mut Command, log_path: &Path, timeout: Duration) -> Result<()> {
    let log = File::create(log_path).context("Cannot create compiler log")?;
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "Failed to start {}; full log: {}",
            cmd.get_program().to_string_lossy(),
            log_path.display(),
        )
    })?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            // latexmk spawns engines: kill its entire process group, then reap it.
            #[cfg(unix)]
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", child.id())])
                .status();
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "Compilation timed out after {}s; full log: {}",
                timeout.as_secs(),
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    if !status.success() {
        let log = std::fs::read_to_string(log_path).unwrap_or_default();
        let lines: Vec<_> = log.lines().collect();
        bail!(
            "Compiler exited with {status}; full log: {}\n{}",
            log_path.display(),
            lines[lines.len().saturating_sub(20)..].join("\n")
        );
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn workspace_preserves_root_entry_and_jobname() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let workspace = tectonic_workspace(
            source.path(),
            Path::new("nested/paper.v1.tex"),
            "paper.v1",
            output.path(),
        )
        .unwrap();
        let config: toml::Value = std::fs::read_to_string(workspace.path().join("Tectonic.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(config["output"][0]["name"].as_str(), Some("paper.v1.tex"));
        assert_eq!(
            config["output"][0]["inputs"].as_str(),
            Some("nested/paper.v1.tex")
        );
        assert_eq!(
            workspace.path().join("src").canonicalize().unwrap(),
            source.path().canonicalize().unwrap()
        );
        assert_eq!(
            workspace.path().join("build").canonicalize().unwrap(),
            output.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn compiler_logs_survive_failure_and_timeout_kills_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("compiler.log");
        let mut failure = Command::new("/bin/sh");
        failure.args(["-c", "echo stdout; echo stderr >&2; exit 2"]);
        assert!(run_command(&mut failure, &log, Duration::from_secs(5)).is_err());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("stdout") && text.contains("stderr"));
        let mut hanging = Command::new("/bin/sh");
        hanging
            .current_dir(dir.path())
            .args(["-c", "(sleep 1; touch survived) & wait"]);
        let err = run_command(&mut hanging, &log, Duration::from_millis(50)).unwrap_err();
        assert!(err.to_string().contains("timed out"));
        std::thread::sleep(Duration::from_millis(1100));
        assert!(!dir.path().join("survived").exists());
    }
}
