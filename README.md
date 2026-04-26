# ratex

Translate arXiv papers from English to Chinese, end to end. Downloads the
source archive, sends each `.tex` chunk through an LLM, and recompiles
the document to PDF with CJK support.

## Install

The repository is private, so install from a local clone:

```sh
git clone git@github.com:SunskyXH/ratex.git
cd ratex
cargo install --path .
```

This puts the `ratex` binary in `~/.cargo/bin/` (make sure that's on your
`PATH`). To upgrade later, `git pull` and rerun `cargo install --path .`.

You also need a TeX engine on `PATH`. Either is fine:

- [tectonic](https://tectonic-typesetting.github.io/) (recommended; auto-downloads missing packages)
- A full TeX Live / MacTeX install that provides `xelatex`

## Usage

```sh
ratex https://arxiv.org/abs/2406.06608
ratex 2406.06608                       # bare ID also works
ratex 2406.06608 --no-compile          # skip PDF, just write translated .tex
ratex 2406.06608 -o paper_zh.pdf       # custom output path
ratex 2406.06608 --concurrency 8       # parallel translation requests
```

The output PDF defaults to `<paper_id>_zh.pdf` in the current directory.
If compilation fails, the translated `.tex` source tree is preserved at
`<paper_id>_zh_tex/` so you can fix it manually without re-paying for
translation.

## Configuration

By default ratex reads `~/.config/ratex/config.toml`. Minimal example:

```toml
default_profile = "gemini"
concurrency = 4

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

`protocol` is the wire format (`openai` or `gemini`); any OpenAI-compatible
endpoint works under `protocol = "openai"`.

Pick a profile per run with `--profile <name>`; CLI flags
(`--model`, `--base-url`, `--api-key`, `--concurrency`) override profile
fields. The API key is read from the environment variable named by
`api_key_env`; a `.env` file in the working directory is loaded
automatically.

Pass `--config <path>` to use a config file at a non-default location.

## Concurrency

`concurrency` bounds how many translation requests are in flight at once,
across both chunk-level and file-level parallelism. Default is `4`.
Increase if your provider's rate limits allow it; decrease if you hit
429s. A single shared semaphore caps total in-flight calls regardless
of how the work is split.
