//! One in-memory, single-use continuation per phone. Text never goes to disk; a mutation,
//! session replacement, owner incarnation/privacy change or expiry invalidates the pending work.
use std::time::{Duration, Instant};

pub const TTL_MS: u64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub owner: String,
    pub epoch: u64,
    pub privacy_epoch: u64,
}

pub struct Pending {
    pub target: String,
    pub remaining: String,
    pub budget_ms: u64,
    scope: Scope,
    session: String,
    token: String,
    expires: Instant,
}

#[derive(Default)]
pub struct Store(Option<Pending>);

impl Store {
    pub fn clear(&mut self) {
        self.0 = None;
    }

    pub fn issue(
        &mut self,
        scope: Scope,
        session: String,
        target: String,
        remaining: String,
        budget_ms: u64,
        now: Instant,
    ) -> Option<String> {
        self.clear();
        if scope.owner.is_empty()
            || remaining.is_empty()
            || remaining.chars().count() > crate::text_input::MAX_TEXT_CHARS
        {
            return None;
        }
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).ok()?;
        let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        self.0 = Some(Pending {
            target,
            remaining,
            budget_ms,
            scope,
            session,
            token: token.clone(),
            expires: now + Duration::from_millis(TTL_MS),
        });
        Some(token)
    }

    pub fn take(
        &mut self,
        token: &str,
        scope: &Scope,
        session: &str,
        now: Instant,
    ) -> Option<Pending> {
        let pending = self.0.as_ref()?;
        // A wrong caller/token cannot consume somebody else's valid continuation.
        if pending.token != token || pending.scope.owner != scope.owner {
            return None;
        }
        let pending = self.0.take()?;
        if pending.scope != *scope || pending.session != session || now >= pending.expires {
            return None;
        }
        Some(pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope(owner: &str, epoch: u64) -> Scope {
        Scope {
            owner: owner.into(),
            epoch,
            privacy_epoch: 0,
        }
    }
    fn issue(store: &mut Store, now: Instant) -> String {
        store
            .issue(
                scope("a", 1),
                "s".into(),
                "field".into(),
                "剩余👋🏻".into(),
                6000,
                now,
            )
            .unwrap()
    }
    #[test]
    fn token_is_single_use_and_retains_the_original_target() {
        let mut store = Store::default();
        let now = Instant::now();
        let token = issue(&mut store, now);
        assert_eq!(token.len(), 64);
        assert!(store.take(&token, &scope("b", 1), "s", now).is_none());
        assert!(store.take("wrong", &scope("a", 1), "s", now).is_none());
        let pending = store.take(&token, &scope("a", 1), "s", now).unwrap();
        assert_eq!(pending.target, "field");
        assert_eq!(pending.remaining, "剩余👋🏻");
        assert!(store.take(&token, &scope("a", 1), "s", now).is_none());
    }
    #[test]
    fn expiry_session_owner_incarnation_and_mutation_invalidate() {
        for reason in 0..5 {
            let mut store = Store::default();
            let now = Instant::now();
            let token = issue(&mut store, now);
            if reason == 3 {
                store.clear();
            }
            assert!(store
                .take(
                    &token,
                    &Scope { privacy_epoch: u64::from(reason == 4), ..scope("a", if reason == 1 { 2 } else { 1 }) },
                    if reason == 2 { "other" } else { "s" },
                    if reason == 0 {
                        now + Duration::from_millis(TTL_MS)
                    } else {
                        now
                    }
                )
                .is_none());
            assert!(store.take(&token, &scope("a", 1), "s", now).is_none());
        }
    }
}
