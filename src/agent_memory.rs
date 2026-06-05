use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use serde::{Serialize, Deserialize};
use tracing::info;
use thiserror::Error;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Error, Debug)]
pub enum MemoryError {
    #[error("Failed to generate embedding: {0}")]
    EmbeddingFailed(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("HTTP request failed: {0}")]
    HttpError(String),
    #[error("Internal memory error: {0}")]
    Internal(String),
}

/// Helper to get current epoch time in milliseconds.
fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Helper to compute cosine similarity between two vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot_product = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for i in 0..a.len() {
        dot_product += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot_product / (norm_a.sqrt() * norm_b.sqrt())
    }
}

// =========================================================================
// Embedding Layer
// =========================================================================

/// Trait defining the behavior of vector embedding generators.
#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Generates embedding for a single text chunk.
    async fn get_embedding(&self, text: &str) -> Result<Vec<f32>, MemoryError>;
    
    /// Generates embeddings for multiple text chunks in a batch.
    async fn get_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError>;

    /// Gets the dimensionality of the generated vectors.
    fn dimension(&self) -> usize;
}

/// A deterministic local embedding provider for offline/local simulation and unit tests.
/// Uses a lightweight hashing/mapping function to produce consistent mock embedding vectors.
pub struct LocalEmbeddingProvider {
    dimension: usize,
}

impl LocalEmbeddingProvider {
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }
}

impl Default for LocalEmbeddingProvider {
    fn default() -> Self {
        Self::new(1536) // Default to standard OpenAI Ada/text-embedding-3-small dimension
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for LocalEmbeddingProvider {
    async fn get_embedding(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let mut vector = vec![0.0f32; self.dimension];
        if text.is_empty() {
            return Ok(vector);
        }
        
        // Simple hash-based vector generation: stable, deterministic, non-random
        for (i, char_val) in text.chars().enumerate() {
            let idx = (char_val as usize + i * 31) % self.dimension;
            vector[idx] += 1.0;
        }
        
        // Normalize the vector
        let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut vector {
                *x /= norm;
            }
        }
        
        Ok(vector)
    }

    async fn get_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.get_embedding(text).await?);
        }
        Ok(results)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}

/// Real embedding provider targeting the Gemini / OpenAI-compatible API endpoints.
pub struct RemoteEmbeddingProvider {
    api_key: String,
    model_name: String,
    endpoint: String,
    dimension: usize,
    client: reqwest::Client,
}

