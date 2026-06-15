//! Server-side session management.
//!
//! Each connected client gets a `Session` that owns the capture → encode →
//! transport pipeline for video, audio, and input.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use flux_core::config::FluxConfig;
use flux_core::error::Result;
use flux_core::platform::PlatformInfo;
use flux_core::types::Resolution;

use crate::pipeline::StreamingPipeline;

/// Manages all active streaming sessions.
#[allow(dead_code)]
pub struct SessionManager {
    config: FluxConfig,
    platform: PlatformInfo,
    sessions: Arc<RwLock<HashMap<Uuid, Session>>>,
}

#[allow(dead_code)]
impl SessionManager {
    pub fn new(config: FluxConfig, platform: PlatformInfo) -> Self {
        Self {
            config,
            platform,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create and start a new streaming session.
    pub async fn create_session(&self, params: SessionParams) -> Result<Uuid> {
        let session_id = Uuid::new_v4();

        tracing::info!(
            "Creating session {} for client '{}': {} {}@{}fps",
            session_id,
            params.client_name,
            params.codec,
            params.resolution,
            params.fps,
        );

        let session = Session::new(
            session_id,
            params,
            self.config.clone(),
            self.platform.clone(),
        )
        .await?;

        self.sessions.write().insert(session_id, session);

        let count = self.sessions.read().len();
        tracing::info!("Active sessions: {}", count);

        Ok(session_id)
    }

    /// Stop and remove a session.
    pub async fn destroy_session(&self, session_id: &Uuid) -> Result<()> {
        if let Some(mut session) = self.sessions.write().remove(session_id) {
            tracing::info!("Destroying session {}", session_id);
            session.stop().await?;
        }
        Ok(())
    }

    /// Get the number of active sessions.
    pub fn active_count(&self) -> usize {
        self.sessions.read().len()
    }

    /// Shut down all active sessions.
    pub async fn shutdown_all(&self) {
        let session_ids: Vec<Uuid> = self.sessions.read().keys().copied().collect();
        for id in session_ids {
            let _ = self.destroy_session(&id).await;
        }
    }
}

/// Parameters for creating a new session (from negotiation).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SessionParams {
    pub client_name: String,
    pub codec: flux_core::types::VideoCodec,
    pub resolution: Resolution,
    pub fps: u32,
    pub video_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    pub enable_input: bool,
}

/// A single active streaming session.
#[allow(dead_code)]
struct Session {
    id: Uuid,
    params: SessionParams,
    pipeline: Option<StreamingPipeline>,
}

#[allow(dead_code)]
impl Session {
    async fn new(
        id: Uuid,
        params: SessionParams,
        config: FluxConfig,
        platform: PlatformInfo,
    ) -> Result<Self> {
        let pipeline = StreamingPipeline::new(
            &config,
            &platform,
            &params,
        )?;

        Ok(Self {
            id,
            params,
            pipeline: Some(pipeline),
        })
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(pipeline) = self.pipeline.take() {
            tracing::info!("Stopping pipeline for session {}", self.id);
            pipeline.stop()?;
        }
        Ok(())
    }
}
