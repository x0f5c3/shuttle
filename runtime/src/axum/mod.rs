use std::convert::Infallible;
use std::io::{BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::ops::DerefMut;
use std::os::unix::prelude::RawFd;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use anyhow::Context;
use async_trait::async_trait;
use cap_std::os::unix::net::UnixStream;
use futures::TryStreamExt;
use hyper::body::HttpBody;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response};
use shuttle_common::wasm::{Bytesable, Log, RequestWrapper, ResponseWrapper};
use shuttle_proto::runtime::runtime_server::Runtime;
use shuttle_proto::runtime::{
    self, LoadRequest, LoadResponse, StartRequest, StartResponse, StopRequest, StopResponse,
    SubscribeLogsRequest,
};
use shuttle_service::ServiceName;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;
use tracing::{error, trace};
use wasi_common::file::FileCaps;
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::sync::net::UnixStream as WasiUnixStream;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder};

extern crate rmp_serde as rmps;

const LOGS_FD: u32 = 20;
const PARTS_FD: u32 = 3;
const BODY_WRITE_FD: u32 = 4;
const BODY_READ_FD: u32 = 5;

pub struct AxumWasm {
    router: Mutex<Option<Router>>,
    logs_rx: Mutex<Option<Receiver<Result<runtime::LogItem, Status>>>>,
    logs_tx: Mutex<Sender<Result<runtime::LogItem, Status>>>,
    kill_tx: Mutex<Option<oneshot::Sender<String>>>,
}

impl AxumWasm {
    pub fn new() -> Self {
        // Allow about 2^15 = 32k logs of backpressure
        // We know the wasm currently handles about 16k requests per second (req / sec) so 16k seems to be a safe number
        // As we make performance gains elsewhere this might eventually become the new bottleneck to increase :D
        //
        // Testing has shown that a number half the req / sec yields poor performance. A number the same as the req / sec
        // seems acceptable so going with double the number for some headroom
        let (tx, rx) = mpsc::channel(1 << 15);

        Self {
            router: Mutex::new(None),
            logs_rx: Mutex::new(Some(rx)),
            logs_tx: Mutex::new(tx),
            kill_tx: Mutex::new(None),
        }
    }
}

impl Default for AxumWasm {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Runtime for AxumWasm {
    async fn load(
        &self,
        request: tonic::Request<LoadRequest>,
    ) -> Result<tonic::Response<LoadResponse>, Status> {
        let wasm_path = request.into_inner().path;
        trace!(wasm_path, "loading");

        let router = RouterBuilder::new()
            .map_err(|err| Status::from_error(err.into()))?
            .src(wasm_path)
            .build()
            .map_err(|err| Status::from_error(err.into()))?;

        *self.router.lock().unwrap() = Some(router);

        let message = LoadResponse { success: true };

        Ok(tonic::Response::new(message))
    }

    async fn start(
        &self,
        request: tonic::Request<StartRequest>,
    ) -> Result<tonic::Response<StartResponse>, Status> {
        let StartRequest {
            deployment_id,
            port,
            ..
        } = request.into_inner();

        let address = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port as u16);

        let logs_tx = self.logs_tx.lock().unwrap().clone();

        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel();

        *self.kill_tx.lock().unwrap() = Some(kill_tx);

        let router = self
            .router
            .lock()
            .unwrap()
            .take()
            .context("tried to start a service that was not loaded")
            .map_err(|err| Status::internal(err.to_string()))?;

        tokio::spawn(run_until_stopped(
            router,
            deployment_id,
            address,
            logs_tx,
            kill_rx,
        ));

        let message = StartResponse { success: true };

        Ok(tonic::Response::new(message))
    }

    type SubscribeLogsStream = ReceiverStream<Result<runtime::LogItem, Status>>;

    async fn subscribe_logs(
        &self,
        _request: tonic::Request<SubscribeLogsRequest>,
    ) -> Result<tonic::Response<Self::SubscribeLogsStream>, Status> {
        let logs_rx = self.logs_rx.lock().unwrap().deref_mut().take();

        if let Some(logs_rx) = logs_rx {
            Ok(tonic::Response::new(ReceiverStream::new(logs_rx)))
        } else {
            Err(Status::internal("logs have already been subscribed to"))
        }
    }

    async fn stop(
        &self,
        request: tonic::Request<StopRequest>,
    ) -> Result<tonic::Response<StopResponse>, Status> {
        let request = request.into_inner();

        let service_name = ServiceName::from_str(request.service_name.as_str())
            .map_err(|err| Status::from_error(Box::new(err)))?;

        let kill_tx = self.kill_tx.lock().unwrap().deref_mut().take();

        if let Some(kill_tx) = kill_tx {
            if kill_tx
                .send(format!("stopping deployment: {}", &service_name))
                .is_err()
            {
                error!("the receiver dropped");
                return Err(Status::internal("failed to stop deployment"));
            }

            Ok(tonic::Response::new(StopResponse { success: true }))
        } else {
            Err(Status::internal(
                "trying to stop a service that was not started",
            ))
        }
    }
}

