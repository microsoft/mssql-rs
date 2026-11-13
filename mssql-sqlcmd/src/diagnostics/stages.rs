// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Records the connect stages of `mssql-tds` as they happen.
//!
//! `mssql-tds` opens one span per stage of a connection attempt (see
//! [`mssql_tds::connection::connect_stage`]): created when the stage starts,
//! closed when it ends, with `ok` recorded first: `true` if it succeeded,
//! `false` if it failed. A span closed without `ok` was abandoned (a cancel or
//! a time limit dropped it). [`StageRecorder`] is a passive `tracing` layer
//! that turns those spans into [`Step`]s, in the order the stages ran; it
//! never steers the attempt. A stage still running when the steps are read
//! (a connect to another address the attempt's bound dropped, whose span the
//! runtime has not closed yet) is read as it stands: without `ok`, abandoned.

use std::collections::HashMap;
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

use crate::i18n;

/// A stage of opening a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Dns,
    InstanceLookup,
    Tcp,
    /// Opening a named pipe, shared memory's pipe or a LocalDB instance's pipe.
    Pipe,
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
            connect_stage::PIPE => Some(Stage::Pipe),
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
            Stage::Pipe => "pipe",
            Stage::Prelogin => "prelogin",
            Stage::Tls => "tls",
            Stage::Login => "login",
        }
    }

    pub fn detail_title_in(self, locale: &str) -> String {
        i18n::tr_for_locale(locale, self.detail_title_id(), &[])
    }

    pub(crate) fn detail_title_id(self) -> &'static str {
        match self {
            Stage::Dns => "diagnose.stage.dns",
            Stage::InstanceLookup => "diagnose.stage.instance_lookup",
            Stage::Tcp => "diagnose.stage.tcp",
            Stage::Pipe => "diagnose.stage.pipe",
            Stage::Prelogin => "diagnose.stage.prelogin_detail",
            Stage::Tls => "diagnose.stage.tls_detail",
            Stage::Login => "diagnose.stage.login_detail",
        }
    }

    /// The stage's name on its own, as an error names a stage that gave no
    /// message of its own.
    pub(crate) fn title_id(self) -> &'static str {
        match self {
            Stage::Prelogin => "diagnose.stage.prelogin",
            Stage::Tls => "diagnose.stage.tls",
            Stage::Login => "diagnose.stage.login",
            _ => self.detail_title_id(),
        }
    }

    pub(crate) fn title_english(self) -> &'static str {
        match self {
            Stage::Prelogin => "Pre-login",
            Stage::Tls => "TLS handshake",
            Stage::Login => "Login",
            _ => self.detail_title_english(),
        }
    }

    pub(crate) fn detail_title_english(self) -> &'static str {
        match self {
            Stage::Dns => "DNS lookup",
            Stage::InstanceLookup => "Instance lookup",
            Stage::Tcp => "TCP connect",
            Stage::Pipe => "Pipe open",
            Stage::Prelogin => "pre-login",
            Stage::Tls => "TLS",
            Stage::Login => "login",
        }
    }
}

/// One stage as it ran.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub stage: Stage,
    /// `Some(true)`: it succeeded; `Some(false)`: it failed; `None`: it was
    /// abandoned, neither.
    pub ok: Option<bool>,
    pub duration_ms: u64,
    /// What the stage found or tried, from its span's fields (see
    /// [`connect_stage`]): `host` and `addresses` for DNS, `address` and
    /// `port` for TCP, `server`, `instance`, `port` and `pipe` for the
    /// instance lookup, `kind` and `pipe` for a pipe, `encryption` for
    /// pre-login; on failure `os_error`, `io_kind`, `error`, ...
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

    /// Whether the stage failed (rather than succeeded or was abandoned).
    pub fn failed(&self) -> bool {
        self.ok == Some(false)
    }

    /// Whether the stage closed without saying how it ended.
    pub fn abandoned(&self) -> bool {
        self.ok.is_none()
    }

    /// The operating system's error code it failed with.
    pub fn os_error(&self) -> Option<i32> {
        self.detail(connect_stage::OS_ERROR)?.parse().ok()
    }
}

