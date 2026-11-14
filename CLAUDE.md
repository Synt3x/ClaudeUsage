# Claude Usage

Windows tray app (`claude_usage.pyw`) that shows Claude plan usage. Built into an exe by `build.py`;
`.github/workflows/release.yml` tags `v<__version__>` and publishes a release on each push to `main`
whose version has no release yet, so bump `__version__` to release.

## Secrets: never leak them

This app handles real login tokens. Treat everything below as hard rules.

**Where secrets live (never open, print, copy or commit these):**
- `~/.claude/.credentials.json` and its backup `~/.claude/.credentials.json.bak-claudeusage`:
  Claude Code's OAuth `accessToken` / `refreshToken`.
- `%APPDATA%\Claude Usage\` (`settings.json`, `history.json`, login/session data): may hold
  claude.ai session cookies and account details.
- Any `.env`, `*.pem`, `*.key`, `~/.ssh/`.

**Rules:**
- Never read the files above to debug. Use `python claude_usage.pyw --demo` or fake data instead.
- Never print, `log()`, or put tokens, cookies, `Authorization` headers or `sessionKey` values in an
  exception message. When logging an HTTP response, don't log request headers, and keep body logging
  to responses that can't contain credentials.
- Never hardcode a token, cookie or API key in source, tests, screenshots or docs. The only
  hardcoded credential-like value allowed is `OAUTH_CLIENT_ID`, which is public (same for every
  Claude Code install) and allowlisted in `.githooks/pre-commit`.
- Don't put real account data (email, org id, usage numbers tied to an account) in commits, README
  screenshots, issues or PR text. Screenshots come from `--demo`.
- Never `git add -A`/`git add .` blindly and never commit files that `.gitignore` excludes
  (no `git add -f`).
- If a secret ever shows up in output, a file or a commit: stop, tell the user, and don't push.
  A pushed secret must be treated as leaked (revoke/sign out), not just deleted from history.
- Don't add new network endpoints that send the token anywhere other than Anthropic's own
  domains (`api.anthropic.com`, `platform.claude.com`, `console.anthropic.com`, `claude.ai`).

## Before every commit: credential check (mandatory)

1. Review `git diff --cached` yourself for tokens, keys, passwords, cookies, `Authorization`
   headers, credential files or real account data.
2. The `.githooks/pre-commit` hook then scans the staged changes and blocks the commit if it finds
   anything. It must be enabled in each clone: `git config core.hooksPath .githooks`.
3. If your review or the hook finds a possible credential: **stop. Tell the user exactly which file
   and what kind of credential (never repeat the value), and do not continue.** Don't remove it,
   rewrite it, unstage it, retry the commit or push on your own. Wait for the user to decide.
4. Never bypass the check: no `--no-verify`, no `-n`, no changing `core.hooksPath`, no editing the
   hook or its allowlist to make a commit pass, unless the user explicitly asks for that.
