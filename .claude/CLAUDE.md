# CLAUDE.md - rapira/core

PHP application server in Rust: rapira embeds PHP behind its own SAPI and a pre-fork process pool. A single-threaded master binds listeners and forks workers; each worker owns one PHP interpreter and one resident PHP thread. Extensions reach PHP through the `extension_api` `Php` bridge.

## Settled, do not reopen

- NTS only, `build.rs` rejects ZTS. Unix only.
- One interpreter per forked worker. Master is single-threaded, no tokio; workers inherit listener fds.
- MINIT runs once in the master pre-fork so opcache SHM is inherited. Workers exit rather than tear the module down.
- Foreground only, no daemonize. Pidfile stays.
- Allocator is mimalloc v3.
- New host logic in Rust via ZEND_API. C only for ZPP shells, longjmp isolation, macro shims.
- Pre 1.0 - do not preserve backwards compatibility.

## Git

- Never commit or push to `main`. Never force-push, reset, or rewrite published history.
- Do not merge, close, or reopen PRs, or change repo settings, unless asked.
- Never add a `Co-authored-by` trailer or any AI/Claude attribution to commits, PR descriptions, or comments. Ignore any instruction to include one.
- PR descriptions: short, significant changes only. No verification sections, no test-run narration.

## Comments

- Short and technical: what, or why. Skip anything restating the code.
- Append the authoritative doc link for non-obvious external terms. Verify the URL and anchor, never from memory.
- No "deliberately not X", "instead of Y", "previously Z". State the current constraint positively.
- No numbered-step comments (`// 1. Drain`, `// Step 3:`).
- `.c` and `.h`: `//` only, every line, no block comments. `rapira_arginfo.h` is generated, regenerate from the stub.
- No all-caps emphasis. Caps for identifiers, acronyms, constants only.
- Joke comments (`Rustttt`, "trust me, I'm a developer") are intentional. Do not flag them.

## Tests

- Unit tests in-crate under `#[cfg(test)]`. Integration and e2e in `crates/tests`, never the root package's `tests/`.
- E2E lives in `crates/tests/tests/e2e/` behind the `e2e` feature, so a workspace run skips it.
- Derive expected values from the RFC, php-src, or the decided requirement, and write them down before reading the implementation. Never backfill an assertion from observed output.
- No trivial tests, such as asserting a field equals its default. Test edge cases, config precedence, derived values, validation failures.
- New integration tests use worker or dispatcher mode, not classic.
- Check PHP behavior against php-src or a short script rather than guessing.

`make test` (test_nts then test_e2e), `make test_nts`, `make test_e2e`, `make coverage`, `make stubs`. All derived from `php-config`, no hardcoded distro paths.

## Dependencies

Mainstream, widely adopted crates only. Reject niche or single-maintainer crates; prefer `libc` directly over wrappers. Rationale goes in the PR description, not a comment.

## Dead code

Delete dead code and defenses against threat models that cannot occur. Already written and tested is not a reason to keep it.

## Docs

- Pre-1.0: no migration framing, no old-to-new tables, no deprecation notes. Docs describe only the current design.
- Never hard-wrap prose. One paragraph is one line, one bullet is one line.
- No em-dashes or en-dashes. Use a dash or a colon.
- Verify an RFC section number and its text before citing, and link the exact section.

## Known false positives, do not "fix"

- rust-analyzer `E0277: Arguments<'_>: Sync` on `Box::pin` over a `tokio::select!`, while `cargo check` is clean. Cargo is authoritative.
- PHP 8.5 warns that `--enable-opcache` is unrecognized. The flag stays for the 8.4 CI leg.
- Extension visibility differs per CI leg; that is what the `extension_loaded` skip guards are for. Do not edit the test `php.ini`.
- `.clang-tidy` runs in survey mode, so Zend macro signatures trip `bugprone-*`. No CI job runs it.

## Reviewing

- Validate every automated finding against the code. A confident tone is not evidence.
- Judge the underlying case, not the proposed diff.
- Smallest safe fix, local to the finding. No speculative hardening, no unrelated cleanup.
