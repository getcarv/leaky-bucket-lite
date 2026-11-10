use crate::TryAcquireError;
use flume::{unbounded, Receiver, Sender};
use std::sync::Arc;
use tokio::{
    sync::{Mutex, MutexGuard},
    time::{sleep_until, Duration, Instant},
};

#[derive(Debug)]
struct LeakyBucketInner {
    locked: Mutex<LeakyBucketInnerLocked>,
    /// Sender for refilling the bucket.
    refill_tx: Sender<u32>,
    /// Receiver for refilling the bucket.
    refill_rx: Receiver<u32>,
}

#[derive(Debug)]
struct LeakyBucketInnerLocked {
    /// How many tokens this bucket can hold.
    max: u32,
    /// Interval at which the bucket gains tokens.
    refill_interval: Duration,
    /// Amount of tokens gained per interval.
    refill_amount: u32,
    /// Current tokens in the bucket.
    tokens: u32,
    /// Last refill of the tokens.
    last_refill: Instant,
    first_refill_at: Option<Instant>,
}

impl LeakyBucketInnerLocked {
    fn next_refill(&self) -> Instant {
        self.first_refill_at
            .unwrap_or_else(|| self.last_refill + self.refill_interval)
    }
}

impl LeakyBucketInner {
    fn new(
        max: u32,
        tokens: u32,
        refill_interval: Duration,
        refill_amount: u32,
        first_refill_at: Option<Instant>,
    ) -> Self {
        let (refill_tx, refill_rx) = unbounded();
        Self {
            locked: Mutex::new(LeakyBucketInnerLocked {
                max,
                refill_interval,
                refill_amount,
                tokens,
                last_refill: Instant::now(),
                first_refill_at,
            }),
            refill_tx,
            refill_rx,
        }
    }

    /// Updates the tokens in the leaky bucket and returns the current amount
    /// of tokens in the bucket.
    #[inline]
    fn update_tokens(locked: &mut MutexGuard<'_, LeakyBucketInnerLocked>) -> u32 {
        let now = Instant::now();
        if let Some(first_refill) = locked.first_refill_at {
            if now < first_refill {
                return locked.tokens;
            }
            locked.first_refill_at = None;
            locked.last_refill = first_refill;
            locked.tokens = locked
                .tokens
                .saturating_add(locked.refill_amount)
                .min(locked.max);
        }
        let time_passed = now - locked.last_refill;
        let refill_interval = locked.refill_interval;
        let refill_amount = locked.refill_amount;

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let refills_since =
            (time_passed.as_secs_f64() / refill_interval.as_secs_f64()).floor() as u32;

        locked.tokens = locked
            .tokens
            .saturating_add(refill_amount.saturating_mul(refills_since));
        locked.last_refill += refill_interval * refills_since;

        locked.tokens = locked.tokens.min(locked.max);

        locked.tokens
    }

    #[inline]
    async fn tokens(&self) -> u32 {
        Self::update_tokens(&mut self.locked.lock().await)
    }

    async fn next_refill(&self) -> Instant {
        let mut locked = self.locked.lock().await;
        Self::update_tokens(&mut locked);

        locked.next_refill()
    }

    async fn acquire(&self, amount: u32) {
        let mut locked = self.locked.lock().await;
        if let Err(target_time) = Self::try_acquire_locked(amount, &mut locked) {
            loop {
                tokio::select! {
                    Ok(refill) = self.refill_rx.recv_async() => {
                        locked.tokens = locked.max.min(locked.tokens + refill);
                    }
                    _ = sleep_until(target_time) => {}
                }

                Self::update_tokens(&mut locked);
                if locked.tokens >= amount {
                    locked.tokens -= amount;
                    return;
                }
            }
        }
    }

    pub async fn refill(&self, amount: u32) {
        if let Ok(mut locked) = self.locked.try_lock() {
            locked.tokens = locked.max.min(locked.tokens + amount);
        } else {
            self.refill_tx.send_async(amount).await.unwrap();
        }
    }

    fn try_acquire(&self, amount: u32) -> Result<(), TryAcquireError> {
        Self::try_acquire_locked(
            amount,
            &mut self
                .locked
                .try_lock()
                .map_err(|_e| TryAcquireError::new_locked())?,
        )
        .map_err(|i| TryAcquireError::new_insufficient_tokens(i.into()))
    }

    fn try_acquire_locked(
        amount: u32,
        locked: &mut MutexGuard<'_, LeakyBucketInnerLocked>,
    ) -> Result<(), Instant> {
        assert!(
            amount <= locked.max,
            "Acquiring more tokens than the configured maximum is not possible"
        );

        let current_tokens = Self::update_tokens(locked);

        if current_tokens < amount {
            let tokens_needed = amount - current_tokens;
            let refill_amount = locked.refill_amount;
            let refill_interval = locked.refill_interval;
            let mut refills_needed = tokens_needed / refill_amount;
            let refills_needed_remainder = tokens_needed % refill_amount;

            if refills_needed_remainder > 0 {
                refills_needed += 1;
            }

            Err(locked.next_refill() + refill_interval * (refills_needed - 1))
        } else {
            locked.tokens -= amount;
            Ok(())
        }
    }
}

