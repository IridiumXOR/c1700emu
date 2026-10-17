//! The user's side of the serial cable: the terminal and/or a TCP socket.
//!
//! * **stdio**: the terminal is put in raw mode so every key goes straight to
//!   the emulated UART. `Ctrl-]` is the escape key:
//!   `Ctrl-] q` quits, `Ctrl-] b` sends a BREAK, `Ctrl-] Ctrl-]` sends a
//!   literal `Ctrl-]`.
//! * **TCP** (`--tcp PORT`): `telnet localhost PORT` (or `nc`). We negotiate
//!   character mode, strip telnet commands, and map telnet's `send brk` to a
//!   serial BREAK.
//!
//! Input from all sources arrives through one channel as [`ConsoleEvent`]s.

use crossterm::terminal;
use std::io::{IsTerminal, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread;

pub const ESCAPE_KEY: u8 = 0x1D; // Ctrl-]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleEvent {
    Input(u8),
    Break,
    Quit,
}

pub struct Console {
    rx: Receiver<ConsoleEvent>,
    use_stdout: bool,
    raw_mode: bool,
    clients: Arc<Mutex<Vec<TcpStream>>>,
}

impl Console {
    pub fn new(use_stdio: bool, tcp_port: Option<u16>) -> std::io::Result<Self> {
        let (tx, rx) = channel();
        let clients = Arc::new(Mutex::new(Vec::new()));
        let mut raw_mode = false;

        if use_stdio {
            if std::io::stdin().is_terminal() {
                terminal::enable_raw_mode()?;
                raw_mode = true;
                eprint!("[c1700emu] console on this terminal; Ctrl-] q = quit, Ctrl-] b = send BREAK\r\n");
            }
            let tx = tx.clone();
            thread::spawn(move || stdin_reader(tx));
        }
        if let Some(port) = tcp_port {
            let listener = TcpListener::bind(("127.0.0.1", port))?;
            eprint!("[c1700emu] console also on telnet://127.0.0.1:{port}\r\n");
            let clients = clients.clone();
            thread::spawn(move || tcp_acceptor(listener, tx, clients));
        }
        Ok(Console { rx, use_stdout: use_stdio, raw_mode, clients })
    }

    /// Next pending input event, if any.
    pub fn poll(&self) -> Option<ConsoleEvent> {
        match self.rx.try_recv() {
            Ok(ev) => Some(ev),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Show bytes the guest wrote to the UART.
    pub fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.use_stdout {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(bytes);
            let _ = out.flush();
        }
        let mut clients = self.clients.lock().unwrap();
        if !clients.is_empty() {
            // In the telnet protocol 0xFF (IAC) must be doubled.
            let mut escaped = Vec::with_capacity(bytes.len());
            for &b in bytes {
                escaped.push(b);
                if b == 0xFF {
                    escaped.push(0xFF);
                }
            }
            clients.retain_mut(|c| c.write_all(&escaped).is_ok());
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if self.raw_mode {
            let _ = terminal::disable_raw_mode();
        }
    }
}

fn stdin_reader(tx: Sender<ConsoleEvent>) {
    let mut stdin = std::io::stdin().lock();
    let mut escape = false;
    let mut byte = [0u8; 1];
    while let Ok(1) = stdin.read(&mut byte) {
        let b = byte[0];
        let ev = if escape {
            escape = false;
            match b {
                b'q' | b'Q' => Some(ConsoleEvent::Quit),
                b'b' | b'B' => Some(ConsoleEvent::Break),
                ESCAPE_KEY => Some(ConsoleEvent::Input(ESCAPE_KEY)),
                _ => None,
            }
        } else if b == ESCAPE_KEY {
            escape = true;
            None
        } else {
            Some(ConsoleEvent::Input(b))
        };
        if let Some(ev) = ev {
            if tx.send(ev).is_err() {
                return;
            }
        }
    }
}

// Telnet protocol bytes (RFC 854/857/858)
const IAC: u8 = 255;
const DONT: u8 = 254;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;
const BRK: u8 = 243;
const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3;

fn tcp_acceptor(listener: TcpListener, tx: Sender<ConsoleEvent>, clients: Arc<Mutex<Vec<TcpStream>>>) {
    for stream in listener.incoming().flatten() {
        let _ = stream.set_nodelay(true);
        // We echo and we don't do line buffering: put telnet in character mode.
        let mut s = &stream;
        let _ = s.write_all(&[IAC, WILL, OPT_ECHO, IAC, WILL, OPT_SGA]);
        if let Ok(reader) = stream.try_clone() {
            let tx = tx.clone();
            thread::spawn(move || telnet_reader(reader, tx));
        }
        clients.lock().unwrap().push(stream);
    }
}

fn telnet_reader(mut stream: TcpStream, tx: Sender<ConsoleEvent>) {
    #[derive(PartialEq)]
    enum St {
        Data,
        Iac,
        Option,
        Sub,
        SubIac,
        AfterCr,
    }
    let mut st = St::Data;
    let mut buf = [0u8; 512];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            return;
        }
        for &b in &buf[..n] {
            let mut ev = None;
            st = match st {
                St::Data | St::AfterCr => {
                    let after_cr = st == St::AfterCr;
                    match b {
                        IAC => St::Iac,
                        // telnet sends CR NUL or CR LF for Enter: keep only CR
                        0 | b'\n' if after_cr => St::Data,
                        b'\r' => {
                            ev = Some(ConsoleEvent::Input(b'\r'));
                            St::AfterCr
                        }
                        _ => {
                            ev = Some(ConsoleEvent::Input(b));
                            St::Data
                        }
                    }
                }
                St::Iac => match b {
                    IAC => {
                        ev = Some(ConsoleEvent::Input(IAC));
                        St::Data
                    }
                    WILL..=DONT => St::Option,
                    SB => St::Sub,
                    BRK => {
                        ev = Some(ConsoleEvent::Break);
                        St::Data
                    }
                    _ => St::Data,
                },
                St::Option => St::Data,
                St::Sub => {
                    if b == IAC {
                        St::SubIac
                    } else {
                        St::Sub
                    }
                }
                St::SubIac => {
                    if b == SE {
                        St::Data
                    } else {
                        St::Sub
                    }
                }
            };
            if let Some(ev) = ev {
                if tx.send(ev).is_err() {
                    return;
                }
            }
        }
    }
}