struct RouterBuilder {
    engine: Engine,
    linker: Linker<WasiCtx>,
    src: Option<PathBuf>,
}

impl RouterBuilder {
    fn new() -> anyhow::Result<Self> {
        let engine = Engine::default();

        let mut linker: Linker<WasiCtx> = Linker::new(&engine);
        wasmtime_wasi::add_to_linker(&mut linker, |s| s)?;

        Ok(Self {
            engine,
            linker,
            src: None,
        })
    }

    fn src<P: AsRef<Path>>(mut self, src: P) -> Self {
        self.src = Some(src.as_ref().to_path_buf());
        self
    }

    fn build(self) -> anyhow::Result<Router> {
        let file = self.src.expect("module path should be set");
        let module = Module::from_file(&self.engine, file)?;

        for export in module.exports() {
            println!("export: {}", export.name());
        }

        Ok(Router {
            linker: self.linker,
            engine: self.engine,
            module,
        })
    }
}

#[derive(Clone)]
struct Router {
    linker: Linker<WasiCtx>,
    engine: Engine,
    module: Module,
}

impl Router {
    /// Send a HTTP request with body to given endpoint on the axum-wasm router and return the response
    async fn handle_request(
        &mut self,
        deployment_id: Vec<u8>,
        req: hyper::Request<Body>,
        logs_tx: Sender<Result<runtime::LogItem, Status>>,
    ) -> anyhow::Result<Response<Body>> {
        let wasi = WasiCtxBuilder::new()
            .inherit_stdio()
            .inherit_args()
            .context("failed to read args")?
            .build();

        let mut store = Store::new(&self.engine, wasi);
        self.linker.module(&mut store, "axum", &self.module)?;

        let (logs_stream, logs_client) =
            UnixStream::pair().context("failed to open logs unixstream")?;
        let (mut parts_stream, parts_client) =
            UnixStream::pair().context("failed to open parts unixstream")?;
        let (mut body_write_stream, body_write_client) =
            UnixStream::pair().context("failed to open body write unixstream")?;
        let (body_read_stream, body_read_client) =
            UnixStream::pair().context("failed to open body read unixstream")?;

        let logs_client = WasiUnixStream::from_cap_std(logs_client);
        let parts_client = WasiUnixStream::from_cap_std(parts_client);
        let body_write_client = WasiUnixStream::from_cap_std(body_write_client);
        let body_read_client = WasiUnixStream::from_cap_std(body_read_client);

        store
            .data_mut()
            .insert_file(LOGS_FD, Box::new(logs_client), FileCaps::all());

        store
            .data_mut()
            .insert_file(PARTS_FD, Box::new(parts_client), FileCaps::all());
        store
            .data_mut()
            .insert_file(BODY_WRITE_FD, Box::new(body_write_client), FileCaps::all());
        store
            .data_mut()
            .insert_file(BODY_READ_FD, Box::new(body_read_client), FileCaps::all());

        tokio::task::spawn(async move {
            let mut iter = logs_stream.bytes().filter_map(Result::ok);

            while let Some(log) = Log::from_bytes(&mut iter) {
                let mut log: runtime::LogItem = log.into();
                log.id = deployment_id.clone();

                logs_tx.send(Ok(log)).await.unwrap();
            }
        });

        let (parts, body) = req.into_parts();

        // Serialise request parts to rmp
        let request_rmp = RequestWrapper::from(parts).into_rmp();

        // Write request parts to wasm module
        parts_stream
            .write_all(&request_rmp)
            .context("failed to write http parts to wasm")?;

        // To protect our server, reject requests with bodies larger than
        // 64kbs of data.
        let body_size = body.size_hint().upper().unwrap_or(u64::MAX);

        if body_size > 1024 * 64 {
            let response = Response::builder()
                .status(hyper::http::StatusCode::PAYLOAD_TOO_LARGE)
                .body(Body::empty())
                .expect("building request with empty body should not fail");

            // Return early if body is too big
            return Ok(response);
        }

        let body_bytes = hyper::body::to_bytes(body)
            .await
            .context("failed to concatenate request body buffers")?;

        // Write body to wasm
        body_write_stream
            .write_all(body_bytes.as_ref())
            .context("failed to write body to wasm")?;

        // Drop stream to signal EOF
        drop(body_write_stream);

        // Call our function in wasm, telling it to route the request we've written to it
        // and write back a response
        trace!("calling Router");
        self.linker
            .get(&mut store, "axum", "__SHUTTLE_Axum_call")
            .expect("wasm module should be loaded and the router function should be available")
            .into_func()
            .expect("router function should be a function")
            .typed::<(RawFd, RawFd, RawFd, RawFd), ()>(&store)?
            .call(
                &mut store,
                (
                    LOGS_FD as i32,
                    PARTS_FD as i32,
                    BODY_WRITE_FD as i32,
                    BODY_READ_FD as i32,
                ),
            )?;

        // Read response parts from wasm
        let reader = BufReader::new(&mut parts_stream);

        // Deserialize response parts from rust messagepack
        let wrapper: ResponseWrapper =
            rmps::from_read(reader).context("failed to deserialize response parts")?;

        // Read response body from wasm, convert it to a Stream and pass it to hyper
        let reader = BufReader::new(body_read_stream);
        let stream = futures::stream::iter(reader.bytes()).try_chunks(2);
        let body = hyper::Body::wrap_stream(stream);

        let response: Response<Body> = wrapper
            .into_response_builder()
            .body(body)
            .context("failed to construct http response")?;

        Ok(response)
    }
}

