//! The primary metric registry of SPEC 17.4: identifiers, kinds,
//! definitions, and the C1/C2 budgets the result records carry.
//!
//! The budgets are copied from the SPEC table and stay fixed here; a
//! change to any number requires the ADR process of SPEC 17.7, not an
//! edit that quietly loosens a gate.

/// How one metric is measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Wall time of one fresh CLI process, before creation through exit
    /// and full response delivery (SPEC 17.3).
    Cli,
    /// PTY interaction timing: launch-to-frame, key-to-bytes, or
    /// edit-to-indicator, measured against a real pseudo-terminal (SPEC
    /// 17.4) by the `pty` sampler module.
    Pty,
    /// A resource quantity: idle CPU, RSS, or process-tree memory, read
    /// from the kernel's own accounting by the `resource` sampler module.
    Resource,
}

impl Kind {
    /// The record spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Cli => "cli-fresh-process",
            Kind::Pty => "pty-interaction",
            Kind::Resource => "resource",
        }
    }
}

/// The budget shape of one metric row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Budget {
    /// A p95 latency budget in milliseconds.
    P95Ms(u64),
    /// A p95 and p99 latency budget in milliseconds (P-KEY).
    P95P99Ms(u64, u64),
    /// A ceiling on five-minute idle CPU as a percentage of one core.
    IdleCpuPercent(f64),
    /// A ceiling on peak memory in MiB.
    PeakMemoryMiB(u64),
}

impl Budget {
    /// The `budget_p95_ms` field of a result record, when the budget is
    /// a p95 latency.
    pub fn p95_ms(&self) -> Option<u64> {
        match *self {
            Budget::P95Ms(p95) => Some(p95),
            Budget::P95P99Ms(p95, _) => Some(p95),
            _ => None,
        }
    }

    /// The human sentence a record prints beside the raw numbers.
    pub fn describe(&self) -> String {
        match *self {
            Budget::P95Ms(ms) => format!("p95 <= {ms} ms"),
            Budget::P95P99Ms(p95, p99) => format!("p95 <= {p95} ms, p99 <= {p99} ms"),
            Budget::IdleCpuPercent(p) => format!("<= {p}% of one core over five idle minutes"),
            Budget::PeakMemoryMiB(mib) => format!("peak <= {mib} MiB"),
        }
    }
}

/// One row of the SPEC 17.4 table.
#[derive(Clone, Copy, Debug)]
pub struct MetricDef {
    /// The metric identifier, such as `P-DIFF`.
    pub id: &'static str,
    /// How the metric is measured.
    pub kind: Kind,
    /// The metric definition in one sentence.
    pub about: &'static str,
    /// The C1 budget.
    pub c1: Budget,
    /// The C2 budget.
    pub c2: Budget,
}

impl MetricDef {
    /// The budget of one corpus label (`C1` or `C2`); C3 carries no
    /// latency budget (SPEC 17.5: ordinary C1/C2 budgets do not apply).
    pub fn budget(&self, corpus: &str) -> Option<Budget> {
        match corpus {
            "C1" => Some(self.c1),
            "C2" => Some(self.c2),
            _ => None,
        }
    }
}

