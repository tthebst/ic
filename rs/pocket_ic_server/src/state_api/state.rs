/// This module contains the core state of the PocketIc server.
/// Axum handlers operate on a global state of type ApiState, whose
/// interface guarantees consistency and determinism.
use crate::pocket_ic::{
    AdvanceTimeAndTick, ApiResponse, EffectivePrincipal, GetCanisterHttp, MockCanisterHttp,
    PocketIc,
};
use crate::InstanceId;
use crate::{OpId, Operation};
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;
use base64;
use futures::future::Shared;
use hyper::header::{HeaderValue, HOST};
use hyper::Version;
use hyper_legacy::{client::connect::HttpConnector, Client};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_socks2::SocksConnector;
use ic_http_endpoints_public::cors_layer;
use ic_https_outcalls_adapter::CanisterHttp;
use ic_https_outcalls_adapter_client::grpc_status_code_to_reject;
use ic_https_outcalls_service::{
    canister_http_service_server::CanisterHttpService, CanisterHttpSendRequest,
    CanisterHttpSendResponse, HttpHeader, HttpMethod,
};
use ic_logger::replica_logger::no_op_logger;
use ic_metrics::MetricsRegistry;
use ic_state_machine_tests::RejectCode;
use ic_types::canister_http::CanisterHttpRequestId;
use ic_types::{canister_http::MAX_CANISTER_HTTP_RESPONSE_BYTES, CanisterId, SubnetId};
use pocket_ic::common::rest::{
    CanisterHttpHeader, CanisterHttpMethod, CanisterHttpReject, CanisterHttpReply,
    CanisterHttpRequest, CanisterHttpResponse, HttpGatewayBackend, HttpGatewayConfig,
    MockCanisterHttpResponse, Topology,
};
use pocket_ic::{ErrorCode, UserError, WasmResult};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::mpsc::error::TryRecvError,
    sync::mpsc::Receiver,
    sync::{mpsc, Mutex, RwLock},
    task::{spawn, spawn_blocking, JoinHandle},
    time::{self, sleep, Instant},
};
use tonic::Request;
use tracing::{error, info, trace};

// The maximum wait time for a computation to finish synchronously.
const DEFAULT_SYNC_WAIT_DURATION: Duration = Duration::from_secs(10);

// The timeout for executing an operation in auto progress mode.
const AUTO_PROGRESS_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
// The minimum delay between consecutive attempts to run an operation in auto progress mode.
const MIN_OPERATION_DELAY: Duration = Duration::from_millis(100);
// The minimum delay between consecutive attempts to read the graph in auto progress mode.
const READ_GRAPH_DELAY: Duration = Duration::from_millis(100);

pub const STATE_LABEL_HASH_SIZE: usize = 32;

/// Uniquely identifies a state.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Default, Deserialize)]
pub struct StateLabel(pub [u8; STATE_LABEL_HASH_SIZE]);

// The only error condition is if the vector has the wrong size.
pub struct InvalidSize;

impl std::fmt::Debug for StateLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StateLabel(")?;
        self.0.iter().try_for_each(|b| write!(f, "{:02X}", b))?;
        write!(f, ")")
    }
}

impl std::convert::TryFrom<Vec<u8>> for StateLabel {
    // The input vector having the wrong size is the only possible error condition.
    type Error = InvalidSize;

    fn try_from(v: Vec<u8>) -> Result<StateLabel, InvalidSize> {
        if v.len() != STATE_LABEL_HASH_SIZE {
            return Err(InvalidSize);
        }

        let mut res = StateLabel::default();
        res.0[0..STATE_LABEL_HASH_SIZE].clone_from_slice(v.as_slice());
        Ok(res)
    }
}

struct ProgressThread {
    handle: JoinHandle<()>,
    sender: mpsc::Sender<()>,
}

/// The state of the PocketIC API.
pub struct ApiState {
    // impl note: If locks are acquired on both fields, acquire first on instances, then on graph.
    instances: Arc<RwLock<Vec<Mutex<InstanceState>>>>,
    graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
    // threads making IC instances progress automatically
    progress_threads: RwLock<Vec<Mutex<Option<ProgressThread>>>>,
    sync_wait_time: Duration,
    // PocketIC server port
    port: Option<u16>,
    // status of HTTP gateway (true = running, false = stopped)
    http_gateways: Arc<RwLock<Vec<bool>>>,
}

#[derive(Default)]
pub struct PocketIcApiStateBuilder {
    initial_instances: Vec<PocketIc>,
    sync_wait_time: Option<Duration>,
    port: Option<u16>,
}