impl RemoteEmbeddingProvider {
    pub fn new(api_key: String, model_name: String, endpoint: String, dimension: usize) -> Self {
        Self {
            api_key,
            model_name,
            endpoint,
            dimension,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for RemoteEmbeddingProvider {
    async fn get_embedding(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let embeddings = self.get_embeddings(&[text.to_string()]).await?;
        embeddings.into_iter().next().ok_or_else(|| {
            MemoryError::EmbeddingFailed("API returned an empty embeddings array".to_string())
        })
    }

    async fn get_embeddings(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        // Send request to API endpoint (handles both Gemini and OpenAI compatible payloads)
        let body = serde_json::json!({
            "model": self.model_name,
            "input": texts
        });

        let response = self.client.post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| MemoryError::HttpError(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let err_text = response.text().await.unwrap_or_default();
            return Err(MemoryError::EmbeddingFailed(format!(
                "API returned error status: {}. Body: {}", status, err_text
            )));
        }

        let parsed: serde_json::Value = response.json().await
            .map_err(|e| MemoryError::HttpError(e.to_string()))?;

        // Extract embeddings from response standard structure
        let mut results = Vec::new();
        if let Some(data) = parsed.get("data").and_then(|d| d.as_array()) {
            for item in data {
                if let Some(emb_val) = item.get("embedding").and_then(|e| e.as_array()) {
                    let vec: Result<Vec<f32>, _> = emb_val.iter()
                        .map(|v| v.as_f64().map(|f| f as f32).ok_or("Invalid float in embedding"))
                        .collect();
                    match vec {
                        Ok(v) => results.push(v),
                        Err(e) => return Err(MemoryError::EmbeddingFailed(e.to_string())),
                    }
                }
            }
        }

        if results.is_empty() {
            return Err(MemoryError::EmbeddingFailed("No embedding data extracted from response".to_string()));
        }

        Ok(results)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}

// =========================================================================
// Short-Term Memory (Conversation Working Buffer)
// =========================================================================

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub timestamp_ms: u64,
    pub metadata: HashMap<String, String>,
}

pub struct ShortTermMemory {
    messages: Vec<Message>,
    max_messages: usize,
}

impl ShortTermMemory {
    pub fn new(max_messages: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_messages,
        }
    }

    pub fn add_message(&mut self, role: MessageRole, content: String, metadata: HashMap<String, String>) {
        let msg = Message {
            role,
            content,
            timestamp_ms: current_time_ms(),
            metadata,
        };
        self.messages.push(msg);
    }

    pub fn get_messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn needs_consolidation(&self) -> bool {
        self.messages.len() >= self.max_messages
    }

    /// Truncates the message list, keeping the most recent `keep` messages.
    /// Returns the truncated older messages.
    pub fn truncate_older(&mut self, keep: usize) -> Vec<Message> {
        if self.messages.len() <= keep {
            return Vec::new();
        }
        let split_idx = self.messages.len() - keep;
        let older = self.messages.drain(0..split_idx).collect();
        older
    }
}

// =========================================================================
// Long-Term Memory (Persistent Semantic Vector DB)
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub embedding: Vec<f32>,
    pub created_at_ms: u64,
    pub last_accessed_at_ms: u64,
    pub access_count: usize,
    pub tags: Vec<String>,
    pub metadata: HashMap<String, String>,
}

pub struct LongTermMemory {
    entries: Vec<MemoryEntry>,
}

impl LongTermMemory {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn insert(&mut self, entry: MemoryEntry) {
        self.entries.push(entry);
    }

    /// Queries the vector space using Cosine Similarity.
    /// Returns the top-matching memory entries along with their similarity score (range -1.0 to 1.0).
    pub fn query(&self, query_embedding: &[f32], limit: usize, min_score: f32) -> Vec<(MemoryEntry, f32)> {
        let mut results = Vec::new();

        for entry in &self.entries {
            let score = cosine_similarity(query_embedding, &entry.embedding);
            if score >= min_score {
                results.push((entry.clone(), score));
            }
        }

        // Sort by score in descending order
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        
        results.truncate(limit);
        results
    }

    /// Updates access statistics for a specific memory entry.
    pub fn touch_entry(&mut self, id: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.last_accessed_at_ms = current_time_ms();
            entry.access_count += 1;
        }
    }

    pub fn size(&self) -> usize {
        self.entries.len()
    }
}

// =========================================================================
// Episodic Memory (Action traces / Experiences)
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeStep {
    pub action: String,
    pub observation: String,
    pub timestamp_ms: u64,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: String,
    pub task: String,
    pub steps: Vec<EpisodeStep>,
    pub success: bool,
    pub reward: Option<f32>,
    pub timestamp_ms: u64,
}

pub struct EpisodicMemory {
    episodes: Vec<Episode>,
}

impl EpisodicMemory {
    pub fn new() -> Self {
        Self {
            episodes: Vec::new(),
        }
    }

    pub fn add_episode(&mut self, episode: Episode) {
        self.episodes.push(episode);
    }

    pub fn get_episodes(&self) -> &[Episode] {
        &self.episodes
    }

    pub fn get_last_n_episodes(&self, n: usize) -> &[Episode] {
        let len = self.episodes.len();
        if len <= n {
            &self.episodes
        } else {
            &self.episodes[len - n..]
        }
    }
}

// =========================================================================
// Agent Memory Coordinator (Orchestrator)
// =========================================================================

/// The orchestrator wrapping short-term, long-term, and episodic memory layers
/// in thread-safe locks.
pub struct AgentMemorySystem {
    short_term: Arc<RwLock<ShortTermMemory>>,
    long_term: Arc<RwLock<LongTermMemory>>,
    episodic: Arc<RwLock<EpisodicMemory>>,
    embedding_provider: Arc<dyn EmbeddingProvider>,
}