/// The leaky bucket.
#[derive(Clone, Debug)]
pub struct LeakyBucket {
    inner: Arc<LeakyBucketInner>,
}

impl LeakyBucket {
    fn new(
        max: u32,
        tokens: u32,
        refill_interval: Duration,
        refill_amount: u32,
        first_refill_at: Option<Instant>,
    ) -> Self {
        let inner = Arc::new(LeakyBucketInner::new(
            max,
            tokens,
            refill_interval,
            refill_amount,
            first_refill_at,
        ));

        Self { inner }
    }

    /// Construct a new leaky bucket through a builder.
    #[must_use]
    pub const fn builder() -> Builder {
        Builder::new()
    }

    /// Get the max number of tokens this rate limiter is configured for.
    pub async fn max(&self) -> u32 {
        self.inner.locked.lock().await.max
    }

    /// Get the current number of tokens available.
    pub async fn tokens(&self) -> u32 {
        self.inner.tokens().await
    }

    /// Set the maximum number of tokens. Clamps current tokens to the new max.
    pub async fn set_max(&self, max: u32) {
        let mut locked = self.inner.locked.lock().await;
        locked.max = max;
        locked.tokens = locked.tokens.min(max);
    }

    /// Set the current number of tokens. Clamps to the configured max.
    pub async fn set_tokens(&self, tokens: u32) {
        let mut locked = self.inner.locked.lock().await;
        locked.tokens = tokens.min(locked.max);
    }

    /// Atomically set both the max and current tokens under a single lock.
    /// Tokens are clamped to the new max.
    pub async fn set_state(&self, max: u32, tokens: u32) {
        let mut locked = self.inner.locked.lock().await;
        locked.max = max;
        locked.tokens = tokens.min(max);
    }

    /// Set the refill amount (tokens added per interval).
    ///
    /// # Panics
    ///
    /// Panics if `amount` is zero.
    pub async fn set_refill_amount(&self, amount: u32) {
        assert!(amount > 0, "Refill amount must be greater than zero");
        self.inner.locked.lock().await.refill_amount = amount;
    }

    /// Set the refill interval.
    ///
    /// # Panics
    ///
    /// Panics if `interval` is zero.
    pub async fn set_refill_interval(&self, interval: Duration) {
        assert!(!interval.is_zero(), "Refill interval must be greater than zero");
        self.inner.locked.lock().await.refill_interval = interval;
    }

    /// Get the next time at which the tokens will be refilled.
    pub async fn next_refill(&self) -> Instant {
        self.inner.next_refill().await
    }

    /// Acquire a single token.
    ///
    /// This is identical to [`acquire`] with an argument of `1.0`.
    ///
    /// [`acquire`]: LeakyBucket::acquire
    ///
    /// # Example
    ///
    /// ```rust
    /// use leaky_bucket_lite::LeakyBucket;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let rate_limiter = LeakyBucket::builder()
    ///         .max(5)
    ///         .tokens(0)
    ///         .refill_interval(Duration::from_secs(5))
    ///         .refill_amount(1)
    ///         .build();
    ///
    ///     println!("Waiting for permit...");
    ///     // should take about 5 seconds to acquire.
    ///     rate_limiter.acquire_one().await;
    ///     println!("I made it!");
    /// }
    /// ```
    #[inline]
    pub async fn acquire_one(&self) {
        self.acquire(1).await;
    }

    /// Acquire the given `amount` of tokens.
    ///
    /// # Example
    ///
    /// ```rust
    /// use leaky_bucket_lite::LeakyBucket;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let rate_limiter = LeakyBucket::builder()
    ///         .max(5)
    ///         .tokens(0)
    ///         .refill_interval(Duration::from_secs(5))
    ///         .refill_amount(1)
    ///         .build();
    ///
    ///     println!("Waiting for permit...");
    ///     // should take about 25 seconds to acquire.
    ///     rate_limiter.acquire(5).await;
    ///     println!("I made it!");
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// This method will panic when acquiring more tokens than the configured maximum.
    #[inline]
    pub async fn acquire(&self, amount: u32) {
        self.inner.acquire(amount).await;
    }

    /// Refill the given `amount` of tokens.
    ///
    /// # Example
    ///
    /// ```rust
    /// use leaky_bucket_lite::LeakyBucket;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let rate_limiter = LeakyBucket::builder()
    ///         .max(5)
    ///         .tokens(0)
    ///         .refill_interval(Duration::from_secs(5))
    ///         .refill_amount(1)
    ///         .build();
    ///
    ///     println!("Waiting for permit...");
    ///     // should take about 5 seconds to acquire.
    ///     let rate_limiter_clone = rate_limiter.clone();
    ///     tokio::spawn(async move {
    ///         tokio::time::sleep(Duration::from_secs(5)).await;
    ///         rate_limiter_clone.refill(4).await;
    ///     });
    ///     rate_limiter.acquire(5).await;
    ///     println!("I made it!");
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// This method will panic when acquiring more tokens than the configured maximum.
    #[inline]
    pub async fn refill(&self, amount: u32) {
        self.inner.refill(amount).await;
    }

