// OMP hook installation.
//
// The relay is a single TypeScript factory omp discovers at
// ~/.omp/agent/hooks/post/coucou-relay.ts (ambient discovery scans
// <agentDir>/hooks/pre|post/*.ts — see docs/hooks.md in the omp repo). There is
// no JSON to merge: the file is Coucou's by construction, so the contract is
// simpler than the settings.json merger this replaced — read the target, take
// a dated backup, show the exact bytes that will change, write only after an
// explicit click, and refuse to touch a file we did not write.

use std::path::PathBuf;

use serde::Serialize;

use crate::settings;

/// The header line of every relay we write. A file at the same path without
/// the marker is somebody else's: never overwritten, never removed.
const MARKER: &str = "coucou-relay v1";

/// The relay itself, compiled into the binary. include_str! removes the whole
/// "find the resource at runtime" failure mode the old hook exe had — there is
/// no path to guess, no copy step to go silently wrong.
const RELAY: &str = include_str!("coucou-relay.ts");

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HookStatus {
    pub installed: bool,
    pub settings_path: String,
    pub hook_path: String,
    pub hook_ready: bool,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HookPreview {
    pub diff: String,
    pub backup: String,
    pub settings_path: String,
    /// Identifies the exact bytes this diff was computed from; handed back to
    /// `write` so we only ever apply what the user actually looked at.
    pub fingerprint: String,
}

fn hook_path() -> PathBuf {
    settings::omp_hook_path()
}

/// True when the file at the relay path was written by somebody other than us.
/// An absent (empty) file is not foreign — there is simply nothing there yet.
fn foreign(text: &str) -> bool {
    !text.is_empty() && !text.contains(MARKER)
}

/// Reads the relay file.
///
/// The only error that means "start from nothing" is the file not being there.
/// Everything else — a permission problem, invalid UTF-8 — is reported, because
/// the alternative is treating somebody's unreadable file as empty and then
/// writing over it.
fn read_relay() -> Result<String, String> {
    let path = hook_path();
    match std::fs::read(&path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|_| format!("{} isn't UTF-8 — Coucou won't touch it.", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(format!("Can't read {}: {err}", path.display())),
    }
}

/// For `status()` only, which must never fail loudly; anything that writes uses
/// `read_relay()` and surfaces the error instead.
fn read_relay_lossy() -> String {
    read_relay().unwrap_or_default()
}

/// Down to the second: installing then uninstalling in the same minute must not
/// quietly overwrite the first backup.
fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

fn backup_path() -> PathBuf {
    hook_path().with_file_name(format!("coucou-relay.ts.bak-{}", stamp()))
}

/// Identifies the exact bytes a preview was computed from. FNV-1a is plenty:
/// the question is only "is this still the file I showed the user?".
fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn status() -> HookStatus {
    let current = read_relay_lossy();
    let installed = !current.is_empty() && !foreign(&current);
    let path = hook_path();
    // The relay template ships inside this binary; the only thing that can be
    // missing is omp itself, whose agent directory appears on first run.
    let omp_ready = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|d| d.exists())
        .unwrap_or(false);
    HookStatus {
        installed,
        settings_path: path.display().to_string(),
        hook_path: path.display().to_string(),
        hook_ready: omp_ready,
    }
}

pub fn preview(install: bool) -> Result<HookPreview, String> {
    let current = read_relay()?;
    if foreign(&current) {
        return Err(format!(
            "{} exists and was not written by Coucou — Coucou won't overwrite or remove it.",
            hook_path().display()
        ));
    }
    let next = if install { RELAY } else { "" };
    Ok(HookPreview {
        diff: unified_diff(&current, next),
        backup: backup_path().display().to_string(),
        settings_path: hook_path().display().to_string(),
        fingerprint: fingerprint(current.as_bytes()),
    })
}

/// Writes (or removes) the relay after taking a dated backup.
///
/// `expected` is the fingerprint the preview was computed from. If the file
/// changed in between — another tool, another window, the user's own editor —
/// we stop and make them look at a fresh diff, because the only thing worse
/// than not installing the relay is silently reverting somebody else's edit.
pub fn write(install: bool, expected: &str) -> Result<String, String> {
    let path = hook_path();
    let current = read_relay()?;

    if fingerprint(current.as_bytes()) != expected {
        return Err(format!(
            "{} changed since the preview. Nothing was written — review the new diff.",
            path.display()
        ));
    }
    if foreign(&current) {
        return Err(format!(
            "{} was not written by Coucou — refusing to touch it.",
            path.display()
        ));
    }

    // Back up the exact bytes the preview showed before anything changes —
    // install and uninstall alike.
    let mut backup = String::new();
    if !current.is_empty() {
        let target = backup_path();
        std::fs::write(&target, current.as_bytes()).map_err(|e| format!("backup failed: {e}"))?;
        backup = target.display().to_string();
    }

    if !install {
        if current.is_empty() {
            return Ok(backup);
        }
        std::fs::remove_file(&path).map_err(|e| format!("remove failed: {e}"))?;
        return Ok(backup);
    }

    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    // Write beside the target and rename over it: a crash or a full disk leaves
    // the original file intact rather than half a relay.
    let temp = dir.join(format!("coucou-relay.ts.coucou-{}", std::process::id()));
    std::fs::write(&temp, RELAY.as_bytes()).map_err(|e| format!("write failed: {e}"))?;
    if let Err(err) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("write failed: {err}"));
    }
    Ok(backup)
}

