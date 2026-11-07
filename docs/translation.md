# Documentation and translation policy

**English** | [简体中文](zh-CN/translation.md)

English is the canonical language for the runtime contract, protocol, security
model, and compatibility claims. Chinese (`zh-CN`) translations exist so
Chinese-speaking users can adopt and operate the relay without reading English
first.

## File naming

- English: `README.md`, `docs/protocol.md`, `SECURITY.md`, ...
- Simplified Chinese: `README.zh-CN.md`, `docs/zh-CN/protocol.md`,
  `SECURITY.zh-CN.md`, ...
- The Chinese docs index is [docs/zh-CN/README.md](zh-CN/README.md).
- Full Chinese DESIGN translation: [DESIGN.zh-CN.md](../DESIGN.zh-CN.md);
  [docs/zh-CN/design-summary.md](zh-CN/design-summary.md) is a shorter architecture/invariant summary.

## Rules

1. **Change English first**, then update the Chinese file in the same PR when
   the change affects user-facing behavior, limits, commands, security claims,
   or compatibility.
2. **Do not translate identifiers**: keep environment variable names, paths,
   HTTP paths, close codes, metric names, Rust/TOML snippets, and Paseo
   protocol field names exactly as-is.
3. **Chinese docs must not silently link to English docs.** If a Chinese
   translation exists, link it. If it does not, label the link
   `English`/`英文` (or link a Chinese summary) so readers know what they are
   opening.
4. **Keep the language switcher** at the top of paired documents.
5. **Do not duplicate the English canonical content** in a way that makes
   security/limits claims diverge; use a summary + explicit English link when
   the full document is not translated.
6. Update [docs/zh-CN/README.md](zh-CN/README.md) and the root READMEs when
   adding a new translated document.

## Local check

```sh
python3 scripts/check-docs.py
```

The script verifies relative Markdown links and flags Chinese documents that
link a known English canonical document without linking the Chinese
counterpart or labelling the link English/英文. CI runs it.

## Current coverage

Chinese: README, full design, protocol, configuration, deployment, quickstart,
compatibility, operations, migration, benchmarks, FAQ, security, contributing,
support, design summary, ADR summary, evidence snapshot, code of
conduct.

English-only: ADR originals, changelog, and license text. These are explicitly
labelled where linked from Chinese docs. The full DESIGN has a Chinese
translation; the English version remains canonical.