impl AgentMemorySystem {
    pub fn new(max_short_term_messages: usize, embedding_provider: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            short_term: Arc::new(RwLock::new(ShortTermMemory::new(max_short_term_messages))),
            long_term: Arc::new(RwLock::new(LongTermMemory::new())),
            episodic: Arc::new(RwLock::new(EpisodicMemory::new())),
            embedding_provider,
        }
    }

    /// Access the short-term working memory.
    pub fn short_term(&self) -> &Arc<RwLock<ShortTermMemory>> {
        &self.short_term
    }

    /// Access the long-term semantic memory.
    pub fn long_term(&self) -> &Arc<RwLock<LongTermMemory>> {
        &self.long_term
    }

    /// Access the episodic trace memory.
    pub fn episodic(&self) -> &Arc<RwLock<EpisodicMemory>> {
        &self.episodic
    }

    /// Adds a message to the agent's active short-term memory session.
    pub async fn add_message(&self, role: MessageRole, content: String, metadata: HashMap<String, String>) {
        let mut st = self.short_term.write().await;
        st.add_message(role, content, metadata);
    }

    /// Commits a discrete semantic fact directly to long-term memory.
    pub async fn remember_fact(&self, fact: &str, tags: Vec<String>, metadata: HashMap<String, String>) -> Result<String, MemoryError> {
        let embedding = self.embedding_provider.get_embedding(fact).await?;
        let id = uuid_v4_placeholder();
        let now = current_time_ms();
        
        let entry = MemoryEntry {
            id: id.clone(),
            content: fact.to_string(),
            embedding,
            created_at_ms: now,
            last_accessed_at_ms: now,
            access_count: 1,
            tags,
            metadata,
        };

        let mut lt = self.long_term.write().await;
        lt.insert(entry);
        info!("Fact remembered in long-term memory: ID {}", id);
        
        Ok(id)
    }

    /// Queries semantic long-term memory for entries related to `query`.
    pub async fn recall(&self, query: &str, limit: usize, min_score: f32) -> Result<Vec<(MemoryEntry, f32)>, MemoryError> {
        let query_embedding = self.embedding_provider.get_embedding(query).await?;
        
        let mut lt = self.long_term.write().await;
        let results = lt.query(&query_embedding, limit, min_score);
        
        // Touch entries to update their access statistics
        for (entry, _) in &results {
            lt.touch_entry(&entry.id);
        }
        
        Ok(results)
    }

    /// Consolidates old short-term memory into long-term memory.
    /// This happens automatically or can be triggered manually.
    /// It formats older messages into a digest/summary, embeds it, and pushes to long-term memory.
    pub async fn consolidate(&self, keep_recent: usize) -> Result<Option<String>, MemoryError> {
        let mut st = self.short_term.write().await;
        
        if !st.needs_consolidation() {
            return Ok(None);
        }

        let truncated = st.truncate_older(keep_recent);
        if truncated.is_empty() {
            return Ok(None);
        }

        // Generate context digest
        let mut digest = String::new();
        digest.push_str("Summary of past conversation steps:\n");
        for msg in &truncated {
            let role_str = match msg.role {
                MessageRole::System => "System",
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
                MessageRole::Tool => "Tool",
            };
            digest.push_str(&format!("- {}: {}\n", role_str, msg.content));
        }

        // Generate embedding and insert into Long-term Memory
        let embedding = self.embedding_provider.get_embedding(&digest).await?;
        let id = uuid_v4_placeholder();
        let now = current_time_ms();

        let mut lt = self.long_term.write().await;
        lt.insert(MemoryEntry {
            id: id.clone(),
            content: digest,
            embedding,
            created_at_ms: now,
            last_accessed_at_ms: now,
            access_count: 1,
            tags: vec!["consolidation_summary".to_string()],
            metadata: HashMap::from([("source".to_string(), "consolidation".to_string())]),
        });

        info!("Short-term memory consolidated. Created summary long-term entry: {}", id);
        Ok(Some(id))
    }

    /// Records an execution episode (e.g. tool actions and observations) in episodic memory.
    pub async fn record_episode(&self, task: String, steps: Vec<EpisodeStep>, success: bool, reward: Option<f32>) -> String {
        let id = uuid_v4_placeholder();
        let episode = Episode {
            id: id.clone(),
            task,
            steps,
            success,
            reward,
            timestamp_ms: current_time_ms(),
        };

        let mut ep = self.episodic.write().await;
        ep.add_episode(episode);
        info!("Recorded new action episode: {}", id);
        id
    }
}

