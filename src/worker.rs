use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::diff_view;

pub const PREVIEW_MAX_LINES: usize = 2_000;
pub const PREVIEW_MAX_BYTES: usize = 512 * 1024;
/// Max *changed* files rendered in the live preview.
pub const PREVIEW_MAX_CHANGED_FILES: usize = 40;
const DIFF_CONTEXT: usize = 3;
const SED_TIMEOUT: Duration = Duration::from_secs(1);
const BINARY_PROBE_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct FilePreview {
    pub path: PathBuf,
    pub content: String,
    pub truncated: bool,
}

#[derive(Debug)]
pub enum WorkerRequest {
    Preview {
        generation: u64,
        expression: String,
        files: Vec<PathBuf>,
    },
    Apply {
        generation: u64,
        expression: String,
        files: Vec<PathBuf>,
        backup_suffix: Option<String>,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum WorkerEvent {
    PreviewReady {
        generation: u64,
        lines: Vec<ratatui::text::Line<'static>>,
        /// True when file contents were size-truncated or more changed files exist than shown.
        truncated: bool,
        changed: usize,
        shown: usize,
        scanned: usize,
        /// False while the worker is still scanning; UI should keep the spinner.
        done: bool,
        error: Option<String>,
    },
    ApplyFinished {
        generation: u64,
        error: Option<String>,
    },
}

pub struct WorkerHandle {
    pub tx: Sender<WorkerRequest>,
    pub rx: Receiver<WorkerEvent>,
}

pub fn spawn_worker(sed_bin: PathBuf) -> WorkerHandle {
    let (req_tx, req_rx) = std::sync::mpsc::channel::<WorkerRequest>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<WorkerEvent>();

    thread::Builder::new()
        .name("sid-sed-worker".into())
        .spawn(move || worker_loop(sed_bin, req_rx, evt_tx))
        .expect("failed to spawn sed worker thread");

    WorkerHandle {
        tx: req_tx,
        rx: evt_rx,
    }
}

fn worker_loop(sed_bin: PathBuf, req_rx: Receiver<WorkerRequest>, evt_tx: Sender<WorkerEvent>) {
    let mut pending: Option<WorkerRequest> = None;

    loop {
        let mut req = match pending.take() {
            Some(r) => r,
            None => match req_rx.recv() {
                Ok(r) => r,
                Err(_) => break,
            },
        };

        // Latest-wins coalesce.
        while let Ok(newer) = req_rx.try_recv() {
            req = newer;
        }

        if matches!(req, WorkerRequest::Shutdown) {
            break;
        }

        match req {
            WorkerRequest::Preview {
                generation,
                expression,
                files,
            } => {
                if let Err(err) = run_preview(
                    &sed_bin,
                    generation,
                    &expression,
                    &files,
                    &req_rx,
                    &mut pending,
                    &evt_tx,
                ) {
                    let _ = evt_tx.send(WorkerEvent::PreviewReady {
                        generation,
                        lines: Vec::new(),
                        truncated: false,
                        changed: 0,
                        shown: 0,
                        scanned: 0,
                        done: true,
                        error: Some(err.to_string()),
                    });
                }
            }
            WorkerRequest::Apply {
                generation,
                expression,
                files,
                backup_suffix,
            } => {
                let error = run_apply(&sed_bin, &expression, &files, backup_suffix.as_deref())
                    .err()
                    .map(|e| e.to_string());
                if evt_tx
                    .send(WorkerEvent::ApplyFinished { generation, error })
                    .is_err()
                {
                    break;
                }
            }
            WorkerRequest::Shutdown => break,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_preview(
    evt_tx: &Sender<WorkerEvent>,
    generation: u64,
    lines: &[ratatui::text::Line<'static>],
    truncated: bool,
    changed: usize,
    shown: usize,
    scanned: usize,
    done: bool,
) -> bool {
    evt_tx
        .send(WorkerEvent::PreviewReady {
            generation,
            lines: lines.to_vec(),
            truncated,
            changed,
            shown,
            scanned,
            done,
            error: None,
        })
        .is_ok()
}

/// Runs a preview. Returns `Ok(())` when finished or superseded (`pending` set).
fn run_preview(
    sed_bin: &Path,
    generation: u64,
    expression: &str,
    files: &[PathBuf],
    req_rx: &Receiver<WorkerRequest>,
    pending: &mut Option<WorkerRequest>,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<()> {
    if expression.trim().is_empty() {
        let _ = emit_preview(
            evt_tx,
            generation,
            &[ratatui::text::Line::from(
                " Type a sed expression to preview changes",
            )],
            false,
            0,
            0,
            0,
            true,
        );
        return Ok(());
    }
    if files.is_empty() {
        let _ = emit_preview(
            evt_tx,
            generation,
            &[ratatui::text::Line::from(
                " No files found under . (or pass FILE args)",
            )],
            false,
            0,
            0,
            0,
            true,
        );
        return Ok(());
    }

    // Fail fast on invalid/incomplete expressions before walking the tree.
    if let Err(err) = run_sed_once(sed_bin, expression, "\n") {
        let _ = evt_tx.send(WorkerEvent::PreviewReady {
            generation,
            lines: Vec::new(),
            truncated: false,
            changed: 0,
            shown: 0,
            scanned: 0,
            done: true,
            error: Some(err.to_string()),
        });
        return Ok(());
    }

    let mut all_lines = Vec::new();
    let mut content_truncated = false;
    let mut changed = 0usize;
    let mut shown = 0usize;
    let mut scanned = 0usize;
    let total = files.len();

    if !emit_preview(
        evt_tx,
        generation,
        &[ratatui::text::Line::from(format!(
            " Scanning 0/{total} files…"
        ))],
        false,
        0,
        0,
        0,
        false,
    ) {
        return Ok(());
    }

    for path in files {
        if take_pending(req_rx, pending) {
            return Ok(());
        }

        scanned += 1;
        let label = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");

        if shown == 0
            && !emit_preview(
                evt_tx,
                generation,
                &[ratatui::text::Line::from(format!(
                    " Scanning {scanned}/{total}: {label}"
                ))],
                false,
                changed,
                shown,
                scanned,
                false,
            )
        {
            return Ok(());
        }

        let file = match load_preview_file(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        content_truncated |= file.truncated;

        let output = match run_sed_once(sed_bin, expression, &file.content) {
            Ok(out) => out,
            Err(_) => continue,
        };

        let hunks = diff_view::diff_lines(&file.content, &output, DIFF_CONTEXT);
        if hunks.is_empty() {
            continue;
        }

        changed += 1;
        if shown < PREVIEW_MAX_CHANGED_FILES {
            if shown > 0 {
                all_lines.push(ratatui::text::Line::from(""));
            }
            all_lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                format!("--- {}", file.path.display()),
                ratatui::style::Style::default()
                    .fg(ratatui::style::Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )));
            all_lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                format!("+++ {}", file.path.display()),
                ratatui::style::Style::default()
                    .fg(ratatui::style::Color::Yellow)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )));
            all_lines.extend(hunks);
            shown += 1;

            if !emit_preview(
                evt_tx,
                generation,
                &all_lines,
                content_truncated || changed > shown,
                changed,
                shown,
                scanned,
                false,
            ) {
                return Ok(());
            }
        }

        // Enough diffs on screen — stop scanning so the TUI stays responsive.
        if shown >= PREVIEW_MAX_CHANGED_FILES {
            content_truncated = true;
            break;
        }
    }

    if changed == 0 {
        all_lines = vec![ratatui::text::Line::from(ratatui::text::Span::styled(
            format!(" (no changes in {scanned} scanned files)"),
            ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
        ))];
    }

    let _ = emit_preview(
        evt_tx,
        generation,
        &all_lines,
        content_truncated || changed > shown,
        changed,
        shown,
        scanned,
        true,
    );
    Ok(())
}

fn take_pending(req_rx: &Receiver<WorkerRequest>, pending: &mut Option<WorkerRequest>) -> bool {
    match req_rx.try_recv() {
        Ok(req) => {
            let mut latest = req;
            while let Ok(newer) = req_rx.try_recv() {
                latest = newer;
            }
            *pending = Some(latest);
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
            *pending = Some(WorkerRequest::Shutdown);
            true
        }
    }
}

/// Run sed once with a timeout. Uses a stdin writer thread and polled wait so a hung
/// sed cannot stall the preview scan.
fn run_sed_once(sed_bin: &Path, expression: &str, input: &str) -> Result<String> {
    let mut child = Command::new(sed_bin)
        .arg("-e")
        .arg(expression)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", sed_bin.display()))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("sed stdin missing"))?;
    let input = input.to_owned();
    let writer = thread::spawn(move || stdin.write_all(input.as_bytes()));

    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("sed stdout missing"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("sed stderr missing"))?;
    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + SED_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_child(&mut child);
                let _ = writer.join();
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return Err(anyhow!("sed timed out after {SED_TIMEOUT:?}"));
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(err) => {
                kill_child(&mut child);
                let _ = writer.join();
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return Err(anyhow!("failed waiting for sed: {err}"));
            }
        }
    };

    let write_result = writer.join();
    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    match write_result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            if status.success() {
                return Err(anyhow!("failed writing to sed stdin: {err}"));
            }
        }
        Err(_) => return Err(anyhow!("sed stdin writer thread panicked")),
    }

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(anyhow!(
            "sed exited {}: {}",
            status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }

    String::from_utf8(stdout).context("sed stdout was not valid UTF-8")
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn run_apply(
    sed_bin: &Path,
    expression: &str,
    files: &[PathBuf],
    backup_suffix: Option<&str>,
) -> Result<()> {
    if expression.trim().is_empty() {
        return Err(anyhow!("empty sed expression"));
    }
    if files.is_empty() {
        return Err(anyhow!("no files to apply"));
    }

    let mut cmd = Command::new(sed_bin);
    match backup_suffix {
        Some(suffix) if !suffix.is_empty() => {
            cmd.arg(format!("-i{suffix}"));
        }
        _ => {
            cmd.arg("-i");
        }
    }
    cmd.arg("-e").arg(expression).arg("--");
    for f in files {
        cmd.arg(f);
    }

    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to spawn {}", sed_bin.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "sed apply failed ({}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }
    Ok(())
}

/// Load file content capped for preview. Rejects binaries (NUL in the first 8KiB).
pub fn load_preview_file(path: &Path) -> Result<FilePreview> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let probe_len = raw.len().min(BINARY_PROBE_BYTES);
    if raw[..probe_len].contains(&0) {
        return Err(anyhow!("binary file"));
    }

    let (slice, truncated_bytes) = if raw.len() > PREVIEW_MAX_BYTES {
        (&raw[..PREVIEW_MAX_BYTES], true)
    } else {
        (raw.as_slice(), false)
    };

    let text = String::from_utf8_lossy(slice);
    let lossy = text.contains('\u{FFFD}');
    let mut lines: Vec<&str> = text.lines().collect();
    let mut truncated = truncated_bytes || lossy;
    if lines.len() > PREVIEW_MAX_LINES {
        lines.truncate(PREVIEW_MAX_LINES);
        truncated = true;
    }
    let mut content = lines.join("\n");
    if text.ends_with('\n') && !content.ends_with('\n') {
        content.push('\n');
    }

    Ok(FilePreview {
        path: path.to_path_buf(),
        content,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sed_substitution_via_system_binary() {
        let sed = which::which("sed").expect("sed on PATH");
        let out = run_sed_once(&sed, "s/world/rust/", "hello world\n").unwrap();
        assert_eq!(out, "hello rust\n");
    }

    #[test]
    fn apply_in_place() {
        let sed = which::which("sed").expect("sed on PATH");
        let dir = tempfile_dir();
        let path = dir.join("sample.txt");
        std::fs::write(&path, "foo bar\n").unwrap();
        run_apply(&sed, "s/foo/baz/", std::slice::from_ref(&path), None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "baz bar\n");
    }

    fn tempfile_dir() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("sid-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&path);
        path
    }

    #[test]
    fn load_truncates_lines() {
        let dir = tempfile_dir();
        let path = dir.join("big.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..(PREVIEW_MAX_LINES + 50) {
            writeln!(f, "line {i}").unwrap();
        }
        let preview = load_preview_file(&path).unwrap();
        assert!(preview.truncated);
        assert!(preview.content.lines().count() <= PREVIEW_MAX_LINES);
    }

    #[test]
    fn load_skips_binary_with_nul() {
        let dir = tempfile_dir();
        let path = dir.join("blob.bin");
        std::fs::write(&path, b"hello\0world").unwrap();
        let err = load_preview_file(&path).unwrap_err();
        assert!(err.to_string().contains("binary"), "{err}");
    }

    #[test]
    fn preview_finds_change_past_early_files() {
        let sed = which::which("sed").expect("sed on PATH");
        let dir = tempfile_dir().join("preview-order");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut files = Vec::new();
        for i in 0..30 {
            let path = dir.join(format!("a{i:02}.txt"));
            std::fs::write(&path, "unchanged\n").unwrap();
            files.push(path);
        }
        let hit = dir.join("z-hit.txt");
        std::fs::write(&hit, "hello world\n").unwrap();
        files.push(hit);

        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let mut pending = None;
        run_preview(
            &sed,
            1,
            "s/world/rust/",
            &files,
            &req_rx,
            &mut pending,
            &evt_tx,
        )
        .unwrap();
        drop(req_tx);

        let mut last = None;
        while let Ok(evt) = evt_rx.try_recv() {
            last = Some(evt);
        }
        let WorkerEvent::PreviewReady {
            lines,
            changed,
            done,
            error,
            ..
        } = last.expect("preview event")
        else {
            panic!("unexpected event");
        };
        assert!(done);
        assert!(error.is_none());
        assert_eq!(changed, 1);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("z-hit.txt"), "{text}");
        assert!(
            text.contains("+hello rust") || text.contains("hello rust"),
            "{text}"
        );
    }

    #[test]
    fn preview_skips_binary_and_still_finds_text_hit() {
        let sed = which::which("sed").expect("sed on PATH");
        let dir = tempfile_dir().join("preview-binary");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bin = dir.join("a.bin");
        std::fs::write(&bin, b"uses: pnpm\0action").unwrap();
        let hit = dir.join("workflow.yml");
        std::fs::write(
            &hit,
            "uses: pnpm/action-setup@ea17c68df8912ef543352723c149a84f56e3d413\n",
        )
        .unwrap();

        let files = vec![bin, hit];
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let mut pending = None;
        run_preview(
            &sed,
            1,
            "s/uses: pnpm\\/action-setup@ea17c68df8912ef543352723c149a84f56e3d413//",
            &files,
            &req_rx,
            &mut pending,
            &evt_tx,
        )
        .unwrap();
        drop(req_tx);

        let mut last = None;
        while let Ok(evt) = evt_rx.try_recv() {
            last = Some(evt);
        }
        let WorkerEvent::PreviewReady {
            lines,
            changed,
            done,
            error,
            ..
        } = last.expect("preview event")
        else {
            panic!("unexpected event");
        };
        assert!(done);
        assert!(error.is_none());
        assert_eq!(changed, 1);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("workflow.yml"), "{text}");
    }
}
