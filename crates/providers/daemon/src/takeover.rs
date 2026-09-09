//! `12` "Takeover is a decision with logged inputs": one interactive owner per session. A second
//! client may take over iff it is the same client instance, or it has local history of the
//! session and is not provably a different instance. The server logs every decision as
//! `SurfaceAttached{client_instance, has_local_history, takeover}`; a refused client stays
//! attached read-only (it sees the stream, it cannot prompt, steer or abort).

use aho_log::Takeover;

/// The session's current interactive owner as the daemon last saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub client_instance: String,
    /// The owner's connection is still open.
    pub live: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachRequest {
    pub client_instance: String,
    pub has_local_history: bool,
}

pub fn decide(owner: Option<&Owner>, req: &AttachRequest) -> Takeover {
    let Some(owner) = owner else {
        return Takeover::Allowed;
    };
    if !owner.client_instance.is_empty() && owner.client_instance == req.client_instance {
        return Takeover::Allowed;
    }
    let provably_distinct = owner.live && !owner.client_instance.is_empty() && owner.client_instance != req.client_instance;
    if req.has_local_history && !provably_distinct {
        return Takeover::Allowed;
    }
    Takeover::Refused(refusal(owner, provably_distinct))
}

fn refusal(owner: &Owner, provably_distinct: bool) -> String {
    let who = if owner.client_instance.is_empty() { "another client".to_string() } else { format!("client `{}`", owner.client_instance) };
    // Refused with local history happens only when the owner is provably someone else.
    let what = if provably_distinct {
        format!("This session is driven by {who}, which is still attached")
    } else {
        format!(
            "This session was last driven by {who}, and this client has no local history of it, so we cannot tell it is the same client"
        )
    };
    format!(
        "{what}. We attached this client read-only: it can watch, but not prompt, steer or abort. To take over, detach the other client (`session.detach`) or reconnect from it."
    )
}