/// What is kept about a stage while it runs.
#[derive(Clone)]
struct Running {
    stage: Stage,
    sequence: usize,
    started: Instant,
    ok: Option<bool>,
    details: Vec<(&'static str, String)>,
}

impl Visit for Running {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == connect_stage::OK {
            self.ok = Some(value);
        } else {
            self.set(field.name(), value.to_string());
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

    /// The stage as a step that ended at `ended`.
    fn step(self, ended: Instant) -> Step {
        let duration = ended.saturating_duration_since(self.started).as_millis();
        Step {
            stage: self.stage,
            ok: self.ok,
            duration_ms: u64::try_from(duration).unwrap_or(u64::MAX),
            details: self.details,
            started: self.started,
            ended,
        }
    }
}

/// The key, kept in a stage's span, of what is recorded about it while it
/// runs.
struct Sequence(usize);

/// A `tracing` layer that records the connect stages. Clones share what they
/// record.
#[derive(Clone, Default)]
pub struct StageRecorder {
    steps: Arc<Mutex<Vec<(usize, Step)>>>,
    /// The stages whose spans are still open, by sequence.
    running: Arc<Mutex<HashMap<usize, Running>>>,
    next_sequence: Arc<Mutex<usize>>,
}

impl StageRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the stages recorded so far: a second connection's stages are
    /// not mixed with the first's.
    pub fn clear(&self) {
        if let Ok(mut running) = self.running.lock()
            && let Ok(mut steps) = self.steps.lock()
        {
            running.clear();
            steps.clear();
        }
    }

