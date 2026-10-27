//! In-container pseudo-console host for interactive `processcontainer` runs.
//!
//! MXC cannot attach a caller-controlled ConPTY to a BaseContainer process
//! (`unsupported_containment: ... PTY spawning for the 'processcontainer'
//! backend`; microsoft/mxc#1381 was closed pending an OS change). Plain pipes
//! do work. So for an interactive launch the relay runs
//! `varda pty-host --rows R --cols C --command-line <agent>` INSIDE the
//! container over pipes, and `pty-host` creates the ConPTY there itself with
//! an ordinary `CreateProcessW` + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, so the
//! agent gets a real console (TUI, line editing, Ctrl-C).
//!
//! Window resizes cross the pipe in-band: the host relay writes
//! [`resize_sequence`] into the container's stdin and `pty-host` strips it out
//! ([`ResizeFilter`]) and calls `ResizePseudoConsole`.

// The pure helpers are exercised by tests on every OS but only wired in on Windows.
#![cfg_attr(not(windows), allow(dead_code))]

/// In-band resize request: `ESC ] 9999 ; varda-resize ; ROWS ; COLS BEL`.
/// An OSC with a private number, so a stray copy reaching a terminal is ignored.
const RESIZE_PREFIX: &[u8] = b"\x1b]9999;varda-resize;";
const RESIZE_END: u8 = 0x07;
/// Longest accepted `ROWS;COLS` payload; anything longer is passed through.
const MAX_PAYLOAD: usize = 16;

pub fn resize_sequence(rows: u16, cols: u16) -> Vec<u8> {
    let mut out = RESIZE_PREFIX.to_vec();
    out.extend_from_slice(format!("{rows};{cols}").as_bytes());
    out.push(RESIZE_END);
    out
}

/// Streaming splitter: feed stdin chunks, get back the bytes for the console
/// and any resize requests, in order. A partial sequence at a chunk boundary
/// is held until the next chunk.
#[derive(Debug, Default)]
pub struct ResizeFilter {
    pending: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Filtered {
    Input(Vec<u8>),
    Resize(u16, u16),
}

impl ResizeFilter {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Filtered> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut passthrough = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            let rest = &self.pending[i..];
            if rest.starts_with(RESIZE_PREFIX) {
                let payload = &rest[RESIZE_PREFIX.len()..];
                match payload.iter().position(|&b| b == RESIZE_END) {
                    Some(end) if end <= MAX_PAYLOAD => {
                        if let Some((rows, cols)) = parse_payload(&payload[..end]) {
                            if !passthrough.is_empty() {
                                out.push(Filtered::Input(std::mem::take(&mut passthrough)));
                            }
                            out.push(Filtered::Resize(rows, cols));
                        } else {
                            passthrough.extend_from_slice(&rest[..RESIZE_PREFIX.len() + end + 1]);
                        }
                        i += RESIZE_PREFIX.len() + end + 1;
                        continue;
                    }
                    None if payload.len() <= MAX_PAYLOAD => {
                        // Incomplete: keep it for the next chunk.
                        break;
                    }
                    _ => {
                        // Not ours (too long): pass the ESC through and move on.
                        passthrough.push(rest[0]);
                        i += 1;
                        continue;
                    }
                }
            }
            // A possible prefix split across chunks. A lone ESC is NOT held:
            // it is the Esc key (Copilot's cancel), and the relay writes each
            // resize sequence in one write, so a split right after ESC does not
            // happen in practice.
            if rest.len() >= 2 && RESIZE_PREFIX.starts_with(rest) {
                break;
            }
            passthrough.push(rest[0]);
            i += 1;
        }
        self.pending.drain(..i);
        if !passthrough.is_empty() {
            out.push(Filtered::Input(passthrough));
        }
        out
    }
}

fn parse_payload(payload: &[u8]) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(payload).ok()?;
    let (rows, cols) = text.split_once(';')?;
    let rows = rows.parse::<u16>().ok().filter(|r| *r > 0)?;
    let cols = cols.parse::<u16>().ok().filter(|c| *c > 0)?;
    Some((rows, cols))
}

