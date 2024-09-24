use tokio::time::Instant;

pub struct RateLimiter {
    tokens: f64,
    pub last_update: Instant,
    rate: f64,
    capacity: f64,
}

// Implementation of https://en.wikipedia.org/wiki/Token_bucket
impl RateLimiter {
    pub fn new(rate: f64) -> Self {
        RateLimiter {
            tokens: rate,
            last_update: Instant::now(),
            rate,
            capacity: rate,
        }
    }

    pub fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f64();

        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_update = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
