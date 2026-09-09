use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::diff_view;

pub const PREVIEW_MAX_LINES: usize = 2_000;
pub const PREVIEW_MAX_BYTES: usize = 512 * 1024;
/// Max files included in the live preview (apply still uses the full list).
pub const PREVIEW_MAX_FILES: usize = 40;
const DIFF_CONTEXT: usize = 3;

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
        files: Vec<FilePreview>,
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
        truncated: bool,
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
                match run_preview(&sed_bin, &expression, &files, &req_rx, &mut pending) {
                    Ok(None) => {
                        // Superseded; `pending` holds the newer request.
                    }
                    Ok(Some((lines, truncated))) => {
                        if evt_tx
                            .send(WorkerEvent::PreviewReady {
                                generation,
                                lines,
                                truncated,
                                error: None,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(err) => {
                        if evt_tx
                            .send(WorkerEvent::PreviewReady {
                                generation,
                                lines: Vec::new(),
                                truncated: false,
                                error: Some(err.to_string()),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
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

/// Returns `Ok(None)` when superseded by a newer request (stored in `pending`).
fn run_preview(
    sed_bin: &Path,
    expression: &str,
    files: &[FilePreview],
    req_rx: &Receiver<WorkerRequest>,
    pending: &mut Option<WorkerRequest>,
) -> Result<Option<(Vec<ratatui::text::Line<'static>>, bool)>> {
    if expression.trim().is_empty() {
        return Ok(Some((
            vec![ratatui::text::Line::from(
                " Type a sed expression to preview changes",
            )],
            false,
        )));
    }
    if files.is_empty() {
        return Ok(Some((
            vec![ratatui::text::Line::from(
                " No files found under . (or pass FILE args)",
            )],
            false,
        )));
    }

    let mut all_lines = Vec::new();
    let mut any_truncated = false;

    for (i, file) in files.iter().enumerate() {
        if take_pending(req_rx, pending) {
            return Ok(None);
        }

        if i > 0 {
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

        any_truncated |= file.truncated;
        match run_sed_stdin(sed_bin, expression, &file.content, req_rx, pending)? {
            None => return Ok(None),
            Some(output) => {
                all_lines.extend(diff_view::diff_lines(
                    &file.content,
                    &output,
                    DIFF_CONTEXT,
                ));
            }
        }
    }

    Ok(Some((all_lines, any_truncated)))
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

/// Run sed with stdin; returns `Ok(None)` if cancelled due to a newer request.
fn run_sed_stdin(
    sed_bin: &Path,
    expression: &str,
    input: &str,
    req_rx: &Receiver<WorkerRequest>,
    pending: &mut Option<WorkerRequest>,
) -> Result<Option<String>> {
    let mut child = Command::new(sed_bin)
        .arg("-e")
        .arg(expression)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", sed_bin.display()))?;

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(err) = stdin.write_all(input.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("failed writing to sed stdin: {err}"));
        }
        drop(stdin);
    }

    // Drain pipes on helper threads so a large preview cannot deadlock on a full pipe.
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

    loop {
        if take_pending(req_rx, pending) {
            kill_child(&mut child);
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Ok(None);
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout_handle
                    .join()
                    .unwrap_or_default();
                let stderr = stderr_handle
                    .join()
                    .unwrap_or_default();
                if !status.success() {
                    let stderr = String::from_utf8_lossy(&stderr);
                    return Err(anyhow!(
                        "sed exited {}: {}",
                        status.code().unwrap_or(-1),
                        stderr.trim()
                    ));
                }
                return String::from_utf8(stdout)
                    .context("sed stdout was not valid UTF-8")
                    .map(Some);
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(err) => {
                kill_child(&mut child);
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return Err(anyhow!("failed waiting for sed: {err}"));
            }
        }
    }
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

/// Load file content capped for preview.
pub fn load_preview_file(path: &Path) -> Result<FilePreview> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
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
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let mut pending = None;
        let out = run_sed_stdin(
            &sed,
            "s/world/rust/",
            "hello world\n",
            &req_rx,
            &mut pending,
        )
        .unwrap()
        .unwrap();
        drop(req_tx);
        assert_eq!(out, "hello rust\n");
    }

    #[test]
    fn apply_in_place() {
        let sed = which::which("sed").expect("sed on PATH");
        let dir = tempfile_dir();
        let path = dir.join("sample.txt");
        std::fs::write(&path, "foo bar\n").unwrap();
        run_apply(&sed, "s/foo/baz/", &[path.clone()], None).unwrap();
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
}
