//! # Swarms API Client
//!
//! A production-grade Rust client for the Swarms API with both synchronous and asynchronous interfaces.
//!
//! ## Features
//!
//! - **High Performance**: Built with `reqwest` and `tokio` for maximum throughput
//! - **Connection Pooling**: Automatic HTTP connection reuse and pooling
//! - **Circuit Breaker**: Automatic failure detection and recovery
//! - **Intelligent Caching**: TTL-based in-memory caching with concurrent access
//! - **Rate Limiting**: Configurable concurrent request limits
//! - **Retry Logic**: Exponential backoff with jitter
//! - **Comprehensive Logging**: Structured logging with `tracing`
//! - **Type Safety**: Full compile-time type checking with `serde`
//!
//! ## Example Usage
//!
//! ```rust
//! use swarms_client::SwarmsClient;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Initialize the client with API key from environment
//!     let client = SwarmsClient::builder()
//!         .unwrap()
//!         .from_env()?  // Loads API key from SWARMS_API_KEY environment variable or .env file
//!         .timeout(std::time::Duration::from_secs(60))
//!         .max_retries(3)
//!         .build()?;
//!
//!     // Make a swarm completion request
//!     let response = client.swarm()
//!         .create()
//!         .name("My Swarm")
//!         .swarm_type("auto")
//!         .task("Analyze the pros and cons of quantum computing")
//!         .agent(|agent| {
//!             agent
//!                 .name("Researcher")
//!                 .description("Conducts in-depth research")
//!                 .model("gpt-4o")
//!         })
//!         .agent(|agent| {
//!             agent
//!                 .name("Critic")
//!                 .description("Evaluates arguments for flaws")
//!                 .model("gpt-4o-mini")
//!         })
//!         .send()
//!         .await?;
//!
//!     println!("Swarm output: {}", response.output);
//!     Ok(())
//! }
//! ```

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use dashmap::DashMap;
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{RwLock, Semaphore},
    time::{sleep, timeout},
};
use tracing::{debug, error, instrument, warn};
use url::Url;

// ================================================================================================
// ERROR TYPES
// ================================================================================================

/// Main error type for all Swarms API operations
#[derive(Error, Debug)]
pub enum SwarmsError {
    #[error("Authentication error: {message}")]
    Authentication {
        message: String,
        status: Option<u16>,
        request_id: Option<String>,
    },

    #[error("Rate limit exceeded: {message}")]
    RateLimit {
        message: String,
        status: Option<u16>,
        request_id: Option<String>,
    },

    #[error("Invalid request: {message}")]
    InvalidRequest {
        message: String,
        status: Option<u16>,
        request_id: Option<String>,
    },

    #[error("Insufficient credits: {message}")]
    InsufficientCredits {
        message: String,
        status: Option<u16>,
        request_id: Option<String>,
    },

    #[error("API error: {message}")]
    Api {
        message: String,
        status: Option<u16>,
        request_id: Option<String>,
    },

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Timeout error: {message}")]
    Timeout { message: String },

    #[error("Circuit breaker open")]
    CircuitBreakerOpen,

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("URL parse error: {0}")]
    UrlParse(#[from] url::ParseError),

    #[error("Invalid configuration: {message}")]
    InvalidConfig { message: String },
}

pub type Result<T> = std::result::Result<T, SwarmsError>;

// ================================================================================================
// REQUEST/RESPONSE MODELS
// ================================================================================================

/// Supported swarm types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum SwarmType {
    AgentRearrange,
    MixtureOfAgents,
    SpreadSheetSwarm,
    SequentialWorkflow,
    ConcurrentWorkflow,
    GroupChat,
    MultiAgentRouter,
    AutoSwarmBuilder,
    HiearchicalSwarm,
    #[serde(rename = "auto")]
    Auto,
    MajorityVoting,
    #[serde(rename = "MALT")]
    Malt,
    DeepResearchSwarm,
}

