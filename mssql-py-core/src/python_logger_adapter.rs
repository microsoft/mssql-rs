// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tracing bridge for forwarding Rust tracing events to Python logger.
//!
//! This module provides a custom tracing subscriber that intercepts tracing events
//! and forwards them to Python's logging system via the `py_core_log` method.

use pyo3::prelude::*;
use std::sync::Arc;
use tracing::{Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// A tracing layer that forwards events to a Python logger.
pub struct PythonLoggerLayer {
    logger: Arc<Py<PyAny>>,
    module_name: &'static str,
}

impl PythonLoggerLayer {
    /// Create a new Python logger layer.
    ///
    /// # Arguments
    /// * `logger` - Python logger object with `py_core_log` method
    /// * `module_name` - Module name for log attribution (e.g., "bulkcopy.rs")
    pub fn new(logger: Arc<Py<PyAny>>, module_name: &'static str) -> Self {
        Self {
            logger,
            module_name,
        }
    }

    /// Convert tracing level to Python logging level integer.
    #[inline]
    fn level_to_int(level: &Level) -> u32 {
        match *level {
            Level::TRACE => 10, // DEBUG
            Level::DEBUG => 10, // DEBUG
            Level::INFO => 20,  // INFO
            Level::WARN => 30,  // WARNING
            Level::ERROR => 40, // ERROR
        }
    }
}

impl<S> Layer<S> for PythonLoggerLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        // Extract the message from the event
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        if let Some(message) = visitor.message {
            let metadata = event.metadata();
            let level_int = Self::level_to_int(metadata.level());
            let line = metadata.line().unwrap_or(0);

            // Extract the actual source file name from the target module path
            // e.g., "mssql_py_core::bulkcopy" -> "bulkcopy.rs"
            let module_name = metadata
                .target()
                .split("::")
                .last()
                .map(|name| format!("{}.rs", name))
                .unwrap_or_else(|| self.module_name.to_string());

            // Forward to Python logger
            Python::attach(|py| {
                let logger_bound = self.logger.bind(py);
                if let Ok(method) = logger_bound.getattr("py_core_log") {
                    let _ = method.call1((level_int, message, module_name, line));
                }
            });
        }
    }
}

/// Visitor to extract the message from a tracing event.
#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{:?}", value));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }
}

/// Initialize tracing with Python logger bridge.
///
/// This function sets up a global tracing subscriber that forwards events to Python.
/// Should be called once at initialization.
/// Only captures events from the `mssql_py_core` crate.
///
/// # Arguments
/// * `logger` - Python logger object
/// * `module_name` - Module name for attribution
///
/// # Example
/// ```rust,ignore
/// if let Some(logger) = python_logger {
///     init_tracing_bridge(Arc::new(logger.clone().unbind()), "my_module.rs");
/// }
/// ```
pub fn init_tracing_bridge(logger: Arc<Py<PyAny>>, module_name: &'static str) {
    use tracing_subscriber::Registry;
    use tracing_subscriber::filter::filter_fn;

    let layer = PythonLoggerLayer::new(logger, module_name);

    // Filter to only capture events from mssql_py_core crate
    let filtered_layer = layer.with_filter(filter_fn(|metadata| {
        metadata.target().starts_with("mssql_py_core")
    }));

    let subscriber = Registry::default().with(filtered_layer);

    // Set as global default
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// Create a scoped tracing bridge for temporary use.
///
/// Returns a guard that when dropped, restores the previous subscriber.
/// Useful for per-operation tracing setup.
/// Only captures events from the `mssql_py_core` crate.
pub fn scoped_tracing_bridge(
    logger: Arc<Py<PyAny>>,
    module_name: &'static str,
) -> tracing::subscriber::DefaultGuard {
    let dispatch = python_logger_dispatch(logger, module_name);
    tracing::dispatcher::set_default(&dispatch)
}

/// Create an owned tracing dispatch that can follow asynchronous work across
/// executor threads.
pub(crate) fn python_logger_dispatch(
    logger: Arc<Py<PyAny>>,
    module_name: &'static str,
) -> tracing::Dispatch {
    use tracing_subscriber::Registry;
    use tracing_subscriber::filter::filter_fn;

    let layer = PythonLoggerLayer::new(logger, module_name);

    // Filter to only capture events from mssql_py_core crate
    let filtered_layer = layer.with_filter(filter_fn(|metadata| {
        metadata.target().starts_with("mssql_py_core")
    }));

    let subscriber = Registry::default().with(filtered_layer);

    tracing::Dispatch::new(subscriber)
}