/// Start a hyper server with a service that calls an axum router in WASM,
/// and a kill receiver for stopping the server.
async fn run_until_stopped(
    router: Router,
    deployment_id: Vec<u8>,
    address: SocketAddr,
    logs_tx: Sender<Result<runtime::LogItem, Status>>,
    kill_rx: tokio::sync::oneshot::Receiver<String>,
) {
    let make_service = make_service_fn(move |_conn| {
        let deployment_id = deployment_id.clone();
        let router = router.clone();
        let logs_tx = logs_tx.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                let deployment_id = deployment_id.clone();
                let mut router = router.clone();
                let logs_tx = logs_tx.clone();
                async move {
                    Ok::<_, Infallible>(
                        match router.handle_request(deployment_id, req, logs_tx).await {
                            Ok(res) => res,
                            Err(err) => {
                                error!("error sending request: {}", err);
                                Response::builder()
                                    .status(hyper::http::StatusCode::INTERNAL_SERVER_ERROR)
                                    .body(Body::empty())
                                    .expect("building request with empty body should not fail")
                            }
                        },
                    )
                }
            }))
        }
    });

    let server = hyper::Server::bind(&address).serve(make_service);

    trace!("starting hyper server on: {}", &address);
    tokio::select! {
        _ = server => {
            trace!("axum wasm server stopped");
        },
        message = kill_rx => {
            match message {
                Ok(msg) => trace!("{msg}"),
                Err(_) => trace!("the sender dropped")
            }
        }
    };
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use hyper::{http::HeaderValue, Method, Request, StatusCode, Version};
    use uuid::Uuid;

    #[tokio::test]
    async fn axum() {
        let router = RouterBuilder::new()
            .unwrap()
            .src("axum.wasm")
            .build()
            .unwrap();
        let id = Uuid::default().as_bytes().to_vec();
        let (tx, mut rx) = mpsc::channel(1);

        tokio::spawn(async move {
            while let Some(log) = rx.recv().await {
                println!("{log:?}");
            }
        });

        // GET /hello
        let request: Request<Body> = Request::builder()
            .method(Method::GET)
            .version(Version::HTTP_11)
            .uri("https://axum-wasm.example/hello")
            .body(Body::empty())
            .unwrap();

        let res = router
            .clone()
            .handle_request(id.clone(), request, tx.clone())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            &hyper::body::to_bytes(res.into_body())
                .await
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<u8>>()
                .as_ref(),
            b"Hello, World!"
        );

        // GET /goodbye
        let request: Request<Body> = Request::builder()
            .method(Method::GET)
            .version(Version::HTTP_11)
            .header("test", HeaderValue::from_static("goodbye"))
            .uri("https://axum-wasm.example/goodbye")
            .body(Body::from("Goodbye world body"))
            .unwrap();

        let res = router
            .clone()
            .handle_request(id.clone(), request, tx.clone())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            &hyper::body::to_bytes(res.into_body())
                .await
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<u8>>()
                .as_ref(),
            b"Goodbye, World!"
        );

        // GET /invalid
        let request: Request<Body> = Request::builder()
            .method(Method::GET)
            .version(Version::HTTP_11)
            .header("test", HeaderValue::from_static("invalid"))
            .uri("https://axum-wasm.example/invalid")
            .body(Body::empty())
            .unwrap();

        let res = router
            .clone()
            .handle_request(id.clone(), request, tx.clone())
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // POST /uppercase
        let request: Request<Body> = Request::builder()
            .method(Method::POST)
            .version(Version::HTTP_11)
            .header("test", HeaderValue::from_static("invalid"))
            .uri("https://axum-wasm.example/uppercase")
            .body("this should be uppercased".into())
            .unwrap();

        let res = router
            .clone()
            .handle_request(id, request, tx)
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            &hyper::body::to_bytes(res.into_body())
                .await
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<u8>>()
                .as_ref(),
            b"THIS SHOULD BE UPPERCASED"
        );
    }
}