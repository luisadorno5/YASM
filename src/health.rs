//! Startup HTTP health endpoint checker (`Services\HealthEndpointChecker` equivalent).

use crate::config::ResolvedHealthCheck;
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Outcome of waiting for an application's startup health endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthCheckOutcome {
    /// Health check is disabled in configuration.
    Disabled,
    /// Received a 2xx HTTP response within `StartupTimeout`.
    Healthy { elapsed: Duration },
    /// `StartupTimeout` expired without a 2xx response.
    TimedOut { last_result: String },
    /// Cancelled because a stop was requested or the child process exited early.
    Cancelled,
}

/// Trait abstracting a single HTTP GET probe so unit tests can inject a fake handler.
pub trait HttpHealthProbe: Send + Sync {
    fn probe(
        &self,
        url: &str,
        timeout: Duration,
    ) -> impl Future<Output = Result<u16, String>> + Send;
}

/// Production HTTP probe backed by `reqwest::Client`.
#[derive(Clone)]
pub struct ReqwestHealthProbe {
    client: reqwest::Client,
}

impl ReqwestHealthProbe {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for ReqwestHealthProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpHealthProbe for ReqwestHealthProbe {
    async fn probe(&self, url: &str, timeout: Duration) -> Result<u16, String> {
        let resp = self
            .client
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                if let Some(src) = std::error::Error::source(&e) {
                    format!("{e}: {src}")
                } else {
                    e.to_string()
                }
            })?;
        Ok(resp.status().as_u16())
    }
}

/// Poll `settings.url` every `settings.retry_interval` until a 2xx response arrives,
/// `settings.startup_timeout` elapses, or `cancel_rx` signals cancellation.
pub async fn wait_for_startup_health<P: HttpHealthProbe>(
    settings: &ResolvedHealthCheck,
    probe: &P,
    mut cancel_rx: watch::Receiver<bool>,
) -> HealthCheckOutcome {
    if !settings.enabled {
        return HealthCheckOutcome::Disabled;
    }

    let Some(url) = settings.url.as_deref() else {
        return HealthCheckOutcome::Disabled;
    };

    let start = Instant::now();
    let deadline = start + settings.startup_timeout;
    let mut last_result = "no response".to_string();

    loop {
        if *cancel_rx.borrow() {
            return HealthCheckOutcome::Cancelled;
        }

        let now = Instant::now();
        if now >= deadline {
            return HealthCheckOutcome::TimedOut { last_result };
        }

        let remaining = deadline.duration_since(now);
        let req_timeout = settings.request_timeout.min(remaining);

        tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    return HealthCheckOutcome::Cancelled;
                }
            }
            res = probe.probe(url, req_timeout) => {
                match res {
                    Ok(status) if (200..300).contains(&status) => {
                        return HealthCheckOutcome::Healthy {
                            elapsed: start.elapsed(),
                        };
                    }
                    Ok(status) => {
                        last_result = format!("HTTP {status}");
                    }
                    Err(err) => {
                        last_result = err;
                    }
                }
            }
        }

        let now = Instant::now();
        if now >= deadline {
            return HealthCheckOutcome::TimedOut { last_result };
        }

        let sleep_dur = settings.retry_interval.min(deadline.duration_since(now));
        tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    return HealthCheckOutcome::Cancelled;
                }
            }
            _ = tokio::time::sleep(sleep_dur) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct FakeProbe {
        responses: Arc<Mutex<VecDeque<Result<u16, String>>>>,
        calls: Arc<Mutex<usize>>,
    }

    impl FakeProbe {
        fn new(items: Vec<Result<u16, String>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::from(items))),
                calls: Arc::new(Mutex::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl HttpHealthProbe for FakeProbe {
        async fn probe(&self, _url: &str, _timeout: Duration) -> Result<u16, String> {
            *self.calls.lock().unwrap() += 1;
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err("connection refused".to_string()))
        }
    }

    #[tokio::test]
    async fn wait_for_startup_health_should_pass_on_first_2xx() {
        // Arrange
        let probe = FakeProbe::new(vec![Ok(200)]);
        let settings = ResolvedHealthCheck {
            enabled: true,
            url: Some("http://127.0.0.1:3000/api/health".to_string()),
            startup_timeout: Duration::from_secs(5),
            retry_interval: Duration::from_millis(20),
            request_timeout: Duration::from_secs(1),
        };
        let (_tx, rx) = watch::channel(false);

        // Act
        let outcome = wait_for_startup_health(&settings, &probe, rx).await;

        // Assert
        assert!(matches!(outcome, HealthCheckOutcome::Healthy { .. }));
        assert_eq!(probe.call_count(), 1);
    }

    #[tokio::test]
    async fn wait_for_startup_health_should_retry_on_5xx_and_connection_errors_then_succeed() {
        // Arrange
        let probe = FakeProbe::new(vec![
            Err("connection refused".to_string()),
            Ok(503),
            Ok(204),
        ]);
        let settings = ResolvedHealthCheck {
            enabled: true,
            url: Some("http://127.0.0.1:3000/api/health".to_string()),
            startup_timeout: Duration::from_secs(5),
            retry_interval: Duration::from_millis(15),
            request_timeout: Duration::from_secs(1),
        };
        let (_tx, rx) = watch::channel(false);

        // Act
        let outcome = wait_for_startup_health(&settings, &probe, rx).await;

        // Assert
        assert!(matches!(outcome, HealthCheckOutcome::Healthy { .. }));
        assert_eq!(probe.call_count(), 3);
    }

    #[tokio::test]
    async fn wait_for_startup_health_should_time_out_after_startup_timeout() {
        // Arrange
        let probe = FakeProbe::new(vec![Ok(404), Ok(404), Ok(404), Ok(404)]);
        let settings = ResolvedHealthCheck {
            enabled: true,
            url: Some("http://127.0.0.1:3000/api/missing".to_string()),
            startup_timeout: Duration::from_millis(80),
            retry_interval: Duration::from_millis(25),
            request_timeout: Duration::from_millis(25),
        };
        let (_tx, rx) = watch::channel(false);

        // Act
        let outcome = wait_for_startup_health(&settings, &probe, rx).await;

        // Assert
        assert_eq!(
            outcome,
            HealthCheckOutcome::TimedOut {
                last_result: "HTTP 404".to_string()
            }
        );
    }

    #[tokio::test]
    async fn wait_for_startup_health_should_never_call_probe_when_disabled() {
        // Arrange
        let probe = FakeProbe::new(vec![Ok(200)]);
        let settings = ResolvedHealthCheck {
            enabled: false,
            url: None,
            startup_timeout: Duration::from_secs(5),
            retry_interval: Duration::from_millis(20),
            request_timeout: Duration::from_secs(1),
        };
        let (_tx, rx) = watch::channel(false);

        // Act
        let outcome = wait_for_startup_health(&settings, &probe, rx).await;

        // Assert
        assert_eq!(outcome, HealthCheckOutcome::Disabled);
        assert_eq!(probe.call_count(), 0);
    }
}
