use std::time::Instant;

pub struct Timings {
    command: &'static str,
    enabled: bool,
    json: bool,
    start: Instant,
    last: Instant,
    entries: Vec<TimingEntry>,
    counters: Vec<TimingCounter>,
}

struct TimingEntry {
    name: &'static str,
    duration_ms: f64,
}

struct TimingCounter {
    name: &'static str,
    value: u64,
}

impl Timings {
    pub fn from_env(command: &'static str) -> Self {
        let value = std::env::var("SEM_TIMINGS").unwrap_or_default();
        let enabled = !matches!(value.as_str(), "" | "0" | "false" | "off");
        let now = Instant::now();
        Self {
            command,
            enabled,
            json: value == "json",
            start: now,
            last: now,
            entries: Vec::new(),
            counters: Vec::new(),
        }
    }

    pub fn disabled(command: &'static str) -> Self {
        let now = Instant::now();
        Self {
            command,
            enabled: false,
            json: false,
            start: now,
            last: now,
            entries: Vec::new(),
            counters: Vec::new(),
        }
    }

    pub fn mark(&mut self, name: &'static str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        self.entries.push(TimingEntry {
            name,
            duration_ms: elapsed_ms(now.duration_since(self.last)),
        });
        self.last = now;
    }

    pub fn counter(&mut self, name: &'static str, value: u64) {
        if !self.enabled {
            return;
        }
        self.counters.push(TimingCounter { name, value });
    }

    pub fn finish(&self) {
        if !self.enabled {
            return;
        }
        let total_ms = elapsed_ms(self.start.elapsed());
        if self.json {
            let phases = self
                .entries
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "name": entry.name,
                        "durationMs": entry.duration_ms,
                    })
                })
                .collect::<Vec<_>>();
            let mut output = serde_json::json!({
                "command": self.command,
                "phases": phases,
                "totalMs": total_ms,
            });
            if !self.counters.is_empty() {
                output["counters"] = serde_json::json!(self
                    .counters
                    .iter()
                    .map(|counter| {
                        serde_json::json!({
                            "name": counter.name,
                            "value": counter.value,
                        })
                    })
                    .collect::<Vec<_>>());
            }
            eprintln!("{}", serde_json::to_string(&output).unwrap());
        } else {
            eprintln!("sem timings ({})", self.command);
            for entry in &self.entries {
                eprintln!("  {:<32} {:>8.3} ms", entry.name, entry.duration_ms);
            }
            for counter in &self.counters {
                eprintln!("  {:<32} {:>8}", counter.name, counter.value);
            }
            eprintln!("  {:<32} {:>8.3} ms", "total", total_ms);
        }
    }
}

fn elapsed_ms(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