impl PocketIcApiStateBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    /// Computations are dispatched into background tasks. If a computation takes longer than
    /// [sync_wait_time], the update-operation returns, indicating that the given instance is busy.
    pub fn with_sync_wait_time(self, sync_wait_time: Duration) -> Self {
        Self {
            sync_wait_time: Some(sync_wait_time),
            ..self
        }
    }

    pub fn with_port(self, port: u16) -> Self {
        Self {
            port: Some(port),
            ..self
        }
    }

    /// Will make the given instance available in the initial state.
    pub fn add_initial_instance(mut self, instance: PocketIc) -> Self {
        self.initial_instances.push(instance);
        self
    }

    pub fn build(self) -> Arc<ApiState> {
        let graph: HashMap<StateLabel, Computations> = self
            .initial_instances
            .iter()
            .map(|i| (i.get_state_label(), Computations::default()))
            .collect();
        let graph = RwLock::new(graph);

        let instances: Vec<_> = self
            .initial_instances
            .into_iter()
            .map(|inst| Mutex::new(InstanceState::Available(inst)))
            .collect();
        let instances_len = instances.len();
        let instances = RwLock::new(instances);

        let progress_threads = RwLock::new((0..instances_len).map(|_| Mutex::new(None)).collect());

        let sync_wait_time = self.sync_wait_time.unwrap_or(DEFAULT_SYNC_WAIT_DURATION);

        Arc::new(ApiState {
            instances: instances.into(),
            graph: graph.into(),
            progress_threads,
            sync_wait_time,
            port: self.port,
            http_gateways: Arc::new(RwLock::new(Vec::new())),
        })
    }
}

#[derive(Clone)]
pub enum OpOut {
    NoOutput,
    Time(u64),
    CanisterResult(Result<WasmResult, UserError>),
    CanisterId(CanisterId),
    Cycles(u128),
    Bytes(Vec<u8>),
    StableMemBytes(Vec<u8>),
    MaybeSubnetId(Option<SubnetId>),
    Error(PocketIcError),
    RawResponse(Shared<ApiResponse>),
    Pruned,
    MessageId((EffectivePrincipal, Vec<u8>)),
    Topology(Topology),
    CanisterHttp(Vec<CanisterHttpRequest>),
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum PocketIcError {
    CanisterNotFound(CanisterId),
    BadIngressMessage(String),
    SubnetNotFound(candid::Principal),
    RequestRoutingError(String),
    InvalidCanisterHttpRequestId((SubnetId, CanisterHttpRequestId)),
}

impl From<Result<ic_state_machine_tests::WasmResult, ic_state_machine_tests::UserError>> for OpOut {
    fn from(
        r: Result<ic_state_machine_tests::WasmResult, ic_state_machine_tests::UserError>,
    ) -> Self {
        let res = {
            match r {
                Ok(ic_state_machine_tests::WasmResult::Reply(wasm)) => Ok(WasmResult::Reply(wasm)),
                Ok(ic_state_machine_tests::WasmResult::Reject(s)) => Ok(WasmResult::Reject(s)),
                Err(user_err) => Err(UserError {
                    code: ErrorCode::try_from(user_err.code() as u64).unwrap(),
                    description: user_err.description().to_string(),
                }),
            }
        };
        OpOut::CanisterResult(res)
    }
}

// TODO: Remove this Into: It's only used in the InstallCanisterAsController Operation, which also should be removed.
impl From<Result<(), ic_state_machine_tests::UserError>> for OpOut {
    fn from(r: Result<(), ic_state_machine_tests::UserError>) -> Self {
        let res = {
            match r {
                Ok(_) => Ok(WasmResult::Reply(vec![])),
                Err(user_err) => Err(UserError {
                    code: ErrorCode::try_from(user_err.code() as u64).unwrap(),
                    description: user_err.description().to_string(),
                }),
            }
        };
        OpOut::CanisterResult(res)
    }
}

impl std::fmt::Debug for OpOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpOut::NoOutput => write!(f, "NoOutput"),
            OpOut::Time(x) => write!(f, "Time({})", x),
            OpOut::Topology(t) => write!(f, "Topology({:?})", t),
            OpOut::CanisterId(cid) => write!(f, "CanisterId({})", cid),
            OpOut::Cycles(x) => write!(f, "Cycles({})", x),
            OpOut::CanisterResult(Ok(x)) => write!(f, "CanisterResult: Ok({:?})", x),
            OpOut::CanisterResult(Err(x)) => write!(f, "CanisterResult: Err({})", x),
            OpOut::Error(PocketIcError::CanisterNotFound(cid)) => {
                write!(f, "CanisterNotFound({})", cid)
            }
            OpOut::Error(PocketIcError::BadIngressMessage(msg)) => {
                write!(f, "BadIngressMessage({})", msg)
            }
            OpOut::Error(PocketIcError::SubnetNotFound(sid)) => {
                write!(f, "SubnetNotFound({})", sid)
            }
            OpOut::Error(PocketIcError::RequestRoutingError(msg)) => {
                write!(f, "RequestRoutingError({:?})", msg)
            }
            OpOut::Error(PocketIcError::InvalidCanisterHttpRequestId((
                subnet_id,
                canister_http_request_id,
            ))) => {
                write!(
                    f,
                    "InvalidCanisterHttpRequestId({},{:?})",
                    subnet_id, canister_http_request_id
                )
            }
            OpOut::Bytes(bytes) => write!(f, "Bytes({})", base64::encode(bytes)),
            OpOut::StableMemBytes(bytes) => write!(f, "StableMemory({})", base64::encode(bytes)),
            OpOut::MaybeSubnetId(Some(subnet_id)) => write!(f, "SubnetId({})", subnet_id),
            OpOut::MaybeSubnetId(None) => write!(f, "NoSubnetId"),
            OpOut::RawResponse(fut) => {
                write!(
                    f,
                    "ApiResp({:?})",
                    fut.peek().map(|(status, headers, bytes)| format!(
                        "{}:{:?}:{}",
                        status,
                        headers,
                        base64::encode(bytes)
                    ))
                )
            }
            OpOut::Pruned => write!(f, "Pruned"),
            OpOut::MessageId((effective_principal, message_id)) => {
                write!(
                    f,
                    "MessageId({:?},{})",
                    effective_principal,
                    hex::encode(message_id)
                )
            }
            OpOut::CanisterHttp(canister_http_reqeusts) => {
                write!(f, "CanisterHttp({:?})", canister_http_reqeusts)
            }
        }
    }
}

