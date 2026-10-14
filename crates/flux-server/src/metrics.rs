//! Minimal Prometheus metrics: request counts and latency histograms per
//! surface (gRPC method family or web), rendered at `/-/metrics`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const BUCKETS_MS: [f64; 12] = [0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0];

struct Histo {
    count: AtomicU64,
    errors: AtomicU64,
    sum_us: AtomicU64,
    buckets: [AtomicU64; 12],
}

impl Histo {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            buckets: [const { AtomicU64::new(0) }; 12],
        }
    }

    fn observe(&self, ms: f64, error: bool) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_us.fetch_add((ms * 1000.0) as u64, Ordering::Relaxed);
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            if ms <= *b {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

static GRPC: Histo = Histo::new();
static WEB: Histo = Histo::new();

pub fn observe(grpc: bool, started: Instant, error: bool) {
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    if grpc { GRPC.observe(ms, error) } else { WEB.observe(ms, error) }
}

pub fn render() -> String {
    let mut out = String::from(
        "# TYPE flux_requests_total counter\n# TYPE flux_request_errors_total counter\n# TYPE flux_request_duration_ms histogram\n",
    );
    for (name, h) in [("grpc", &GRPC), ("web", &WEB)] {
        out.push_str(&format!("flux_requests_total{{surface=\"{name}\"}} {}\n", h.count.load(Ordering::Relaxed)));
        out.push_str(&format!("flux_request_errors_total{{surface=\"{name}\"}} {}\n", h.errors.load(Ordering::Relaxed)));
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            out.push_str(&format!(
                "flux_request_duration_ms_bucket{{surface=\"{name}\",le=\"{b}\"}} {}\n",
                h.buckets[i].load(Ordering::Relaxed)
            ));
        }
        out.push_str(&format!(
            "flux_request_duration_ms_bucket{{surface=\"{name}\",le=\"+Inf\"}} {}\n",
            h.count.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "flux_request_duration_ms_sum{{surface=\"{name}\"}} {}\n",
            h.sum_us.load(Ordering::Relaxed) as f64 / 1000.0
        ));
        out.push_str(&format!("flux_request_duration_ms_count{{surface=\"{name}\"}} {}\n", h.count.load(Ordering::Relaxed)));
    }
    out
}
