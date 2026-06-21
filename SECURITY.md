# Security Policy

## Reporting a vulnerability

Please report security issues **privately** — don't open a public issue.

- Preferred: GitHub's private vulnerability reporting (this repo's **Security** tab → **Report a vulnerability**).
- Or email: **hello@hologramhacks.com**.

Include the affected version and steps to reproduce. We'll acknowledge within a few days and keep you updated through the fix.

## Supported versions

The latest released version receives security fixes.

## Security posture

ClipStack opens **no network connection** and sends **no telemetry**. Clipboard history is **memory-only by default** (wiped on exit; an opt-in DPAPI-encrypted persistence is available). **Pinned secrets are encrypted in memory** (`CryptProtectMemory`, with pages `VirtualLock`'d out of the pagefile) **and at rest** (DPAPI), decrypted only for the instant you paste — and a pasted pin is tagged to be **excluded from Windows clipboard history (Win+V) and cloud sync**. See the README's *Security notes* for the full picture and honest limits.
