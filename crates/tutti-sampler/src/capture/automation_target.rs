use serde::{Deserialize, Serialize};
use std::fmt;

/// Trait for types that can serve as automation recording targets.
///
/// Downstream crates implement this for their own target enums so
/// `Recorder<T>` and `Manager<T>` work with any target schema.
pub trait RecordingTarget: Clone + Send + Sync + 'static {
    /// `(min, max, default)` value range for this target.
    fn default_range(&self) -> (f32, f32, f32);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AutomationTarget {
    NodeParam {
        node_id: u64,
        param_index: usize,
        param_name: Option<String>,
    },
    MasterVolume,
    MasterPan,
    Tempo,
    Custom(String),
}

impl AutomationTarget {
    pub fn node_param(node_id: u64, param_index: usize) -> Self {
        Self::NodeParam {
            node_id,
            param_index,
            param_name: None,
        }
    }

    pub fn node_param_named(node_id: u64, param_index: usize, name: impl Into<String>) -> Self {
        Self::NodeParam {
            node_id,
            param_index,
            param_name: Some(name.into()),
        }
    }

    pub fn custom(id: impl Into<String>) -> Self {
        Self::Custom(id.into())
    }

    pub fn key(&self) -> String {
        match self {
            Self::NodeParam {
                node_id,
                param_index,
                ..
            } => format!("node:{node_id}:{param_index}"),
            Self::MasterVolume => "master:volume".to_string(),
            Self::MasterPan => "master:pan".to_string(),
            Self::Tempo => "transport:tempo".to_string(),
            Self::Custom(id) => format!("custom:{id}"),
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::NodeParam {
                node_id,
                param_index,
                param_name,
            } => {
                if let Some(name) = param_name {
                    format!("Node {node_id}: {name}")
                } else {
                    format!("Node {node_id}: Param {param_index}")
                }
            }
            Self::MasterVolume => "Master Volume".to_string(),
            Self::MasterPan => "Master Pan".to_string(),
            Self::Tempo => "Tempo".to_string(),
            Self::Custom(id) => id.clone(),
        }
    }

    pub fn is_node_param(&self) -> bool {
        matches!(self, Self::NodeParam { .. })
    }

    pub fn is_master(&self) -> bool {
        matches!(self, Self::MasterVolume | Self::MasterPan)
    }

    pub fn is_tempo(&self) -> bool {
        matches!(self, Self::Tempo)
    }

    pub fn node_id(&self) -> Option<u64> {
        match self {
            Self::NodeParam { node_id, .. } => Some(*node_id),
            _ => None,
        }
    }

}

impl RecordingTarget for AutomationTarget {
    fn default_range(&self) -> (f32, f32, f32) {
        match self {
            Self::MasterVolume => (0.0, 2.0, 1.0),
            Self::MasterPan => (-1.0, 1.0, 0.0),
            Self::Tempo => (20.0, 300.0, 120.0),
            Self::NodeParam { .. } | Self::Custom(_) => (0.0, 1.0, 0.5),
        }
    }
}

impl fmt::Display for AutomationTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

impl Default for AutomationTarget {
    fn default() -> Self {
        Self::Custom("default".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_param_named() {
        let target = AutomationTarget::node_param_named(42, 0, "cutoff");
        assert_eq!(target.display_name(), "Node 42: cutoff");
    }

    #[test]
    fn test_master_targets() {
        assert!(AutomationTarget::MasterVolume.is_master());
        assert!(AutomationTarget::MasterPan.is_master());
        assert!(!AutomationTarget::Tempo.is_master());
    }

    #[test]
    fn test_unique_keys() {
        let targets = [
            AutomationTarget::node_param(0, 0),
            AutomationTarget::node_param(0, 1),
            AutomationTarget::node_param(1, 0),
            AutomationTarget::MasterVolume,
            AutomationTarget::MasterPan,
            AutomationTarget::Tempo,
            AutomationTarget::custom("my_target"),
        ];

        let keys: std::collections::HashSet<_> = targets.iter().map(|t| t.key()).collect();
        assert_eq!(keys.len(), targets.len(), "All keys should be unique");
    }

    #[test]
    fn test_serialization() {
        let target = AutomationTarget::node_param_named(42, 0, "cutoff");
        let json = serde_json::to_string(&target).unwrap();
        let deserialized: AutomationTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(target, deserialized);
    }
}