pub type Computations = HashMap<OpId, (StateLabel, OpOut)>;

/// The PocketIcApiState has a vector with elements of InstanceState.
/// When an operation is bound to an instance, the corresponding element in the
/// vector is replaced by a Busy variant which contains information about the
/// computation that is currently running. Afterwards, the instance is put back as
/// Available.
pub enum InstanceState {
    Busy {
        state_label: StateLabel,
        op_id: OpId,
    },
    Available(PocketIc),
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateError {
    message: String,
}

pub type UpdateResult = std::result::Result<UpdateReply, UpdateError>;

/// An operation bound to an instance can be dispatched, which updates the instance.
/// If the instance is already busy with an operation, the initial state and that operation
/// are returned.
/// If the result can be read from a cache, or if the computation is a fast read, an Output is
/// returned directly.
/// If the computation can be run and takes longer, a Started variant is returned, containing the
/// requested op and the initial state.
#[derive(Debug)]
pub enum UpdateReply {
    /// The requested instance is busy executing another update.
    Busy {
        state_label: StateLabel,
        op_id: OpId,
    },
    /// The requested instance is busy executing this current update.
    Started {
        state_label: StateLabel,
        op_id: OpId,
    },
    // This request is either cached or quickly executable, so we return
    // the output immediately.
    Output(OpOut),
}

impl UpdateReply {
    pub fn get_in_progress(&self) -> Option<(StateLabel, OpId)> {
        match self {
            Self::Busy { state_label, op_id } => Some((state_label.clone(), op_id.clone())),
            Self::Started { state_label, op_id } => Some((state_label.clone(), op_id.clone())),
            _ => None,
        }
    }
}

/// This trait lets us put a mock of the pocket_ic into the PocketIcApiState.
pub trait HasStateLabel {
    fn get_state_label(&self) -> StateLabel;
}

enum ApiVersion {
    V2,
    V3,
}

impl std::fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiVersion::V2 => write!(f, "v2"),
            ApiVersion::V3 => write!(f, "v3"),
        }
    }
}

fn received_stop_signal(rx: &mut Receiver<()>) -> bool {
    match rx.try_recv() {
        Ok(_) | Err(TryRecvError::Disconnected) => true,
        Err(TryRecvError::Empty) => false,
    }
}

impl ApiState {
    // Helper function for auto progress mode.
    // Executes an operation to completion and returns its `OpOut`
    // or `None` if the auto progress mode received a stop signal.
    async fn execute_operation(
        instances: Arc<RwLock<Vec<Mutex<InstanceState>>>>,
        graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
        instance_id: InstanceId,
        op: impl Operation + Send + Sync + 'static,
        rx: &mut Receiver<()>,
    ) -> Option<OpOut> {
        let op = Arc::new(op);
        loop {
            // It is safe to unwrap as there can only be an error if the instance does not exist
            // and there cannot be a progress thread for a non-existing instance (progress threads
            // are stopped before an instance is deleted).
            match Self::update_instances_with_timeout(
                instances.clone(),
                graph.clone(),
                op.clone(),
                instance_id,
                AUTO_PROGRESS_OPERATION_TIMEOUT,
            )
            .await
            .unwrap()
            {
                UpdateReply::Started { state_label, op_id } => {
                    break loop {
                        sleep(READ_GRAPH_DELAY).await;
                        if let Some((_, op_out)) =
                            Self::read_result(graph.clone(), &state_label, &op_id)
                        {
                            break Some(op_out);
                        }
                        if received_stop_signal(rx) {
                            break None;
                        }
                    }
                }
                UpdateReply::Busy { .. } => {}
                UpdateReply::Output(op_out) => break Some(op_out),
            };
            if received_stop_signal(rx) {
                break None;
            }
        }
    }

