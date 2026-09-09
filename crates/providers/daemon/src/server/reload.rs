//! `daemon.reload` (12, "Reload = exec-in-place with a readiness fd"), split out of server.rs for
//! the file-size ratchet.

use super::*;

impl Daemon {
    /// `daemon.reload` (12): (1) intents, (2) abort + grace, (3) abort the rest, (4) close open
    /// turns, (5) the host's `before_exec`, `reload.started` + unlink the socket, (6) exec with this
    /// connection as the readiness descriptor. When exec fails: rebind, the host's `exec_failed`,
    /// continue the interrupted sessions here, reply `reload.failed`.
    #[allow(clippy::disallowed_methods, reason = "authority: daemon-reload-exec")]
    pub(super) async fn reload(self: &Arc<Self>, conn: ConnId, id: u64, tx: &Tx, params: Value, fd: BorrowedFd<'_>) {
        let failed = |reason: String| ServerFrame::reply(id, "reload.failed", json!({ "reason": reason }));
        let p: ReloadParams = match parse("daemon.reload", params) {
            Ok(p) => p,
            Err(e) => return send(tx, failed(e)),
        };
        let binary = match p.binary.map_or_else(std::env::current_exe, Ok) {
            Ok(b) => b,
            Err(e) => {
                return send(
                    tx,
                    failed(format!("could not find the running binary ({e}), so we did not reload. Pass `binary` explicitly")),
                );
            }
        };
        if let Err(e) = executable(&binary) {
            return send(tx, failed(e));
        }
        let targets: Vec<(Arc<Session>, RecoveryRole)> = {
            let mut st = self.st();
            if st.closing {
                return send(tx, failed(CLOSING.to_string()));
            }
            if let Some(s) = p.session.filter(|s| !st.sessions.contains_key(s)) {
                return send(tx, failed(format!("{}, so we did not reload", not_open(s))));
            }
            st.closing = true;
            st.sessions
                .iter()
                .filter_map(|(sid, l)| {
                    let role = if Some(*sid) == p.session {
                        RecoveryRole::Initiator
                    } else if l.turn.is_none() {
                        return None;
                    } else if l.owner.as_ref().is_some_and(|o| o.live) {
                        RecoveryRole::InterruptedPeer
                    } else {
                        RecoveryRole::Headless
                    };
                    Some((l.session.clone(), role))
                })
                .collect()
        };

        // (1)
        let directive = format!("reload into {}", binary.display());
        let mut problems = Vec::new();
        let mut intents = 0usize;
        for (s, role) in &targets {
            match recovery::persist_intent(&**s.log(), *role, &directive) {
                Ok(_) => intents += 1,
                Err(e) => problems.push(format!("session {}: {e}", s.id.0)),
            }
        }
        // (2)–(4)
        problems.extend(self.quiesce(CancelReason::Reload, Some(conn)).await);
        // (5) exec runs no destructors: what the host runs next to the sessions stops now.
        problems.extend(self.host.before_exec().await);
        if let Err(e) = std::fs::remove_file(&self.opts.socket)
            && e.kind() != io::ErrorKind::NotFound
        {
            problems
                .push(format!("the socket {} could not be removed ({e}); the new image replaces it as stale", self.opts.socket.display()));
        }
        send(
            tx,
            ServerFrame::reply(
                id,
                "reload.started",
                json!({ "binary": binary.display().to_string(), "intents": intents, "problems": problems }),
            ),
        );
        flush(tx).await;
        // (6)
        let mut cmd = Command::new(&binary);
        cmd.args(self.opts.argv()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let err = ready::exec_with_ready(&mut cmd, fd);

        // Still here: exec failed. Keep serving with this image.
        let rebound = match bind(&self.opts.socket).await {
            Ok(l) => match self.control.send(Control::Listener(l)) {
                Ok(()) => String::new(),
                Err(_) => " The daemon is stopping, so it accepts no new clients.".to_string(),
            },
            Err(e) => format!(" The socket could not be re-created ({e}), so new clients cannot connect: restart the daemon."),
        };
        let restarted = self.host.exec_failed().await;
        let restarted = if restarted.is_empty() {
            String::new()
        } else {
            format!(" Restarting what runs next to the sessions reported: {}.", restarted.join("; "))
        };
        let resumable: Vec<(SessionId, Arc<Session>)> = {
            let mut st = self.st();
            st.closing = false;
            st.sessions.iter().map(|(sid, l)| (*sid, l.session.clone())).collect()
        };
        for (sid, s) in resumable {
            if !recovery::unconsumed(&**s.log()).is_empty() {
                self.start_consume(sid, s);
            }
        }
        send(
            tx,
            failed(format!(
                "could not start {} ({err}), so we kept serving with the running binary; the interrupted turns continue here and other clients must reconnect. Fix the binary and reload again.{rebound}{restarted}",
                binary.display()
            )),
        );
    }
}