/// Agent specification for creating agents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub agent_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default = "default_model")]
    pub model_name: String,
    #[serde(default)]
    pub auto_generate_prompt: bool,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default = "default_max_loops")]
    pub max_loops: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_dictionary: Option<Vec<HashMap<String, serde_json::Value>>>,
}

fn default_model() -> String {
    "gpt-4o-mini".to_string()
}

fn default_max_tokens() -> u32 {
    8192
}

fn default_temperature() -> f32 {
    0.5
}

fn default_max_loops() -> u32 {
    1
}

/// Agent completion request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCompletion {
    pub agent_config: AgentSpec,
    pub task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<HashMap<String, serde_json::Value>>,
}

/// Schedule specification for swarm execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSpec {
    pub scheduled_time: String, // ISO formatted datetime
    #[serde(default = "default_timezone")]
    pub timezone: String,
}

fn default_timezone() -> String {
    "UTC".to_string()
}

/// Swarm specification for creating swarms
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentSpec>>,
    #[serde(default = "default_max_loops")]
    pub max_loops: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swarm_type: Option<SwarmType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rearrange_flow: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img: Option<String>,
    #[serde(default = "default_return_history")]
    pub return_history: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<HashMap<String, serde_json::Value>>>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default = "default_service_tier")]
    pub service_tier: String,
}

fn default_return_history() -> bool {
    true
}

fn default_service_tier() -> String {
    "standard".to_string()
}

/// Token usage information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Response from an agent completion request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCompletionResponse {
    pub id: String,
    pub success: bool,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub temperature: f32,
    pub outputs: serde_json::Value,
    pub usage: Usage,
    pub timestamp: String,
}

/// Response from a swarm completion request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCompletionResponse {
    pub job_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swarm_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swarm_type: Option<SwarmType>,
    pub output: serde_json::Value,
    pub number_of_agents: u32,
    pub service_tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<HashMap<String, serde_json::Value>>>,
}

/// API request log entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub api_key: String,
    pub data: HashMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Response from a logs request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsResponse {
    pub status: String,
    pub count: u32,
    pub logs: Vec<LogEntry>,
    pub timestamp: String,
}

/// Response from a swarm types request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmTypesResponse {
    pub success: bool,
    pub swarm_types: Vec<String>,
}

/// Response from a models request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub success: bool,
    pub models: Vec<String>,
}

/// Generic error response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub detail: String,
}

// ================================================================================================
// CACHING
// ================================================================================================

/// Cache entry with TTL support
#[derive(Debug, Clone)]
struct CacheEntry<T> {
    value: T,
    expires_at: Instant,
}

/// High-performance concurrent cache with TTL support
#[derive(Debug)]
pub struct Cache<T> {
    entries: DashMap<String, CacheEntry<T>>,
    default_ttl: Duration,
}

impl<T: Clone> Cache<T> {
    /// Create a new cache with default TTL
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            default_ttl,
        }
    }

    /// Get a value from the cache
    pub fn get(&self, key: &str) -> Option<T> {
        if let Some(entry) = self.entries.get(key) {
            if entry.expires_at > Instant::now() {
                return Some(entry.value.clone());
            } else {
                // Entry expired, remove it
                drop(entry);
                self.entries.remove(key);
            }
        }
        None
    }

    /// Set a value in the cache with default TTL
    pub fn set(&self, key: String, value: T) {
        self.set_with_ttl(key, value, self.default_ttl);
    }

    /// Set a value in the cache with custom TTL
    pub fn set_with_ttl(&self, key: String, value: T, ttl: Duration) {
        let entry = CacheEntry {
            value,
            expires_at: Instant::now() + ttl,
        };
        self.entries.insert(key, entry);
    }

    /// Clear all entries from the cache
    pub fn clear(&self) {
        self.entries.clear();
    }

    /// Remove expired entries
    pub fn cleanup(&self) {
        let now = Instant::now();
        self.entries.retain(|_, entry| entry.expires_at > now);
    }

    /// Get cache statistics
    pub fn stats(&self) -> (usize, usize) {
        let total = self.entries.len();
        let now = Instant::now();
        let valid = self
            .entries
            .iter()
            .filter(|entry| entry.expires_at > now)
            .count();
        (valid, total)
    }
}

