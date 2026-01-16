// circuit_breaker.rs

use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::warn;
use anyhow::Result;

pub struct CircuitBreaker {
    failure_count: AtomicU64,
    last_failure: Arc<RwLock<Option<Instant>>>,
    is_open: AtomicBool,
    threshold: u64,
    timeout: Duration,
}

impl CircuitBreaker {
    pub fn new(threshold: u64, timeout_secs: u64) -> Self {
        Self {
            failure_count: AtomicU64::new(0),
            last_failure: Arc::new(RwLock::new(None)),
            is_open: AtomicBool::new(false),
            threshold,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    pub async fn call<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        // Vérifier si le circuit est ouvert
        if self.is_open.load(Ordering::Relaxed) {
            if let Some(last) = *self.last_failure.read().await {
                if last.elapsed() > self.timeout {
                    // Réinitialiser après timeout
                    self.is_open.store(false, Ordering::Relaxed);
                    self.failure_count.store(0, Ordering::Relaxed);
                } else {
                    return Err(anyhow::anyhow!("Circuit breaker open"));
                }
            }
        }

        match f().await {
            Ok(result) => {
                self.failure_count.store(0, Ordering::Relaxed);
                Ok(result)
            }
            Err(e) => {
                let count = self.failure_count.fetch_add(1, Ordering::Relaxed) + 1;
                
                if count >= self.threshold {
                    self.is_open.store(true, Ordering::Relaxed);
                    *self.last_failure.write().await = Some(Instant::now());
                    warn!("🚨 Circuit breaker opened after {} failures", count);
                }
                
                Err(e)
            }
        }
    }

    pub fn reset(&self) {
        self.failure_count.store(0, Ordering::Relaxed);
        self.is_open.store(false, Ordering::Relaxed);
    }
}