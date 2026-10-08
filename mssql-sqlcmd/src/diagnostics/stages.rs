// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Records the connect stages of `mssql-tds` as they happen.
//!
//! `mssql-tds` opens one span per stage of a connection attempt (see
//! [`mssql_tds::connection::connect_stage`]): created when the stage starts,
//! closed when it ends, with `ok = true` recorded first if it succeeded.
//! [`StageRecorder`] is a `tracing` layer that turns those spans into
//! [`Step`]s, in the order the stages ran.

use std::fmt::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mssql_tds::connection::connect_stage;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, Filter};
use tracing_subscriber::registry::LookupSpan;

/// A stage of opening a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Dns,
    InstanceLookup,
    Tcp,
    Prelogin,
    Tls,
    Login,
}

impl Stage {
    fn from_span_name(name: &str) -> Option<Stage> {
        match name {
            connect_stage::DNS => Some(Stage::Dns),
            connect_stage::INSTANCE_LOOKUP => Some(Stage::InstanceLookup),
            connect_stage::TCP => Some(Stage::Tcp),
            connect_stage::PRELOGIN => Some(Stage::Prelogin),
            connect_stage::TLS => Some(Stage::Tls),
            connect_stage::LOGIN => Some(Stage::Login),
            _ => None,
        }
    }

    /// The stage's name in the JSON report.
    pub fn name(self) -> &'static str {
        match self {
            Stage::Dns => "dns",
            Stage::InstanceLookup => "instanceLookup",
            Stage::Tcp => "tcp",
            Stage::Prelogin => "prelogin",
            Stage::Tls => "tls",
            Stage::Login => "login",
        }
    }

    /// The stage's name in the text report.
    pub fn title(self) -> &'static str {
        match self {
            Stage::Dns => "DNS lookup",
            Stage::InstanceLookup => "Instance lookup",
            Stage::Tcp => "TCP connect",
            Stage::Prelogin => "Pre-login",
            Stage::Tls => "TLS handshake",
            Stage::Login => "Login",
        }
    }
}

/// One stage as it ran.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub stage: Stage,
    pub ok: bool,
    pub duration_ms: u64,
    /// What the stage found or tried, from its span's fields: `host` and
    /// `addresses` for DNS, `address` for TCP, `server` and `instance` for the
    /// instance lookup, `encryption` for pre-login.
    pub details: Vec<(&'static str, String)>,
    pub(crate) started: Instant,
    pub(crate) ended: Instant,
}