    /// For polling:
    /// The client lib dispatches a long running operation and gets a Started {state_label, op_id}.
    /// It then polls on that via this state tree api function.
    pub fn read_result(
        graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
        state_label: &StateLabel,
        op_id: &OpId,
    ) -> Option<(StateLabel, OpOut)> {
        if let Some((new_state_label, op_out)) = graph.try_read().ok()?.get(state_label)?.get(op_id)
        {
            Some((new_state_label.clone(), op_out.clone()))
        } else {
            None
        }
    }

    pub fn get_graph(&self) -> Arc<RwLock<HashMap<StateLabel, Computations>>> {
        self.graph.clone()
    }

    pub async fn add_instance(&self, instance: PocketIc) -> InstanceId {
        let mut instances = self.instances.write().await;
        let mut progress_threads = self.progress_threads.write().await;
        instances.push(Mutex::new(InstanceState::Available(instance)));
        progress_threads.push(Mutex::new(None));
        instances.len() - 1
    }

    pub async fn delete_instance(&self, instance_id: InstanceId) {
        self.stop_progress(instance_id).await;
        let instances = self.instances.read().await;
        let mut instance_state = instances[instance_id].lock().await;
        if let InstanceState::Available(pocket_ic) =
            std::mem::replace(&mut *instance_state, InstanceState::Deleted)
        {
            std::mem::drop(pocket_ic);
        }
    }