// ================================================================================================
// CIRCUIT BREAKER
// ================================================================================================

/// Circuit breaker states
#[derive(Debug, Clone, Copy, PartialEq)]
enum CircuitBreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// Circuit breaker for API resilience
#[derive(Debug)]
pub struct CircuitBreaker {
    state: RwLock<CircuitBreakerState>,
    failure_count: AtomicUsize,
    last_failure_time: AtomicU64,
    success_count: AtomicUsize,
    failure_threshold: usize,
    recovery_timeout: Duration,
    half_open_max_calls: usize,
}

impl CircuitBreaker {
    /// Create a new circuit breaker
    pub fn new(failure_threshold: usize, recovery_timeout: Duration) -> Self {
        Self {
            state: RwLock::new(CircuitBreakerState::Closed),
            failure_count: AtomicUsize::new(0),
            last_failure_time: AtomicU64::new(0),
            success_count: AtomicUsize::new(0),
            failure_threshold,
            recovery_timeout,
            half_open_max_calls: 3,
        }
    }

    /// Check if a call can proceed
    pub async fn can_proceed(&self) -> Result<()> {
        let state = *self.state.read().await;
        match state {
            CircuitBreakerState::Closed => Ok(()),
            CircuitBreakerState::Open => {
                let last_failure = self.last_failure_time.load(Ordering::Relaxed);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                if now - last_failure > self.recovery_timeout.as_secs() {
                    // Transition to half-open
                    let mut state_guard = self.state.write().await;
                    *state_guard = CircuitBreakerState::HalfOpen;
                    self.success_count.store(0, Ordering::Relaxed);
                    debug!("Circuit breaker transitioning to half-open");
                    Ok(())
                } else {
                    Err(SwarmsError::CircuitBreakerOpen)
                }
            },
            CircuitBreakerState::HalfOpen => {
                if self.success_count.load(Ordering::Relaxed) < self.half_open_max_calls {
                    Ok(())
                } else {
                    // Transition back to closed
                    let mut state_guard = self.state.write().await;
                    *state_guard = CircuitBreakerState::Closed;
                    self.failure_count.store(0, Ordering::Relaxed);
                    debug!("Circuit breaker transitioning to closed");
                    Ok(())
                }
            },
        }
    }

    /// Record a successful call
    pub async fn record_success(&self) {
        let state = *self.state.read().await;
        match state {
            CircuitBreakerState::HalfOpen => {
                self.success_count.fetch_add(1, Ordering::Relaxed);
            },
            _ => {
                self.failure_count.store(0, Ordering::Relaxed);
            },
        }
    }

    /// Record a failed call
    pub async fn record_failure(&self) {
        let failures = self.failure_count.fetch_add(1, Ordering::Relaxed) + 1;
        self.last_failure_time.store(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            Ordering::Relaxed,
        );

        if failures >= self.failure_threshold {
            let mut state_guard = self.state.write().await;
            if *state_guard == CircuitBreakerState::Closed {
                *state_guard = CircuitBreakerState::Open;
                warn!("Circuit breaker opened after {} failures", failures);
            }
        }
    }

    /// Get current state for monitoring
    pub async fn state(&self) -> CircuitBreakerState {
        *self.state.read().await
    }
}

// ================================================================================================
// MAIN CLIENT
// ================================================================================================

/// Configuration for the Swarms API client
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub api_key: String,
    pub base_url: Url,
    pub timeout: Duration,
    pub max_retries: usize,
    pub retry_delay: Duration,
    pub max_concurrent_requests: usize,
    pub circuit_breaker_threshold: usize,
    pub circuit_breaker_timeout: Duration,
    pub enable_cache: bool,
    pub cache_ttl: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://swarms-api-285321057562.us-east1.run.app/"
                .parse()
                .unwrap(),
            timeout: Duration::from_secs(60),
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
            max_concurrent_requests: 100,
            circuit_breaker_threshold: 5,
            circuit_breaker_timeout: Duration::from_secs(60),
            enable_cache: true,
            cache_ttl: Duration::from_secs(300),
        }
    }
}

