use thiserror::Error;

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("model error: {0}")]
    Model(#[from] ModelError),
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),
    #[error("guide error: {0}")]
    Guide(#[from] GuideError),
    #[error("sensor error: {0}")]
    Sensor(#[from] SensorError),
    #[error("compaction error: {0}")]
    Compact(#[from] CompactError),
    #[error("skill error: {0}")]
    Skill(#[from] SkillError),
    #[error("budget exhausted after {iters} iterations")]
    BudgetExhausted { iters: u32 },
    #[error("policy violation: {0}")]
    Policy(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("invalid response: {0}")]
    Invalid(String),
    #[error("rate limited (retry after {retry_after_ms}ms)")]
    RateLimited { retry_after_ms: u64 },
    #[error("context overflow: needed {needed} tokens, window is {window}")]
    ContextOverflow { needed: u32, window: u32 },
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool `{name}` not found")]
    NotFound { name: String },
    #[error("invalid args for `{name}`: {reason}")]
    InvalidArgs { name: String, reason: String },
    #[error("execution failed: {0}")]
    Exec(String),
    #[error("permission denied: {0}")]
    Permission(String),
}

#[derive(Debug, Error)]
pub enum GuideError {
    #[error("guide `{id}` failed: {reason}")]
    Failed { id: String, reason: String },
}

#[derive(Debug, Error)]
pub enum SensorError {
    #[error("sensor `{id}` failed: {reason}")]
    Failed { id: String, reason: String },
}

#[derive(Debug, Error)]
pub enum CompactError {
    #[error("compaction stage {stage:?} failed: {reason}")]
    Failed { stage: String, reason: String },
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("io error: {0}")]
    Io(String),
    #[error("invalid SKILL.md at {path}: {reason}")]
    Invalid { path: String, reason: String },
    #[error("name regex violation: `{name}` — {reason}")]
    NameRegex { name: String, reason: String },
    #[error("description too long: {len} > 1024")]
    DescriptionTooLong { len: usize },
    #[error("compatibility too long: {len} > 500")]
    CompatibilityTooLong { len: usize },
    #[error("name `{name}` does not match parent directory `{dir}`")]
    NameDirMismatch { name: String, dir: String },
    #[error("missing required field `{field}`")]
    MissingField { field: String },
    #[error("skill `{name}` already registered")]
    Duplicate { name: String },
}

pub type Result<T, E = HarnessError> = std::result::Result<T, E>;
