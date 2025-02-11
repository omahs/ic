use std::{borrow::Cow, net::SocketAddr, pin::Pin, time::Instant};

use anyhow::{Context, Error};

use axum::{
    body::Body,
    extract::State,
    handler::Handler,
    http::Request,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Extension, Router,
};

use bytes::Buf;
use candid::Principal;
use clap::Args;
use futures::task::{Context as FutContext, Poll};
use http_body::Body as HttpBody;
use hyper::http::header::HeaderMap;
use hyper::{self, StatusCode};
use ic_agent::Agent;
use opentelemetry::{
    metrics::{Counter, Histogram, Meter, MeterProvider as _},
    sdk::metrics::{new_view, Aggregation, Instrument, MeterProvider, Stream},
    KeyValue,
};
use opentelemetry_prometheus::exporter;

use prometheus::{Encoder as PrometheusEncoder, Registry, TextEncoder};

use crate::http::request::HttpRequest;
use crate::http::response::HttpResponse;
use crate::{logging::add_trace_layer, validate::Validate};

/// The options for metrics
#[derive(Args)]
pub struct MetricsOpts {
    /// Address to expose Prometheus metrics on
    /// Examples: 127.0.0.1:9090, [::1]:9090
    #[clap(long)]
    metrics_addr: Option<SocketAddr>,
}

// Context that holds request-specific data for later logging/metrics
#[derive(Clone, Default)]
pub struct RequestContext {
    pub request_size: u64,
    pub streaming_request: bool,
}

#[derive(Clone)]
pub struct WithMetrics<T>(pub T, pub MetricParams);

#[derive(Clone)]
pub struct MetricParams {
    pub counter: Counter<u64>,
}

impl MetricParams {
    pub fn new(meter: &Meter, name: &str) -> Self {
        Self {
            counter: meter
                .u64_counter(name.to_string())
                .with_description(format!("Counts occurrences of {name} calls"))
                .init(),
        }
    }
}

impl<T: Validate> Validate for WithMetrics<T> {
    fn validate(
        &self,
        agent: &Agent,
        canister_id: &Principal,
        request: &HttpRequest,
        response: &HttpResponse,
    ) -> Result<(), Cow<'static, str>> {
        let out = self.0.validate(agent, canister_id, request, response);

        let mut status = if out.is_ok() { "ok" } else { "fail" };
        if cfg!(feature = "skip_body_verification") {
            status = "skip";
        }

        let labels = &[KeyValue::new("status", status)];

        let MetricParams { counter } = &self.1;
        counter.add(1, labels);

        out
    }
}

#[derive(Clone)]
struct HandlerArgs {
    registry: Registry,
}

async fn metrics_handler(
    Extension(HandlerArgs { registry }): Extension<HandlerArgs>,
    _: Request<Body>,
) -> Response<Body> {
    let metric_families = registry.gather();

    let encoder = TextEncoder::new();

    let mut metrics_text = Vec::new();
    if encoder.encode(&metric_families, &mut metrics_text).is_err() {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body("Internal Server Error".into())
            .unwrap();
    };

    Response::builder()
        .status(200)
        .body(metrics_text.into())
        .unwrap()
}

pub fn setup(opts: MetricsOpts) -> (Meter, Runner) {
    let registry: Registry = Registry::new_custom(None, None).unwrap();

    // Change default buckets
    // What an ugly way to do it in otel...
    let view_req_size = new_view(
        Instrument::new().name("http_request_size"),
        Stream::new().aggregation(Aggregation::ExplicitBucketHistogram {
            boundaries: vec![
                1000.0, 2000.0, 4000.0, 8000.0, 16000.0, 32000.0, 64000.0, 128000.0, 256000.0,
                512000.0,
            ],
            record_min_max: false,
        }),
    )
    .unwrap();

    let view_resp_size = new_view(
        Instrument::new().name("http_response_size"),
        Stream::new().aggregation(Aggregation::ExplicitBucketHistogram {
            boundaries: vec![
                1000.0, 10000.0, 50000.0, 100000.0, 200000.0, 400000.0, 800000.0, 1600000.0,
                3200000.0, 6400000.0,
            ],
            record_min_max: false,
        }),
    )
    .unwrap();

    let view_resp_dur = new_view(
        Instrument::new().name("http_request_*_duration_sec"),
        Stream::new().aggregation(Aggregation::ExplicitBucketHistogram {
            boundaries: vec![
                0.05, 0.1, 0.2, 0.4, 0.6, 0.8, 1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 10.0,
            ],
            record_min_max: false,
        }),
    )
    .unwrap();

    let exporter = exporter().with_registry(registry.clone()).build().unwrap();
    let provider = MeterProvider::builder()
        .with_reader(exporter)
        .with_view(view_req_size)
        .with_view(view_resp_size)
        .with_view(view_resp_dur)
        .build();

    (
        provider.meter("icx_proxy"),
        Runner {
            registry,
            metrics_addr: opts.metrics_addr,
        },
    )
}

