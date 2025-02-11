use std::{
    fs,
    io::{prelude::*, BufReader},
    net::{TcpListener, TcpStream},
    thread,
    time::Duration,
};
use tracing::{info, warn, error, instrument};
use metrics::{counter, histogram};
use opentelemetry::global;
use opentelemetry_sdk::{trace as sdktrace, Resource};
use opentelemetry_otlp::WithExportConfig;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

use rust_web_server::ThreadPool;

async fn init_telemetry() {
    use std::net::SocketAddr;
    use hyper::{Body, Response, Server};
    use hyper::service::{make_service_fn, service_fn};
    use std::convert::Infallible;
    use std::sync::Arc;

    // Initialize prometheus metrics endpoint binding to all interfaces
    let addr: SocketAddr = ([0, 0, 0, 0], 9091).into();
    
    // Set up a recorder and wrap it in Arc for sharing
    let recorder = Arc::new(
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
            .expect("failed to install Prometheus recorder")
    );

    // Create a metrics service
    let make_svc = make_service_fn(move |_conn| {
        let recorder = Arc::clone(&recorder);
        async move {
            Ok::<_, Infallible>(service_fn(move |_req| {
                let recorder = Arc::clone(&recorder);
                async move {
                    let metrics = recorder.render();
                    Ok::<_, Infallible>(Response::builder()
                        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
                        .body(Body::from(metrics))
                        .unwrap())
                }
            }))
        }
    });

    // Spawn the server in a separate task
    tokio::spawn(async move {
        let server = Server::bind(&addr).serve(make_svc);
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
        }
    });

    // Initialize OpenTelemetry OTLP exporter
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .http()
                .with_endpoint("http://localhost:4318")
                .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        )
        .with_trace_config(sdktrace::config().with_resource(
            Resource::new(vec![opentelemetry::KeyValue::new(
                "service.name",
                "rust-web-server",
            )])
        ))
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .expect("failed to initialize OpenTelemetry tracer");

    // Initialize tracing subscriber with OpenTelemetry
    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    tracing_subscriber::registry()
        .with(telemetry)
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(tracing_subscriber::fmt::layer())
        .init();
}

#[tokio::main]
#[instrument]
async fn main() {
    init_telemetry().await;

    let listener = TcpListener::bind("127.0.0.1:7878").unwrap();
    info!("Server started on port 7878");
    
    let pool = ThreadPool::new(16);
    counter!("thread_pool_size", 16);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                counter!("connections_total", 1);
                let request_id = Uuid::new_v4();
                
                info!(request_id = ?request_id, "New connection accepted");
                
                pool.execute(move || {
                    handle_connection(stream, request_id);
                });
            }
            Err(e) => {
                error!("Failed to establish connection: {}", e);
                counter!("connection_errors_total", 1);
            }
        }
    }

    info!("Shutting down server");
    global::shutdown_tracer_provider();
}

#[instrument(skip(stream))]
fn handle_connection(mut stream: TcpStream, request_id: Uuid) {
    let start = std::time::Instant::now();
    
    // Increment total connections counter
    counter!("connections_total", 1);
    
    let buf_reader = BufReader::new(&mut stream);
    
    let request_line = match buf_reader.lines().next() {
        Some(Ok(line)) => line,
        Some(Err(e)) => {
            error!(request_id = ?request_id, "Failed to read request: {}", e);
            counter!("request_errors_total", 1);
            counter!("requests_total", 1, "status" => "500", "path" => "error");
            return;
        }
        None => {
            warn!(request_id = ?request_id, "Empty request received");
            counter!("request_errors_total", 1);
            counter!("requests_total", 1, "status" => "400", "path" => "empty");
            return;
        }
    };

    let (status_line, filename) = match &request_line[..] {
        "GET / HTTP/1.1" => {
            counter!("requests_total", 1, "path" => "root", "status" => "200");
            counter!("requests_by_path", 1, "path" => "root");
            ("HTTP/1.1 200 OK", "hello.html")
        }
        "GET /sleep HTTP/1.1" => {
            info!(request_id = ?request_id, "Processing sleep request");
            thread::sleep(Duration::from_secs(5));
            counter!("requests_total", 1, "path" => "sleep", "status" => "200");
            counter!("requests_by_path", 1, "path" => "sleep");
            ("HTTP/1.1 200 OK", "hello.html")
        }
        _ => {
            warn!(request_id = ?request_id, "Not found: {}", request_line);
            counter!("requests_total", 1, "path" => "notfound", "status" => "404");
            counter!("request_errors_total", 1);
            ("HTTP/1.1 404 NOT FOUND", "404.html")
        }
    };

    let contents = match fs::read_to_string(filename) {
        Ok(contents) => contents,
        Err(e) => {
            error!(request_id = ?request_id, "Failed to read file {}: {}", filename, e);
            counter!("file_read_errors_total", 1);
            return;
        }
    };

    let length = contents.len();
    let response = format!("{status_line}\r\nContent-Length: {length}\r\n\r\n{contents}");

    if let Err(e) = stream.write_all(response.as_bytes()) {
        error!(request_id = ?request_id, "Failed to write response: {}", e);
        counter!("response_errors_total", 1);
        return;
    }

    if let Err(e) = stream.flush() {
        error!(request_id = ?request_id, "Failed to flush response: {}", e);
        return;
    }

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    histogram!("request_duration_seconds", duration_secs);
    histogram!("request_duration_by_path", duration_secs, "path" => filename);
    
    info!(
        request_id = ?request_id,
        path = request_line,
        status = status_line,
        duration = ?duration,
        "Request completed"
    );
}