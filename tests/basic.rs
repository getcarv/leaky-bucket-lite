use leaky_bucket_lite::Builder;

use std::time::{Duration, Instant};

#[tokio::test]
async fn test_leaky_bucket() {
    let interval = Duration::from_millis(20);

    let leaky = Builder::new()
        .tokens(0)
        .max(10)
        .refill_amount(10)
        .refill_interval(interval)
        .build();

    let mut wakeups = 0u32;
    let mut duration = None;

    let test = async {
        let start = Instant::now();
        leaky.acquire(10).await;
        wakeups += 1;
        leaky.acquire(10).await;
        wakeups += 1;
        leaky.acquire(10).await;
        wakeups += 1;
        duration = Some(Instant::now().duration_since(start));
    };

    test.await;

    assert_eq!(3, wakeups);
    assert!(duration.expect("expected measured duration") > interval * 2);
}

#[tokio::test]
async fn test_refill() {
    let rate_limiter = Builder::new()
        .max(5)
        .tokens(0)
        .refill_interval(Duration::from_secs(5))
        .refill_amount(1)
        .build();
    let begin = Instant::now();
    // should take about 5 seconds to acquire.
    let rate_limiter_clone = rate_limiter.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        rate_limiter_clone.refill(4).await;
    });
    rate_limiter.acquire(5).await;
    let elapsed = Instant::now().duration_since(begin);
    println!("Elapsed: {:?}", elapsed);
    assert!((elapsed.as_secs_f64() - 5.).abs() < 0.1);
}
