use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorKind {
    ContextLength,
    RateLimit,
    Network,
    Auth,
    Other,
}

pub struct RecoveryPolicy {
    max_retries: usize,
    backoff_base: Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_base: Duration::from_secs(1),
        }
    }
}

impl RecoveryPolicy {
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    pub fn should_retry(&self, attempts: usize, kind: ErrorKind) -> bool {
        if attempts >= self.max_retries {
            return false;
        }
        matches!(kind, ErrorKind::Network | ErrorKind::RateLimit)
    }

    pub fn backoff_duration(&self, attempts: usize) -> Duration {
        let exp = 1u64 << attempts.min(6); // cap at 2^6 = 64s
        let base = self.backoff_base.as_millis() as u64;
        let ms = base.saturating_mul(exp);
        // Additive jitter up to +25% so concurrent agents don't retry in
        // lockstep against a rate-limited endpoint. Never shorter than the
        // policy minimum. Seeded from the system clock — pseudo-random is
        // sufficient here.
        let jitter = pseudo_random(attempts as u64) % (ms / 4).max(1);
        Duration::from_millis(ms.saturating_add(jitter))
    }
}

fn pseudo_random(salt: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // splitmix64 finalizer for decent dispersion
    let mut z = nanos.wrapping_add(salt).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn classify_error(msg: &str) -> ErrorKind {
    let lower = msg.to_lowercase();

    // Auth: HTTP status codes in error context
    if lower.contains(" 401 ")
        || lower.contains(" 403 ")
        || lower.contains("error 401")
        || lower.contains("error 403")
        || lower.starts_with("401 ")
        || lower.starts_with("403 ")
    {
        return ErrorKind::Auth;
    }

    if lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("authentication failed")
    {
        return ErrorKind::Auth;
    }

    if lower.contains("rate limit") || lower.contains("too many requests") {
        return ErrorKind::RateLimit;
    }

    if lower.contains(" 429 ") || lower.contains("error 429") || lower.starts_with("429 ") {
        return ErrorKind::RateLimit;
    }

    // HTTP status codes for server errors (502/503/504 are unambiguous)
    if lower.contains(" 503 ")
        || lower.contains(" 502 ")
        || lower.contains(" 504 ")
        || lower.starts_with("503 ")
        || lower.starts_with("502 ")
        || lower.starts_with("504 ")
    {
        return ErrorKind::Network;
    }

    // Context-length indicators
    if lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("reduce the length of the messages")
        || lower.contains("request too large")
    {
        return ErrorKind::ContextLength;
    }

    // Network errors — check for specific phrases (avoid "connection" false positive)
    if lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("dns error")
        || lower.contains("tls")
        || lower.contains("ssl")
        || lower.contains("timed out")
        || lower.contains("request timeout")
        || lower.contains("server error")
    {
        return ErrorKind::Network;
    }

    ErrorKind::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_context_length() {
        assert_eq!(
            classify_error("context_length_exceeded: prompt too long"),
            ErrorKind::ContextLength
        );
        assert_eq!(
            classify_error("reduce the length of the messages"),
            ErrorKind::ContextLength
        );
        assert_eq!(
            classify_error("request too large for model"),
            ErrorKind::ContextLength
        );
    }

    #[test]
    fn test_classify_network() {
        assert_eq!(classify_error("connection refused"), ErrorKind::Network);
        assert_eq!(
            classify_error("connection reset by peer"),
            ErrorKind::Network
        );
        assert_eq!(classify_error("request timed out"), ErrorKind::Network);
        assert_eq!(
            classify_error("503 service unavailable"),
            ErrorKind::Network
        );
    }

    #[test]
    fn test_classify_rate_limit() {
        assert_eq!(classify_error("rate limit exceeded"), ErrorKind::RateLimit);
        assert_eq!(
            classify_error("429 too many requests"),
            ErrorKind::RateLimit
        );
    }

    #[test]
    fn test_classify_auth() {
        assert_eq!(classify_error("401 unauthorized"), ErrorKind::Auth);
        assert_eq!(classify_error("invalid api key"), ErrorKind::Auth);
    }

    #[test]
    fn test_classify_other() {
        assert_eq!(classify_error("something else"), ErrorKind::Other);
        assert_eq!(classify_error("file not found"), ErrorKind::Other);
        // "connection" alone should not trigger network
        assert_eq!(
            classify_error("database connection closed"),
            ErrorKind::Other
        );
        // "reset" alone should not trigger
        assert_eq!(classify_error("form reset successful"), ErrorKind::Other);
        // "500" in non-HTTP context should not trigger
        assert_eq!(classify_error("processed 500 items"), ErrorKind::Other);
    }

    #[test]
    fn test_retry_policy() {
        let policy = RecoveryPolicy::default();

        // Network errors are retryable
        assert!(policy.should_retry(0, ErrorKind::Network));
        assert!(policy.should_retry(1, ErrorKind::Network));
        assert!(policy.should_retry(2, ErrorKind::Network));
        assert!(!policy.should_retry(3, ErrorKind::Network));

        // Rate limits are retryable
        assert!(policy.should_retry(0, ErrorKind::RateLimit));

        // Context length is NOT retryable (needs compaction)
        assert!(!policy.should_retry(0, ErrorKind::ContextLength));

        // Auth is not retryable
        assert!(!policy.should_retry(0, ErrorKind::Auth));

        // Other is not retryable
        assert!(!policy.should_retry(0, ErrorKind::Other));
    }

    #[test]
    fn test_backoff_duration() {
        let policy = RecoveryPolicy::default();
        let d0 = policy.backoff_duration(0);
        let d1 = policy.backoff_duration(1);
        let d2 = policy.backoff_duration(2);

        assert!(d0 >= Duration::from_secs(1));
        assert!(d1 >= Duration::from_secs(2));
        assert!(d2 >= Duration::from_secs(4));
    }

    #[test]
    fn test_backoff_overflow_guard() {
        let policy = RecoveryPolicy::default();
        let d = policy.backoff_duration(20); // capped at attempts=6 via min()
        // 1s * 2^6 = 64s plus up to +25% jitter = 80s ceiling
        assert!(d >= Duration::from_secs(64));
        assert!(d < Duration::from_secs(81));
    }

    #[test]
    fn test_backoff_jitter_present() {
        let policy = RecoveryPolicy::default();
        // Repeated calls at the same attempt count should yield differing values
        // most of the time. Run a small batch and confirm we see at least two
        // distinct values — proves jitter is wired in.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            seen.insert(policy.backoff_duration(3));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            seen.len() > 1,
            "expected jittered backoff to vary across calls"
        );
    }
}
