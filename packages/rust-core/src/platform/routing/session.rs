//! Session affinity: which model a session last ran, for routing its next task to the same runner.
use super::{BTreeMap, Duration, Instant, Uuid};

#[derive(Debug, Clone, Default)]
pub struct SessionAffinity {
    sessions: BTreeMap<String, SessionRecord>,
}

#[derive(Debug, Clone)]
struct SessionRecord {
    model: Option<String>,
    last_used: Instant,
    killed: tokio::sync::watch::Sender<bool>,
}

impl SessionAffinity {
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            sessions: pairs
                .into_iter()
                .map(|(session, model)| {
                    (
                        session,
                        SessionRecord {
                            model: Some(model),
                            last_used: Instant::now(),
                            killed: tokio::sync::watch::channel(false).0,
                        },
                    )
                })
                .collect(),
        }
    }

    pub(in crate::platform) fn create(&mut self) -> String {
        let id = Uuid::new_v4().to_string();
        self.sessions.insert(
            id.clone(),
            SessionRecord {
                model: None,
                last_used: Instant::now(),
                killed: tokio::sync::watch::channel(false).0,
            },
        );
        id
    }

    pub(in crate::platform) fn assigned(&self, id: &str) -> Option<&str> {
        self.sessions
            .get(id)
            .and_then(|record| record.model.as_deref())
    }

    pub(in crate::platform) fn bind(&mut self, id: &str, model: &str) {
        if let Some(record) = self.sessions.get_mut(id) {
            record.model = Some(model.to_owned());
            record.last_used = Instant::now();
        }
    }

    pub(in crate::platform) fn touch(&mut self, id: &str, ttl: Duration) -> bool {
        self.prune_expired(ttl);
        if let Some(record) = self.sessions.get_mut(id) {
            record.last_used = Instant::now();
            true
        } else {
            false
        }
    }

    pub(in crate::platform) fn prune_expired(&mut self, ttl: Duration) {
        let now = Instant::now();
        self.sessions
            .retain(|_, record| now.duration_since(record.last_used) < ttl);
    }

    pub(in crate::platform) fn remove(&mut self, id: &str) -> bool {
        self.sessions.remove(id).is_some()
    }

    pub(in crate::platform) fn cancellation(
        &self,
        id: &str,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.sessions
            .get(id)
            .map(|record| record.killed.subscribe())
    }

    pub(in crate::platform) fn kill(&mut self, id: &str) -> bool {
        let Some(record) = self.sessions.remove(id) else {
            return false;
        };
        record.killed.send_replace(true);
        true
    }

    pub(in crate::platform) fn len(&self) -> usize {
        self.sessions.len()
    }
}
