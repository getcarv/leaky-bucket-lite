#![cfg(feature = "tokio")]

use leaky_bucket_lite::LeakyBucket;
use tokio::time::{self, Duration, Instant};

#[tokio::test(start_paused = true)]
async fn refills_at_the_first_deadline_then_at_regular_intervals() {
    let first = Instant::now() + Duration::from_secs(2);
    let bucket = LeakyBucket::builder()
        .max(2)
        .tokens(0)
        .refill_amount(2)
        .refill_interval(Duration::from_secs(15))
        .first_refill_at(first)
        .build();

    assert_eq!(
        bucket.try_acquire_one().unwrap_err().target_time(),
        Some(first.into())
    );
    time::advance(Duration::from_secs(1)).await;
    assert_eq!(bucket.tokens().await, 0);
    time::advance(Duration::from_secs(1)).await;
    bucket.try_acquire(2).unwrap();

    assert_eq!(bucket.next_refill().await, first + Duration::from_secs(15));
    time::advance(Duration::from_secs(15)).await;
    assert_eq!(bucket.tokens().await, 2);
}