/// Builder for creating a Swarms API client
#[derive(Debug, Default)]
pub struct ClientBuilder {
    config: ClientConfig,
}

impl ClientBuilder {
    /// Create a new client builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Load API key from environment variables and .env file
    pub fn from_env() -> Result<Self> {
        // Load .env file if it exists
        dotenv::dotenv().ok();

        // Try to get API key from environment
        let api_key = std::env::var("SWARMS_API_KEY").map_err(|_| SwarmsError::InvalidConfig {
            message: "SWARMS_API_KEY not found in environment or .env file".to_string(),
        })?;

        Ok(Self::new().api_key(api_key))
    }

    /// Set the API key
    pub fn api_key<S: Into<String>>(mut self, api_key: S) -> Self {
        self.config.api_key = api_key.into();
        self
    }

    /// Set the base URL
    pub fn base_url<S: AsRef<str>>(mut self, base_url: S) -> Result<Self> {
        self.config.base_url = base_url.as_ref().parse()?;
        Ok(self)
    }

    /// Set the request timeout
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    /// Set the maximum number of retries
    pub fn max_retries(mut self, max_retries: usize) -> Self {
        self.config.max_retries = max_retries;
        self
    }

    /// Set the retry delay
    pub fn retry_delay(mut self, retry_delay: Duration) -> Self {
        self.config.retry_delay = retry_delay;
        self
    }

    /// Set the maximum concurrent requests
    pub fn max_concurrent_requests(mut self, max_concurrent_requests: usize) -> Self {
        self.config.max_concurrent_requests = max_concurrent_requests;
        self
    }

    /// Enable or disable caching
    pub fn enable_cache(mut self, enable_cache: bool) -> Self {
        self.config.enable_cache = enable_cache;
        self
    }

    /// Set the cache TTL
    pub fn cache_ttl(mut self, cache_ttl: Duration) -> Self {
        self.config.cache_ttl = cache_ttl;
        self
    }

    /// Build the client
    pub fn build(self) -> Result<SwarmsClient> {
        if self.config.api_key.is_empty() {
            return Err(SwarmsError::InvalidConfig {
                message: "API key is required".to_string(),
            });
        }

        SwarmsClient::with_config(self.config)
    }
}

/// Main Swarms API client
#[derive(Debug)]
pub struct SwarmsClient {
    client: Client,
    config: ClientConfig,
    semaphore: Arc<Semaphore>,
    circuit_breaker: Arc<CircuitBreaker>,
    cache: Option<Arc<Cache<serde_json::Value>>>,
}

impl SwarmsClient {
    /// Create a new client builder
    pub fn builder() -> Result<ClientBuilder> {
        Ok(ClientBuilder::new())
    }