#[cfg(windows)]
pub fn run(rows: u16, cols: u16, command_line: &str) -> anyhow::Result<i32> {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::io::FromRawHandle;
    use std::ptr::{null, null_mut};

    use anyhow::bail;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Console::{
        COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
        UpdateProcThreadAttribute, WaitForSingleObject,
    };

    const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

    fn coord(rows: u16, cols: u16) -> COORD {
        COORD {
            X: cols.max(1) as i16,
            Y: rows.max(1) as i16,
        }
    }

    unsafe {
        let (mut in_read, mut in_write): (HANDLE, HANDLE) = (null_mut(), null_mut());
        let (mut out_read, mut out_write): (HANDLE, HANDLE) = (null_mut(), null_mut());
        if CreatePipe(&mut in_read, &mut in_write, null(), 0) == 0
            || CreatePipe(&mut out_read, &mut out_write, null(), 0) == 0
        {
            bail!("pty-host: CreatePipe failed: {}", std::io::Error::last_os_error());
        }
        let mut hpc: HPCON = 0;
        let hr = CreatePseudoConsole(coord(rows, cols), in_read, out_write, 0, &mut hpc);
        if hr != 0 {
            bail!("pty-host: CreatePseudoConsole failed: HRESULT {hr:#010x}");
        }
        // The pseudo-console owns its ends now.
        CloseHandle(in_read);
        CloseHandle(out_write);

        let mut attr_size: usize = 0;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
        let mut attr_buf = vec![0u8; attr_size];
        let attrs = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(attrs, 1, 0, &mut attr_size) == 0 {
            bail!(
                "pty-host: InitializeProcThreadAttributeList failed: {}",
                std::io::Error::last_os_error()
            );
        }
        if UpdateProcThreadAttribute(
            attrs,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            hpc as *const core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            null_mut(),
            null(),
        ) == 0
        {
            bail!(
                "pty-host: UpdateProcThreadAttribute failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // Our own stdio are MXC's pipes. Without STARTF_USESTDHANDLES and null
        // handles the child can pick those up instead of the pseudo-console.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.lpAttributeList = attrs;
        let mut wide: Vec<u16> = command_line.encode_utf16().chain(Some(0)).collect();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessW(
            null(),
            wide.as_mut_ptr(),
            null(),
            null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT,
            null(),
            null(),
            &si.StartupInfo,
            &mut pi,
        ) == 0
        {
            let error = std::io::Error::last_os_error();
            ClosePseudoConsole(hpc);
            bail!("pty-host: failed to start `{command_line}`: {error}");
        }
        CloseHandle(pi.hThread);

        let mut console_out = File::from_raw_handle(out_read as _);
        let output = std::thread::spawn(move || {
            let mut stdout = std::io::stdout();
            let mut buf = [0u8; 8192];
            loop {
                match console_out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = stdout.flush();
                    }
                }
            }
        });
        let mut console_in = File::from_raw_handle(in_write as _);
        let hpc_for_input = hpc;
        // Detached: a blocked pipe read cannot be cancelled; the process exits
        // right after the agent does.
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut filter = ResizeFilter::default();
            let mut buf = [0u8; 4096];
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for item in filter.feed(&buf[..n]) {
                    match item {
                        Filtered::Input(bytes) => {
                            if console_in.write_all(&bytes).is_err() {
                                return;
                            }
                        }
                        Filtered::Resize(rows, cols) => {
                            ResizePseudoConsole(hpc_for_input, coord(rows, cols));
                        }
                    }
                }
            }
        });

        let mut code: u32 = 1;
        if WaitForSingleObject(pi.hProcess, INFINITE) == WAIT_OBJECT_0 {
            GetExitCodeProcess(pi.hProcess, &mut code);
        }
        CloseHandle(pi.hProcess);
        // Closing the pseudo-console ends its output pipe, which ends `output`.
        ClosePseudoConsole(hpc);
        let _ = output.join();
        DeleteProcThreadAttributeList(attrs);
        drop(attr_buf);
        Ok(code as i32)
    }
}

#[cfg(not(windows))]
pub fn run(_rows: u16, _cols: u16, _command_line: &str) -> anyhow::Result<i32> {
    anyhow::bail!("`varda pty-host` runs inside a Windows processcontainer and is Windows-only")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(bytes: &[u8]) -> Filtered {
        Filtered::Input(bytes.to_vec())
    }

    #[test]
    fn plain_input_passes_through() {
        let mut filter = ResizeFilter::default();
        assert_eq!(filter.feed(b"hello\r"), vec![input(b"hello\r")]);
        // Ordinary escape sequences (arrow keys) are untouched.
        assert_eq!(filter.feed(b"\x1b[A"), vec![input(b"\x1b[A")]);
    }

    #[test]
    fn resize_is_extracted_in_order() {
        let mut filter = ResizeFilter::default();
        let mut chunk = b"ab".to_vec();
        chunk.extend(resize_sequence(40, 120));
        chunk.extend(b"cd");
        assert_eq!(
            filter.feed(&chunk),
            vec![input(b"ab"), Filtered::Resize(40, 120), input(b"cd")]
        );
    }

    #[test]
    fn resize_split_across_chunks_is_reassembled() {
        let sequence = resize_sequence(30, 100);
        for split in 2..sequence.len() {
            let mut filter = ResizeFilter::default();
            let mut first = b"x".to_vec();
            first.extend_from_slice(&sequence[..split]);
            let mut items = filter.feed(&first);
            let mut second = sequence[split..].to_vec();
            second.extend(b"y");
            items.extend(filter.feed(&second));
            assert_eq!(
                items,
                vec![input(b"x"), Filtered::Resize(30, 100), input(b"y")],
                "split at {split}"
            );
        }
    }

    #[test]
    fn malformed_or_foreign_sequences_pass_through() {
        let mut filter = ResizeFilter::default();
        let bad = b"\x1b]9999;varda-resize;x;y\x07";
        assert_eq!(filter.feed(bad), vec![input(bad)]);
        let mut filter = ResizeFilter::default();
        let other_osc = b"\x1b]0;title\x07";
        assert_eq!(filter.feed(other_osc), vec![input(other_osc)]);
    }

    #[test]
    fn a_lone_escape_is_delivered_immediately() {
        let mut filter = ResizeFilter::default();
        assert_eq!(filter.feed(b"\x1b"), vec![input(b"\x1b")]);
        assert_eq!(filter.feed(b"[B"), vec![input(b"[B")]);
    }
}
