use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::runtime::{RuntimeError, RuntimeInner, now_unix};

#[derive(Debug, Clone)]
pub struct LeaseEntry {
    pub holder: String,
    pub acquired_at: i64,
}

pub struct LeaseGuard {
    inner: Arc<RuntimeInner>,
    resource_id: String,
    holder: String,
}

impl LeaseGuard {
    pub fn holder(&self) -> &str {
        &self.holder
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let mut leases = self.inner.leases.lock().expect("leases lock");
        if leases
            .get(&self.resource_id)
            .is_some_and(|lease| lease.holder == self.holder)
        {
            leases.remove(&self.resource_id);
        }
        drop(leases);
        if let Some(resource) = self
            .inner
            .resources
            .lock()
            .expect("resources lock")
            .get_mut(&self.resource_id)
        {
            resource.state = "present".to_string();
        }
    }
}

pub async fn acquire(
    inner: &Arc<RuntimeInner>,
    resource_id: &str,
    holder: &str,
    wait: Duration,
) -> Result<LeaseGuard, RuntimeError> {
    if !inner
        .resources
        .lock()
        .expect("resources lock")
        .contains_key(resource_id)
    {
        return Err(RuntimeError::not_found(format!(
            "resource '{resource_id}' not found"
        )));
    }
    let deadline = Instant::now() + wait;
    loop {
        {
            let mut leases = inner.leases.lock().expect("leases lock");
            if let Some(lease) = leases.get(resource_id) {
                if Instant::now() >= deadline {
                    return Err(RuntimeError {
                        code: "resource_busy",
                        message: format!("resource is in use by {}", lease.holder),
                        retry_after_seconds: Some(1),
                    });
                }
            } else {
                leases.insert(
                    resource_id.to_string(),
                    LeaseEntry {
                        holder: holder.to_string(),
                        acquired_at: now_unix(),
                    },
                );
                if let Some(resource) = inner
                    .resources
                    .lock()
                    .expect("resources lock")
                    .get_mut(resource_id)
                {
                    resource.state = "busy".to_string();
                }
                return Ok(LeaseGuard {
                    inner: inner.clone(),
                    resource_id: resource_id.to_string(),
                    holder: holder.to_string(),
                });
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