    /// Try to acquire one token, without waiting for the internal lock.
    ///
    /// # Errors
    ///
    /// A [`TryAcquireError`] is returned if the operation can't be completed immediately.
    ///
    /// # Example
    ///
    /// ```rust
    /// use leaky_bucket_lite::LeakyBucket;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let rate_limiter = LeakyBucket::builder()
    ///         .max(5)
    ///         .tokens(1)
    ///         .refill_interval(Duration::from_secs(1))
    ///         .refill_amount(1)
    ///         .build();
    ///
    ///     assert!(matches!(rate_limiter.try_acquire_one(), Ok(())));
    ///     assert!(matches!(rate_limiter.try_acquire_one(), Err(e) if e.is_insufficient_tokens()));
    ///     tokio::spawn({
    ///         let rate_limiter = rate_limiter.clone();
    ///         async move {
    ///             rate_limiter.acquire(2).await;
    ///         }
    ///     });
    ///     tokio::time::sleep(Duration::from_millis(100)).await;
    ///     assert!(matches!(rate_limiter.try_acquire_one(), Err(e) if e.is_locked()));
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// This method will panic when acquiring more tokens than the configured maximum.
    #[inline]
    pub fn try_acquire_one(&self) -> Result<(), TryAcquireError> {
        self.try_acquire(1)
    }

    /// Try to acquire the given `amount` of tokens, without waiting for the internal lock.
    ///
    /// # Errors
    ///
    /// A [`TryAcquireError`] is returned if the operation can't be completed immediately.
    ///
    /// # Example
    ///
    /// ```rust
    /// use leaky_bucket_lite::LeakyBucket;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let rate_limiter = LeakyBucket::builder()
    ///         .max(5)
    ///         .tokens(1)
    ///         .refill_interval(Duration::from_secs(1))
    ///         .refill_amount(1)
    ///         .build();
    ///
    ///     assert!(matches!(rate_limiter.try_acquire(1), Ok(())));
    ///     assert!(matches!(rate_limiter.try_acquire(1), Err(e) if e.is_insufficient_tokens()));
    ///     tokio::spawn({
    ///         let rate_limiter = rate_limiter.clone();
    ///         async move {
    ///             rate_limiter.acquire(2).await;
    ///         }
    ///     });
    ///     tokio::time::sleep(Duration::from_millis(100)).await;
    ///     assert!(matches!(rate_limiter.try_acquire(1), Err(e) if e.is_locked()));
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// This method will panic when acquiring more tokens than the configured maximum.
    #[inline]
    pub fn try_acquire(&self, amount: u32) -> Result<(), TryAcquireError> {
        self.inner.try_acquire(amount)
    }
}

/// Builder for a leaky bucket.
#[derive(Debug)]
pub struct Builder {
    max: Option<u32>,
    tokens: Option<u32>,
    refill_interval: Option<Duration>,
    refill_amount: Option<u32>,
    first_refill_at: Option<Instant>,
}

impl Builder {
    /// Create a new builder with all defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max: None,
            tokens: None,
            refill_interval: None,
            refill_amount: None,
            first_refill_at: None,
        }
    }

    /// Set the max value for the builder.
    #[must_use]
    pub const fn max(mut self, max: u32) -> Self {
        self.max = Some(max);
        self
    }

    /// The number of tokens that the bucket should start with.
    ///
    /// If set to larger than `max` at build time, will only saturate to max.
    #[must_use]
    pub const fn tokens(mut self, tokens: u32) -> Self {
        self.tokens = Some(tokens);
        self
    }

    /// Set the max value for the builder.
    #[must_use]
    pub const fn refill_interval(mut self, refill_interval: Duration) -> Self {
        self.refill_interval = Some(refill_interval);
        self
    }

    /// Set the refill amount to use.
    #[must_use]
    pub const fn refill_amount(mut self, refill_amount: u32) -> Self {
        self.refill_amount = Some(refill_amount);
        self
    }

    /// Set the deadline of the first automatic refill.
    ///
    /// Later refills follow the configured interval from that first refill.
    #[must_use]
    pub const fn first_refill_at(mut self, deadline: Instant) -> Self {
        self.first_refill_at = Some(deadline);
        self
    }

    /// Construct a new leaky bucket.
    #[must_use]
    pub fn build(self) -> LeakyBucket {
        const DEFAULT_MAX: u32 = 120;
        const DEFAULT_TOKENS: u32 = 0;
        const DEFAULT_REFILL_INTERVAL: Duration = Duration::from_secs(1);
        const DEFAULT_REFILL_AMOUNT: u32 = 1;

        let max = self.max.unwrap_or(DEFAULT_MAX);
        let tokens = self.tokens.unwrap_or(DEFAULT_TOKENS);
        let refill_interval = self.refill_interval.unwrap_or(DEFAULT_REFILL_INTERVAL);
        let refill_amount = self.refill_amount.unwrap_or(DEFAULT_REFILL_AMOUNT);

        LeakyBucket::new(
            max,
            tokens,
            refill_interval,
            refill_amount,
            self.first_refill_at,
        )
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}