// ── Minimal unified diff (LCS) ────────────────────────────────────────────────

/// The relay file is small enough that a plain O(n·m) LCS is the simplest
/// honest diff.
fn unified_diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let (n, m) = (a.len(), b.len());

    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out: Vec<String> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(format!("  {}", a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(format!("- {}", a[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    while i < n {
        out.push(format!("- {}", a[i]));
        i += 1;
    }
    while j < m {
        out.push(format!("+ {}", b[j]));
        j += 1;
    }

    // Keep three lines of context around each change so the panel stays readable.
    let changed: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with('+') || l.starts_with('-'))
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return "No change.".into();
    }
    let mut keep = vec![false; out.len()];
    for idx in changed {
        let lo = idx.saturating_sub(3);
        let hi = (idx + 4).min(out.len());
        for k in lo..hi {
            keep[k] = true;
        }
    }
    let mut result = String::new();
    let mut gap = false;
    for (idx, line) in out.iter().enumerate() {
        if keep[idx] {
            result.push_str(line);
            result.push('\n');
            gap = false;
        } else if !gap {
            result.push_str("  …\n");
            gap = true;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_notices_any_change() {
        assert_eq!(fingerprint(b"{}"), fingerprint(b"{}"));
        assert_ne!(fingerprint(b"{}"), fingerprint(b"{ }"));
        assert_ne!(fingerprint(b""), fingerprint(b"{}"));
    }

    #[test]
    fn a_foreign_file_is_never_previewed_nor_written() {
        // Pure logic, no filesystem: the preview path refuses before it ever
        // computes a diff, and write refuses even with a matching fingerprint.
        // Absent (empty) is not foreign — there is simply nothing there yet.
        assert!(!foreign(""));
        assert!(!foreign(MARKER));
        assert!(foreign("// somebody else's relay\n"));
        assert!(!foreign("// header\n// coucou-relay v1\n"));
    }

    /// Everything filesystem-shaped lives in one test on purpose: it points
    /// HOME at a temp directory, and that is process-wide.
    #[test]
    fn install_backs_up_edits_and_uninstalls_cleanly() {
        let tmp = std::env::temp_dir().join(format!("coucou-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // The agent dir existing is what "omp has run before" means for status.
        std::fs::create_dir_all(tmp.join(".omp").join("agent")).unwrap();
        std::env::set_var("HOME", &tmp);

        let path = hook_path();
        assert!(path.starts_with(&tmp), "the test must not touch the real home");

        // Fresh install: nothing present, omp ready.
        let st = status();
        assert!(!st.installed, "nothing is installed yet");
        assert!(st.hook_ready, "the agent directory exists");

        // Install: the diff must show what will be written, and the write must
        // produce byte-identical relay source.
        let plan = preview(true).expect("a fresh preview must succeed");
        assert!(plan.diff.contains(MARKER), "the diff must show our own bytes");
        let backup = write(true, &plan.fingerprint).expect("fresh install must succeed");
        assert_eq!(backup, "", "nothing existed, so nothing to back up");
        assert!(status().installed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), RELAY);

        // Re-installing over our own file backs the previous copy up first.
        let plan = preview(true).unwrap();
        let backup = write(true, &plan.fingerprint).expect("reinstall must succeed");
        assert!(!backup.is_empty(), "the old copy must be backed up");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), RELAY);

        // An edit since the preview is refused, and the file is left alone.
        let stale = preview(false).unwrap();
        std::fs::write(&path, format!("{MARKER}\ndrifted\n")).unwrap();
        let err = write(false, &stale.fingerprint).unwrap_err();
        assert!(err.contains("changed since the preview"), "got: {err}");
        assert!(std::fs::read_to_string(&path).unwrap().contains("drifted"));

        // A foreign file at our path is refused by preview and by write.
        std::fs::write(&path, "// somebody else's relay\n").unwrap();
        let err = preview(true).unwrap_err();
        assert!(err.contains("not written by Coucou"), "got: {err}");
        assert!(write(true, "whatever").is_err());
        assert!(preview(false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "// somebody else's relay\n");

        // Uninstall removes exactly our file and reports not-installed after.
        std::fs::write(&path, RELAY).unwrap();
        let plan = preview(false).unwrap();
        assert!(plan.diff.contains('-'), "uninstall diff must show removals");
        write(false, &plan.fingerprint).expect("uninstall must succeed");
        assert!(!path.exists(), "our own file is gone");
        assert!(!status().installed);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
