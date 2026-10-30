//! Prometheus text exposition of the telemetry counters and caller-supplied gauges.

use std::fmt::Write as _;

use super::{TaskCounters, Telemetry};

impl Telemetry {
    /// Render counters plus caller-supplied gauges in the Prometheus text format (0.0.4).
    #[expect(
        clippy::too_many_lines,
        reason = "one flat list of metric families is easier to audit than helpers"
    )]
    pub(crate) fn render_prometheus(&self, gauges: &[Gauge]) -> String {
        let mut out = String::new();
        let inner = self.lock();
        let family = |out: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };
        let task_series = |out: &mut String, name: &str, pick: &dyn Fn(&TaskCounters) -> f64| {
            for ((model, backend, task, outcome), counters) in &inner.tasks {
                let _ = writeln!(
                    out,
                    "{name}{{model=\"{}\",backend=\"{}\",task=\"{}\",outcome=\"{}\"}} {}",
                    escape(model),
                    escape(backend),
                    escape(task),
                    escape(outcome),
                    pick(counters)
                );
            }
        };
        let seconds = |ms: u64| super::super::float(ms) / 1000.0;

        family(
            &mut out,
            "freellama_tasks_total",
            "counter",
            "Managed tasks finished, by outcome.",
        );
        task_series(&mut out, "freellama_tasks_total", &|c| {
            super::super::float(c.count)
        });
        family(
            &mut out,
            "freellama_prompt_tokens_total",
            "counter",
            "Prompt tokens reported by Ollama.",
        );
        task_series(&mut out, "freellama_prompt_tokens_total", &|c| {
            super::super::float(c.prompt_tokens)
        });
        family(
            &mut out,
            "freellama_output_tokens_total",
            "counter",
            "Output tokens reported by Ollama.",
        );
        task_series(&mut out, "freellama_output_tokens_total", &|c| {
            super::super::float(c.output_tokens)
        });
        family(
            &mut out,
            "freellama_task_seconds_total",
            "counter",
            "Wall time of managed tasks, queueing included.",
        );
        task_series(&mut out, "freellama_task_seconds_total", &|c| {
            seconds(c.duration_ms)
        });
        family(
            &mut out,
            "freellama_queue_wait_seconds_total",
            "counter",
            "Time managed tasks spent waiting for admission.",
        );
        task_series(&mut out, "freellama_queue_wait_seconds_total", &|c| {
            seconds(c.queue_wait_ms)
        });
        family(
            &mut out,
            "freellama_load_seconds_total",
            "counter",
            "Model load time reported by Ollama.",
        );
        task_series(&mut out, "freellama_load_seconds_total", &|c| {
            seconds(c.load_ms)
        });

        family(
            &mut out,
            "freellama_raw_requests_total",
            "counter",
            "Raw passthrough requests, by kind and outcome.",
        );
        for ((kind, outcome), count) in &inner.raw {
            let _ = writeln!(
                out,
                "freellama_raw_requests_total{{kind=\"{}\",outcome=\"{}\"}} {count}",
                escape(kind),
                escape(outcome)
            );
        }
        family(
            &mut out,
            "freellama_evictions_total",
            "counter",
            "Idle models unloaded to make room.",
        );
        for (backend, count) in &inner.evictions {
            let _ = writeln!(
                out,
                "freellama_evictions_total{{backend=\"{}\"}} {count}",
                escape(backend)
            );
        }
        family(
            &mut out,
            "freellama_circuit_open_total",
            "counter",
            "Times an upstream circuit breaker opened.",
        );
        for (backend, count) in &inner.breaker_opens {
            let _ = writeln!(
                out,
                "freellama_circuit_open_total{{backend=\"{}\"}} {count}",
                escape(backend)
            );
        }
        family(
            &mut out,
            "freellama_concurrency_limit_changes_total",
            "counter",
            "Adaptive concurrency adjustments.",
        );
        for ((backend, direction), count) in &inner.limit_changes {
            let _ = writeln!(
                out,
                "freellama_concurrency_limit_changes_total{{backend=\"{}\",direction=\"{}\"}} {count}",
                escape(backend),
                escape(direction)
            );
        }
        drop(inner);

        let mut current: Option<&str> = None;
        for gauge in gauges {
            if current != Some(gauge.name) {
                family(&mut out, gauge.name, gauge.kind, gauge.help);
                current = Some(gauge.name);
            }
            let labels = gauge
                .labels
                .iter()
                .map(|(key, value)| format!("{key}=\"{}\"", escape(value)))
                .collect::<Vec<_>>()
                .join(",");
            if labels.is_empty() {
                let _ = writeln!(out, "{} {}", gauge.name, gauge.value);
            } else {
                let _ = writeln!(out, "{}{{{labels}}} {}", gauge.name, gauge.value);
            }
        }
        family(
            &mut out,
            "freellama_uptime_seconds",
            "gauge",
            "Seconds since this FreeLlama process started.",
        );
        let _ = writeln!(
            out,
            "freellama_uptime_seconds {}",
            self.started.elapsed().as_secs()
        );
        out
    }
}

/// One sample of a gauge; consecutive samples with the same name form one metric family.
pub(crate) struct Gauge {
    pub(crate) name: &'static str,
    pub(crate) help: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) labels: Vec<(&'static str, String)>,
    pub(crate) value: f64,
}

impl Gauge {
    pub(crate) fn new(name: &'static str, help: &'static str, value: f64) -> Self {
        Self {
            name,
            help,
            kind: "gauge",
            labels: Vec::new(),
            value,
        }
    }

    #[must_use]
    pub(crate) fn label(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.labels.push((key, value.into()));
        self
    }

    #[must_use]
    pub(crate) fn counter(mut self) -> Self {
        self.kind = "counter";
        self
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