    pub async fn create_http_gateway(
        &self,
        http_gateway_config: HttpGatewayConfig,
    ) -> (InstanceId, u16) {
        use crate::state_api::routes::verify_cbor_content_header;
        use axum::extract::{DefaultBodyLimit, Path, Request as AxumRequest, State};
        use axum::handler::Handler;
        use axum::middleware::{self, Next};
        use axum::response::Response as AxumResponse;
        use axum::routing::{get, post};
        use axum::Router;
        use http_body_util::Full;
        use hyper::body::{Bytes, Incoming};
        use hyper::header::CONTENT_TYPE;
        use hyper::{Method, Request, Response, StatusCode, Uri};
        use hyper_util::client::legacy::{connect::HttpConnector, Client};
        use icx_proxy::{agent_handler, AppState, DnsCanisterConfig, ResolverState, Validator};
        use std::str::FromStr;

        async fn handler_status(
            State(replica_url): State<String>,
            bytes: Bytes,
        ) -> (StatusCode, Response<Incoming>) {
            let client =
                Client::builder(hyper_util::rt::TokioExecutor::new()).build(HttpConnector::new());
            let url = format!("{}/api/v2/status", replica_url);
            let req = Request::builder()
                .uri(url)
                .header(CONTENT_TYPE, "application/cbor")
                .body(Full::<Bytes>::new(bytes))
                .unwrap();
            let resp = client.request(req).await.unwrap();

            (resp.status(), resp)
        }

        async fn handler_api_canister(
            api_version: ApiVersion,
            replica_url: String,
            effective_canister_id: CanisterId,
            endpoint: &str,
            bytes: Bytes,
        ) -> (StatusCode, Response<Incoming>) {
            let client =
                Client::builder(hyper_util::rt::TokioExecutor::new()).build(HttpConnector::new());
            let url = format!(
                "{}/api/{}/canister/{}/{}",
                replica_url, api_version, effective_canister_id, endpoint
            );
            let req = Request::builder()
                .method(Method::POST)
                .uri(url)
                .header(CONTENT_TYPE, "application/cbor")
                .body(Full::<Bytes>::new(bytes))
                .unwrap();
            let resp = client.request(req).await.unwrap();

            (resp.status(), resp)
        }

        async fn handler_call_v2(
            State(replica_url): State<String>,
            Path(effective_canister_id): Path<CanisterId>,
            bytes: Bytes,
        ) -> (StatusCode, Response<Incoming>) {
            handler_api_canister(
                ApiVersion::V2,
                replica_url,
                effective_canister_id,
                "call",
                bytes,
            )
            .await
        }

        async fn handler_call_v3(
            State(replica_url): State<String>,
            Path(effective_canister_id): Path<CanisterId>,
            bytes: Bytes,
        ) -> (StatusCode, Response<Incoming>) {
            handler_api_canister(
                ApiVersion::V3,
                replica_url,
                effective_canister_id,
                "call",
                bytes,
            )
            .await
        }

        async fn handler_query(
            State(replica_url): State<String>,
            Path(effective_canister_id): Path<CanisterId>,
            bytes: Bytes,
        ) -> (StatusCode, Response<Incoming>) {
            handler_api_canister(
                ApiVersion::V2,
                replica_url,
                effective_canister_id,
                "query",
                bytes,
            )
            .await
        }

        async fn handler_read_state(
            State(replica_url): State<String>,
            Path(effective_canister_id): Path<CanisterId>,
            bytes: Bytes,
        ) -> (StatusCode, Response<Incoming>) {
            handler_api_canister(
                ApiVersion::V2,
                replica_url,
                effective_canister_id,
                "read_state",
                bytes,
            )
            .await
        }

        // converts an HTTP request to an HTTP/1.1 request required by icx-proxy
        async fn http2_middleware(mut request: AxumRequest, next: Next) -> AxumResponse {
            let uri = Uri::try_from(
                request
                    .uri()
                    .path_and_query()
                    .map(|v| v.as_str())
                    .unwrap_or(request.uri().path()),
            )
            .unwrap();
            let authority = request.uri().authority().map(|a| a.to_string());
            *request.version_mut() = Version::HTTP_11;
            *request.uri_mut() = uri;
            if let Some(authority) = authority {
                if !request.headers().contains_key(HOST) {
                    request
                        .headers_mut()
                        .insert(HOST, HeaderValue::from_str(&authority).unwrap());
                }
            }
            next.run(request).await
        }

        let port = http_gateway_config.listen_at.unwrap_or_default();
        let addr = format!("[::]:{}", port);
        let listener = std::net::TcpListener::bind(&addr)
            .unwrap_or_else(|_| panic!("Failed to start HTTP gateway on port {}", port));
        let real_port = listener.local_addr().unwrap().port();

        let mut http_gateways = self.http_gateways.write().await;
        http_gateways.push(true);
        let instance_id = http_gateways.len() - 1;
        drop(http_gateways);

        let http_gateways = self.http_gateways.clone();
        let pocket_ic_server_port = self.port.unwrap();
        spawn(async move {
            let replica_url = match http_gateway_config.forward_to {
                HttpGatewayBackend::Replica(replica_url) => replica_url,
                HttpGatewayBackend::PocketIcInstance(instance_id) => {
                    format!(
                        "http://localhost:{}/instances/{}/",
                        pocket_ic_server_port, instance_id
                    )
                }
            };
            let agent = ic_agent::Agent::builder()
                .with_url(replica_url.clone())
                .build()
                .unwrap();
            agent.fetch_root_key().await.unwrap();
            let replica_uri = Uri::from_str(&replica_url).unwrap();
            let replicas = vec![(agent, replica_uri)];
            let gateway_domains = http_gateway_config
                .domains
                .unwrap_or(vec!["localhost".to_string()]);
            let aliases: Vec<String> = vec![];
            let suffixes: Vec<String> = gateway_domains;
            let resolver = ResolverState {
                dns: DnsCanisterConfig::new(aliases, suffixes).unwrap(),
            };
            let validator = Validator::default();
            let app_state = AppState::new_for_testing(replicas, resolver, validator);
            let fallback_handler = agent_handler.with_state(app_state);

            let router = Router::new()
                .route("/api/v2/status", get(handler_status))
                .route(
                    "/api/v2/canister/:ecid/call",
                    post(handler_call_v2)
                        .layer(axum::middleware::from_fn(verify_cbor_content_header)),
                )
                .route(
                    "/api/v3/canister/:ecid/call",
                    post(handler_call_v3)
                        .layer(axum::middleware::from_fn(verify_cbor_content_header)),
                )
                .route(
                    "/api/v2/canister/:ecid/query",
                    post(handler_query)
                        .layer(axum::middleware::from_fn(verify_cbor_content_header)),
                )
                .route(
                    "/api/v2/canister/:ecid/read_state",
                    post(handler_read_state)
                        .layer(axum::middleware::from_fn(verify_cbor_content_header)),
                )
                .fallback_service(fallback_handler)
                .layer(DefaultBodyLimit::disable())
                .layer(cors_layer())
                .layer(middleware::from_fn(http2_middleware))
                .with_state(replica_url.trim_end_matches('/').to_string())
                .into_make_service();

            let handle = Handle::new();
            let shutdown_handle = handle.clone();
            let http_gateways_for_shutdown = http_gateways.clone();
            tokio::spawn(async move {
                loop {
                    let guard = http_gateways_for_shutdown.read().await;
                    if !guard[instance_id] {
                        shutdown_handle.shutdown();
                        break;
                    }
                    drop(guard);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
            if let Some(https_config) = http_gateway_config.https_config {
                let config = RustlsConfig::from_pem_file(
                    PathBuf::from(https_config.cert_path),
                    PathBuf::from(https_config.key_path),
                )
                .await;
                match config {
                    Ok(config) => {
                        axum_server::from_tcp_rustls(listener, config)
                            .handle(handle)
                            .serve(router)
                            .await
                            .unwrap();
                    }
                    Err(e) => {
                        error!("TLS config could not be created: {:?}", e);
                        let mut guard = http_gateways.write().await;
                        guard[instance_id] = false;
                        return;
                    }
                }
            } else {
                axum_server::from_tcp(listener)
                    .handle(handle)
                    .serve(router)
                    .await
                    .unwrap();
            }

            info!("Terminating HTTP gateway.");
        });
        (instance_id, real_port)
    }

    pub async fn stop_http_gateway(&self, instance_id: InstanceId) {
        let mut http_gateways = self.http_gateways.write().await;
        if instance_id < http_gateways.len() {
            http_gateways[instance_id] = false;
        }
    }

    async fn make_http_request(
        canister_http_request: CanisterHttpRequest,
    ) -> Result<CanisterHttpReply, (RejectCode, String)> {
        // Socks client setup
        // We don't really use the Socks client in PocketIC as we set `socks_proxy_allowed: false` in the request,
        // but we still have to provide one when constructing the production `CanisterHttp` object
        // and thus we use a reserved (and invalid) proxy IP address.
        let mut http_connector = HttpConnector::new();
        http_connector.enforce_http(false);
        http_connector.set_connect_timeout(Some(Duration::from_secs(2)));
        let proxy_connector = SocksConnector {
            proxy_addr: "http://240.0.0.0:8080"
                .parse::<tonic::transport::Uri>()
                .expect("Failed to parse socks url."),
            auth: None,
            connector: http_connector.clone(),
        };
        let https_connector = HttpsConnectorBuilder::new()
            .with_native_roots()
            .https_only()
            .enable_http1()
            .wrap_connector(proxy_connector);
        let socks_client = Client::builder().build::<_, hyper_legacy::Body>(https_connector);

        // Https client setup.
        let builder = HttpsConnectorBuilder::new()
            .with_native_roots()
            .https_or_http()
            .enable_http1();
        let https_client = Client::builder()
            .build::<_, hyper_legacy::Body>(builder.wrap_connector(http_connector));

        let canister_http = CanisterHttp::new(
            https_client,
            socks_client,
            no_op_logger(),
            &MetricsRegistry::default(),
        );
        let canister_http_request = CanisterHttpSendRequest {
            url: canister_http_request.url,
            method: match canister_http_request.http_method {
                CanisterHttpMethod::GET => HttpMethod::Get.into(),
                CanisterHttpMethod::POST => HttpMethod::Post.into(),
                CanisterHttpMethod::HEAD => HttpMethod::Head.into(),
            },
            max_response_size_bytes: canister_http_request
                .max_response_bytes
                .unwrap_or(MAX_CANISTER_HTTP_RESPONSE_BYTES),
            headers: canister_http_request
                .headers
                .into_iter()
                .map(|h| HttpHeader {
                    name: h.name,
                    value: h.value,
                })
                .collect(),
            body: canister_http_request.body,
            socks_proxy_allowed: false,
        };
        let request = Request::new(canister_http_request);
        canister_http
            .canister_http_send(request)
            .await
            .map(|adapter_response| {
                let CanisterHttpSendResponse {
                    status,
                    headers,
                    content: body,
                } = adapter_response.into_inner();
                CanisterHttpReply {
                    status: status.try_into().unwrap(),
                    headers: headers
                        .into_iter()
                        .map(|HttpHeader { name, value }| CanisterHttpHeader { name, value })
                        .collect(),
                    body,
                }
            })
            .map_err(|grpc_status| {
                (
                    grpc_status_code_to_reject(grpc_status.code()),
                    grpc_status.message().to_string(),
                )
            })
    }

    async fn process_canister_http_requests(
        instances: Arc<RwLock<Vec<Mutex<InstanceState>>>>,
        graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
        instance_id: InstanceId,
        rx: &mut Receiver<()>,
    ) -> Option<()> {
        let get_canister_http_op = GetCanisterHttp;
        let canister_http_requests = match Self::execute_operation(
            instances.clone(),
            graph.clone(),
            instance_id,
            get_canister_http_op,
            rx,
        )
        .await?
        {
            OpOut::CanisterHttp(canister_http) => canister_http,
            out => panic!("Unexpected OpOut: {:?}", out),
        };
        let mut mock_canister_http_responses = vec![];
        for canister_http_request in canister_http_requests {
            let subnet_id = canister_http_request.subnet_id;
            let request_id = canister_http_request.request_id;
            let response = match Self::make_http_request(canister_http_request).await {
                Ok(reply) => CanisterHttpResponse::CanisterHttpReply(reply),
                Err((reject_code, e)) => {
                    CanisterHttpResponse::CanisterHttpReject(CanisterHttpReject {
                        reject_code: reject_code as u64,
                        message: e,
                    })
                }
            };
            let mock_canister_http_response = MockCanisterHttpResponse {
                subnet_id,
                request_id,
                response,
            };
            mock_canister_http_responses.push(mock_canister_http_response);
        }
        for mock_canister_http_response in mock_canister_http_responses {
            let mock_canister_http_op = MockCanisterHttp {
                mock_canister_http_response,
            };
            Self::execute_operation(
                instances.clone(),
                graph.clone(),
                instance_id,
                mock_canister_http_op,
                rx,
            )
            .await?;
        }
        Some(())
    }

    pub async fn auto_progress(&self, instance_id: InstanceId) {
        let progress_threads = self.progress_threads.read().await;
        let mut progress_thread = progress_threads[instance_id].lock().await;
        let instances = self.instances.clone();
        let graph = self.graph.clone();
        if progress_thread.is_none() {
            let (tx, mut rx) = mpsc::channel::<()>(1);
            let handle = spawn(async move {
                let mut now = Instant::now();
                loop {
                    let start = Instant::now();
                    let old = std::mem::replace(&mut now, Instant::now());
                    let op = AdvanceTimeAndTick(now.duration_since(old));
                    if Self::execute_operation(
                        instances.clone(),
                        graph.clone(),
                        instance_id,
                        op,
                        &mut rx,
                    )
                    .await
                    .is_none()
                    {
                        return;
                    }
                    if Self::process_canister_http_requests(
                        instances.clone(),
                        graph.clone(),
                        instance_id,
                        &mut rx,
                    )
                    .await
                    .is_none()
                    {
                        return;
                    }
                    let duration = start.elapsed();
                    sleep(std::cmp::max(duration, MIN_OPERATION_DELAY)).await;
                    if received_stop_signal(&mut rx) {
                        return;
                    }
                }
            });
            *progress_thread = Some(ProgressThread { handle, sender: tx });
        }
    }

    pub async fn stop_progress(&self, instance_id: InstanceId) {
        let progress_threads = self.progress_threads.read().await;
        let mut progress_thread = progress_threads[instance_id].lock().await;
        if let Some(t) = progress_thread.take() {
            t.sender.send(()).await.unwrap();
            t.handle.await.unwrap();
        }
    }

    pub async fn list_instance_states(&self) -> Vec<String> {
        let instances = self.instances.read().await;
        let mut res = vec![];

        for instance_state in &*instances {
            let instance_state = &*instance_state.lock().await;
            match instance_state {
                InstanceState::Busy { state_label, op_id } => {
                    res.push(format!("Busy({:?}, {:?})", state_label, op_id))
                }
                InstanceState::Available(_) => res.push("Available".to_string()),
                InstanceState::Deleted => res.push("Deleted".to_string()),
            }
        }
        res
    }

    /// An operation bound to an instance (a Computation) can update the PocketIC state.
    ///
    /// * If the instance is busy executing an operation, the call returns [UpdateReply::Busy]
    /// immediately. In that case, the state label and operation id contained in the result
    /// indicate that the instance is busy with a previous operation.
    ///
    /// * If the instance is available and the computation exceeds a (short) timeout,
    /// [UpdateReply::Busy] is returned.
    ///
    /// * If the computation finished within the timeout, [UpdateReply::Output] is returned
    /// containing the result.
    ///
    /// Operations are _not_ queued by default. Thus, if the instance is busy with an existing operation,
    /// the client has to retry until the operation is done. Some operations for which the client
    /// might be unable to retry are exceptions to this rule and they are queued up implicitly
    /// by a retry mechanism inside PocketIc.
    pub async fn update<O>(&self, op: Arc<O>, instance_id: InstanceId) -> UpdateResult
    where
        O: Operation + Send + Sync + 'static,
    {
        self.update_with_timeout(op, instance_id, None).await
    }

    /// Same as [Self::update] except that the timeout can be specified manually. This is useful in
    /// cases when clients want to enforce a long-running blocking call.
    pub async fn update_with_timeout<O>(
        &self,
        op: Arc<O>,
        instance_id: InstanceId,
        sync_wait_time: Option<Duration>,
    ) -> UpdateResult
    where
        O: Operation + Send + Sync + 'static,
    {
        let sync_wait_time = sync_wait_time.unwrap_or(self.sync_wait_time);
        Self::update_instances_with_timeout(
            self.instances.clone(),
            self.graph.clone(),
            op,
            instance_id,
            sync_wait_time,
        )
        .await
    }

    /// Same as [Self::update] except that the timeout can be specified manually. This is useful in
    /// cases when clients want to enforce a long-running blocking call.
    async fn update_instances_with_timeout<O>(
        instances: Arc<RwLock<Vec<Mutex<InstanceState>>>>,
        graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
        op: Arc<O>,
        instance_id: InstanceId,
        sync_wait_time: Duration,
    ) -> UpdateResult
    where
        O: Operation + Send + Sync + 'static,
    {
        let op_id = op.id().0;
        trace!(
            "update_with_timeout::start instance_id={} op_id={}",
            instance_id,
            op_id,
        );
        let instances_cloned = instances.clone();
        let instances_locked = instances_cloned.read().await;
        let (bg_task, busy_outcome) = if let Some(instance_mutex) =
            instances_locked.get(instance_id)
        {
            let mut instance_state = instance_mutex.lock().await;
            // If this instance is busy, return the running op and initial state
            match &*instance_state {
                InstanceState::Deleted => {
                    return Err(UpdateError {
                        message: "Instance was deleted".to_string(),
                    });
                }
                // TODO: cache lookup possible with this state_label and our own op_id
                InstanceState::Busy { state_label, op_id } => {
                    return Ok(UpdateReply::Busy {
                        state_label: state_label.clone(),
                        op_id: op_id.clone(),
                    });
                }
                InstanceState::Available(pocket_ic) => {
                    // move pocket_ic out

                    let state_label = pocket_ic.get_state_label();
                    let op_id = op.id();
                    let busy = InstanceState::Busy {
                        state_label: state_label.clone(),
                        op_id: op_id.clone(),
                    };
                    let InstanceState::Available(mut pocket_ic) =
                        std::mem::replace(&mut *instance_state, busy)
                    else {
                        unreachable!()
                    };

                    let bg_task = {
                        let old_state_label = state_label.clone();
                        let op_id = op_id.clone();
                        let graph = graph.clone();
                        move || {
                            trace!(
                                "bg_task::start instance_id={} state_label={:?} op_id={}",
                                instance_id,
                                old_state_label,
                                op_id.0,
                            );
                            let result = op.compute(&mut pocket_ic);
                            let new_state_label = pocket_ic.get_state_label();
                            // add result to graph, but grab instance lock first!
                            let instances = instances.blocking_read();
                            let mut graph_guard = graph.blocking_write();
                            let cached_computations =
                                graph_guard.entry(old_state_label.clone()).or_default();
                            cached_computations
                                .insert(op_id.clone(), (new_state_label, result.clone()));
                            drop(graph_guard);
                            let mut instance_state = instances[instance_id].blocking_lock();
                            if let InstanceState::Deleted = &*instance_state {
                                std::mem::drop(pocket_ic);
                            } else {
                                *instance_state = InstanceState::Available(pocket_ic);
                            }
                            trace!("bg_task::end instance_id={} op_id={}", instance_id, op_id.0);
                            // also return old_state_label so we can prune graph if we return quickly
                            (result, old_state_label)
                        }
                    };

                    // cache miss: replace pocket_ic instance in the vector with Busy
                    (bg_task, UpdateReply::Started { state_label, op_id })
                }
            }
        } else {
            return Err(UpdateError {
                message: "Instance not found".to_string(),
            });
        };
        // drop lock, otherwise we end up with a deadlock
        std::mem::drop(instances_locked);

        // We schedule a blocking background task on the tokio runtime. Note that if all
        // blocking workers are busy, the task is put on a queue (which is what we want).
        //
        // Note: One issue here is that we drop the join handle "on the floor". Threads
        // that are not awaited upon before exiting the process are known to cause spurios
        // issues. This should not be a problem as the tokio Executor will wait
        // indefinitively for threads to return, unless a shutdown timeout is configured.
        //
        // See: https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
        let bg_handle = spawn_blocking(bg_task);

        // if the operation returns "in time", we return the result, otherwise we indicate to the
        // client that the instance is busy.
        //
        // note: this assumes that cancelling the JoinHandle does not stop the execution of the
        // background task. This only works because the background thread, in this case, is a
        // kernel thread.
        if let Ok(Ok((op_out, old_state_label))) = time::timeout(sync_wait_time, bg_handle).await {
            trace!(
                "update_with_timeout::synchronous instance_id={} op_id={}",
                instance_id,
                op_id,
            );
            // prune this sync computation from graph, but only the value
            let mut graph_guard = graph.write().await;
            let cached_computations = graph_guard.entry(old_state_label.clone()).or_default();
            let (new_state_label, _) = cached_computations.get(&OpId(op_id.clone())).unwrap();
            cached_computations.insert(OpId(op_id), (new_state_label.clone(), OpOut::Pruned));
            drop(graph_guard);

            return Ok(UpdateReply::Output(op_out));
        }

        trace!(
            "update_with_timeout::timeout instance_id={} op_id={}",
            instance_id,
            op_id,
        );
        Ok(busy_outcome)
    }
}

impl std::fmt::Debug for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { state_label, op_id } => {
                write!(f, "Busy {{ {state_label:?}, {op_id:?} }}")?
            }
            Self::Available(pic) => write!(f, "Available({:?})", pic.get_state_label())?,
            Self::Deleted => write!(f, "Deleted")?,
        }
        Ok(())
    }
}

impl std::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let instances = self.instances.blocking_read();
        let graph = self.graph.blocking_read();

        writeln!(f, "Instances:")?;
        for (idx, instance) in instances.iter().enumerate() {
            writeln!(f, "  [{idx}] {instance:?}")?;
        }

        writeln!(f, "Graph:")?;
        for (k, v) in graph.iter() {
            writeln!(f, "  {k:?} => {v:?}")?;
        }
        Ok(())
    }
}