    /// The stages recorded so far, in the order they ended, which is the order
    /// they ran in (login, which contains the TLS handshake when encryption
    /// starts at login, follows it). A stage whose span is still open ends
    /// now, with what it recorded so far: without `ok` it was abandoned, as a
    /// stage a limit cut short is; once its span closes it is read with how
    /// it ended instead, once. A login is timed without its TLS handshake. No
    /// other stage is: the connects to the other addresses run alongside the
    /// attempt, so one that is slow can span stages that are not its own.
    pub fn steps(&self) -> Vec<Step> {
        // Both are read under the running stages' lock, which `on_close` also
        // takes first, so a read is one moment: a stage `on_close` is moving
        // is read as it was then, running or closed, and the lock order is
        // the same as `on_close`'s. Read under two locks, a stage could be read
        // running though it had closed by the time the closed ones were read;
        // its outcome was recorded before it closed, so only its end time
        // differed.
        let now = Instant::now();
        let running = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut recorded = self
            .steps
            .lock()
            .map(|steps| steps.clone())
            .unwrap_or_default();
        recorded.extend(
            running
                .iter()
                .map(|(sequence, stage)| (*sequence, stage.clone().step(now))),
        );
        drop(running);
        recorded.sort_by_key(|(sequence, step)| (step.ended, *sequence));
        let mut steps: Vec<Step> = recorded.into_iter().map(|(_, step)| step).collect();
        let handshakes: Vec<(Instant, Instant)> = steps
            .iter()
            .filter(|step| step.stage == Stage::Tls)
            .map(|step| (step.started, step.ended))
            .collect();
        for step in steps.iter_mut().filter(|step| step.stage == Stage::Login) {
            let nested: u128 = handshakes
                .iter()
                .filter(|(start, end)| *start >= step.started && *end <= step.ended)
                .map(|(start, end)| end.saturating_duration_since(*start).as_millis())
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
            ok: None,
            details: Vec::new(),
        };
        attrs.record(&mut running);
        let sequence = running.sequence;
        if let Ok(mut stages) = self.running.lock() {
            stages.insert(sequence, running);
        }
        span.extensions_mut().insert(Sequence(sequence));
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let Some(sequence) = span.extensions().get::<Sequence>().map(|s| s.0) else {
            return;
        };
        if let Ok(mut stages) = self.running.lock()
            && let Some(running) = stages.get_mut(&sequence)
        {
            values.record(running);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(Sequence(sequence)) = span.extensions_mut().remove::<Sequence>() else {
            return;
        };
        let ended = Instant::now();
        // The running stage leaves only once its step is recorded, so a read
        // in between finds it in one or the other.
        let Ok(mut stages) = self.running.lock() else {
            return;
        };
        let Some(running) = stages.remove(&sequence) else {
            return;
        };
        if let Ok(mut steps) = self.steps.lock() {
            steps.push((sequence, running.step(ended)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn clear_forgets_open_and_closed_stages_and_ignores_late_closes() {
        let recorder = StageRecorder::new();
        let subscriber =
            tracing_subscriber::registry().with(recorder.clone().with_filter(ConnectStagesOnly));
        tracing::subscriber::with_default(subscriber, || {
            let dns = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::DNS,
                ok = tracing::field::Empty
            );
            dns.record(connect_stage::OK, true);
            drop(dns);
            let tcp = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::TCP,
                ok = tracing::field::Empty
            );
            assert_eq!(recorder.steps().len(), 2);

            recorder.clear();
            assert!(recorder.steps().is_empty());

            let tls = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::TLS,
                ok = tracing::field::Empty
            );
            tls.record(connect_stage::OK, true);
            drop(tls);
            tcp.record(connect_stage::OK, true);
            drop(tcp);

            let steps = recorder.steps();
            assert_eq!(steps.len(), 1);
            assert_eq!(steps[0].stage, Stage::Tls);
            assert_eq!(steps[0].ok, Some(true));
            recorder.clear();
            assert!(recorder.steps().is_empty());
        });
    }

    /// A concurrency check, not a test of the single lock: stages that close
    /// on other threads while `steps` reads are each read once, and every
    /// stage that closed before a read is closed in it. That holds with the
    /// stages read under two locks too; what one lock adds (a read is one
    /// moment) is not observable from outside.
    #[test]
    fn steps_read_while_stages_close_on_other_threads() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        const THREADS: usize = 4;
        const STAGES: usize = 200;
        let recorder = StageRecorder::new();
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::registry().with(recorder.clone().with_filter(ConnectStagesOnly)),
        );
        let closed = AtomicUsize::new(0);
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    tracing::dispatcher::with_default(&dispatch, || {
                        for _ in 0..STAGES {
                            let tcp = tracing::info_span!(
                                target: connect_stage::TARGET,
                                connect_stage::TCP,
                                ok = tracing::field::Empty
                            );
                            tcp.record(connect_stage::OK, true);
                            drop(tcp);
                            closed.fetch_add(1, Ordering::SeqCst);
                        }
                    });
                });
            }
            scope.spawn(|| {
                while !done.load(Ordering::SeqCst) {
                    let before = closed.load(Ordering::SeqCst);
                    let steps = recorder.steps();
                    assert!(steps.len() <= THREADS * STAGES);
                    let finished = steps.iter().filter(|s| s.ok == Some(true)).count();
                    assert!(finished >= before, "{finished} < {before}");
                    if before == THREADS * STAGES {
                        done.store(true, Ordering::SeqCst);
                    }
                }
            });
        });
        let steps = recorder.steps();
        assert_eq!(steps.len(), THREADS * STAGES);
        assert!(steps.iter().all(|s| s.ok == Some(true)));
    }

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
        assert_eq!(steps[0].ok, Some(true));
        assert_eq!(steps[0].detail("host"), Some("db"));
        assert_eq!(steps[0].detail("addresses"), Some("10.0.0.1, 10.0.0.2"));
        assert_eq!(steps[1].stage, Stage::Tcp);
        assert_eq!(
            steps[1].ok, None,
            "a stage that never recorded ok was abandoned"
        );
        assert!(steps[1].abandoned() && !steps[1].failed());
        assert_eq!(steps[1].detail("address"), Some("10.0.0.1:1433"));
    }

    #[test]
    fn a_failed_stage_keeps_what_it_failed_with() {
        let steps = record(|| {
            let pipe = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::PIPE,
                kind = connect_stage::KIND_SHARED_MEMORY,
                pipe = r"\\.\pipe\SQLLocal\MSSQLSERVER",
                os_error = tracing::field::Empty,
                error = tracing::field::Empty,
                ok = tracing::field::Empty
            );
            pipe.record(connect_stage::OS_ERROR, 2);
            pipe.record(connect_stage::ERROR, "not found");
            pipe.record(connect_stage::OK, false);
        });
        assert_eq!(steps[0].stage, Stage::Pipe);
        assert!(steps[0].failed() && !steps[0].abandoned());
        assert_eq!(steps[0].os_error(), Some(2));
        assert_eq!(steps[0].detail("kind"), Some("sharedMemory"));
        assert_eq!(steps[0].detail("error"), Some("not found"));
    }

    /// The stages of the connects `mssql-tds` runs in tasks of their own (to
    /// the addresses after the one used) are recorded too: the runtime has
    /// one thread, on which the subscriber is set.
    #[test]
    fn stages_of_spawned_tasks_are_recorded() {
        let steps = record(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let made_here = tracing::info_span!(
                    target: connect_stage::TARGET,
                    connect_stage::TCP,
                    address = "10.0.0.2",
                    ok = tracing::field::Empty
                );
                let task = tokio::spawn(async move {
                    tokio::task::yield_now().await;
                    made_here.record(connect_stage::OK, true);
                    let made_in_task = tracing::info_span!(
                        target: connect_stage::TARGET,
                        connect_stage::TCP,
                        address = "10.0.0.3",
                        ok = tracing::field::Empty
                    );
                    made_in_task.record(connect_stage::OK, false);
                });
                task.await.unwrap();
            });
        });
        let mut addresses: Vec<(Option<&str>, Option<bool>)> =
            steps.iter().map(|s| (s.detail("address"), s.ok)).collect();
        addresses.sort();
        assert_eq!(
            addresses,
            [
                (Some("10.0.0.2"), Some(true)),
                (Some("10.0.0.3"), Some(false))
            ]
        );
    }

    /// A stage whose span is still open when the steps are read (a connect
    /// the attempt's bound dropped, which the runtime closes later) is read
    /// as abandoned, ending then; once it closes it is read once, with how
    /// it ended.
    #[test]
    fn a_stage_still_running_is_read_as_abandoned_until_it_closes() {
        let recorder = StageRecorder::new();
        let subscriber =
            tracing_subscriber::registry().with(recorder.clone().with_filter(ConnectStagesOnly));
        tracing::subscriber::with_default(subscriber, || {
            let used = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::TCP,
                address = "10.0.0.1",
                ok = tracing::field::Empty
            );
            used.record(connect_stage::OK, true);
            drop(used);
            let dropped = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::TCP,
                address = "10.0.0.2",
                port = "1433",
                ok = tracing::field::Empty
            );
            std::thread::sleep(std::time::Duration::from_millis(20));

            let steps = recorder.steps();
            assert_eq!(steps.len(), 2, "{steps:#?}");
            assert_eq!(steps[0].detail("address"), Some("10.0.0.1"));
            assert_eq!(steps[0].ok, Some(true));
            assert_eq!(steps[1].detail("address"), Some("10.0.0.2"));
            assert_eq!(steps[1].detail("port"), Some("1433"));
            assert!(steps[1].abandoned(), "{:#?}", steps[1]);
            assert!(steps[1].duration_ms >= 20, "{:#?}", steps[1]);
            assert_eq!(recorder.steps().len(), 2, "reading again adds nothing");

            dropped.record(connect_stage::OK, false);
            drop(dropped);
            let steps = recorder.steps();
            let late: Vec<&Step> = steps
                .iter()
                .filter(|s| s.detail("address") == Some("10.0.0.2"))
                .collect();
            assert_eq!(late.len(), 1, "{steps:#?}");
            assert_eq!(late[0].ok, Some(false));
        });
        assert_eq!(recorder.steps().len(), 2);
    }

    #[test]
    fn a_redirected_login_keeps_the_flag() {
        let steps = record(|| {
            let login = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::LOGIN,
                ok = tracing::field::Empty,
                redirected = tracing::field::Empty
            );
            login.record(connect_stage::REDIRECTED, true);
            drop(login);
        });
        assert_eq!(steps[0].ok, None);
        assert_eq!(steps[0].detail("redirected"), Some("true"));
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

    /// A slow connect to another address spans the whole attempt's stages,
    /// which are not its own: it keeps its elapsed time, and the TLS
    /// handshake is taken from the login only once.
    #[test]
    fn a_slow_probe_keeps_its_time_and_tls_is_not_counted_twice() {
        let steps = record(|| {
            let probe = tracing::info_span!(
                target: connect_stage::TARGET,
                connect_stage::TCP,
                address = "10.0.0.2",
                ok = tracing::field::Empty
            );
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
            drop(login);
            probe.record(connect_stage::OK, true);
        });
        let probe = steps.iter().find(|s| s.stage == Stage::Tcp).unwrap();
        let login = steps.iter().find(|s| s.stage == Stage::Login).unwrap();
        assert!(
            probe.duration_ms >= 60,
            "probe took {} ms",
            probe.duration_ms
        );
        assert!(
            login.duration_ms < 30,
            "login took {} ms",
            login.duration_ms
        );
    }
}
