//! Per-logical-action admission for each physical runner request. No UI/data in the ledger.
use reqwest::{Method, StatusCode};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
#[derive(Clone, Copy, Debug, Default)]
pub struct Counts {
    pub approved: u64,
    pub confirmed: u64,
    pub rejected: u64,
}
impl Counts {
    pub fn uncertain(self) -> u64 {
        self.approved.saturating_sub(self.confirmed + self.rejected)
    }
}
pub struct Guard {
    check: Arc<dyn Fn() -> Option<&'static str> + Send + Sync>,
    approved: AtomicU64,
    confirmed: AtomicU64,
    rejected: AtomicU64,
    denied: AtomicBool,
    reason: Mutex<Option<&'static str>>,
}
tokio::task_local! {static CURRENT:Arc<Guard>;}
#[derive(Debug)]
struct Revoked;
impl std::fmt::Display for Revoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native request scope revoked before dispatch")
    }
}
impl std::error::Error for Revoked {}
impl Guard {
    pub fn new(check: impl Fn() -> Option<&'static str> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            check: Arc::new(check),
            approved: AtomicU64::new(0),
            confirmed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            denied: AtomicBool::new(false),
            reason: Mutex::new(None),
        })
    }
    fn revoke(&self, reason: &'static str) {
        self.denied.store(true, Ordering::Release);
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
    }
    pub fn denied(&self) -> bool {
        self.denied.load(Ordering::Acquire)
    }
    pub fn reason(&self) -> &'static str {
        self.reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .or_else(|| (self.check)())
            .unwrap_or("input_privacy_changed")
    }
    pub fn counts(&self) -> Counts {
        Counts {
            approved: self.approved.load(Ordering::Acquire),
            confirmed: self.confirmed.load(Ordering::Acquire),
            rejected: self.rejected.load(Ordering::Acquire),
        }
    }
    pub async fn scoped<T>(self: &Arc<Self>, work: impl std::future::Future<Output = T>) -> T {
        CURRENT.scope(self.clone(), work).await
    }
    pub async fn run<T>(self: &Arc<Self>, work: impl std::future::Future<Output = T>) -> Option<T> {
        let guard = self.clone();
        CURRENT
            .scope(self.clone(), async move {
                tokio::pin!(work);
                loop {
                    tokio::select! {
                        biased;
                        value = &mut work => return Some(value),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                            if let Some(reason) = (guard.check)() {
                                // Keep an approved mutation's transport alive for its reply.
                                if guard.counts().approved == 0 {
                                    guard.revoke(reason);
                                    return None;
                                }
                            }
                            if guard.denied() {
                                return None;
                            }
                        }
                    }
                }
            })
            .await
    }
}
pub fn is_mutation(method: &Method, path: &str) -> bool {
    *method != Method::GET
        && *method != Method::HEAD
        && !(path.ends_with("/session")
            || path.ends_with("/appium/settings")
            || path.ends_with("/elements")
            || path.ends_with("/element"))
}
/// Check an already-captured scope without admitting a physical request.
pub fn ensure_current() -> anyhow::Result<()> {
    CURRENT
        .try_with(|guard| {
            if let Some(reason) = (guard.check)() {
                guard.revoke(reason);
                Err(anyhow::Error::new(Revoked))
            } else {
                Ok(())
            }
        })
        .unwrap_or(Ok(()))
}

/// Only read-only work may be cancelled on scope loss. A mutation retains its
/// transport until its authoritative acknowledgement has arrived.
pub async fn read<T>(
    work: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::pin!(work);
    loop {
        ensure_current()?;
        tokio::select! {
            biased;
            value = &mut work => { ensure_current()?; return value; }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }
}

pub fn admit(request: &reqwest::Request) -> anyhow::Result<()> {
    ensure_current()?;
    let _ = CURRENT.try_with(|guard| {
        if is_mutation(request.method(), request.url().path()) {
            guard.approved.fetch_add(1, Ordering::AcqRel);
        }
    });
    Ok(())
}
pub fn reply(mutation: bool, status: StatusCode, body: &[u8]) {
    if !mutation {
        return;
    }
    let _ = CURRENT.try_with(|guard| {
        let value = serde_json::from_slice::<serde_json::Value>(body).ok();
        let confirmed = status.is_success()
            && value
                .as_ref()
                .and_then(|v| v.get("value"))
                .is_some_and(|v| v.get("error").is_none());
        if confirmed {
            guard.confirmed.fetch_add(1, Ordering::AcqRel);
        } else if crate::runner::RunnerHttpError::from_body(
            "native request",
            status,
            &String::from_utf8_lossy(body),
        )
        .nothing_sent()
        {
            guard.rejected.fetch_add(1, Ordering::AcqRel);
        }
    });
}

pub fn is_revoked(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<Revoked>().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_metadata_is_not_counted_as_user_mutation() {
        for path in [
            "/session",
            "/session/S/appium/settings",
            "/session/S/elements",
            "/session/S/element",
        ] {
            assert!(!is_mutation(&Method::POST, path));
        }
        assert!(!is_mutation(&Method::GET, "/session/S/source"));
        for path in [
            "/session/S/wda/pressButton",
            "/session/S/wda/apps/launch",
            "/session/S/element/E/click",
        ] {
            assert!(is_mutation(&Method::POST, path));
        }
    }
    #[test]
    fn only_valid_native_acknowledgements_are_confirmed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let guard = Guard::new(|| None);
            guard
                .scoped(async {
                    let client = reqwest::Client::new();
                    for _ in 0..3 {
                        admit(
                            &client
                                .post("http://127.0.0.1:1/session/S/wda/pressButton")
                                .build()
                                .unwrap(),
                        )
                        .unwrap();
                    }
                    reply(true, StatusCode::OK, br#"{"value":null}"#);
                    reply(
                        true,
                        StatusCode::BAD_REQUEST,
                        br#"{"value":{"error":"invalid argument","message":"nothing was sent"}}"#,
                    );
                    reply(
                        true,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        br#"{"value":{"error":"unknown error","message":"uncertain"}}"#,
                    );
                })
                .await;
            let counts = guard.counts();
            assert_eq!(counts.confirmed, 1);
            assert_eq!(counts.rejected, 1);
            assert_eq!(counts.uncertain(), 1);
        });
    }
}
