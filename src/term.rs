//! PTY-backed agent terminals, so the browser can host real Claude Code
//! sessions rather than a description of them. Each terminal is a pty running
//! `claude` in the lab repo with its own KNOOT_USER identity.

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Bytes of output kept per terminal so a page load (or reload) can replay
/// what already happened instead of showing an empty screen.
const SCROLLBACK_MAX: usize = 256 * 1024;

pub struct Term {
    pub name: String,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    tx: broadcast::Sender<Vec<u8>>,
    scrollback: Mutex<Vec<u8>>,
    _child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl Term {
    pub fn subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Vec<u8>>) {
        // Subscribe first, then snapshot: the other order can drop bytes that
        // arrive between the two.
        let rx = self.tx.subscribe();
        (self.scrollback.lock().unwrap().clone(), rx)
    }

    pub fn write_input(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
    }
}

pub struct Terms {
    pub terms: Vec<Arc<Term>>,
    pub dir: String,
}

impl Terms {
    /// Spawn one `claude` per agent name in `dir`.
    pub fn spawn(dir: &Path, agents: &[String], program: &str) -> Result<Arc<Terms>> {
        let mut terms = Vec::new();
        for name in agents {
            terms.push(spawn_one(dir, name, program)?);
        }
        Ok(Arc::new(Terms { terms, dir: dir.to_string_lossy().to_string() }))
    }

    pub fn get(&self, idx: usize) -> Option<Arc<Term>> {
        self.terms.get(idx).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.terms.iter().map(|t| t.name.clone()).collect()
    }
}

fn spawn_one(dir: &Path, name: &str, program: &str) -> Result<Arc<Term>> {
    let pair = native_pty_system()
        .openpty(PtySize { rows: 34, cols: 110, pixel_width: 0, pixel_height: 0 })
        .context("failed to open a pty")?;

    let mut cmd = CommandBuilder::new(program);
    cmd.cwd(dir);
    cmd.env("KNOOT_USER", name);
    cmd.env("TERM", "xterm-256color");
    // Inherit the rest of the environment: PATH and the user's credentials
    // both matter, and overriding USER breaks Claude Code's login.
    for (k, v) in std::env::vars() {
        if k != "KNOOT_USER" && k != "TERM" {
            cmd.env(k, v);
        }
    }

    let child = pair.slave.spawn_command(cmd).with_context(|| format!("failed to start {program}"))?;
    drop(pair.slave); // the master keeps the pty alive

    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let (tx, _) = broadcast::channel::<Vec<u8>>(2048);

    let term = Arc::new(Term {
        name: name.to_string(),
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        tx: tx.clone(),
        scrollback: Mutex::new(Vec::new()),
        _child: Mutex::new(child),
    });

    // Pty reads block, so they get their own thread rather than a task.
    let t = term.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    {
                        let mut sb = t.scrollback.lock().unwrap();
                        sb.extend_from_slice(&chunk);
                        if sb.len() > SCROLLBACK_MAX {
                            let cut = sb.len() - SCROLLBACK_MAX;
                            sb.drain(..cut);
                        }
                    }
                    let _ = tx.send(chunk);
                }
            }
        }
    });

    Ok(term)
}