pub struct Runner {
    registry: Registry,
    metrics_addr: Option<SocketAddr>,
}

impl Runner {
    pub async fn run(self) -> Result<(), Error> {
        if self.metrics_addr.is_none() {
            return Ok(());
        }

        let metrics_router = Router::new().route(
            "/metrics",
            get(metrics_handler.layer(Extension(HandlerArgs {
                registry: self.registry,
            }))),
        );

        axum::Server::bind(&self.metrics_addr.unwrap())
            .serve(add_trace_layer(metrics_router).into_make_service())
            .await
            .context("failed to start metrics server")?;

        Ok(())
    }
}

// A wrapper for http::Body implementations that tracks the number of bytes sent
pub struct MetricsBody<D, E> {
    inner: Pin<Box<dyn HttpBody<Data = D, Error = E> + Send + 'static>>,
    callback: Box<dyn Fn(u64, bool) + Send + 'static>,
    bytes_sent: u64,
}

impl<D, E> MetricsBody<D, E> {
    pub fn new<B>(body: B, callback: impl Fn(u64, bool) + Send + 'static) -> Self
    where
        B: HttpBody<Data = D, Error = E> + Send + 'static,
        D: Buf,
    {
        Self {
            inner: Box::pin(body),
            callback: Box::new(callback),
            bytes_sent: 0,
        }
    }
}

impl<D, E> HttpBody for MetricsBody<D, E>
where
    D: Buf,
    E: std::fmt::Debug,
{
    type Data = D;
    type Error = E;

    fn poll_data(
        mut self: Pin<&mut Self>,
        cx: &mut FutContext<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let poll = self.inner.as_mut().poll_data(cx);

        match &poll {
            Poll::Ready(Some(v)) => match v {
                Ok(v) => self.bytes_sent += v.remaining() as u64,
                Err(_) => (self.callback)(self.bytes_sent, false),
            },

            Poll::Ready(None) => {
                (self.callback)(self.bytes_sent, true);
            }

            _ => {}
        }

        poll
    }

    fn poll_trailers(
        mut self: Pin<&mut Self>,
        cx: &mut FutContext<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        self.inner.as_mut().poll_trailers(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Clone)]
pub struct HttpMetricParams {
    pub request_sizer: Histogram<u64>,
    pub response_sizer: Histogram<u64>,
    pub durationer: Histogram<f64>,
    pub durationer_full: Histogram<f64>,
}

impl HttpMetricParams {
    pub fn new(meter: &Meter) -> Self {
        Self {
            request_sizer: meter
                .u64_histogram("http_request_size")
                .with_description("Records the size of HTTP requests")
                .init(),

            response_sizer: meter
                .u64_histogram("http_response_size")
                .with_description("Records the size of HTTP responses")
                .init(),

            durationer: meter
                .f64_histogram("http_request_processing_duration_sec")
                .with_description("Records the duration of HTTP request processing")
                .init(),

            durationer_full: meter
                .f64_histogram("http_request_full_duration_sec")
                .with_description("Records the full duration of HTTP request")
                .init(),
        }
    }
}

pub async fn with_metrics_middleware(
    State(metric_params): State<HttpMetricParams>,
    request: Request<Body>,
    next: Next<Body>,
) -> impl IntoResponse {
    let start = Instant::now();
    let response = next.run(request).await;
    let proc_duration = start.elapsed().as_secs_f64();

    let request_ctx = response
        .extensions()
        .get::<RequestContext>()
        .cloned()
        .unwrap_or_default();

    let HttpMetricParams {
        request_sizer,
        response_sizer,
        durationer,
        durationer_full,
    } = metric_params;

    let status = response.status().as_u16();

    let (parts, body) = response.into_parts();
    let body = MetricsBody::new(body, move |bytes_sent, fully_read| {
        let labels = &[
            KeyValue::new("status", status.to_string()),
            KeyValue::new("streaming", request_ctx.streaming_request.to_string()),
            KeyValue::new("body_fully_read", fully_read.to_string()),
        ];

        request_sizer.record(request_ctx.request_size, labels);
        response_sizer.record(bytes_sent, labels);
        durationer.record(proc_duration, labels);
        durationer_full.record(start.elapsed().as_secs_f64(), labels);
    });

    Response::from_parts(parts, body)
}
