# Finder — Privacy Policy

**Effective date:** August 15, 2026

This policy explains what data Finder (the application, "Finder", "we", "our") collects, stores, and transmits on your device. Finder is a local file-launcher utility: its core purpose is to index file names on your computer so you can search and open them instantly.

## 1. What Finder collects and where it stays

**Everything below is stored only on your own computer.** Finder has no account system, no cloud sync, and no telemetry.

| Data | Where | Purpose |
|---|---|---|
| File names, paths, and NTFS change records (from the USN journal) | `%LOCALAPPDATA%\Finder\index\` (local cache file) | Powering instant local search |
| A plain-text log of lifecycle events (launches, indexing status, autostart events, errors) | `%LOCALAPPDATA%\Finder\log.txt` | Troubleshooting crashes and indexing issues |
| Your settings (hotkey, theme, "start with Windows" preference, first-run marker) | `%LOCALAPPDATA%\Finder\` | Persisting your preferences |

Finding nothing in those folders is deleted when you uninstall Finder or manually remove `%LOCALAPPDATA%\Finder`.

## 2. What Finder transmits

Finder makes only three kinds of network requests, and **none of them contain your file names or search history**:

1. **Update checks.** On startup and periodically, Finder checks `https://github.com/anshdadhich/Finder/releases/latest/download/latest.json` for a newer version. Only the current version number is sent; the connection goes to GitHub's servers.
2. **Web search (only when you use it).** When you explicitly use Finder's web search, the query you typed is sent to Bing or DuckDuckGo (the same as typing it into your browser). The result is shown inside Finder. This does not happen automatically — only when you press the web-search key.
3. **Installer downloads.** When you install or update Finder from GitHub Releases or winget, the installer is downloaded from those servers. Standard download-source information (IP address, user agent) is visible to those services as with any download.

## 3. What Finder never does

- Never reads, uploads, or transmits file **contents** — only names and paths are indexed.
- Never sends telemetry, crash reports, usage statistics, or analytics anywhere.
- Never shows you ads and never sells or shares any data with third parties.
- Never sends your local search terms or history over the network.

## 4. Permissions and why

- **Runs as administrator.** Finder must be elevated to read the NTFS USN journal (the only reliable way to list files without crawling every folder) and to register its global hotkey. This is a local privilege only — it does not grant any network access beyond section 2.
- **"Start with Windows"** uses the Windows Task Scheduler to launch Finder at logon. This is a local setting you can disable in Finder's settings at any time.

## 5. Third-party services

- **GitHub (github.com)** — hosts the releases, update manifest, and the source code. See GitHub's own privacy policy for how they handle download requests.
- **Bing / DuckDuckGo** — used only for the optional web search feature you trigger manually. Queries go directly to the search engine, and their respective privacy policies apply to those requests.

## 6. Security

No system can guarantee absolute security, but Finder is designed to minimize exposure: it holds no account credentials, no payment data, and no personal documents (contents are never read), and the only remote endpoints are the update manifest and the search engines you invoke. You are free (and encouraged) to review the complete source code at https://github.com/anshdadhich/Finder.

## 7. Changes

If Finder ever changes what data it collects, this policy will be updated **before** that change ships, and the new version of this document will be posted in the repository alongside the release notes.

## 8. Contact

For privacy questions: open an issue at https://github.com/anshdadhich/Finder/issues.

---

*This document is provided in good faith to describe Finder's behavior. It is not legal advice. If your use case requires formal assurance, consult a lawyer.*