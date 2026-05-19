use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Success,
    Error,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VerifyResponse {
    pub status: JobStatus,
    pub request_id: String,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub status: Status,
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobResponse {
    pub status: JobStatus,
    pub respose: Option<JobVerificationResponse>,
}

impl std::fmt::Display for JobResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(response) = &self.respose {
            writeln!(f, "{response}")?;
        } else {
            writeln!(f, "Status: {:?}", self.status)?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum JobStatus {
    #[serde(rename = "in_progress")]
    InProgress,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "unknown")]
    Unknown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobVerificationResponse {
    pub status: JobStatus,
    pub message: String,
    pub on_chain_hash: String,
    pub executable_hash: String,
    pub repo_url: String,
}

impl std::fmt::Display for JobVerificationResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Status: {:?}", self.status)?;
        writeln!(f, "Message: {}", self.message)?;
        writeln!(f, "On-chain Hash: {}", self.on_chain_hash)?;
        writeln!(f, "Executable Hash: {}", self.executable_hash)?;
        write!(f, "Repository URL: {}", self.repo_url)
    }
}

/// Response body for `GET /status/:address`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RemoteStatusResponse {
    pub is_verified: bool,
    pub message: String,
    pub on_chain_hash: String,
    pub executable_hash: String,
    pub repo_url: String,
    pub commit: String,
    pub last_verified_at: Option<String>,
    pub is_frozen: bool,
    pub is_closed: bool,
}

impl std::fmt::Display for RemoteStatusResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "Verified: {}",
            if self.is_verified { "✅" } else { "❌" }
        )?;
        writeln!(f, "Message: {}", self.message)?;
        writeln!(f, "On-chain Hash: {}", self.on_chain_hash)?;
        writeln!(f, "Executable Hash: {}", self.executable_hash)?;
        writeln!(f, "Repository URL: {}", self.repo_url)?;
        writeln!(f, "Commit: {}", self.commit)?;
        if let Some(ts) = &self.last_verified_at {
            writeln!(f, "Last Verified: {ts}")?;
        }
        writeln!(f, "Frozen: {}", self.is_frozen)?;
        write!(f, "Closed: {}", self.is_closed)
    }
}