    /// Create a client with custom configuration
    pub fn with_config(config: ClientConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(config.timeout)
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(20)
            .build()?;

        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_requests));
        let circuit_breaker = Arc::new(CircuitBreaker::new(
            config.circuit_breaker_threshold,
            config.circuit_breaker_timeout,
        ));

        let cache = if config.enable_cache {
            Some(Arc::new(Cache::new(config.cache_ttl)))
        } else {
            None
        };

        Ok(Self {
            client,
            config,
            semaphore,
            circuit_breaker,
            cache,
        })
    }

    /// Get agent resource
    pub fn agent(&self) -> AgentResource {
        AgentResource::new(self)
    }

    /// Get swarm resource
    pub fn swarm(&self) -> SwarmResource {
        SwarmResource::new(self)
    }

    /// Get models resource
    pub fn models(&self) -> ModelsResource {
        ModelsResource::new(self)
    }

    /// Get logs resource
    pub fn logs(&self) -> LogsResource {
        LogsResource::new(self)
    }

    /// Clear the cache
    pub fn clear_cache(&self) {
        if let Some(cache) = &self.cache {
            cache.clear();
        }
    }

    /// Get cache statistics
    pub fn cache_stats(&self) -> Option<(usize, usize)> {
        self.cache.as_ref().map(|cache| cache.stats())
    }

    /// Make an HTTP request with retries and circuit breaker
    #[instrument(skip(self, body), fields(method = %method, url = %url))]
    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        url: Url,
        body: Option<&impl Serialize>,
    ) -> Result<T> {
        // Check cache first for GET requests
        let cache_key = if method == Method::GET {
            Some(format!("{}:{}", method, url))
        } else {
            None
        };

        if let (Some(cache), Some(key)) = (&self.cache, &cache_key) {
            if let Some(cached) = cache.get(key) {
                debug!("Cache hit for {}", key);
                return Ok(serde_json::from_value(cached)?);
            }
        }

        // Acquire semaphore permit
        let _permit = self.semaphore.acquire().await.unwrap();

        // Check circuit breaker
        self.circuit_breaker.can_proceed().await?;

        let mut last_error = None;
        for attempt in 0..=self.config.max_retries {
            match self
                .make_request_attempt(method.clone(), url.clone(), body)
                .await
            {
                Ok(response) => {
                    debug!("Request succeeded on attempt {}", attempt + 1);
                    self.circuit_breaker.record_success().await;

                    let parsed: T = serde_json::from_value(response.clone())?;

                    // Cache successful GET responses
                    if let (Some(cache), Some(key)) = (&self.cache, &cache_key) {
                        cache.set(key.clone(), response);
                    }

                    return Ok(parsed);
                },
                Err(e) => {
                    last_error = Some(e);
                    if attempt < self.config.max_retries {
                        let delay = self.config.retry_delay * 2_u32.pow(attempt as u32);
                        warn!(
                            "Request failed on attempt {}, retrying in {:?}",
                            attempt + 1,
                            delay
                        );
                        sleep(delay).await;
                    }
                },
            }
        }

        // Record failure after all retries exhausted
        self.circuit_breaker.record_failure().await;
        Err(last_error.unwrap())
    }

    /// Make a single request attempt
    #[instrument(skip(self, body))]
    async fn make_request_attempt(
        &self,
        method: Method,
        url: Url,
        body: Option<&impl Serialize>,
    ) -> Result<serde_json::Value> {
        let mut request_builder = self.client.request(method, url);

        // Add headers
        request_builder = request_builder
            .header("Content-Type", "application/json")
            .header("x-api-key", &self.config.api_key);

        // Add body if provided
        if let Some(body) = body {
            request_builder = request_builder.json(body);
        }

        let response = timeout(self.config.timeout, request_builder.send())
            .await
            .map_err(|_| SwarmsError::Timeout {
                message: format!("Request timed out after {:?}", self.config.timeout),
            })??;

        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        if !status.is_success() {
            let body: ErrorResponse = response.json().await.unwrap_or_else(|_| ErrorResponse {
                detail: "Unknown error".to_string(),
            });

            return Err(match status.as_u16() {
                401 | 403 => SwarmsError::Authentication {
                    message: body.detail,
                    status: Some(status.as_u16()),
                    request_id,
                },
                429 => SwarmsError::RateLimit {
                    message: body.detail,
                    status: Some(status.as_u16()),
                    request_id,
                },
                400 => SwarmsError::InvalidRequest {
                    message: body.detail,
                    status: Some(status.as_u16()),
                    request_id,
                },
                402 => SwarmsError::InsufficientCredits {
                    message: body.detail,
                    status: Some(status.as_u16()),
                    request_id,
                },
                _ => SwarmsError::Api {
                    message: body.detail,
                    status: Some(status.as_u16()),
                    request_id,
                },
            });
        }

        let response_body: serde_json::Value = response.json().await?;
        debug!(
            "Response: {}",
            serde_json::to_string_pretty(&response_body)?
        );

        Ok(response_body)
    }

    /// Build URL for endpoint
    fn build_url(&self, endpoint: &str) -> Result<Url> {
        Ok(self.config.base_url.join(endpoint)?)
    }
}