/// Simple pseudorandom UUID v4 placeholder for clean, dependency-free compilation.
fn uuid_v4_placeholder() -> String {
    let mut bytes = [0u8; 16];
    // Fill with current nanos as simple entropy
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    
    for i in 0..16 {
        bytes[i] = ((nanos >> (i * 8)) & 0xff) as u8;
    }
    
    // Set UUID v4 version and variant bits
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // Version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // Variant 10xx

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);

        let c = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &c) - 0.0).abs() < 1e-6);

        let d = vec![-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &d) + 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_local_embedding_provider() {
        let provider = LocalEmbeddingProvider::new(10);
        let emb1 = provider.get_embedding("hello").await.unwrap();
        let emb2 = provider.get_embedding("hello").await.unwrap();
        let emb3 = provider.get_embedding("world").await.unwrap();

        assert_eq!(emb1.len(), 10);
        // Verify deterministic results
        assert_eq!(emb1, emb2);
        // Verify similarity of identical text is 1.0
        assert!((cosine_similarity(&emb1, &emb2) - 1.0).abs() < 1e-6);
        // Verify different text yields different embedding
        assert_ne!(emb1, emb3);
    }

    #[tokio::test]
    async fn test_agent_memory_system() {
        let provider = Arc::new(LocalEmbeddingProvider::new(16));
        // Trigger consolidation at 3 messages
        let system = AgentMemorySystem::new(3, provider);

        // Add 1st message
        system.add_message(MessageRole::User, "Hello world".to_string(), HashMap::new()).await;
        // Add 2nd message
        system.add_message(MessageRole::Assistant, "Greetings! How can I help you?".to_string(), HashMap::new()).await;

        {
            let st = system.short_term().read().await;
            assert_eq!(st.len(), 2);
            assert!(!st.needs_consolidation());
        }

        // Add 3rd message -> triggers consolidation conditions
        system.add_message(MessageRole::User, "Analyze Solana state".to_string(), HashMap::new()).await;

        {
            let st = system.short_term().read().await;
            assert!(st.needs_consolidation());
        }

        // Consolidate, keeping the 1 most recent message
        let summary_id = system.consolidate(1).await.unwrap();
        assert!(summary_id.is_some());

        // Verify short term memory size is reduced
        {
            let st = system.short_term().read().await;
            assert_eq!(st.len(), 1);
            assert_eq!(st.get_messages()[0].content, "Analyze Solana state");
        }

        // Verify consolidation entry is now in long term memory
        {
            let lt = system.long_term().read().await;
            assert_eq!(lt.size(), 1);
        }

        // Query semantic memory for Solana-related content
        let recall_results = system.recall("Solana", 10, 0.1).await.unwrap();
        assert!(!recall_results.is_empty());
        // It should match our consolidated memory entry
        assert_eq!(recall_results[0].0.id, summary_id.unwrap());
    }

    #[tokio::test]
    async fn test_episodic_memory() {
        let provider = Arc::new(LocalEmbeddingProvider::new(16));
        let system = AgentMemorySystem::new(10, provider);

        let step1 = EpisodeStep {
            action: "Call get_balance for Pubkey1".to_string(),
            observation: "Balance: 15.5 SOL".to_string(),
            timestamp_ms: current_time_ms(),
            metadata: HashMap::new(),
        };

        system.record_episode(
            "Check user account balance and format response".to_string(),
            vec![step1],
            true,
            Some(1.0)
        ).await;

        let ep = system.episodic().read().await;
        assert_eq!(ep.get_episodes().len(), 1);
        assert_eq!(ep.get_episodes()[0].success, true);
        assert_eq!(ep.get_episodes()[0].reward, Some(1.0));
    }
}
