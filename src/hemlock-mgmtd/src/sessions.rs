//! Who is logged in to the switch.
//!
//! Both front-ends register here at session start (`WhoAmI` with a
//! client kind), slide the idle timer as they work, and close on the
//! way out — so `show system users` can answer for the CLI and the web
//! console at once, from one place, without either front-end having to
//! talk to the other.
//!
//! A session that stops touching (a CLI killed with its terminal, a
//! webd that crashed) is reaped after [`STALE_AFTER`] rather than
//! lingering in the table forever.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

use hemlock_common::role::Role;

/// A session nobody has touched for this long is gone, whatever it
/// forgot to tell us. Comfortably longer than the console's own idle
/// timeout ceiling (24 h) so a live web session is never reaped under
/// the user.
pub const STALE_AFTER: Duration = Duration::from_secs(36 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub user: String,
    pub role: Role,
    /// "cli" or "web".
    pub client: String,
    /// Peer address, or "console" for a local login.
    pub from: String,
    pub login_time: SystemTime,
    last_active: Instant,
}

impl Session {
    /// Seconds since this session last did anything.
    pub fn idle(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.last_active).as_secs()
    }
}

/// The live session table, newest id last.
#[derive(Debug, Default)]
pub struct Sessions {
    next_id: u64,
    live: BTreeMap<u64, Session>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a session and return its id.
    pub fn open(&mut self, user: &str, role: Role, client: &str, from: &str) -> u64 {
        self.reap(Instant::now());
        self.next_id += 1;
        let id = self.next_id;
        self.live.insert(
            id,
            Session {
                user: user.to_string(),
                role,
                client: client.to_string(),
                from: from.to_string(),
                login_time: SystemTime::now(),
                last_active: Instant::now(),
            },
        );
        id
    }

    /// Slide a session's idle timer. False = no such session (the
    /// caller should re-open, which is what a webd restart does).
    pub fn touch(&mut self, id: u64) -> bool {
        let now = Instant::now();
        self.reap(now);
        match self.live.get_mut(&id) {
            Some(session) => {
                session.last_active = now;
                true
            }
            None => false,
        }
    }

    pub fn close(&mut self, id: u64) {
        self.live.remove(&id);
    }

    /// Every live session, oldest login first — the order
    /// `show system users` prints.
    pub fn list(&mut self) -> Vec<(u64, Session)> {
        let now = Instant::now();
        self.reap(now);
        let mut sessions: Vec<(u64, Session)> =
            self.live.iter().map(|(id, s)| (*id, s.clone())).collect();
        sessions.sort_by(|(left_id, left), (right_id, right)| {
            left.login_time
                .cmp(&right.login_time)
                .then(left_id.cmp(right_id))
        });
        sessions
    }

    /// Update the role of every live session for `user` — a commit that
    /// promotes or demotes an account must not need a re-login to take
    /// effect on the session already open.
    pub fn set_role(&mut self, user: &str, role: Role) {
        for session in self.live.values_mut() {
            if session.user == user {
                session.role = role;
            }
        }
    }

    fn reap(&mut self, now: Instant) {
        self.live
            .retain(|_, session| now.saturating_duration_since(session.last_active) < STALE_AFTER);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn sessions_open_touch_and_close() {
        let mut sessions = Sessions::new();
        let cli = sessions.open("cody", Role::Admin, "cli", "10.42.0.100");
        let web = sessions.open("cody", Role::Admin, "web", "10.42.0.100");
        assert_ne!(cli, web);
        assert_eq!(sessions.list().len(), 2);
        assert!(sessions.touch(cli));
        assert!(!sessions.touch(9999));

        sessions.close(cli);
        let live = sessions.list();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].1.client, "web");
        sessions.close(web);
        assert!(sessions.list().is_empty());
    }

    /// A commit that demotes an account reaches the session already
    /// open, without a re-login.
    #[test]
    fn role_changes_reach_live_sessions() {
        let mut sessions = Sessions::new();
        let id = sessions.open("noc", Role::Admin, "cli", "console");
        sessions.open("cody", Role::Admin, "web", "10.0.0.1");
        sessions.set_role("noc", Role::Operator);
        let live = sessions.list();
        let noc = live.iter().find(|(sid, _)| *sid == id).unwrap();
        assert_eq!(noc.1.role, Role::Operator);
        // Everyone else is untouched.
        assert!(live
            .iter()
            .any(|(_, s)| s.user == "cody" && s.role == Role::Admin));
    }

    /// Listing is oldest login first, and idle counts from the last
    /// touch.
    #[test]
    fn listing_is_ordered_and_reports_idle() {
        let mut sessions = Sessions::new();
        let first = sessions.open("a", Role::Operator, "cli", "console");
        let second = sessions.open("b", Role::Operator, "web", "10.0.0.1");
        let ids: Vec<u64> = sessions.list().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, [first, second]);
        let now = Instant::now();
        assert_eq!(sessions.list()[0].1.idle(now), 0);
    }
}