// ================================================================================================
// API RESOURCES
// ================================================================================================

/// Agent resource for agent operations
#[derive(Debug, Clone)]
pub struct AgentResource<'a> {
    client: &'a SwarmsClient,
}

impl<'a> AgentResource<'a> {
    fn new(client: &'a SwarmsClient) -> Self {
        Self { client }
    }

    /// Create an agent completion
    #[instrument(skip(self))]
    pub async fn create(&self, request: AgentCompletion) -> Result<AgentCompletionResponse> {
        let url = self.client.build_url("v1/agent/completions")?;
        self.client.request(Method::POST, url, Some(&request)).await
    }

    /// Create multiple agent completions in batch
    #[instrument(skip(self))]
    pub async fn create_batch(
        &self,
        requests: Vec<AgentCompletion>,
    ) -> Result<Vec<AgentCompletionResponse>> {
        let url = self.client.build_url("v1/agent/batch/completions")?;
        self.client
            .request(Method::POST, url, Some(&requests))
            .await
    }

    /// Start building an agent completion request
    pub fn completion(&'a self) -> AgentCompletionBuilder<'a> {
        AgentCompletionBuilder::new(self)
    }
}

/// Builder for agent completions
#[derive(Debug)]
pub struct AgentCompletionBuilder<'a> {
    resource: &'a AgentResource<'a>,
    request: AgentCompletion,
}

impl<'a> AgentCompletionBuilder<'a> {
    fn new(resource: &'a AgentResource<'a>) -> Self {
        Self {
            resource,
            request: AgentCompletion {
                agent_config: AgentSpec {
                    agent_name: String::new(),
                    description: None,
                    system_prompt: None,
                    model_name: default_model(),
                    auto_generate_prompt: false,
                    max_tokens: default_max_tokens(),
                    temperature: default_temperature(),
                    role: None,
                    max_loops: default_max_loops(),
                    tools_dictionary: None,
                },
                task: String::new(),
                history: None,
            },
        }
    }

    /// Set the agent name
    pub fn agent_name<S: Into<String>>(mut self, name: S) -> Self {
        self.request.agent_config.agent_name = name.into();
        self
    }

    /// Set the task
    pub fn task<S: Into<String>>(mut self, task: S) -> Self {
        self.request.task = task.into();
        self
    }

    /// Set the model
    pub fn model<S: Into<String>>(mut self, model: S) -> Self {
        self.request.agent_config.model_name = model.into();
        self
    }

    /// Set the description
    pub fn description<S: Into<String>>(mut self, description: S) -> Self {
        self.request.agent_config.description = Some(description.into());
        self
    }

    /// Set the system prompt
    pub fn system_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.request.agent_config.system_prompt = Some(prompt.into());
        self
    }

    /// Set the temperature
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.request.agent_config.temperature = temperature.clamp(0.0, 1.0);
        self
    }

    /// Set max tokens
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.request.agent_config.max_tokens = max_tokens;
        self
    }

    /// Set max loops
    pub fn max_loops(mut self, max_loops: u32) -> Self {
        self.request.agent_config.max_loops = max_loops;
        self
    }

    /// Send the request
    pub async fn send(self) -> Result<AgentCompletionResponse> {
        self.resource.create(self.request).await
    }
}

/// Swarm resource for swarm operations
#[derive(Debug, Clone)]
pub struct SwarmResource<'a> {
    client: &'a SwarmsClient,
}

impl<'a> SwarmResource<'a> {
    fn new(client: &'a SwarmsClient) -> Self {
        Self { client }
    }