/// Every primary metric of SPEC 17.4, in table order.
pub const ALL: &[MetricDef] = &[
    MetricDef {
        id: "P-HELP",
        kind: Kind::Cli,
        about: "Root help, fresh process",
        c1: Budget::P95Ms(75),
        c2: Budget::P95Ms(75),
    },
    MetricDef {
        id: "P-STATUS",
        kind: Kind::Cli,
        about: "Complete bounded `status --json`",
        c1: Budget::P95Ms(150),
        c2: Budget::P95Ms(300),
    },
    MetricDef {
        id: "P-DIFF",
        kind: Kind::Cli,
        about: "Selected-document review data, full required Git/manifest work",
        c1: Budget::P95Ms(200),
        c2: Budget::P95Ms(500),
    },
    MetricDef {
        id: "P-OPEN",
        kind: Kind::Pty,
        about: "TUI launch to first useful document frame flushed to the PTY",
        c1: Budget::P95Ms(300),
        c2: Budget::P95Ms(750),
    },
    MetricDef {
        id: "P-CHECKPOINT",
        kind: Kind::Cli,
        about: "Dirty scoped checkpoint through verified commit and response",
        c1: Budget::P95Ms(500),
        c2: Budget::P95Ms(1500),
    },
    MetricDef {
        id: "P-APPLY",
        kind: Kind::Cli,
        about: "Selective review through verified review commit and response",
        c1: Budget::P95Ms(750),
        c2: Budget::P95Ms(2000),
    },
    MetricDef {
        id: "P-HISTORY",
        kind: Kind::Cli,
        about: "Latest 50 bounded history entries, fresh process",
        c1: Budget::P95Ms(150),
        c2: Budget::P95Ms(250),
    },
    MetricDef {
        id: "P-KEY",
        kind: Kind::Pty,
        about: "PTY input injection to corresponding changed terminal bytes",
        c1: Budget::P95P99Ms(50, 100),
        c2: Budget::P95P99Ms(50, 100),
    },
    MetricDef {
        id: "P-REFRESH",
        kind: Kind::Pty,
        about: "Selected-file external edit to explicit new-edits/stale indicator",
        c1: Budget::P95Ms(3000),
        c2: Budget::P95Ms(3000),
    },
    MetricDef {
        id: "P-CPU",
        kind: Kind::Resource,
        about: "Five-minute idle CPU time of Margin plus Git children",
        c1: Budget::IdleCpuPercent(1.0),
        c2: Budget::IdleCpuPercent(2.0),
    },
    MetricDef {
        id: "P-RSS",
        kind: Kind::Resource,
        about: "TUI parent peak RSS during the fixed open/review/history sequence",
        c1: Budget::PeakMemoryMiB(64),
        c2: Budget::PeakMemoryMiB(128),
    },
    MetricDef {
        id: "P-TREE-MEM",
        kind: Kind::Resource,
        about: "Peak concurrently sampled process-tree RSS, Margin plus Git children",
        c1: Budget::PeakMemoryMiB(128),
        c2: Budget::PeakMemoryMiB(256),
    },
];

/// Looks one metric up by its exact identifier.
pub fn by_id(id: &str) -> Option<&'static MetricDef> {
    ALL.iter().find(|metric| metric.id == id)
}

/// The metrics this harness times as fresh CLI processes. The PTY and
/// resource metrics have their own samplers; both sets write the same
/// raw sample streams and result records.
pub fn cli_metrics() -> Vec<&'static MetricDef> {
    ALL.iter()
        .filter(|metric| metric.kind == Kind::Cli)
        .collect()
}

/// The corpora a result record exists for: every budget-bearing corpus
/// of SPEC 17.4 plus the C3 stress corpus, whose failures are recorded
/// without latency budgets.
pub const RECORD_CORPORA: &[&str] = &["C1", "C2", "C3"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_matches_spec_17_4() {
        let ids: Vec<&str> = ALL.iter().map(|metric| metric.id).collect();
        assert_eq!(
            ids,
            [
                "P-HELP", "P-STATUS", "P-DIFF", "P-OPEN", "P-CHECKPOINT", "P-APPLY",
                "P-HISTORY", "P-KEY", "P-REFRESH", "P-CPU", "P-RSS", "P-TREE-MEM",
            ]
        );
        assert_eq!(by_id("P-STATUS").unwrap().c1.p95_ms(), Some(150));
        assert_eq!(by_id("P-STATUS").unwrap().c2.p95_ms(), Some(300));
        assert_eq!(by_id("P-DIFF").unwrap().c2.p95_ms(), Some(500));
        assert_eq!(by_id("P-APPLY").unwrap().c2.p95_ms(), Some(2000));
        assert_eq!(by_id("P-KEY").unwrap().c1.p95_ms(), Some(50));
        assert_eq!(
            by_id("P-CPU").unwrap().c2.p95_ms(),
            None,
            "an idle-CPU budget is not a latency budget"
        );
    }

    #[test]
    fn the_example_record_of_spec_17_8_carries_its_budget() {
        // The SPEC's initial example record is P-DIFF on C2 with a 500 ms
        // p95 budget; the registry must reproduce that number.
        let diff = by_id("P-DIFF").expect("P-DIFF exists");
        assert_eq!(diff.budget("C2").and_then(|budget| budget.p95_ms()), Some(500));
        assert_eq!(diff.budget("C3"), None, "C3 carries no latency budget");
    }
}
