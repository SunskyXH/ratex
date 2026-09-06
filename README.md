# ratex

Translate arXiv papers from English to Chinese. Downloads LaTeX source,
translates it through an HTTP API or an authenticated local CLI, and compiles
it to PDF. Source and completed translations are kept on disk.

## Install

From a local clone:

```sh
git clone git@github.com:SunskyXH/ratex.git
cd ratex
cargo install --path . --locked
```

This installs `ratex` in `~/.cargo/bin/`. To upgrade, pull and repeat the install.

For PDF output, install either:

- [Tectonic](https://tectonic-typesetting.github.io/) on `PATH`; or
- TeX Live / MacTeX with `latexmk`, `xelatex`, `xeCJK`, and the Fandol fonts.

Before translating, ratex compiles a small Chinese document to check the
engine, packages, and fonts. If this fails, it continues with source output
only. Choose a backend with `--compiler auto|tectonic|latexmk`.

## Usage

```sh
ratex 2406.06608
ratex https://arxiv.org/abs/2406.06608
ratex 2406.06608 --no-compile
ratex 2406.06608 -o papers/paper_zh.pdf
ratex 2406.06608 --source-dir another_translation
ratex 2406.06608 --compiler latexmk
```

The default outputs are `2406.06608_zh.pdf` and the persistent source directory
`2406.06608_zh_tex/`. An existing source directory is never overwritten; use
`--source-dir` to choose a new one. With explicit `--no-compile`, `-o` selects
the source directory instead of a PDF path.

Recompile a finished translation without downloading or calling an LLM:

```sh
ratex --compile-only 2406.06608_zh_tex
ratex --compile-only 2406.06608_zh_tex --compiler latexmk -o paper_zh.pdf
```

For `--compile-only`, the PDF defaults to a sibling of the source directory,
with its trailing `_tex` replaced by `.pdf` (otherwise `.pdf` is appended).
No model profile or API key is needed. Compilation runs relative to the source
root, including when the main `.tex` lives in a subdirectory.

## Failed runs

Translation writes each completed chunk into a neighboring directory, for
example `main.ratex-chunks/00001.tex`. The original `.tex` is replaced atomically
only after all its chunks succeed; that file's chunk directory is then removed.
Empty, refused, or truncated API responses fail instead of replacing source.

A `.ratex-incomplete` marker remains until the whole translation completes.
If a run fails, the source, completed files, and saved chunks remain available
for manual recovery. Automatic translation resume is not implemented. Finish
repairing the source and remove the marker before using `--compile-only`.

Compilation has a five-minute timeout and keeps build logs with the source.
A failed PDF export also leaves the compiled PDF available there. Tectonic runs
in untrusted mode; the TeX Live backend uses `latexmk` without user/project rc
files or shell escape. Package and template compatibility still depend on the
chosen TeX distribution; complex papers may need the TeX Live backend.

## Configuration

Ratex reads `~/.config/ratex/config.toml`. For Codex users without an API key:

```sh
codex login
```

```toml
default_profile = "codex"

[profiles.codex]
protocol = "codex"
concurrency = 1
# Optional model; omit to use the CLI's default.
# model = "..."
# Optional executable path; otherwise "codex" on PATH.
# endpoint = "/Users/me/.local/bin/codex"
```

`codex` uses `codex exec` and your existing local login. ChatGPT account access
and usage limits still apply. Each request uses an empty working directory,
a read-only sandbox, stdin for input, and the final stdout reply for translation.

Other profiles can coexist in the same file:

```toml
[profiles.claude]
protocol = "claude"
concurrency = 1
# Optional: model = "sonnet"
# Optional: endpoint = "/Users/me/.local/bin/claude"

[profiles.gemini]
protocol = "gemini"
api_key_env = "GEMINI_API_KEY"

[profiles.openai]
protocol = "openai"
model = "gpt-4o"
api_key_env = "OPENAI_API_KEY"

[profiles.openrouter]
protocol = "openai"
endpoint = "https://openrouter.ai/api/v1"
model = "anthropic/claude-sonnet-4-5"
api_key_env = "OPENROUTER_API_KEY"
```

`openai` and `gemini` use HTTP APIs. `claude` invokes `claude -p` and reuses
its local login, with no API key required by ratex.

Select a profile with `--profile <name>`. CLI flags `--model`, `--base-url`,
`--api-key`, and `--concurrency` override its fields. For CLI profiles,
`endpoint` / `--base-url` is the executable path. API keys come from the
environment variable named by `api_key_env`; a working-directory `.env` is
loaded automatically. Use `--config <path>` for a different configuration file.

`concurrency` limits total in-flight translation calls across files and chunks.
The default is 4; use a lower value for authenticated CLIs or provider limits.