    /// Create a swarm completion
    #[instrument(skip(self))]
    pub async fn create(&self, request: SwarmSpec) -> Result<SwarmCompletionResponse> {
        let url = self.client.build_url("v1/swarm/completions")?;
        self.client.request(Method::POST, url, Some(&request)).await
    }

    /// Create multiple swarm completions in batch
    #[instrument(skip(self))]
    pub async fn create_batch(
        &self,
        requests: Vec<SwarmSpec>,
    ) -> Result<Vec<SwarmCompletionResponse>> {
        let url = self.client.build_url("v1/swarm/batch/completions")?;
        self.client
            .request(Method::POST, url, Some(&requests))
            .await
    }

    /// List available swarm types
    #[instrument(skip(self))]
    pub async fn list_types(&self) -> Result<SwarmTypesResponse> {
        let url = self.client.build_url("v1/swarms/available")?;
        self.client.request(Method::GET, url, None::<&()>).await
    }

    /// Start building a swarm completion request
    pub fn completion(&self) -> SwarmCompletionBuilder<'_> {
        SwarmCompletionBuilder::new(self)
    }
}

/// Builder for swarm completions
#[derive(Debug)]
pub struct SwarmCompletionBuilder<'a> {
    resource: &'a SwarmResource<'a>,
    request: SwarmSpec,
}

impl<'a> SwarmCompletionBuilder<'a> {
    fn new(resource: &'a SwarmResource<'a>) -> Self {
        Self {
            resource,
            request: SwarmSpec {
                name: None,
                description: None,
                agents: None,
                max_loops: default_max_loops(),
                swarm_type: None,
                rearrange_flow: None,
                task: None,
                img: None,
                return_history: default_return_history(),
                rules: None,
                schedule: None,
                tasks: None,
                messages: None,
                stream: false,
                service_tier: default_service_tier(),
            },
        }
    }

    /// Set the swarm name
    pub fn name<S: Into<String>>(mut self, name: S) -> Self {
        self.request.name = Some(name.into());
        self
    }

    /// Set the description
    pub fn description<S: Into<String>>(mut self, description: S) -> Self {
        self.request.description = Some(description.into());
        self
    }

    /// Set the swarm type
    pub fn swarm_type(mut self, swarm_type: SwarmType) -> Self {
        self.request.swarm_type = Some(swarm_type);
        self
    }

    /// Set the task
    pub fn task<S: Into<String>>(mut self, task: S) -> Self {
        self.request.task = Some(task.into());
        self
    }

    /// Add an agent using a builder function
    pub fn agent<F>(mut self, builder: F) -> Self
    where
        F: FnOnce(AgentSpecBuilder) -> AgentSpecBuilder,
    {
        let agent = builder(AgentSpecBuilder::new()).build();
        if self.request.agents.is_none() {
            self.request.agents = Some(Vec::new());
        }
        self.request.agents.as_mut().unwrap().push(agent);
        self
    }

    /// Set max loops
    pub fn max_loops(mut self, max_loops: u32) -> Self {
        self.request.max_loops = max_loops;
        self
    }

    /// Set service tier
    pub fn service_tier<S: Into<String>>(mut self, tier: S) -> Self {
        self.request.service_tier = tier.into();
        self
    }

    /// Send the request
    pub async fn send(self) -> Result<SwarmCompletionResponse> {
        // Validate that we have either task, tasks, or messages
        if self.request.task.is_none()
            && self.request.tasks.is_none()
            && self.request.messages.is_none()
        {
            return Err(SwarmsError::InvalidRequest {
                message: "Either task, tasks, or messages must be provided".to_string(),
                status: None,
                request_id: None,
            });
        }

        self.resource.create(self.request).await
    }
}

/// Builder for agent specifications
#[derive(Debug)]
pub struct AgentSpecBuilder {
    spec: AgentSpec,
}

impl AgentSpecBuilder {
    fn new() -> Self {
        Self {
            spec: AgentSpec {
                agent_name: String::new(),
                description: None,
                system_prompt: None,
                model_name: default_model(),
                auto_generate_prompt: false,
                max_tokens: default_max_tokens(),
                temperature: default_temperature(),
                role: None,
                max_loops: default_max_loops(),
                tools_dictionary: None,
            },
        }
    }

