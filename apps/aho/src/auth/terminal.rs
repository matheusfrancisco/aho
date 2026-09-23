use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use provider_auth::{AuthError, InputFuture, LoginEvent, LoginUi};

pub(super) struct Terminal {
    open_browser: bool,
}

impl Terminal {
    pub(super) fn new(open_browser: bool) -> Self {
        Self { open_browser }
    }

    #[allow(clippy::disallowed_methods, reason = "authority: auth-browser-opener")]
    fn open(&self, url: &str) {
        if !self.open_browser {
            return;
        }
        let mut command = if cfg!(target_os = "macos") {
            Command::new("open")
        } else if cfg!(target_os = "windows") {
            let mut command = Command::new("rundll32");
            command.arg("url.dll,FileProtocolHandler");
            command
        } else {
            Command::new("xdg-open")
        };
        match command.arg(url).env_remove("AHO_READY_FD").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    if child.wait().is_err() {
                        eprintln!("Could not wait for the browser opener; use the printed URL.");
                    }
                });
            }
            Err(_) => eprintln!("Could not open a browser; open the printed URL on a trusted device."),
        }
    }
}

impl LoginUi for Terminal {
    fn event(&self, event: LoginEvent) {
        match event {
            LoginEvent::AuthUrl { url, instructions } => {
                println!("\nOpen this URL:\n{url}\n{instructions}");
                self.open(&url);
            }
            LoginEvent::DeviceCode { user_code, verification_uri, verification_uri_complete, expires_in, .. } => {
                println!("\nOpen {verification_uri}\nEnter code: {user_code}\nThis code expires in {expires_in} seconds.");
                self.open(verification_uri_complete.as_deref().unwrap_or(&verification_uri));
            }
            LoginEvent::Progress { message } => println!("{message}"),
        }
    }

    fn input<'a>(&'a self, prompt: &'a str) -> InputFuture<'a> {
        Box::pin(async move {
            print!("{prompt}");
            std::io::stdout().flush().map_err(|_| AuthError::Cancelled)?;
            if crate::ctx::stdin_tty() {
                tty_input().await
            } else {
                // A detached OS reader, not Tokio's blocking pool: successful browser callbacks
                // can cancel manual input without runtime shutdown waiting for another newline.
                let (tx, rx) = tokio::sync::oneshot::channel();
                std::thread::spawn(move || {
                    let mut text = String::new();
                    let result = match std::io::stdin().lock().read_line(&mut text) {
                        Ok(0) | Err(_) => Err(AuthError::Cancelled),
                        Ok(_) => Ok(text.trim_end_matches(['\r', '\n']).to_string()),
                    };
                    // Cancellation intentionally drops the receiver when the browser wins.
                    drop(tx.send(result));
                });
                rx.await.map_err(|_| AuthError::Cancelled)?
            }
        })
    }
}

struct RawMode;

impl Drop for RawMode {
    fn drop(&mut self) {
        if crossterm::terminal::disable_raw_mode().is_err() {
            eprintln!("Could not restore terminal input mode.");
        }
    }
}

async fn tty_input() -> Result<String, AuthError> {
    crossterm::terminal::enable_raw_mode().map_err(|_| AuthError::Cancelled)?;
    let _restore = RawMode;
    let mut value = String::new();
    loop {
        if crossterm::event::poll(Duration::ZERO).map_err(|_| AuthError::Cancelled)? {
            if let Event::Key(key) = crossterm::event::read().map_err(|_| AuthError::Cancelled)? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Enter => {
                        print!("\r\n");
                        return Ok(value);
                    }
                    KeyCode::Esc => return Err(AuthError::Cancelled),
                    KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => return Err(AuthError::Cancelled),
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char(c) => value.push(c),
                    _ => {}
                }
            }
        } else {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
