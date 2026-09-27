//! Persistent Embewi device lifecycle.
//!
//! This is application policy. FiBeWI never interprets these states.

use config_space_manager::{Budget, ConfigSpace};
use espbewi_config_space::NvsConfigBackend;

const MAGIC: &[u8; 4] = b"LFC2";
const ENCODED_LEN: usize = 5;

pub const CONFIG_BUDGET: Budget = Budget::new(ENCODED_LEN);
pub type LifecycleConfigSpace = ConfigSpace<NvsConfigBackend>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LifecycleState {
    Factory = 0,
    Provisioning = 1,
    ReadyForAgent = 2,
    Production = 3,
}

impl LifecycleState {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Factory),
            1 => Some(Self::Provisioning),
            2 => Some(Self::ReadyForAgent),
            3 => Some(Self::Production),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Factory => "factory",
            Self::Provisioning => "provisioning",
            Self::ReadyForAgent => "ready_for_agent",
            Self::Production => "production",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleError {
    Persistence,
    Corrupt,
    InvalidTransition {
        from: LifecycleState,
        to: LifecycleState,
    },
}

fn decode(raw: &[u8]) -> Option<LifecycleState> {
    if raw.len() != ENCODED_LEN || &raw[..4] != MAGIC {
        return None;
    }
    LifecycleState::from_u8(raw[4])
}

fn encode(state: LifecycleState) -> [u8; ENCODED_LEN] {
    let mut encoded = [0u8; ENCODED_LEN];
    encoded[..4].copy_from_slice(MAGIC);
    encoded[4] = state as u8;
    encoded
}

/// Loads the lifecycle. Absence means a genuinely fresh device (Factory).
/// A stored-but-unreadable object is never treated as fresh.
pub async fn state(space: &LifecycleConfigSpace) -> Result<LifecycleState, LifecycleError> {
    match space.load().await {
        Ok(None) => Ok(LifecycleState::Factory),
        Ok(Some(snapshot)) => decode(&snapshot.data).ok_or(LifecycleError::Corrupt),
        Err(_) => Err(LifecycleError::Persistence),
    }
}

async fn commit(
    space: &LifecycleConfigSpace,
    state: LifecycleState,
) -> Result<(), LifecycleError> {
    space
        .commit(&encode(state))
        .await
        .map(|_| ())
        .map_err(|_| LifecycleError::Persistence)
}

async fn transition(
    space: &LifecycleConfigSpace,
    expected: LifecycleState,
    next: LifecycleState,
) -> Result<(), LifecycleError> {
    let current = state(space).await?;
    if current == next {
        return Ok(());
    }
    if current != expected {
        return Err(LifecycleError::InvalidTransition {
            from: current,
            to: next,
        });
    }
    commit(space, next).await
}

/// embewi-init: starts or resumes the one-time provisioning phase.
pub async fn begin_provisioning(
    space: &LifecycleConfigSpace,
) -> Result<(), LifecycleError> {
    transition(
        space,
        LifecycleState::Factory,
        LifecycleState::Provisioning,
    )
    .await
}

/// embewi-init: all durable prerequisites are present and the first agent
/// may be activated.
pub async fn ready_for_agent(
    space: &LifecycleConfigSpace,
) -> Result<(), LifecycleError> {
    transition(
        space,
        LifecycleState::Provisioning,
        LifecycleState::ReadyForAgent,
    )
    .await
}

/// embewi-agent: called only after the first agent image has been confirmed
/// by FiBeWI and all application prerequisites are valid.
pub async fn production(
    space: &LifecycleConfigSpace,
) -> Result<(), LifecycleError> {
    transition(
        space,
        LifecycleState::ReadyForAgent,
        LifecycleState::Production,
    )
    .await
}