    /// Set the agent name
    pub fn name<S: Into<String>>(mut self, name: S) -> Self {
        self.spec.agent_name = name.into();
        self
    }

    /// Set the description
    pub fn description<S: Into<String>>(mut self, description: S) -> Self {
        self.spec.description = Some(description.into());
        self
    }

    /// Set the model
    pub fn model<S: Into<String>>(mut self, model: S) -> Self {
        self.spec.model_name = model.into();
        self
    }

    /// Set the system prompt
    pub fn system_prompt<S: Into<String>>(mut self, prompt: S) -> Self {
        self.spec.system_prompt = Some(prompt.into());
        self
    }

    /// Set the temperature
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.spec.temperature = temperature.clamp(0.0, 1.0);
        self
    }

    /// Set max tokens
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.spec.max_tokens = max_tokens;
        self
    }

    /// Build the agent spec
    pub fn build(self) -> AgentSpec {
        self.spec
    }
}

/// Models resource for model operations
#[derive(Debug, Clone)]
pub struct ModelsResource<'a> {
    client: &'a SwarmsClient,
}

impl<'a> ModelsResource<'a> {
    fn new(client: &'a SwarmsClient) -> Self {
        Self { client }
    }

    /// List available models
    #[instrument(skip(self))]
    pub async fn list(&self) -> Result<ModelsResponse> {
        let url = self.client.build_url("v1/models/available")?;
        self.client.request(Method::GET, url, None::<&()>).await
    }
}

/// Logs resource for log operations
#[derive(Debug, Clone)]
pub struct LogsResource<'a> {
    client: &'a SwarmsClient,
}

impl<'a> LogsResource<'a> {
    fn new(client: &'a SwarmsClient) -> Self {
        Self { client }
    }

    /// List API request logs
    #[instrument(skip(self))]
    pub async fn list(&self) -> Result<LogsResponse> {
        let url = self.client.build_url("v1/swarm/logs")?;
        self.client.request(Method::GET, url, None::<&()>).await
    }
}

// ================================================================================================
// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_cache() {
        let cache = Cache::new(Duration::from_millis(100));

        // Test set and get
        cache.set(
            "key1".to_string(),
            serde_json::Value::String("value1".to_string()),
        );
        assert_eq!(
            cache.get("key1").unwrap(),
            serde_json::Value::String("value1".to_string())
        );

        // Test expiration
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(cache.get("key1").is_none());
    }

    #[tokio::test]
    async fn test_circuit_breaker() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(100));

        // Should be closed initially
        assert_eq!(cb.state().await, CircuitBreakerState::Closed);
        assert!(cb.can_proceed().await.is_ok());

        // Record failures
        cb.record_failure().await;
        cb.record_failure().await;

        // Should be open now
        assert_eq!(cb.state().await, CircuitBreakerState::Open);
        assert!(cb.can_proceed().await.is_err());

        // Wait for recovery timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should allow calls now (half-open)
        assert!(cb.can_proceed().await.is_ok());
    }

    #[test]
    fn test_client_builder() {
        let client = SwarmsClient::builder()
            .unwrap()
            .api_key("test-key")
            .timeout(Duration::from_secs(30))
            .max_retries(5)
            .build();

        assert!(client.is_ok());
    }

    #[test]
    fn test_agent_spec_builder() {
        let spec = AgentSpecBuilder::new()
            .name("Test Agent")
            .description("A test agent")
            .model("gpt-4o")
            .temperature(0.7)
            .max_tokens(1000)
            .build();

        assert_eq!(spec.agent_name, "Test Agent");
        assert_eq!(spec.description, Some("A test agent".to_string()));
        assert_eq!(spec.model_name, "gpt-4o");
        assert_eq!(spec.temperature, 0.7);
        assert_eq!(spec.max_tokens, 1000);
    }
}