impl Step {
    pub fn detail(&self, name: &str) -> Option<&str> {
        self.details
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// What is kept about a stage while it runs.
struct Running {
    stage: Stage,
    sequence: usize,
    started: Instant,
    ok: bool,
    details: Vec<(&'static str, String)>,
}

impl Visit for Running {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == connect_stage::OK {
            self.ok = value;
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let mut text = String::new();
        let _ = write!(text, "{value:?}");
        self.set(field.name(), text);
    }
}

impl Running {
    fn set(&mut self, name: &'static str, value: String) {
        if name == connect_stage::OK {
            return;
        }
        match self.details.iter_mut().find(|(key, _)| *key == name) {
            Some(entry) => entry.1 = value,
            None => self.details.push((name, value)),
        }
    }
}

/// A `tracing` layer that records the connect stages. Clones share what they
/// record.
#[derive(Clone, Default)]
pub struct StageRecorder {
    steps: Arc<Mutex<Vec<(usize, Step)>>>,
    next_sequence: Arc<Mutex<usize>>,
}

impl StageRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The stages recorded so far, in the order they ended, which is the order
    /// they ran in (login, which contains the TLS handshake when encryption
    /// starts at login, follows it). A stage that contains another is timed
    /// without it.
    pub fn steps(&self) -> Vec<Step> {
        let mut recorded = self
            .steps
            .lock()
            .map(|steps| steps.clone())
            .unwrap_or_default();
        recorded.sort_by_key(|(sequence, step)| (step.ended, *sequence));
        let mut steps: Vec<Step> = recorded.into_iter().map(|(_, step)| step).collect();
        let intervals: Vec<(Instant, Instant)> = steps
            .iter()
            .map(|step| (step.started, step.ended))
            .collect();
        for (index, step) in steps.iter_mut().enumerate() {
            let nested: u128 = intervals
                .iter()
                .enumerate()
                .filter(|(other, (start, end))| {
                    *other != index && *start >= step.started && *end <= step.ended
                })
                .map(|(_, (start, end))| end.saturating_duration_since(*start).as_millis())
                .sum();
            step.duration_ms = step
                .duration_ms
                .saturating_sub(u64::try_from(nested).unwrap_or(u64::MAX));
        }
        steps
    }

    fn take_sequence(&self) -> usize {
        let mut next = self
            .next_sequence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = *next;
        *next += 1;
        sequence
    }
}

/// Lets only the connect-stage spans through, so everything else `mssql-tds`
/// traces stays disabled and costs nothing.
pub struct ConnectStagesOnly;

impl<S> Filter<S> for ConnectStagesOnly {
    fn enabled(&self, metadata: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        metadata.is_span() && metadata.target() == connect_stage::TARGET
    }
}

impl<S> Layer<S> for StageRecorder
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(stage) = Stage::from_span_name(attrs.metadata().name()) else {
            return;
        };
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut running = Running {
            stage,
            sequence: self.take_sequence(),
            started: Instant::now(),
            ok: false,
            details: Vec::new(),
        };
        attrs.record(&mut running);
        span.extensions_mut().insert(running);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(running) = span.extensions_mut().get_mut::<Running>()
        {
            values.record(running);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(running) = span.extensions_mut().remove::<Running>() else {
            return;
        };
        let ended = Instant::now();
        let duration = ended.saturating_duration_since(running.started).as_millis();
        let step = Step {
            stage: running.stage,
            ok: running.ok,
            duration_ms: u64::try_from(duration).unwrap_or(u64::MAX),
            details: running.details,
            started: running.started,
            ended,
        };
        if let Ok(mut steps) = self.steps.lock() {
            steps.push((running.sequence, step));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    fn record(body: impl FnOnce()) -> Vec<Step> {
        let recorder = StageRecorder::new();
        let subscriber =
            tracing_subscriber::registry().with(recorder.clone().with_filter(ConnectStagesOnly));
        tracing::subscriber::with_default(subscriber, body);
        recorder.steps()
    }

    #[test]
    fn stages_are_recorded_in_start_order_with_their_fields() {
        let steps = record(|| {
            let dns = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::DNS,
                host = "db",
                addresses = tracing::field::Empty,
                ok = tracing::field::Empty
            );
            dns.record("addresses", "10.0.0.1, 10.0.0.2");
            dns.record(connect_stage::OK, true);
            drop(dns);
            let tcp = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::TCP,
                address = "10.0.0.1:1433",
                ok = tracing::field::Empty
            );
            drop(tcp);
        });
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].stage, Stage::Dns);
        assert!(steps[0].ok);
        assert_eq!(steps[0].detail("host"), Some("db"));
        assert_eq!(steps[0].detail("addresses"), Some("10.0.0.1, 10.0.0.2"));
        assert_eq!(steps[1].stage, Stage::Tcp);
        assert!(!steps[1].ok, "a stage that never recorded ok failed");
        assert_eq!(steps[1].detail("address"), Some("10.0.0.1:1433"));
    }

    #[test]
    fn debug_fields_are_kept_as_text() {
        #[derive(Debug)]
        enum Negotiated {
            Mandatory,
        }
        let steps = record(|| {
            let prelogin = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::PRELOGIN,
                encryption = tracing::field::Empty,
                ok = tracing::field::Empty
            );
            prelogin.record("encryption", tracing::field::debug(Negotiated::Mandatory));
        });
        assert_eq!(steps[0].detail("encryption"), Some("Mandatory"));
    }

    #[test]
    fn other_spans_are_ignored() {
        let steps = record(|| {
            let _other = tracing::info_span!("query");
            let _same_name_other_target = tracing::info_span!(target: "elsewhere", "dns");
        });
        assert!(steps.is_empty());
    }

    #[test]
    fn a_stage_is_timed_without_the_stages_it_contains() {
        let steps = record(|| {
            let login = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::LOGIN,
                ok = tracing::field::Empty
            );
            let tls = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::TLS,
                ok = tracing::field::Empty
            );
            std::thread::sleep(std::time::Duration::from_millis(60));
            tls.record(connect_stage::OK, true);
            drop(tls);
            login.record(connect_stage::OK, true);
        });
        assert_eq!(steps[0].stage, Stage::Tls, "ordered by end");
        assert_eq!(steps[1].stage, Stage::Login);
        let tls = steps.iter().find(|s| s.stage == Stage::Tls).unwrap();
        let login = steps.iter().find(|s| s.stage == Stage::Login).unwrap();
        assert!(tls.duration_ms >= 60, "tls took {} ms", tls.duration_ms);
        assert!(
            login.duration_ms < 30,
            "login without tls took {} ms",
            login.duration_ms
        );
    }
}
