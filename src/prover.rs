use crate::middleware::ZstdRequestCompressionMiddleware;
use async_trait::async_trait;
use core::time::Duration;
use reqwest::{header::CONTENT_TYPE, Url};

use anyhow::{anyhow, Result};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;

use crate::utils::proving_timestamps_from_response;
use scroll_proving_sdk::{
    config::Config as SdkConfig,
    prover::{
        proving_service::{
            GetVkRequest, GetVkResponse, ProveRequest, ProveResponse, QueryTaskRequest,
            QueryTaskResponse, TaskStatus,
        },
        CircuitType, ProvingService,
    },
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CloudProverConfig {
    pub sdk_config: SdkConfig,
    pub base_url: String,
    pub api_key: String,
    pub retry_count: u32,
    pub retry_wait_time_sec: u64,
    pub connection_timeout_sec: u64,
}

impl CloudProverConfig {
    pub fn from_reader<R>(reader: R) -> Result<Self>
    where
        R: std::io::Read,
    {
        serde_json::from_reader(reader).map_err(|e| anyhow!(e))
    }

    pub fn from_file(file_name: String) -> Result<Self> {
        let file = File::open(file_name)?;
        Self::from_reader(&file)
    }

    fn get_env_var(key: &str) -> Result<Option<String>> {
        std::env::var_os(key)
            .map(|val| {
                val.to_str()
                    .ok_or_else(|| anyhow!("{key} env var is not valid UTF-8"))
                    .map(String::from)
            })
            .transpose()
    }

    pub fn from_file_and_env(file_name: String) -> Result<Self> {
        let mut cfg = Self::from_file(file_name)?;
        cfg.sdk_config.override_with_env()?;

        if let Some(val) = Self::get_env_var("PROVING_SERVICE_BASE_URL")? {
            cfg.base_url = val;
        }

        if let Some(val) = Self::get_env_var("PROVING_SERVICE_API_KEY")? {
            cfg.api_key = val;
        }

        Ok(cfg)
    }
}

pub struct CloudProver {
    base_url: Url,
    api_key: String,
    send_timeout: Duration,
    client: ClientWithMiddleware,
}

#[derive(Deserialize)]
pub struct VerificationKey {
    verification_key: String,
}

#[derive(Deserialize)]
pub struct SindriProofInfoResponse {
    pub compute_time_sec: Option<f64>,
    pub date_created: String,
    pub error: Option<String>,
    pub proof_id: String,
    pub proof: Option<serde_json::Value>,
    pub queue_time_sec: Option<f64>,
    pub status: SindriTaskStatus,
    pub verification_key: Option<VerificationKey>,
}

#[derive(Deserialize)]
pub enum SindriTaskStatus {
    #[serde(rename = "Queued")]
    Queued,
    #[serde(rename = "In Progress")]
    Proving,
    #[serde(rename = "Ready")]
    Success,
    #[serde(rename = "Failed")]
    Failed,
}

impl From<SindriTaskStatus> for TaskStatus {
    fn from(status: SindriTaskStatus) -> Self {
        match status {
            SindriTaskStatus::Queued => TaskStatus::Queued,
            SindriTaskStatus::Proving => TaskStatus::Proving,
            SindriTaskStatus::Success => TaskStatus::Success,
            SindriTaskStatus::Failed => TaskStatus::Failed,
        }
    }
}

enum MethodClass {
    Circuit(CircuitType),
    Proof(String),
}

// Re-encode the vk because the encoding scheme used by Sindri is different from the one used in scroll internally.
fn reformat_vk(vk_old: String) -> anyhow::Result<String> {
    log::debug!("vk_old: {:?}", vk_old);

    // decode base64 without padding
    let vk = base64::decode_config(vk_old, base64::URL_SAFE_NO_PAD)?;
    // encode with padding
    let vk_new = base64::encode_config(vk, base64::STANDARD);

    Ok(vk_new)
}

#[async_trait]
impl ProvingService for CloudProver {
    fn is_local(&self) -> bool {
        false
    }

    // There are three steps to prove a circuit:
    // 1. Get the verification key from the Sindri proving service.
    // 2. Submit a proof to the Sindri proving service.
    // 3. Query the status of the proof task.

    async fn get_vks(&self, req: GetVkRequest) -> GetVkResponse {
        if req.circuit_version != THIS_CIRCUIT_VERSION {
            return GetVkResponse {
                vks: Vec::new(),
                error: Some("circuit version mismatch".to_string()),
            };
        };

        #[derive(serde::Deserialize)]
        struct SindriCircuitInfoResponse {
            verification_key: VerificationKey,
        }

        let mut vks: Vec<String> = Vec::new();
        for circuit_type in req.circuit_types {
            match self
                .get_with_token::<SindriCircuitInfoResponse>(
                    MethodClass::Circuit(circuit_type),
                    "detail",
                    None,
                )
                .await
            {
                Ok(resp) => match reformat_vk(resp.verification_key.verification_key) {
                    Ok(vk) => {
                        if !vks.contains(&vk) {
                            vks.push(vk)
                        }
                    }
                    Err(e) => {
                        return GetVkResponse {
                            vks,
                            error: Some(e.to_string()),
                        }
                    }
                },
                Err(e) => {
                    return GetVkResponse {
                        vks,
                        error: Some(e.to_string()),
                    }
                }
            }
        }

        GetVkResponse { vks, error: None }
    }

    async fn prove(&self, req: ProveRequest) -> ProveResponse {
        if req.circuit_version != THIS_CIRCUIT_VERSION {
            return build_prove_error_response(&req, "circuit version mismatch");
        };

        let input = match reprocess_prove_input(&req) {
            Ok(input) => input,
            Err(e) => return build_prove_error_response(&req, &e.to_string()),
        };

        #[derive(serde::Deserialize, serde::Serialize)]
        struct SindriProveRequest {
            proof_input: String,
            perform_verify: bool,
        }

        let sindri_req = SindriProveRequest {
            proof_input: input,
            perform_verify: true,
        };

        match self
            .post_with_token::<SindriProveRequest, SindriProofInfoResponse>(
                MethodClass::Circuit(req.circuit_type),
                "prove",
                &sindri_req,
            )
            .await
        {
            Ok(resp) => {
                let (created_at, started_at, finished_at) = proving_timestamps_from_response(&resp);
                ProveResponse {
                    task_id: resp.proof_id,
                    circuit_type: req.circuit_type,
                    circuit_version: req.circuit_version,
                    hard_fork_name: req.hard_fork_name,
                    status: resp.status.into(),
                    created_at,
                    started_at,
                    finished_at,
                    compute_time_sec: resp.compute_time_sec,
                    input: Some(req.input.clone()),
                    proof: serde_json::to_string(&resp.proof).ok(),
                    vk: resp.verification_key.map(|vk| vk.verification_key),
                    error: resp.error,
                }
            }
            Err(e) => {
                return build_prove_error_response(&req, &format!("Failed to request proof: {}", e))
            }
        }
    }

    async fn query_task(&self, req: QueryTaskRequest) -> QueryTaskResponse {
        let query_params: HashMap<String, String> = [
            ("include_proof", "true"),
            ("include_public", "true"),
            ("include_verification_key", "true"),
        ]
        .iter()
        .map(|&(k, v)| (k.to_string(), v.to_string()))
        .collect();

        match self
            .get_with_token::<SindriProofInfoResponse>(
                MethodClass::Proof(req.task_id.clone()),
                "detail",
                Some(query_params),
            )
            .await
        {
            Ok(resp) => {
                let (created_at, started_at, finished_at) = proving_timestamps_from_response(&resp);
                QueryTaskResponse {
                    task_id: resp.proof_id,
                    circuit_type: CircuitType::Undefined, // TODO:
                    circuit_version: "".to_string(),
                    hard_fork_name: "".to_string(),
                    status: resp.status.into(),
                    created_at,
                    started_at,
                    finished_at,
                    compute_time_sec: resp.compute_time_sec,
                    input: None,
                    proof: serde_json::to_string(&resp.proof).ok(),
                    vk: resp.verification_key.map(|vk| vk.verification_key),
                    error: resp.error,
                }
            }
            Err(e) => {
                log::error!("Failed to query proof: {:?}", e);
                QueryTaskResponse {
                    task_id: req.task_id,
                    circuit_type: CircuitType::Undefined,
                    circuit_version: "".to_string(),
                    hard_fork_name: "".to_string(),
                    status: TaskStatus::Queued,
                    created_at: 0.0,
                    started_at: None,
                    finished_at: None,
                    compute_time_sec: None,
                    input: None,
                    proof: None,
                    vk: None,
                    error: Some(format!("Failed to query proof: {}", e)),
                }
            }
        }
    }
}

fn build_prove_error_response(req: &ProveRequest, error_msg: &str) -> ProveResponse {
    ProveResponse {
        task_id: String::new(),
        circuit_type: req.circuit_type,
        circuit_version: req.circuit_version.clone(),
        hard_fork_name: req.hard_fork_name.clone(),
        status: TaskStatus::Failed,
        created_at: 0.0,
        started_at: None,
        finished_at: None,
        compute_time_sec: None,
        input: Some(req.input.clone()),
        proof: None,
        vk: None,
        error: Some(error_msg.to_string()),
    }
}

// Remove the "batch_proofs" layer because Sindri API expects the inner array as the input directly
fn reprocess_prove_input(req: &ProveRequest) -> anyhow::Result<String> {
    if req.circuit_type == CircuitType::Bundle {
        let bundle_task_detail: prover_darwin_v2::BundleProvingTask =
            serde_json::from_str(&req.input)?;
        Ok(serde_json::to_string(&bundle_task_detail.batch_proofs)?)
    } else {
        Ok(req.input.clone())
    }
}

// alternatively, we can just read it from the config
const THIS_CIRCUIT_VERSION: &str = "v0.13.1";

// Sindri API client path. This is the base path for all
// Sindri API calls in this version of the Sindri Scroll SDK.
const SINDRI_API_PATH: &str = "/api/v1/";

impl CloudProver {
    pub fn new(cfg: CloudProverConfig) -> Self {
        let retry_wait_duration = Duration::from_secs(cfg.retry_wait_time_sec);
        let retry_policy = ExponentialBackoff::builder()
            .retry_bounds(retry_wait_duration / 2, retry_wait_duration)
            .build_with_max_retries(cfg.retry_count);
        let client = ClientBuilder::new(
            // Explicitly enable zstd response compression.
            reqwest::Client::builder().zstd(true).build().unwrap(),
        )
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .with(ZstdRequestCompressionMiddleware)
        .build();

        let base_url = Url::parse(&cfg.base_url).expect("cannot parse cloud prover base_url");
        let api_url = base_url
            .join(SINDRI_API_PATH)
            .expect("cannot parse cloud prover api_url");

        Self {
            base_url: api_url,
            api_key: cfg.api_key,
            send_timeout: Duration::from_secs(cfg.connection_timeout_sec),
            client,
        }
    }

    fn build_url(
        &self,
        method_class: MethodClass,
        method: &str,
        query_params: Option<HashMap<String, String>>,
    ) -> anyhow::Result<Url> {
        let method_base = match method_class {
            MethodClass::Circuit(circuit_type) => {
                let circuit = match circuit_type {
                    CircuitType::Chunk => "chunk_prover",
                    CircuitType::Batch => "batch_prover",
                    CircuitType::Bundle => "bundle_prover",
                    CircuitType::Undefined => unreachable!("circuit type is undefined"),
                };
                format!("circuit/scroll-tech/{}:{}/", circuit, THIS_CIRCUIT_VERSION)
            }
            MethodClass::Proof(id) => format!("proof/{}/", id),
        };

        let mut url = self.base_url.join(&method_base)?.join(method)?;

        if let Some(params) = query_params {
            url.query_pairs_mut().extend_pairs(params);
        }

        Ok(url)
    }

    async fn post_with_token<Req, Resp>(
        &self,
        method_class: MethodClass,
        method: &str,
        req: &Req,
    ) -> anyhow::Result<Resp>
    where
        Req: ?Sized + Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        let request_body = serde_json::to_string(req)?;

        self.request_with_token(method_class, method, None, Some(request_body))
            .await
    }

    async fn get_with_token<Resp>(
        &self,
        method_class: MethodClass,
        method: &str,
        query_params: Option<HashMap<String, String>>,
    ) -> anyhow::Result<Resp>
    where
        Resp: serde::de::DeserializeOwned,
    {
        self.request_with_token(method_class, method, query_params, None)
            .await
    }

    async fn request_with_token<Resp>(
        &self,
        method_class: MethodClass,
        method: &str,
        query_params: Option<HashMap<String, String>>,
        request_body: Option<String>,
    ) -> anyhow::Result<Resp>
    where
        Resp: serde::de::DeserializeOwned,
    {
        let url = self.build_url(method_class, method, query_params)?;

        log::info!("[Sindri client]: {:?}", url.as_str());

        let resp_builder = match request_body {
            Some(body) => self
                .client
                .post(url)
                .header(CONTENT_TYPE, "application/json")
                .body(body),
            None => self.client.get(url),
        };

        let resp_builder = resp_builder
            .timeout(self.send_timeout)
            .bearer_auth(&self.api_key);

        let response = resp_builder.send().await?;

        let status = response.status();
        if !(status >= http::status::StatusCode::OK && status <= http::status::StatusCode::ACCEPTED)
        {
            anyhow::bail!("[Sindri client], {method}, status not ok: {}", status)
        }

        let response_body = response.text().await?;

        log::info!("[Sindri client], {method}, received response");
        log::debug!("[Sindri client], {method}, response: {response_body}");

        // Temporary location of the solution to issues surrounding deserializing deeply nested JSON data.
        // Mimics the upstream solution:
        // https://github.com/scroll-tech/zkevm-circuits/blob/e19504c00b5b5b39b3de7bad0c186b4dbcc61eb5/prover/src/io.rs#L22
        let mut deserializer = serde_json::Deserializer::from_str(&response_body);
        deserializer.disable_recursion_limit();
        let deserializer = serde_stacker::Deserializer::new(&mut deserializer);

        serde::Deserialize::deserialize(deserializer).map_err(|e| anyhow::anyhow!(e))
    }
}
