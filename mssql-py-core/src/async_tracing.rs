// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structured tracing helpers for asynchronous cursor operations.

use std::future::Future;

use tracing::Instrument;

use crate::async_session::{CursorId, OperationId};

pub(crate) async fn in_cursor_operation_span<F>(
    future: F,
    cursor_id: CursorId,
    operation_id: OperationId,
    operation: &'static str,
    initial_result_set_status: &'static str,
) -> F::Output
where
    F: Future,
{
    let span = tracing::info_span!(
        "async_cursor_operation",
        cursor_id,
        operation_id,
        operation,
        result_set_status = initial_result_set_status,
    );
    future.instrument(span).await
}

pub(crate) fn record_result_set_status(status: &'static str) {
    tracing::Span::current().record("result_set_status", status);
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::sync::{Arc, Mutex};

    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    use super::{in_cursor_operation_span, record_result_set_status};

    #[derive(Clone, Default)]
    struct CaptureLayer {
        fields: Arc<Mutex<Vec<(String, String)>>>,
    }

    struct FieldVisitor<'a>(&'a Mutex<Vec<(String, String)>>);

    impl Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            self.0
                .lock()
                .unwrap()
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    impl<S: Subscriber> Layer<S> for CaptureLayer {
        fn on_new_span(
            &self,
            attributes: &Attributes<'_>,
            _id: &Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            attributes.record(&mut FieldVisitor(&self.fields));
        }

        fn on_record(
            &self,
            _id: &Id,
            values: &Record<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            values.record(&mut FieldVisitor(&self.fields));
        }
    }

    #[tokio::test]
    async fn cursor_operation_span_records_correlation_and_result_status() {
        let capture = CaptureLayer::default();
        let fields = Arc::clone(&capture.fields);
        let dispatch =
            tracing::Dispatch::new(tracing_subscriber::Registry::default().with(capture));
        let _default = tracing::dispatcher::set_default(&dispatch);

        in_cursor_operation_span(
            async { record_result_set_status("exhausted") },
            41,
            73,
            "fetchmany",
            "reading",
        )
        .await;

        let fields = fields.lock().unwrap();
        assert!(fields.contains(&("cursor_id".to_string(), "41".to_string())));
        assert!(fields.contains(&("operation_id".to_string(), "73".to_string())));
        assert!(fields.contains(&("operation".to_string(), "\"fetchmany\"".to_string())));
        assert!(fields.contains(&("result_set_status".to_string(), "\"reading\"".to_string())));
        assert!(fields.contains(&("result_set_status".to_string(), "\"exhausted\"".to_string())));
    }
}
